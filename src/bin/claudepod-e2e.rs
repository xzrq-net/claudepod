//! End-to-end test for the nix proxy (docs/nix-proxy.md "Testing").
//!
//! Run with `cargo run --bin claudepod-e2e`. Re-execs itself under
//! user+mount+pid namespaces, builds a host store and an overlayfs
//! "container" store on tmpfs, runs a real nix-daemon for each, with the
//! proxy in between as the guest's lower store.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use nix::mount::{MsFlags, mount};
use tokio::net::UnixListener;
use tokio::process::{Child, Command};

const REEXEC_GUARD: &str = "CLAUDEPOD_E2E_REEXEC";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // unshare --pid --fork makes us pid 1 in the fresh namespace; anything
    // else means we haven't re-exec'd yet.
    if std::process::id() == 1 {
        run().await
    } else if std::env::var_os(REEXEC_GUARD).is_some() {
        bail!("re-exec under unshare did not make us pid 1");
    } else {
        reexec_under_unshare().await
    }
}

async fn reexec_under_unshare() -> Result<()> {
    let exe = std::env::current_exe()?;
    let status = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--pid", "--fork", "--mount-proc", "--"])
        .arg(exe)
        .env(REEXEC_GUARD, "1")
        .status()
        .await
        .context("failed to re-exec under unshare")?;
    std::process::exit(status.code().unwrap_or(1));
}

struct Env {
    /// Logical store dir, shared by every store in the test. Also the host
    /// store's physical dir.
    store: PathBuf,
    home: PathBuf,
    conf: PathBuf,
    host_state: PathBuf,
    host_socket: PathBuf,
    proxy_socket: PathBuf,
    /// Read-only bind mount of the host store, the overlay's lowerdir —
    /// stands in for the `/nix/store:ro` volume in the real setup.
    guest_lower: PathBuf,
    guest_root: PathBuf,
    guest_upper: PathBuf,
    guest_state: PathBuf,
    guest_socket: PathBuf,
}

/// Run the named `Fixture` test methods in order, stopping at the first failure.
macro_rules! run_tests {
    ($fixture:expr, $($test:ident),* $(,)?) => {
        $(
            step(concat!("test: ", stringify!($test)));
            $fixture.$test().await.context(concat!("test failed: ", stringify!($test)))?;
        )*
    };
}

async fn run() -> Result<()> {
    let fixture = Fixture::setup().await?;
    run_tests!(fixture, query_host_path);
    step("PASS");
    Ok(())
}

struct Fixture {
    env: Env,
    /// Store path seeded into the host store, absent from the guest's upper db.
    host_path: String,
    _host_daemon: Child,
    _guest_daemon: Child,
}

impl Fixture {
    async fn setup() -> Result<Fixture> {
        step("set up filesystems");
        let env = Env::setup().context("filesystem setup")?;

        step("start host nix-daemon");
        let host_daemon = env.spawn_daemon(&env.host_state, &env.host_socket, None).await?;

        step("seed host store");
        let seed = env.home.join("seed");
        std::fs::write(&seed, "claudepod e2e seed\n")?;
        let out = run_cmd(
            env.nix_cmd(
                "nix-store",
                &env.host_state,
                &format!("unix://{}", env.host_socket.display()),
            )
            .arg("--add")
            .arg(&seed),
        )
        .await?;
        let host_path = out.trim().to_owned();
        ensure!(
            Path::new(&host_path).exists(),
            "seed path {host_path} missing from host store"
        );
        eprintln!("seeded {host_path}");

        step("start proxy");
        let listener = UnixListener::bind(&env.proxy_socket)?;
        let upstream = env.host_socket.clone();
        tokio::spawn(async move {
            if let Err(err) = claudepod::proxy::serve(listener, upstream).await {
                eprintln!("proxy died: {err:#}");
            }
        });

        step("start guest nix-daemon");
        let guest_store_uri = format!(
            "local-overlay://?root={}&upper-layer={}&check-mount=false&lower-store={}",
            env.guest_root.display(),
            env.guest_upper.display(),
            urlencode(&format!("unix://{}", env.proxy_socket.display())),
        );
        let guest_daemon = env
            .spawn_daemon(&env.guest_state, &env.guest_socket, Some(&guest_store_uri))
            .await?;

        Ok(Fixture {
            env,
            host_path,
            _host_daemon: host_daemon,
            _guest_daemon: guest_daemon,
        })
    }

    /// Client command against the guest daemon.
    fn guest_cmd(&self, program: &str) -> Command {
        self.env.nix_cmd(
            program,
            &self.env.guest_state,
            &format!("unix://{}", self.env.guest_socket.display()),
        )
    }

    async fn query_host_path(&self) -> Result<()> {
        run_cmd(self.guest_cmd("nix").arg("path-info").arg(&self.host_path)).await?;
        Ok(())
    }
}

impl Env {
    fn setup() -> Result<Env> {
        let root = PathBuf::from("/tmp/claudepod-e2e");
        let env = Env {
            store: root.join("store"),
            home: root.join("home"),
            conf: root.join("etc"),
            host_state: root.join("host/var/nix"),
            host_socket: root.join("host/var/nix/daemon-socket/socket"),
            proxy_socket: root.join("proxy.sock"),
            guest_lower: root.join("guest/lower"),
            guest_root: root.join("guest/root"),
            guest_upper: root.join("guest/upper"),
            guest_state: root.join("guest/var/nix"),
            guest_socket: root.join("guest/var/nix/daemon-socket/socket"),
        };

        std::fs::create_dir_all(&root)?;
        mount(Some("tmpfs"), &root, Some("tmpfs"), MsFlags::empty(), None::<&str>)
            .context("mount tmpfs")?;

        let guest_work = root.join("guest/work");
        // The merged overlay must sit at root + logical store dir, where the
        // local-overlay store expects its real store.
        let guest_merged = env.guest_root.join(env.store.strip_prefix("/")?);
        for dir in [
            &env.store,
            &env.home,
            &env.conf,
            &env.host_state,
            &env.guest_lower,
            &env.guest_upper,
            &guest_work,
            &env.guest_state,
            &guest_merged,
        ] {
            std::fs::create_dir_all(dir)?;
        }

        // Empty trusted-users makes even root untrusted, so daemon
        // connections look like the real setup, where the proxy connects to
        // the host daemon as a plain user.
        std::fs::write(
            env.conf.join("nix.conf"),
            "experimental-features = nix-command local-overlay-store read-only-local-store\n\
             sandbox = false\n\
             build-users-group =\n\
             substituters =\n\
             trusted-users =\n",
        )?;

        mount(Some(&env.store), &env.guest_lower, None::<&str>, MsFlags::MS_BIND, None::<&str>)
            .context("bind mount lower store")?;
        mount(
            None::<&str>,
            &env.guest_lower,
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY,
            None::<&str>,
        )
        .context("remount lower store read-only")?;

        let overlay_opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            env.guest_lower.display(),
            env.guest_upper.display(),
            guest_work.display(),
        );
        mount(
            Some("overlay"),
            &guest_merged,
            Some("overlay"),
            MsFlags::empty(),
            Some(overlay_opts.as_str()),
        )
        .context("mount overlayfs")?;

        Ok(env)
    }

    /// Common nix process environment: clean slate, shared store dir and
    /// config, per-side state dir and store URI.
    fn nix_cmd(&self, program: &str, state: &Path, remote: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("NIX_STORE_DIR", &self.store)
            .env("NIX_CONF_DIR", &self.conf)
            .env("NIX_USER_CONF_FILES", "")
            .env("NIX_STATE_DIR", state)
            .env("NIX_LOG_DIR", state.join("log"))
            .env("NIX_REMOTE", remote);
        cmd
    }

    async fn spawn_daemon(
        &self,
        state: &Path,
        socket: &Path,
        store_uri: Option<&str>,
    ) -> Result<Child> {
        let mut cmd = self.nix_cmd("nix-daemon", state, store_uri.unwrap_or("local"));
        eprintln!("+ {}", fmt_cmd(&cmd));
        let mut child = cmd.spawn().context("failed to spawn nix-daemon")?;

        let ready = async {
            while !socket.exists() {
                if let Some(status) = child.try_wait()? {
                    bail!("nix-daemon exited during startup: {status}");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(())
        };
        tokio::time::timeout(Duration::from_secs(10), ready)
            .await
            .with_context(|| format!("timed out waiting for {}", socket.display()))??;
        Ok(child)
    }
}

async fn run_cmd(cmd: &mut Command) -> Result<String> {
    let display = fmt_cmd(cmd);
    eprintln!("+ {display}");
    let out = cmd
        .output()
        .await
        .with_context(|| format!("failed to run {display}"))?;
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    ensure!(out.status.success(), "command failed with {}", out.status);
    Ok(String::from_utf8(out.stdout)?)
}

fn fmt_cmd(cmd: &Command) -> String {
    let cmd = cmd.as_std();
    let remote = cmd
        .get_envs()
        .find(|(name, _)| *name == "NIX_REMOTE")
        .and_then(|(_, value)| value)
        .map(|value| format!("NIX_REMOTE={} ", value.to_string_lossy()))
        .unwrap_or_default();
    let argv = std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    format!("{remote}{argv}")
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn step(name: &str) {
    eprintln!("\n=== {name} ===");
}
