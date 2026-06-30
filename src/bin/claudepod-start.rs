use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::Write;
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
const HOST_LOCALTIME: &str = "/etc/localtime";
const HOST_ZONEINFO: &str = "/etc/zoneinfo";
const TIMEZONE_ENV: &str = "CLAUDEPOD_TIMEZONE";
const STORE_LAYERS_FILE: &str = "/run/claudepod-store-layers";
const TOPLEVEL_FILE: &str = "/run/claudepod-toplevel";
const STORE_LAYER_MOUNT_DIR: &str = "/nix/.l";
const NIX_RUN_ROOTS_EXPR: &str = r#"
let
  nixpkgs = /. + builtins.getEnv "CLAUDEPOD_NIXPKGS";
  guestSystem = builtins.getEnv "CLAUDEPOD_GUEST_SYSTEM";
  pkgs = import nixpkgs {
    system = guestSystem;
    config = {
      allowUnfree = true;
      allowAliases = false;
    };
  };
  outputPathFor = name:
    let
      result = builtins.tryEval (
        let value = pkgs.${name};
        in if pkgs.lib.isDerivation value then toString value else null
      );
    in
      if result.success && result.value != null
      then [ (builtins.unsafeDiscardStringContext result.value) ]
      else [];
  paths = builtins.concatMap outputPathFor (builtins.attrNames pkgs);
  sortedPaths = builtins.attrNames (builtins.listToAttrs (map (path: {
    name = path;
    value = true;
  }) paths));
in
  if sortedPaths == [] then "" else builtins.concatStringsSep "\n" sortedPaths + "\n"
"#;

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

    /// Build the nix run-root manifest cache before starting.
    #[arg(long)]
    build_nix_run_roots: bool,

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
    let timezone = host_timezone();
    // Bind inherited layers at short guest paths before passing them to the
    // child; overlayfs lowerdir strings are length-bounded.
    let child_layers: Vec<PathBuf> = (0..parent_layers.len())
        .map(|idx| PathBuf::from(STORE_LAYER_MOUNT_DIR).join(idx.to_string()))
        .collect();

    let nix_run_roots = nix_run_roots_manifest(args.build_nix_run_roots)?;
    let proxy_socket = spawn_nix_proxy(nix_run_roots.as_deref())?;

    let mut volumes = vec![
        volume_spec(Path::new("/nix/store"), Path::new("/nix/store"), Some("ro"))?,
        volume_spec(
            &proxy_socket,
            Path::new("/nix/.host-nix-daemon/socket"),
            None,
        )?,
        volume_spec(&home_dir, Path::new(&home), None)?,
    ];
    if let Some(src_root) = &src_root {
        volumes.push(volume_spec(src_root, &Path::new(&home).join("src"), None)?);
    }
    for (host, guest) in parent_layers.iter().zip(&child_layers) {
        volumes.push(volume_spec(host, guest, Some("ro"))?);
    }
    if need_project_share {
        volumes.push(volume_spec(&project_dir, Path::new(&guest_path), None)?);
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
        .filter(|(name, value)| claudepod::agent_env::forwarded(name, value))
        .map(|(name, _value)| name)
        .collect::<Vec<_>>();
    env_names.sort();

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
        "--log-driver=none",
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
    if let Some(timezone) = &timezone {
        command
            .arg("-e")
            .arg(env_arg(TIMEZONE_ENV, timezone.as_os_str()));
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
        .arg(rootfs_spec(&rootfs_dir)?)
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
fn spawn_nix_proxy(nix_run_roots: Option<&Path>) -> Result<PathBuf> {
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
    let mut command = Command::new(&proxy_bin);
    command
        .arg("--listen-fd")
        .arg(listener.as_raw_fd().to_string())
        .arg("--parent-pid")
        .arg(std::process::id().to_string());
    if let Some(path) = nix_run_roots {
        command.arg("--nix-run-roots").arg(path);
    }
    command
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

struct NixRunRootsBuildInputs<'a> {
    guest_system: &'a OsStr,
    nixpkgs: &'a OsStr,
    nix: &'a OsStr,
}

fn nix_run_roots_manifest(build: bool) -> Result<Option<PathBuf>> {
    let guest_system = std::env::var_os("CLAUDEPOD_GUEST_SYSTEM").filter(|value| !value.is_empty());
    let nixpkgs = std::env::var_os("CLAUDEPOD_NIXPKGS").filter(|value| !value.is_empty());
    let (Some(guest_system), Some(nixpkgs)) = (guest_system, nixpkgs) else {
        if build {
            bail!(
                "--build-nix-run-roots requires CLAUDEPOD_GUEST_SYSTEM, CLAUDEPOD_NIXPKGS, and CLAUDEPOD_NIX"
            );
        }
        return Ok(None);
    };

    let path = nix_run_roots_manifest_path(&guest_system, &nixpkgs)?;
    if let Some(path) = load_nix_run_roots_manifest(&path)? {
        return Ok(Some(path));
    }

    if build {
        let nix = required_env_os("CLAUDEPOD_NIX")?;
        build_nix_run_roots_manifest(
            &path,
            &NixRunRootsBuildInputs {
                guest_system: &guest_system,
                nixpkgs: &nixpkgs,
                nix: &nix,
            },
        )?;
        return Ok(Some(path.to_path_buf()));
    }

    print_nix_run_roots_disabled();
    Ok(None)
}

fn load_nix_run_roots_manifest(path: &Path) -> Result<Option<PathBuf>> {
    if !path
        .try_exists()
        .with_context(|| format!("stat {}", path.display()))?
    {
        return Ok(None);
    }

    claudepod::proxy::NixRunRoots::load(path)?;
    Ok(Some(path.to_path_buf()))
}

fn print_nix_run_roots_disabled() {
    eprintln!("nix run root manifest missing; proxy fills disabled");
    eprintln!("run: claudepod --build-nix-run-roots");
}

fn build_nix_run_roots_manifest(path: &Path, inputs: &NixRunRootsBuildInputs<'_>) -> Result<()> {
    let manifest = generate_nix_run_roots_manifest(inputs)?;
    write_nix_run_roots_manifest_atomic(path, &manifest)
}

fn generate_nix_run_roots_manifest(inputs: &NixRunRootsBuildInputs<'_>) -> Result<Vec<u8>> {
    let output = nix_run_roots_command(inputs)?
        .output()
        .with_context(|| format!("run {}", Path::new(inputs.nix).display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!(
                "{} exited with {}",
                Path::new(inputs.nix).display(),
                output.status
            );
        }
        bail!(
            "{} exited with {}: {stderr}",
            Path::new(inputs.nix).display(),
            output.status
        );
    }

    Ok(output.stdout)
}

fn nix_run_roots_command(inputs: &NixRunRootsBuildInputs<'_>) -> Result<Command> {
    validate_nix_run_roots_inputs(inputs)?;
    let mut command = Command::new(inputs.nix);
    command
        .env("CLAUDEPOD_GUEST_SYSTEM", inputs.guest_system)
        .env("CLAUDEPOD_NIXPKGS", inputs.nixpkgs)
        .arg("--extra-experimental-features")
        .arg("nix-command")
        .arg("eval")
        .arg("--impure")
        .arg("--raw")
        .arg("--expr")
        .arg(NIX_RUN_ROOTS_EXPR);
    Ok(command)
}

fn write_nix_run_roots_manifest_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    let (tmp_path, mut tmp_file) = create_manifest_temp_file(path)?;

    let result = (|| -> Result<()> {
        tmp_file
            .write_all(contents)
            .with_context(|| format!("write {}", tmp_path.display()))?;
        tmp_file
            .sync_all()
            .with_context(|| format!("fsync {}", tmp_path.display()))?;
        drop(tmp_file);

        claudepod::proxy::NixRunRoots::load(&tmp_path)?;
        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("rename {} to {}", tmp_path.display(), path.display()))?;
        fsync_dir_best_effort(dir);
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }

    result
}

fn create_manifest_temp_file(path: &Path) -> Result<(PathBuf, File)> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("manifest path has no file name: {}", path.display()))?;

    let mut tmp_name = OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp_path = dir.join(tmp_name);

    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
    {
        Ok(file) => Ok((tmp_path, file)),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&tmp_path)
                .with_context(|| format!("remove stale {}", tmp_path.display()))?;
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
                .with_context(|| format!("create {}", tmp_path.display()))?;
            Ok((tmp_path, file))
        }
        Err(err) => Err(err).with_context(|| format!("create {}", tmp_path.display())),
    }
}

fn fsync_dir_best_effort(dir: &Path) {
    if let Ok(file) = File::open(dir) {
        let _ = file.sync_all();
    }
}

fn validate_nix_run_roots_inputs(inputs: &NixRunRootsBuildInputs<'_>) -> Result<()> {
    claudepod::store_path::validate_direct(Path::new(inputs.nixpkgs)).with_context(|| {
        format!(
            "parse CLAUDEPOD_NIXPKGS={}",
            inputs.nixpkgs.to_string_lossy()
        )
    })?;

    let guest_system = inputs
        .guest_system
        .to_str()
        .ok_or_else(|| anyhow!("CLAUDEPOD_GUEST_SYSTEM is not valid UTF-8"))?;
    if guest_system.is_empty()
        || !guest_system
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        bail!("CLAUDEPOD_GUEST_SYSTEM is not a supported Nix system name");
    }
    Ok(())
}

fn nix_run_roots_manifest_path(guest_system: &OsStr, nixpkgs: &OsStr) -> Result<PathBuf> {
    let relative = nix_run_roots_manifest_relative_path(guest_system, nixpkgs)?;
    xdg::BaseDirectories::with_prefix("claudepod")
        .get_cache_file(relative)
        .ok_or_else(|| anyhow!("HOME is not set and XDG_CACHE_HOME is unavailable"))
}

fn nix_run_roots_manifest_relative_path(guest_system: &OsStr, nixpkgs: &OsStr) -> Result<PathBuf> {
    let guest_system = cache_path_segment("CLAUDEPOD_GUEST_SYSTEM", guest_system)?;
    let nixpkgs_hash = claudepod::store_path::direct_hash(Path::new(nixpkgs))
        .with_context(|| format!("parse CLAUDEPOD_NIXPKGS={}", nixpkgs.to_string_lossy()))?;

    Ok(PathBuf::from("nix-run-roots")
        .join("v1")
        .join(guest_system)
        .join(format!("{nixpkgs_hash}.txt")))
}

fn cache_path_segment<'a>(name: &str, value: &'a OsStr) -> Result<&'a str> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
        bail!("{name} is not a valid cache path segment");
    }
    value
        .to_str()
        .ok_or_else(|| anyhow!("{name} is not valid UTF-8"))
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

fn volume_spec(host: &Path, guest: &Path, options: Option<&str>) -> Result<OsString> {
    reject_colon_path("host volume", host)?;
    reject_colon_path("guest volume", guest)?;
    let mut spec = OsString::from(host.as_os_str());
    spec.push(":");
    spec.push(guest.as_os_str());
    if let Some(options) = options {
        spec.push(":");
        spec.push(options);
    }
    Ok(spec)
}

fn rootfs_spec(rootfs_dir: &Path) -> Result<OsString> {
    reject_colon_path("rootfs", rootfs_dir)?;
    let mut spec = OsString::from(rootfs_dir.as_os_str());
    spec.push(":O");
    Ok(spec)
}

fn reject_colon_path(label: &str, path: &Path) -> Result<()> {
    if path.as_os_str().as_bytes().contains(&b':') {
        bail!(
            "{label} path contains ':' and cannot be encoded as a podman volume spec: {}",
            path.display()
        );
    }
    Ok(())
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

fn host_timezone() -> Option<PathBuf> {
    if let Ok(target) = std::fs::read_link(HOST_LOCALTIME)
        && let Ok(timezone) = target.strip_prefix(HOST_ZONEINFO)
        && !timezone.as_os_str().is_empty()
    {
        return Some(timezone.to_path_buf());
    }

    eprintln!(
        "claudepod-start: host timezone is not a NixOS /etc/zoneinfo symlink; guest will use its default timezone"
    );
    None
}

fn guest_project_path(
    project_dir: &Path,
    src_root: Option<&Path>,
    username: &str,
) -> Result<(String, bool)> {
    if let Some(src_root) = src_root
        && let Ok(rel_path) = project_dir.strip_prefix(src_root)
    {
        let guest_path = if rel_path.as_os_str().is_empty() {
            format!("/home/{username}/src")
        } else {
            format!("/home/{username}/src/{}", rel_path.display())
        };
        return Ok((guest_path, false));
    }

    let project_name = project_dir
        .file_name()
        .ok_or_else(|| anyhow!("project directory has no file name"))?
        .to_string_lossy();
    Ok((format!("/projects/{project_name}"), true))
}

#[cfg(test)]
mod tests {
    use super::{
        NixRunRootsBuildInputs, PortMap, guest_project_path, load_nix_run_roots_manifest,
        nix_run_roots_command, nix_run_roots_manifest_relative_path, pasta_tcp_ns_arg, publish_arg,
        volume_spec, write_nix_run_roots_manifest_atomic,
    };
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn volume_specs_preserve_os_path_components() {
        assert_eq!(
            volume_spec(
                Path::new("/host path"),
                Path::new("/guest path"),
                Some("ro")
            )
            .unwrap(),
            OsStr::new("/host path:/guest path:ro")
        );
        assert!(volume_spec(Path::new("/bad:path"), Path::new("/guest"), None).is_err());
        assert!(volume_spec(Path::new("/host"), Path::new("/bad:guest"), None).is_err());
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

    #[test]
    fn nix_run_roots_manifest_relative_path_uses_store_hashes() {
        assert_eq!(
            nix_run_roots_manifest_relative_path(
                OsStr::new("x86_64-linux"),
                OsStr::new("/nix/store/nixpkgs123-source")
            )
            .unwrap(),
            Path::new("nix-run-roots")
                .join("v1")
                .join("x86_64-linux")
                .join("nixpkgs123.txt")
        );
        assert!(
            nix_run_roots_manifest_relative_path(
                OsStr::new("../bad"),
                OsStr::new("/nix/store/nixpkgs123-source")
            )
            .is_err()
        );
    }

    #[test]
    fn atomic_manifest_write_creates_parent_and_replaces_file() {
        let dir = temp_test_dir("atomic-valid");
        let manifest = dir.join("cache/run-roots.txt");

        write_nix_run_roots_manifest_atomic(
            &manifest,
            b"/nix/store/aaa111-one\n/nix/store/bbb222-two\n",
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&manifest).unwrap(),
            "/nix/store/aaa111-one\n/nix/store/bbb222-two\n"
        );
    }

    #[test]
    fn atomic_manifest_write_validates_before_replace() {
        let dir = temp_test_dir("atomic-invalid");
        let manifest = dir.join("run-roots.txt");
        std::fs::write(&manifest, b"/nix/store/old111-old\n").unwrap();

        assert!(write_nix_run_roots_manifest_atomic(&manifest, b"/tmp/not-store\n").is_err());

        assert_eq!(
            std::fs::read_to_string(&manifest).unwrap(),
            "/nix/store/old111-old\n"
        );
        let entries = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(entries, 1);
    }

    #[test]
    fn manifest_loader_accepts_existing_file() {
        let dir = temp_test_dir("load-existing");
        let manifest = dir.join("run-roots.txt");
        std::fs::write(&manifest, b"/nix/store/aaa111-one\n").unwrap();

        assert_eq!(
            load_nix_run_roots_manifest(&manifest).unwrap(),
            Some(manifest)
        );
    }

    #[test]
    fn manifest_loader_returns_none_when_missing() {
        let dir = temp_test_dir("load-missing");
        let manifest = dir.join("run-roots.txt");

        assert_eq!(load_nix_run_roots_manifest(&manifest).unwrap(), None);
        assert!(!manifest.exists());
    }

    #[test]
    fn nix_run_roots_command_uses_pinned_inputs() {
        let inputs = NixRunRootsBuildInputs {
            guest_system: OsStr::new("x86_64-linux"),
            nixpkgs: OsStr::new("/nix/store/nixpkgs123-source"),
            nix: OsStr::new("/nix/store/nix123-nix/bin/nix"),
        };

        let command = nix_run_roots_command(&inputs).unwrap();

        assert_eq!(
            command.get_program(),
            OsStr::new("/nix/store/nix123-nix/bin/nix")
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            &args[..6],
            [
                "--extra-experimental-features",
                "nix-command",
                "eval",
                "--impure",
                "--raw",
                "--expr"
            ]
        );
        assert!(args[6].contains("builtins.getEnv \"CLAUDEPOD_NIXPKGS\""));
        assert!(args[6].contains("builtins.getEnv \"CLAUDEPOD_GUEST_SYSTEM\""));
        assert!(args[6].contains("allowUnfree = true;"));
        assert!(args[6].contains("allowAliases = false;"));
        assert!(args[6].contains("builtins.unsafeDiscardStringContext"));

        let env = command
            .get_envs()
            .map(|(name, value)| (name, value.unwrap()))
            .collect::<Vec<_>>();
        assert!(env.contains(&(
            OsStr::new("CLAUDEPOD_GUEST_SYSTEM"),
            OsStr::new("x86_64-linux")
        )));
        assert!(env.contains(&(
            OsStr::new("CLAUDEPOD_NIXPKGS"),
            OsStr::new("/nix/store/nixpkgs123-source")
        )));
    }

    #[test]
    fn nix_run_roots_command_rejects_bad_inputs() {
        assert!(
            nix_run_roots_command(&NixRunRootsBuildInputs {
                guest_system: OsStr::new("x86_64-linux\""),
                nixpkgs: OsStr::new("/nix/store/nixpkgs123-source"),
                nix: OsStr::new("/nix/store/nix123-nix/bin/nix"),
            })
            .is_err()
        );
        assert!(
            nix_run_roots_command(&NixRunRootsBuildInputs {
                guest_system: OsStr::new("x86_64-linux"),
                nixpkgs: OsStr::new("nixpkgs123-source"),
                nix: OsStr::new("/nix/store/nix123-nix/bin/nix"),
            })
            .is_err()
        );
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let id = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "claudepod-start-{name}-{}-{nonce}-{id}",
            std::process::id(),
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }
}
