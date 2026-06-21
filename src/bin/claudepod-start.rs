use std::ffi::{OsStr, OsString};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use claudepod::store_layers;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};

const HOST_DAEMON_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";
const STORE_LAYERS_FILE: &str = "/run/claudepod-store-layers";
const TOPLEVEL_FILE: &str = "/run/claudepod-toplevel";
const STORE_LAYER_MOUNT_DIR: &str = "/nix/.l";

#[derive(Debug, Parser)]
#[command(disable_version_flag = true, trailing_var_arg = true)]
struct Args {
    /// Start a login shell instead of the default agent mode.
    #[arg(short = 's')]
    shell: bool,

    /// Verbose mode: show systemd boot messages in the guest.
    #[arg(short = 'V')]
    verbose: bool,

    /// Mount path, or host:guest volume spec, into the guest.
    #[arg(short = 'v', value_name = "SPEC")]
    extra_volumes: Vec<OsString>,

    /// Forward guest localhost port to host localhost port: PORT or GUEST:HOST.
    #[arg(long, value_name = "PORT|GUEST:HOST")]
    host_port: Vec<PortMap>,

    /// Publish a guest port on host localhost: PORT or HOST:GUEST.
    #[arg(short = 'p', long, value_name = "PORT|HOST:GUEST")]
    publish: Vec<PortMap>,

    /// Use DIR as the guest home backing directory and do not mount host ~/src.
    #[arg(long, value_name = "DIR")]
    sandbox_home: Option<PathBuf>,

    /// Command to run inside the guest.
    #[arg(value_name = "COMMAND", num_args = 0.., allow_hyphen_values = true)]
    command: Vec<OsString>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let command_name = command_name();
    let toplevel = toplevel()?;
    let podman = required_env_os("CLAUDEPOD_PODMAN")?;
    let fuse_overlayfs = required_env_os("CLAUDEPOD_FUSE_OVERLAYFS")?;
    let username = username()?;
    let home = format!("/home/{username}");

    let mode = if args.shell {
        "shell"
    } else {
        default_mode_from_command_name(&command_name)?
    };

    let state_dir = state_dir()?;
    let home_dir = if let Some(path) = &args.sandbox_home {
        absolute_path(path)?
    } else {
        state_dir.join("home")
    };
    std::fs::create_dir_all(&home_dir).with_context(|| format!("create {}", home_dir.display()))?;
    // Lowerdir for `podman run --rootfs ...:O`; all writes go to podman's
    // temporary overlay upperdir, so this must remain empty.
    let rootfs_dir = state_dir.join("empty-rootfs");
    std::fs::create_dir_all(&rootfs_dir)
        .with_context(|| format!("create {}", rootfs_dir.display()))?;
    if rootfs_dir
        .read_dir()
        .with_context(|| format!("read {}", rootfs_dir.display()))?
        .next()
        .is_some()
    {
        bail!("{} is not empty", rootfs_dir.display());
    }

    let project_dir = std::env::current_dir().context("current directory")?;
    let src_root = if args.sandbox_home.is_some() {
        None
    } else {
        Some(src_root()?)
    };
    let (guest_path, need_project_share) =
        guest_project_path(&project_dir, src_root.as_deref(), &username)?;
    let parent_layers = parent_store_layers()?;
    // Bind inherited layers at short guest paths before passing them to the
    // child; overlayfs lowerdir strings are length-bounded.
    let child_layers: Vec<PathBuf> = (0..parent_layers.len())
        .map(|idx| PathBuf::from(STORE_LAYER_MOUNT_DIR).join(idx.to_string()))
        .collect();

    let proxy_socket = spawn_nix_proxy()?;

    let mut volumes = vec![
        volume_spec(Path::new("/nix/store"), Path::new("/nix/store"), Some("ro")),
        volume_spec(
            &proxy_socket,
            Path::new("/nix/.host-nix-daemon/socket"),
            None,
        ),
        volume_spec(&home_dir, Path::new(&home), None),
    ];
    if let Some(src_root) = &src_root {
        volumes.push(volume_spec(src_root, &Path::new(&home).join("src"), None));
    }
    for (host, guest) in parent_layers.iter().zip(&child_layers) {
        volumes.push(volume_spec(host, guest, Some("ro")));
    }
    if need_project_share {
        volumes.push(volume_spec(&project_dir, Path::new(&guest_path), None));
    }
    for spec in args.extra_volumes {
        if spec.as_encoded_bytes().contains(&b':') {
            volumes.push(spec);
        } else {
            let mut vol = spec.clone();
            vol.push(":");
            vol.push(spec);
            volumes.push(vol);
        }
    }

    let mut env_names = std::env::vars_os()
        .map(|(name, _value)| name)
        .filter(|name| name.as_bytes().starts_with(b"CLAUDE_CODE_"))
        .collect::<Vec<_>>();
    env_names.sort();
    if std::env::var_os("MAX_THINKING_TOKENS").is_some_and(|value| !value.is_empty()) {
        env_names.push(OsString::from("MAX_THINKING_TOKENS"));
    }

    println!("Starting {}...", command_name.to_string_lossy());
    println!("  Host path: {}", project_dir.display());
    println!("  Guest path: {guest_path}");
    if args.sandbox_home.is_some() {
        println!("  Sandbox home: {}", home_dir.display());
        println!("  Host src: disabled");
    }
    for port in &args.host_port {
        println!(
            "  Guest localhost:{} -> host localhost:{}",
            port.left, port.right
        );
    }
    for port in &args.publish {
        println!(
            "  Host localhost:{} -> guest port {}",
            port.left, port.right
        );
    }
    println!();

    let init = std::env::current_exe()
        .context("resolve own executable path")?
        .with_file_name("claudepod-entry");

    let mut command = Command::new(&podman);
    command.args([
        "run",
        "--rm",
        "-it",
        "--userns=keep-id:uid=1000,gid=100",
        "--user",
        "0:0",
        "--cap-add=SYS_ADMIN",
        "--cap-add=NET_ADMIN",
        "--cap-add=NET_RAW",
        "--cap-add=SYS_PTRACE",
        "--device",
        "/dev/fuse",
        "--device",
        "/dev/net/tun",
        "--rootfs",
        "--systemd=always",
        "--no-hostname",
        "--no-hosts",
        "--dns=none",
        "--pids-limit=16384",
        "--security-opt",
        "unmask=ALL",
    ]);
    // Podman's rootless native overlay teardown can block for seconds at
    // shutdown. Force the rootfs :O overlay through fuse-overlayfs instead;
    // hot paths are bind mounts or the guest's tmpfs-backed /nix/store overlay.
    command
        .arg("--storage-opt")
        .arg(env_arg("overlay.mount_program", &fuse_overlayfs));
    if !args.host_port.is_empty() {
        command
            .arg("--network")
            .arg(pasta_tcp_ns_arg(&args.host_port));
    }
    for port in &args.publish {
        command.arg("-p").arg(publish_arg(port));
    }
    for volume in volumes {
        command.arg("-v").arg(volume);
    }
    for name in env_names {
        command.arg("-e").arg(name);
    }
    if args.verbose {
        command.arg("-e").arg("CLAUDEPOD_VERBOSE=1");
    }
    command.arg("-e").arg(env_arg(
        store_layers::STORE_LAYERS_ENV,
        &store_layers::join(&child_layers),
    ));
    command
        .arg("-e")
        .arg(format!("CLAUDEPOD_USERNAME={username}"))
        .arg("-e")
        .arg(env_arg("CLAUDEPOD_TOPLEVEL", &toplevel))
        .arg("-e")
        .arg(format!("CLAUDEPOD_PROJECT_PATH={guest_path}"))
        .arg("-e")
        .arg(format!("CLAUDEPOD_MODE={mode}"))
        .arg(rootfs_spec(&rootfs_dir))
        .arg(init)
        .args(args.command);

    Err(command.exec()).context("exec podman")
}

/// Username for the guest passwd entry, from the host user database.
fn username() -> Result<String> {
    let user =
        nix::unistd::User::from_uid(nix::unistd::Uid::current()).context("look up current user")?;
    Ok(user
        .ok_or_else(|| anyhow!("current uid has no passwd entry"))?
        .name)
}

/// Start the nix proxy and return the host path of its listening socket, ready
/// to bind-mount into the container.
///
/// The listener is bound here and inherited by the proxy child, so it can
/// accept before podman starts.
fn spawn_nix_proxy() -> Result<PathBuf> {
    if !Path::new(HOST_DAEMON_SOCKET).exists() {
        bail!("host nix daemon socket {HOST_DAEMON_SOCKET} not found; is nix-daemon running?");
    }

    let socket_path = proxy_socket_path()?;
    // Remove any stale pid-named socket left by a crashed proxy.
    match std::fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("remove stale {}", socket_path.display()));
        }
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("listen on {}", socket_path.display()))?;
    // The guest nix-daemon connects as container root, which keep-id maps
    // to a host subuid — not the socket's owner — and connect() needs write
    // permission on the inode. The 0700 runtime dir still gates host-side
    // access.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o666))
        .with_context(|| format!("chmod {}", socket_path.display()))?;

    // Strip CLOEXEC so the proxy child inherits the listener at the same fd
    // number. Single-threaded, and the listener drops right after spawn, so
    // the cleared flag never leaks into the later exec of podman.
    fcntl(&listener, FcntlArg::F_SETFD(FdFlag::empty()))
        .context("clear CLOEXEC on proxy listener")?;

    let proxy_bin = std::env::current_exe()
        .context("resolve own executable path")?
        .with_file_name("claudepod-nix-proxy");
    Command::new(&proxy_bin)
        .arg("--listen-fd")
        .arg(listener.as_raw_fd().to_string())
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {}", proxy_bin.display()))?;

    Ok(socket_path)
}

/// Per-instance socket path. Prefer XDG_RUNTIME_DIR; fall back to a private
/// state dir where stale sockets are removed before binding.
fn proxy_socket_path() -> Result<PathBuf> {
    let name = format!("nix-proxy-{}.sock", std::process::id());
    let dirs = xdg::BaseDirectories::with_prefix("claudepod");
    if let Ok(path) = dirs.place_runtime_file(&name) {
        return Ok(path);
    }
    let run_dir = state_dir()?.join("run");
    // 0700 like XDG_RUNTIME_DIR: the socket inside is world-writable.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&run_dir)
        .with_context(|| format!("create {}", run_dir.display()))?;
    Ok(run_dir.join(name))
}

fn command_name() -> OsString {
    let argv0 = std::env::args_os().next().expect("argv[0] is missing");
    Path::new(&argv0)
        .file_name()
        .expect("argv[0] has no file name")
        .to_os_string()
}

fn default_mode_from_command_name(command_name: &OsStr) -> Result<&'static str> {
    if command_name == OsStr::new("claudepod") {
        Ok("claude")
    } else if command_name == OsStr::new("gptpod") {
        Ok("codex")
    } else {
        bail!(
            "unknown command name {:?}; expected claudepod or gptpod",
            command_name.to_string_lossy()
        )
    }
}

fn required_env_os(name: &str) -> Result<OsString> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{name} is not set"))
}

/// System toplevel store path to boot in the container. Host launchers set
/// CLAUDEPOD_TOPLEVEL; nested launchers read the path written by the parent
/// entrypoint.
fn toplevel() -> Result<OsString> {
    if let Some(value) = std::env::var_os("CLAUDEPOD_TOPLEVEL").filter(|value| !value.is_empty()) {
        return Ok(value);
    }
    let raw = std::fs::read(TOPLEVEL_FILE).with_context(|| format!("read {TOPLEVEL_FILE}"))?;
    let trimmed = raw.strip_suffix(b"\n").unwrap_or(&raw);
    if trimmed.is_empty() {
        bail!("{TOPLEVEL_FILE} is empty");
    }
    Ok(OsStr::from_bytes(trimmed).to_os_string())
}

fn state_dir() -> Result<PathBuf> {
    xdg::BaseDirectories::with_prefix("claudepod")
        .get_data_home()
        .ok_or_else(|| anyhow!("HOME is not set and XDG_DATA_HOME is unavailable"))
}

fn src_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join("src"))
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("current directory")?
            .join(path))
    }
}

fn volume_spec(host: &Path, guest: &Path, options: Option<&str>) -> OsString {
    let mut spec = OsString::from(host.as_os_str());
    spec.push(":");
    spec.push(guest.as_os_str());
    if let Some(options) = options {
        spec.push(":");
        spec.push(options);
    }
    spec
}

fn rootfs_spec(rootfs_dir: &Path) -> OsString {
    let mut spec = OsString::from(rootfs_dir.as_os_str());
    spec.push(":O");
    spec
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct PortMap {
    left: u16,
    right: u16,
}

impl FromStr for PortMap {
    type Err = String;

    fn from_str(spec: &str) -> std::result::Result<Self, Self::Err> {
        if let Some((left, right)) = spec.split_once(':') {
            if right.contains(':') {
                return Err("expected PORT or LEFT:RIGHT".to_string());
            }
            Ok(Self {
                left: parse_port(left)?,
                right: parse_port(right)?,
            })
        } else {
            let port = parse_port(spec)?;
            Ok(Self {
                left: port,
                right: port,
            })
        }
    }
}

fn parse_port(raw: &str) -> std::result::Result<u16, String> {
    let port: u16 = raw
        .parse()
        .map_err(|_| format!("{raw:?} is not a valid TCP port"))?;
    if port == 0 {
        Err("port 0 is not supported".to_string())
    } else {
        Ok(port)
    }
}

fn port_map_spec(port: &PortMap) -> String {
    if port.left == port.right {
        port.left.to_string()
    } else {
        format!("{}:{}", port.left, port.right)
    }
}

fn pasta_tcp_ns_arg(ports: &[PortMap]) -> OsString {
    let mut arg = String::from("pasta");
    for (idx, port) in ports.iter().enumerate() {
        if idx == 0 {
            arg.push_str(":-T,");
        } else {
            arg.push_str(",-T,");
        }
        arg.push_str(&port_map_spec(port));
    }
    OsString::from(arg)
}

fn publish_arg(port: &PortMap) -> String {
    format!("127.0.0.1:{}:{}", port.left, port.right)
}

fn env_arg(name: &str, value: &OsStr) -> OsString {
    let mut arg = OsString::from(name);
    arg.push("=");
    arg.push(value);
    arg
}

/// Store layers to hand the child, in priority order (highest first). Nested
/// launchers read the parent's flattened stack; top-level launchers use only
/// the host store.
fn parent_store_layers() -> Result<Vec<PathBuf>> {
    let raw = match std::fs::read(STORE_LAYERS_FILE) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(vec![PathBuf::from("/nix/store")]);
        }
        Err(err) => return Err(err).with_context(|| format!("read {STORE_LAYERS_FILE}")),
    };
    let trimmed = raw.strip_suffix(b"\n").unwrap_or(&raw);
    store_layers::parse(OsStr::from_bytes(trimmed))
        .with_context(|| format!("parse {STORE_LAYERS_FILE}"))
}

fn guest_project_path(
    project_dir: &Path,
    src_root: Option<&Path>,
    username: &str,
) -> Result<(String, bool)> {
    if let Some(src_root) = src_root {
        if let Ok(rel_path) = project_dir.strip_prefix(src_root) {
            let guest_path = if rel_path.as_os_str().is_empty() {
                format!("/home/{username}/src")
            } else {
                format!("/home/{username}/src/{}", rel_path.display())
            };
            return Ok((guest_path, false));
        }
    }

    let project_name = project_dir
        .file_name()
        .ok_or_else(|| anyhow!("project directory has no file name"))?
        .to_string_lossy();
    Ok((format!("/projects/{project_name}"), true))
}

#[cfg(test)]
mod tests {
    use super::{PortMap, guest_project_path, pasta_tcp_ns_arg, publish_arg, volume_spec};
    use std::ffi::OsStr;
    use std::path::Path;

    #[test]
    fn volume_specs_preserve_os_path_components() {
        assert_eq!(
            volume_spec(
                Path::new("/host path"),
                Path::new("/guest path"),
                Some("ro")
            ),
            OsStr::new("/host path:/guest path:ro")
        );
    }

    #[test]
    fn projects_under_src_use_src_mount_by_default() {
        assert_eq!(
            guest_project_path(
                Path::new("/host/home/src/proj"),
                Some(Path::new("/host/home/src")),
                "alice"
            )
            .unwrap(),
            ("/home/alice/src/proj".to_string(), false)
        );
    }

    #[test]
    fn projects_under_src_are_mounted_separately_without_src_mount() {
        assert_eq!(
            guest_project_path(Path::new("/host/home/src/proj"), None, "alice").unwrap(),
            ("/projects/proj".to_string(), true)
        );
    }

    #[test]
    fn port_maps_parse_single_or_pair() {
        assert_eq!(
            "3000".parse::<PortMap>().unwrap(),
            PortMap {
                left: 3000,
                right: 3000
            }
        );
        assert_eq!(
            "8080:3000".parse::<PortMap>().unwrap(),
            PortMap {
                left: 8080,
                right: 3000
            }
        );
        assert!("0".parse::<PortMap>().is_err());
        assert!("8080:".parse::<PortMap>().is_err());
        assert!("1:2:3".parse::<PortMap>().is_err());
    }

    #[test]
    fn port_maps_render_podman_args() {
        let ports = [
            PortMap {
                left: 15432,
                right: 5432,
            },
            PortMap {
                left: 3000,
                right: 3000,
            },
        ];
        assert_eq!(
            pasta_tcp_ns_arg(&ports),
            OsStr::new("pasta:-T,15432:5432,-T,3000")
        );
        assert_eq!(publish_arg(&ports[0]), "127.0.0.1:15432:5432");
        assert_eq!(publish_arg(&ports[1]), "127.0.0.1:3000:3000");
    }
}
