use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};

const HOST_DAEMON_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";

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

    /// Command to run inside the guest.
    #[arg(value_name = "COMMAND", num_args = 0.., allow_hyphen_values = true)]
    command: Vec<OsString>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let command_name = command_name();
    let toplevel = std::env::var("CLAUDEPOD_TOPLEVEL").context("CLAUDEPOD_TOPLEVEL is not set")?;
    let podman = std::env::var("CLAUDEPOD_PODMAN").context("CLAUDEPOD_PODMAN is not set")?;
    let fuse_overlayfs =
        std::env::var("CLAUDEPOD_FUSE_OVERLAYFS").context("CLAUDEPOD_FUSE_OVERLAYFS is not set")?;
    let username = username()?;
    let home = format!("/home/{username}");

    let mode = if args.shell {
        "shell"
    } else {
        default_mode_from_command_name(&command_name)?
    };

    let state_dir = state_dir()?;
    let home_dir = state_dir.join("home");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;
    // Lowerdir for `podman run --rootfs ...:O`; all writes go to podman's
    // temporary overlay upperdir, so this must remain empty.
    let rootfs_dir = state_dir.join("empty-rootfs");
    std::fs::create_dir_all(&rootfs_dir)
        .with_context(|| format!("failed to create {}", rootfs_dir.display()))?;
    if rootfs_dir
        .read_dir()
        .with_context(|| format!("failed to read {}", rootfs_dir.display()))?
        .next()
        .is_some()
    {
        bail!("{} is not empty", rootfs_dir.display());
    }

    let src_root = src_root()?;
    let project_dir = std::env::current_dir().context("failed to get current directory")?;
    let (guest_path, need_project_share) = guest_project_path(&project_dir, &src_root, &username)?;

    let proxy_socket = spawn_nix_proxy()?;

    let mut volumes = vec![
        OsString::from("/nix/store:/nix/store:ro"),
        OsString::from(format!(
            "{}:/nix/.host-nix-daemon/socket",
            proxy_socket.display()
        )),
        OsString::from(format!("{}:{home}", home_dir.display())),
        OsString::from(format!("{}:{home}/src", src_root.display())),
    ];
    if need_project_share {
        volumes.push(OsString::from(format!(
            "{}:{guest_path}",
            project_dir.display()
        )));
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
        .filter(|name| name.to_string_lossy().starts_with("CLAUDE_CODE_"))
        .collect::<Vec<_>>();
    env_names.sort();
    if std::env::var_os("MAX_THINKING_TOKENS").is_some_and(|value| !value.is_empty()) {
        env_names.push(OsString::from("MAX_THINKING_TOKENS"));
    }

    println!("Starting {command_name}...");
    println!("  Host path: {}", project_dir.display());
    println!("  Guest path: {guest_path}");
    println!();

    let init = std::env::current_exe()
        .context("failed to resolve own executable path")?
        .with_file_name("claudepod-init");

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
        .arg(format!("overlay.mount_program={fuse_overlayfs}"));
    for volume in volumes {
        command.arg("-v").arg(volume);
    }
    for name in env_names {
        command.arg("-e").arg(name);
    }
    if args.verbose {
        command.arg("-e").arg("CLAUDEPOD_VERBOSE=1");
    }
    command
        .arg("-e")
        .arg(format!("CLAUDEPOD_USERNAME={username}"))
        .arg("-e")
        .arg(format!("CLAUDEPOD_TOPLEVEL={toplevel}"))
        .arg("-e")
        .arg(format!("CLAUDEPOD_PROJECT_PATH={guest_path}"))
        .arg("-e")
        .arg(format!("CLAUDEPOD_MODE={mode}"))
        .arg(format!("{}:O", rootfs_dir.display()))
        .arg(init)
        .args(args.command);

    Err(command.exec()).context("failed to exec podman")
}

/// Username for the guest passwd entry, from the host user database.
fn username() -> Result<String> {
    let user =
        nix::unistd::User::from_uid(nix::unistd::Uid::current()).context("look up current user")?;
    Ok(user
        .ok_or_else(|| anyhow!("current uid has no passwd entry"))?
        .name)
}

/// Start the nix proxy (see docs/nix-proxy.md) and return the host path of
/// its listening socket, ready to bind-mount into the container.
///
/// The listener is bound here and passed across the exec boundary as an
/// inherited fd, so it is accepting before podman starts — no readiness
/// race. The proxy outlives this process's exec into podman and exits via
/// PR_SET_PDEATHSIG when the podman process dies; it unlinks the socket on
/// the first accepted connection (podman's bind mount pins the inode), so
/// the happy path leaves no host filesystem residue even if the proxy is
/// later SIGKILLed.
fn spawn_nix_proxy() -> Result<PathBuf> {
    if !Path::new(HOST_DAEMON_SOCKET).exists() {
        bail!("host nix daemon socket {HOST_DAEMON_SOCKET} not found; is nix-daemon running?");
    }

    let socket_path = proxy_socket_path()?;
    // Pid reuse could collide with a socket leaked by a SIGKILLed proxy.
    match std::fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to remove stale {}", socket_path.display()));
        }
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to listen on {}", socket_path.display()))?;
    // The guest nix-daemon connects as container root, which keep-id maps
    // to a host subuid — not the socket's owner — and connect() needs write
    // permission on the inode. The 0700 runtime dir still gates host-side
    // access.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o666))
        .with_context(|| format!("failed to chmod {}", socket_path.display()))?;

    // Strip CLOEXEC so the proxy child inherits the listener at the same fd
    // number. Single-threaded, and the listener drops right after spawn, so
    // the cleared flag never leaks into the later exec of podman.
    fcntl(&listener, FcntlArg::F_SETFD(FdFlag::empty()))
        .context("failed to clear CLOEXEC on proxy listener")?;

    let proxy_bin = std::env::current_exe()
        .context("failed to resolve own executable path")?
        .with_file_name("claudepod-nix-proxy");
    Command::new(&proxy_bin)
        .arg("--listen-fd")
        .arg(listener.as_raw_fd().to_string())
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {}", proxy_bin.display()))?;

    Ok(socket_path)
}

/// Per-instance socket path; the pid is unique for the session's lifetime
/// since this process becomes podman via exec. Prefer XDG_RUNTIME_DIR
/// (tmpfs, wiped at logout) so even a SIGKILLed proxy leaves nothing
/// durable.
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
        .with_context(|| format!("failed to create {}", run_dir.display()))?;
    Ok(run_dir.join(name))
}

fn command_name() -> String {
    let argv0 = std::env::args().next().expect("argv[0] is missing");
    Path::new(&argv0)
        .file_name()
        .expect("argv[0] has no file name")
        .to_string_lossy()
        .into_owned()
}

fn default_mode_from_command_name(command_name: &str) -> Result<&'static str> {
    match command_name {
        "claudepod" => Ok("claude"),
        "gptpod" => Ok("codex"),
        other => bail!("unknown command name {other:?}; expected claudepod or gptpod"),
    }
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

fn guest_project_path(
    project_dir: &Path,
    src_root: &Path,
    username: &str,
) -> Result<(String, bool)> {
    if let Ok(rel_path) = project_dir.strip_prefix(src_root) {
        let guest_path = if rel_path.as_os_str().is_empty() {
            format!("/home/{username}/src")
        } else {
            format!("/home/{username}/src/{}", rel_path.display())
        };
        return Ok((guest_path, false));
    }

    let project_name = project_dir
        .file_name()
        .ok_or_else(|| anyhow!("failed to determine project directory name"))?
        .to_string_lossy();
    Ok((format!("/projects/{project_name}"), true))
}
