//! Container entrypoint, run by podman as pid 1.
//!
//! Sets up the writable nix store overlay and writes runtime configuration
//! for the guest systemd services, then hands off to NixOS init.
//!
//! Configuration arrives via environment variables set by claudepod-start
//! (CLAUDEPOD_TOPLEVEL, CLAUDEPOD_USERNAME, CLAUDEPOD_PROJECT_PATH,
//! CLAUDEPOD_MODE, CLAUDEPOD_TIMEZONE, CLAUDEPOD_VERBOSE, CLAUDEPOD_USB, and
//! explicit agent environment selected by claudepod-start); the agent command
//! arrives as argv.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use claudepod::store_layers;
use nix::mount::{MsFlags, mount};

const RUNTIME_UID: u64 = 1000;
const SUBID_DELEGATE_START: u64 = RUNTIME_UID + 1;
const TIMEZONE_ENV: &str = "CLAUDEPOD_TIMEZONE";
const STORE_UPPER_DIR: &str = "/nix/.rw-store/store";
const USB_ENV: &str = "CLAUDEPOD_USB";
// Host /dev is bound here by claudepod-start --usb. USB class drivers create
// nodes directly in /dev with a fixed name and a hotplug-assigned number, so
// the canonical names below are pre-linked into the live host tree.
const HOST_DEV_SUBDIR: &str = "host";
const USB_DEVICE_CLASSES: [&str; 2] = ["hidraw", "ttyUSB"];
// Kernel HIDRAW_MAX_DEVICES; a desktop with a few HID devices and Bluetooth
// peripherals already sits in the 20s.
const USB_DEVICE_LINKS_PER_CLASS: u32 = 64;

fn main() {
    if let Err(err) = run() {
        eprintln!("claudepod-entry: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let system = std::env::var_os("CLAUDEPOD_TOPLEVEL").context("CLAUDEPOD_TOPLEVEL is not set")?;
    let username = std::env::var("CLAUDEPOD_USERNAME").context("CLAUDEPOD_USERNAME is not set")?;
    let command: Vec<OsString> = std::env::args_os().skip(1).collect();

    let descendant_store_layers = setup_store_overlay().context("mount setup")?;
    write_runtime_config(&system, &username, &command, &descendant_store_layers)
        .context("write runtime config")?;
    setup_localtime().context("setup localtime")?;
    if std::env::var_os(USB_ENV).is_some_and(|v| !v.is_empty()) {
        create_usb_device_links(Path::new("/dev")).context("create USB device links")?;
    }

    let mut init_path = system;
    init_path.push("/init");
    let mut init = Command::new(init_path);
    // Podman normally sets this via its built-in default env, but --rootfs
    // skips that path. NixOS stage 2 uses it to avoid bare-metal boot steps.
    if std::env::var_os("container").is_none() {
        init.env("container", "podman");
    }
    // Early guest boot produces a burst of /proc/self/mountinfo changes.
    // systemd's default mount monitor ratelimit is only 5 events per second,
    // and mount start jobs are held while it is ratelimited; avoid the fixed
    // startup stall before /run/wrappers.mount without disabling the guard
    // entirely.
    init.env("SYSTEMD_DEFAULT_MOUNT_RATE_LIMIT_BURST", "1000");
    Err(init.exec()).context("exec NixOS init")
}

/// Set up the writable /nix/store overlay and return the layer stack for nested
/// launches: this container's writable upper first, followed by inherited lower
/// layers.
fn setup_store_overlay() -> Result<OsString> {
    let env = store_layers::STORE_LAYERS_ENV;
    let raw = std::env::var_os(env)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{env} is not set"))?;
    let lower_layers = store_layers::parse(&raw).with_context(|| format!("parse {env}"))?;

    std::fs::create_dir_all("/nix/.rw-store").context("create /nix/.rw-store")?;
    mount(
        Some("none"),
        "/nix/.rw-store",
        Some("tmpfs"),
        MsFlags::empty(),
        Some("mode=755"),
    )
    .context("mount tmpfs on /nix/.rw-store")?;
    std::fs::create_dir_all(STORE_UPPER_DIR).context("create overlay upper dir")?;
    std::fs::create_dir_all("/nix/.rw-store/work").context("create overlay work dir")?;

    let mut overlay_options = OsString::from("lowerdir=");
    overlay_options.push(store_layers::join(&lower_layers));
    overlay_options.push(",upperdir=");
    overlay_options.push(STORE_UPPER_DIR);
    overlay_options.push(",workdir=/nix/.rw-store/work,userxattr");
    mount(
        Some("overlay"),
        "/nix/store",
        Some("overlay"),
        MsFlags::empty(),
        Some(overlay_options.as_os_str()),
    )
    .context("mount overlay on /nix/store")?;

    let mut descendant_layers = vec![PathBuf::from(STORE_UPPER_DIR)];
    descendant_layers.extend(lower_layers);
    Ok(store_layers::join(&descendant_layers))
}

fn setup_localtime() -> Result<()> {
    let Some(timezone) = std::env::var_os(TIMEZONE_ENV).filter(|value| !value.is_empty()) else {
        return Ok(());
    };

    let target = Path::new("/etc/zoneinfo").join(timezone);
    match std::fs::remove_file("/etc/localtime") {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("remove /etc/localtime"),
    }
    symlink(&target, "/etc/localtime")
        .with_context(|| format!("create /etc/localtime -> {}", target.display()))
}

/// Dangling-by-design symlinks /dev/<class><n> -> host/<class><n>. sysfs
/// enumeration only ever names nodes that exist on the host, so libraries that
/// derive "/dev/" + DEVNAME resolve to the live host node, across replugs, with
/// no hotplug events needed.
fn create_usb_device_links(dev: &Path) -> Result<()> {
    for class in USB_DEVICE_CLASSES {
        for n in 0..USB_DEVICE_LINKS_PER_CLASS {
            let name = format!("{class}{n}");
            let target = Path::new(HOST_DEV_SUBDIR).join(&name);
            let link = dev.join(&name);
            symlink(&target, &link)
                .with_context(|| format!("create {} -> {}", link.display(), target.display()))?;
        }
    }
    Ok(())
}

/// Project path, mode, agent command, and explicit agent environment, written
/// under /run where the claudepod-shell unit picks them up across the
/// systemd boundary.
fn write_runtime_config(
    system: &OsStr,
    username: &str,
    command: &[OsString],
    descendant_store_layers: &OsStr,
) -> Result<()> {
    std::fs::write("/run/claudepod-username", format!("{username}\n"))
        .context("write /run/claudepod-username")?;

    let mut project = required_env_bytes("CLAUDEPOD_PROJECT_PATH")?;
    project.push(b'\n');
    std::fs::write("/run/claudepod-project", project).context("write /run/claudepod-project")?;

    let mut mode = required_env_bytes("CLAUDEPOD_MODE")?;
    mode.push(b'\n');
    std::fs::write("/run/claudepod-mode", mode).context("write /run/claudepod-mode")?;

    std::fs::write(
        "/run/claudepod-subuid",
        subid_file_from_map_path("/proc/self/uid_map")?,
    )
    .context("write /run/claudepod-subuid")?;
    std::fs::write(
        "/run/claudepod-subgid",
        subid_file_from_map_path("/proc/self/gid_map")?,
    )
    .context("write /run/claudepod-subgid")?;

    let mut args = Vec::new();
    for arg in command {
        args.extend_from_slice(arg.as_bytes());
        args.push(0);
    }
    std::fs::write("/run/claudepod-command", args).context("write /run/claudepod-command")?;

    // Layer stack for nested claudepod-start. This is launcher plumbing, not
    // agent configuration, so keep it out of /run/claudepod-env.
    let mut store_layers = descendant_store_layers.as_bytes().to_vec();
    store_layers.push(b'\n');
    std::fs::write("/run/claudepod-store-layers", store_layers)
        .context("write /run/claudepod-store-layers")?;

    // System toplevel for nested claudepod-start. The in-guest launcher leaves
    // CLAUDEPOD_TOPLEVEL unset and reads this explicit store path instead.
    let mut toplevel = system.as_bytes().to_vec();
    toplevel.push(b'\n');
    std::fs::write("/run/claudepod-toplevel", toplevel).context("write /run/claudepod-toplevel")?;

    // The guest service reads this via `set -a; . file; set +a`, so values
    // are bash single-quoted.
    let mut env = Vec::new();
    for name in agent_env_names()? {
        if let Some(value) = std::env::var_os(&name) {
            append_env_line(&mut env, &name, &value);
        }
    }
    std::fs::write("/run/claudepod-env", env).context("write /run/claudepod-env")?;

    if std::env::var_os("CLAUDEPOD_VERBOSE").is_some_and(|v| !v.is_empty()) {
        std::fs::create_dir_all("/run/systemd/system.conf.d")
            .context("create /run/systemd/system.conf.d")?;
        std::fs::write(
            "/run/systemd/system.conf.d/50-claudepod-verbose.conf",
            "[Manager]\nShowStatus=yes\n",
        )
        .context("write systemd verbose config")?;
    }

    Ok(())
}

fn agent_env_names() -> Result<Vec<OsString>> {
    let raw = std::env::var_os(claudepod::agent_env::NAMES_ENV);
    agent_env_names_from_raw(raw.as_deref())
}

fn agent_env_names_from_raw(raw: Option<&OsStr>) -> Result<Vec<OsString>> {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(Vec::new());
    };

    let mut names = Vec::new();
    for name_bytes in raw.as_bytes().split(|byte| *byte == b'\n') {
        let name = OsString::from_vec(name_bytes.to_vec());
        if !claudepod::agent_env::is_shell_identifier(&name) {
            bail!(
                "{} contains invalid variable name {}",
                claudepod::agent_env::NAMES_ENV,
                name.to_string_lossy()
            );
        }
        names.push(name);
    }
    Ok(names)
}

fn append_env_line(out: &mut Vec<u8>, name: &OsStr, value: &OsStr) {
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b"='");
    for &byte in value.as_bytes() {
        if byte == b'\'' {
            out.extend_from_slice(br"'\''");
        } else {
            out.push(byte);
        }
    }
    out.extend_from_slice(b"'\n");
}

fn required_env_bytes(name: &str) -> Result<Vec<u8>> {
    Ok(std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{name} is not set"))?
        .as_bytes()
        .to_vec())
}

fn subid_file_from_map_path(path: &str) -> Result<String> {
    let map = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    subid_file_from_map(&map)
}

fn subid_file_from_map(map: &str) -> Result<String> {
    let mut out = String::new();

    // /etc/subuid and /etc/subgid are namespace-local here: nested
    // newuidmap/newgidmap requests parent IDs from this namespace, so use the
    // first column of our map and delegate only IDs above the runtime user.
    for (line_idx, line) in map.lines().enumerate() {
        let line_no = line_idx + 1;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let mut fields = line.split_whitespace();
        let inside = parse_map_field(fields.next(), line_no, "inside id")?;
        let _parent = parse_map_field(fields.next(), line_no, "parent id")?;
        let count = parse_map_field(fields.next(), line_no, "count")?;
        if fields.next().is_some() {
            bail!("uid/gid map line {line_no} has extra fields");
        }

        let end = inside
            .checked_add(count)
            .with_context(|| format!("uid/gid map line {line_no} overflows"))?;
        let start = inside.max(SUBID_DELEGATE_START);
        if start < end {
            writeln!(out, "{RUNTIME_UID}:{start}:{}", end - start)
                .expect("writing to String cannot fail");
        }
    }

    Ok(out)
}

fn parse_map_field(field: Option<&str>, line_no: usize, name: &str) -> Result<u64> {
    field
        .with_context(|| format!("uid/gid map line {line_no} missing {name}"))?
        .parse()
        .with_context(|| format!("uid/gid map line {line_no} invalid {name}"))
}

#[cfg(test)]
mod tests {
    use super::{
        agent_env_names_from_raw, append_env_line, create_usb_device_links, subid_file_from_map,
    };
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    #[test]
    fn usb_device_links_point_into_host_dev() {
        let dev = std::env::temp_dir().join(format!(
            "claudepod-entry-usb-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir(&dev).unwrap();
        create_usb_device_links(&dev).unwrap();
        assert_eq!(
            std::fs::read_link(dev.join("hidraw0")).unwrap(),
            Path::new("host/hidraw0")
        );
        assert_eq!(
            std::fs::read_link(dev.join("ttyUSB63")).unwrap(),
            Path::new("host/ttyUSB63")
        );
        assert!(std::fs::symlink_metadata(dev.join("ttyUSB64")).is_err());
        assert_eq!(std::fs::read_dir(&dev).unwrap().count(), 128);
        // Dangling until the host creates the node; that's the point.
        assert!(!dev.join("hidraw0").exists());
        std::fs::remove_dir_all(&dev).unwrap();
    }

    #[test]
    fn subid_file_from_outer_keep_id_map() {
        let map = "\
0 100000 1000
1000 1000 1
1001 101000 64536
";
        assert_eq!(subid_file_from_map(map).unwrap(), "1000:1001:64536\n");
    }

    #[test]
    fn subid_file_shrinks_after_one_nested_level() {
        let map = "\
0 1001 1000
1000 1000 1
1001 2001 63536
";
        assert_eq!(subid_file_from_map(map).unwrap(), "1000:1001:63536\n");
    }

    #[test]
    fn subid_file_clips_ranges_below_runtime_user() {
        let map = "\
0 100000 500
500 100500 1000
";
        assert_eq!(subid_file_from_map(map).unwrap(), "1000:1001:499\n");
    }

    #[test]
    fn subid_file_rejects_malformed_maps() {
        assert!(subid_file_from_map("0 100000\n").is_err());
        assert!(subid_file_from_map("0 100000 1 extra\n").is_err());
        assert!(subid_file_from_map("0 nope 1\n").is_err());
    }

    #[test]
    fn agent_env_names_parse_newline_list() {
        assert_eq!(
            agent_env_names_from_raw(None).unwrap(),
            Vec::<OsString>::new()
        );
        assert_eq!(
            agent_env_names_from_raw(Some(OsStr::new("FOO\nBAR"))).unwrap(),
            [OsString::from("FOO"), OsString::from("BAR")]
        );
        assert!(agent_env_names_from_raw(Some(OsStr::new("BAD-NAME"))).is_err());
    }

    #[test]
    fn env_lines_are_sourceable_by_bash() {
        let mut out = Vec::new();
        append_env_line(&mut out, OsStr::new("FOO"), OsStr::from_bytes(b"a'b\nc"));

        assert_eq!(out, b"FOO='a'\\''b\nc'\n");
    }
}
