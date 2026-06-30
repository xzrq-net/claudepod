//! Container entrypoint, run by podman as pid 1.
//!
//! Sets up the writable nix store overlay and writes runtime configuration
//! for the guest systemd services, then hands off to NixOS init.
//!
//! Configuration arrives via environment variables set by claudepod-start
//! (CLAUDEPOD_TOPLEVEL, CLAUDEPOD_USERNAME, CLAUDEPOD_PROJECT_PATH,
//! CLAUDEPOD_MODE, CLAUDEPOD_TIMEZONE, CLAUDEPOD_VERBOSE,
//! CLAUDE_CODE_*); the agent command arrives as argv.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
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

/// Project path, mode, agent command, and forwarded environment, written
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
    for (name, value) in std::env::vars_os() {
        if claudepod::agent_env::forwarded(&name, &value) {
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
    use super::subid_file_from_map;

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
}
