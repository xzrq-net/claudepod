//! End-to-end test for the nix proxy.
//!
//! Run with `cargo run --bin claudepod-e2e`. Re-execs itself under
//! user+mount+pid namespaces, builds a host store and an overlayfs
//! "container" store on tmpfs, runs a real nix-daemon for each, with the
//! proxy in between as the guest's lower store.

use std::collections::BTreeSet;
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
        .args([
            "--user",
            "--map-root-user",
            "--mount",
            "--pid",
            "--fork",
            "--mount-proc",
            "--",
        ])
        .arg(exe)
        .env(REEXEC_GUARD, "1")
        .status()
        .await
        .context("re-exec under unshare")?;
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
    guest_upper: PathBuf,
    guest_state: PathBuf,
    guest_socket: PathBuf,
    /// Wrapper script that enters the "container" view: a mount namespace
    /// with the merged overlay bound over the logical store dir, mirroring
    /// the real setup where the container sees the merged store at
    /// /nix/store. Everything guest-side (daemon, clients, builders) must
    /// run under it — nix clients are LocalFSStores that read store files
    /// directly at the logical path.
    guest_ns: PathBuf,
}

/// Run the named `Fixture` test methods in order, stopping at the first failure.
macro_rules! run_tests {
    ($fixture:expr, $($test:ident),* $(,)?) => {
        $(
            step(concat!("test: ", stringify!($test)));
            $fixture.$test().await.context(concat!("test: ", stringify!($test)))?;
        )*
    };
}

async fn run() -> Result<()> {
    let fixture = Fixture::setup().await?;
    run_tests!(
        fixture,
        query_host_path,
        closure_sync,
        guest_build_with_host_deps,
        build_dedup,
        invalid_then_valid,
        demand_sweep,
        guest_gc,
        desync_repair,
    );
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
        let host_daemon = env
            .spawn_daemon(
                env.nix_cmd("nix-daemon", &env.host_state, "local"),
                &env.host_socket,
            )
            .await?;

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
            if let Err(err) = claudepod::proxy::serve(listener, upstream, None).await {
                eprintln!("proxy died: {err:#}");
            }
        });

        step("start guest nix-daemon");
        // No root/real params: inside the guest namespace the merged
        // overlay shadows the logical store dir, so the real store is the
        // logical store — same as the real container, where the merged
        // store sits at /nix/store.
        let guest_store_uri = format!(
            "local-overlay://?upper-layer={}&check-mount=false&lower-store={}",
            env.guest_upper.display(),
            urlencode(&format!("unix://{}", env.proxy_socket.display())),
        );
        let guest_daemon = env
            .spawn_daemon(
                env.guest_ns_cmd("nix-daemon", &env.guest_state, &guest_store_uri),
                &env.guest_socket,
            )
            .await?;

        Ok(Fixture {
            env,
            host_path,
            _host_daemon: host_daemon,
            _guest_daemon: guest_daemon,
        })
    }

    /// Client command against the guest daemon, run inside the guest mount
    /// namespace like everything else guest-side.
    fn guest_cmd(&self, program: &str) -> Command {
        self.env.guest_ns_cmd(
            program,
            &self.env.guest_state,
            &format!("unix://{}", self.env.guest_socket.display()),
        )
    }

    /// Client command against the host daemon.
    fn host_cmd(&self, program: &str) -> Command {
        self.env.nix_cmd(
            program,
            &self.env.host_state,
            &format!("unix://{}", self.env.host_socket.display()),
        )
    }

    /// `nix-build` a one-echo derivation; returns the output path. `dep`
    /// (a store path) is pulled in via `builtins.storePath` and echoed into
    /// the output, so the reference scanner registers it as a runtime
    /// reference. Deterministic: same name + dep on either side produces
    /// the same drv and output path.
    async fn build(
        &self,
        nix_build: Command,
        name: &str,
        dep: Option<&str>,
        out_link: Option<&Path>,
    ) -> Result<String> {
        let expr = match dep {
            Some(dep) => format!(
                "let dep = builtins.storePath \"{dep}\"; in \
                 derivation {{ name = \"{name}\"; system = builtins.currentSystem; \
                 builder = \"/bin/sh\"; args = [ \"-c\" \"echo {name} ${{dep}} > $out\" ]; }}"
            ),
            None => format!(
                "derivation {{ name = \"{name}\"; system = builtins.currentSystem; \
                 builder = \"/bin/sh\"; args = [ \"-c\" \"echo {name} > $out\" ]; }}"
            ),
        };
        let mut cmd = nix_build;
        cmd.arg("-E").arg(expr);
        match out_link {
            Some(link) => cmd.arg("-o").arg(link),
            None => cmd.arg("--no-out-link"),
        };
        Ok(run_cmd(&mut cmd).await?.trim().to_owned())
    }

    /// Predict the store path `nix-store --add <file>` will produce without
    /// registering it anywhere the test stores can see: add it to a
    /// throwaway local store sharing the logical store dir. Returns the
    /// logical path and its physical location inside the throwaway store.
    async fn predict_add_path(&self, file: &Path, tag: &str) -> Result<(String, PathBuf)> {
        let scratch = self.env.home.join(format!("scratch-{tag}"));
        let state = scratch.join("var/nix");
        std::fs::create_dir_all(&state)?;
        let out = run_cmd(
            self.env
                .nix_cmd(
                    "nix-store",
                    &state,
                    &format!("local://?root={}", scratch.display()),
                )
                .arg("--add")
                .arg(file),
        )
        .await?;
        let logical = out.trim().to_owned();
        // With `root` set, the physical store is `<root>/nix/store`
        // regardless of the logical store dir (local-fs-store.hh
        // `realStoreDir`).
        let physical = scratch
            .join("nix/store")
            .join(Path::new(&logical).file_name().unwrap());
        ensure!(physical.exists(), "scratch store add left no file");
        Ok((logical, physical))
    }

    async fn query_host_path(&self) -> Result<()> {
        run_cmd(self.guest_cmd("nix").arg("path-info").arg(&self.host_path)).await?;
        Ok(())
    }

    /// Friction: a guest query of a host closure root must pull the whole
    /// reference chain across the proxy into the upper db (local-overlay
    /// closure sync), not just the queried path — and the synced metadata
    /// must agree with the host db edge for edge.
    async fn closure_sync(&self) -> Result<()> {
        let b = self
            .build(
                self.host_cmd("nix-build"),
                "sync-b",
                Some(&self.host_path),
                None,
            )
            .await?;
        let c = self
            .build(self.host_cmd("nix-build"), "sync-c", Some(&b), None)
            .await?;
        run_cmd(self.guest_cmd("nix").args(["path-info", &c])).await?;
        let closure = run_cmd(self.guest_cmd("nix-store").args(["-qR", &c])).await?;
        let got: BTreeSet<&str> = closure.lines().collect();
        let want: BTreeSet<&str> = [self.host_path.as_str(), &b, &c].into();
        ensure!(got == want, "closure mismatch: got {got:?}, want {want:?}");
        Ok(())
    }

    /// Friction: a guest build whose output references a host path crosses
    /// the layer boundary twice — eval-time `builtins.storePath` validity
    /// goes through the proxy, and the reference scanner must register an
    /// upper→lower edge in the guest db.
    async fn guest_build_with_host_deps(&self) -> Result<()> {
        let out = self
            .build(
                self.guest_cmd("nix-build"),
                "guest-leaf",
                Some(&self.host_path),
                None,
            )
            .await?;
        let refs = run_cmd(
            self.guest_cmd("nix-store")
                .args(["-q", "--references", &out]),
        )
        .await?;
        ensure!(
            refs.lines().any(|l| l == self.host_path),
            "guest build output does not reference its host dep: {refs}"
        );
        Ok(())
    }

    /// Friction: rebuilding something the host already has. The output path
    /// is already valid in the lower store, so the guest must dedup — skip
    /// the build and not copy the output into the upper layer.
    async fn build_dedup(&self) -> Result<()> {
        let host_out = self
            .build(self.host_cmd("nix-build"), "dedup", None, None)
            .await?;
        let guest_out = self
            .build(self.guest_cmd("nix-build"), "dedup", None, None)
            .await?;
        ensure!(
            host_out == guest_out,
            "same drv built different paths: host {host_out}, guest {guest_out}"
        );
        let base = Path::new(&guest_out).file_name().unwrap();
        ensure!(
            !self.env.guest_upper.join(base).exists(),
            "deduped output was copied into the upper layer"
        );
        Ok(())
    }

    /// Friction: "I installed it on the host, why doesn't the pod see it."
    /// A path queried (and found invalid) through the guest must become
    /// visible as soon as the host registers it — no stale negative
    /// anywhere in the daemon/proxy/daemon chain.
    async fn invalid_then_valid(&self) -> Result<()> {
        let seed = self.env.home.join("seed-iv");
        std::fs::write(&seed, "invalid then valid\n")?;
        let (predicted, _) = self.predict_add_path(&seed, "iv").await?;
        run_cmd_fail(self.guest_cmd("nix").args(["path-info", &predicted])).await?;
        let actual = run_cmd(self.host_cmd("nix-store").arg("--add").arg(&seed)).await?;
        ensure!(
            actual.trim() == predicted,
            "scratch store predicted {predicted}, host added {}",
            actual.trim()
        );
        run_cmd(self.guest_cmd("nix").args(["path-info", &predicted])).await?;
        Ok(())
    }

    /// Innocuous read-only commands a guest user plausibly runs. Success
    /// means local-overlay's demand on the lower store stayed within the
    /// proxy allowlist — a rejection fails the command loudly.
    async fn demand_sweep(&self) -> Result<()> {
        let b = self
            .build(
                self.host_cmd("nix-build"),
                "sync-b",
                Some(&self.host_path),
                None,
            )
            .await?;
        let c = self
            .build(self.host_cmd("nix-build"), "sync-c", Some(&b), None)
            .await?;
        run_cmd(
            self.guest_cmd("nix")
                .args(["path-info", "-r", "--json", &c]),
        )
        .await?;
        run_cmd(
            self.guest_cmd("nix")
                .args(["path-info", "--closure-size", &c]),
        )
        .await?;
        run_cmd(self.guest_cmd("nix-store").args(["-q", "--references", &c])).await?;
        run_cmd(
            self.guest_cmd("nix-store")
                .args(["-q", "--referrers", &self.host_path]),
        )
        .await?;
        run_cmd(self.guest_cmd("nix-store").args(["-q", "--deriver", &c])).await?;
        run_cmd(self.guest_cmd("nix").args(["store", "ls", &self.host_path])).await?;
        Ok(())
    }

    /// Friction: GC in the guest walks the merged store dir, which lists
    /// every host path. It must drop unrooted upper paths and leave lower
    /// paths alone — no whiteout storm, no disallowed ops on the proxy.
    /// Observed but not asserted: gc also drops synced upper-db entries
    /// for lower paths (they re-sync on the next query), logging a nix
    /// "BUG: cannot delete ... in use" line for some while carrying on.
    async fn guest_gc(&self) -> Result<()> {
        let keep = self
            .build(
                self.guest_cmd("nix-build"),
                "gc-keep",
                Some(&self.host_path),
                Some(&self.env.home.join("gc-root")),
            )
            .await?;
        let drop = self
            .build(self.guest_cmd("nix-build"), "gc-drop", None, None)
            .await?;
        run_cmd(self.guest_cmd("nix-store").arg("--gc")).await?;
        run_cmd(self.guest_cmd("nix").args(["path-info", &keep])).await?;
        run_cmd_fail(self.guest_cmd("nix").args(["path-info", &drop])).await?;
        // The lower store survived: valid per the host db, files on disk.
        run_cmd(self.host_cmd("nix").args(["path-info", &self.host_path])).await?;
        ensure!(
            Path::new(&self.host_path).exists(),
            "guest gc deleted a lower store path from disk"
        );
        Ok(())
    }

    /// The README "fchmodat" condition, manufactured without the WAL: a
    /// path's files sit in the lower layer but no db knows them, and the
    /// guest re-adds the same content, forcing nix to delete the impostor
    /// before writing ("path exists but is invalid"). The files appeared
    /// *after* the overlay was mounted — production reality, the host
    /// store changes under a live container — which overlayfs treats as
    /// undefined behavior. Observed: the copy-up/unlink fails with EIO,
    /// i.e. the guest cannot self-repair this state. Pin that, and pin
    /// what matters: the lower store is untouched and the guest daemon
    /// survives. If a kernel change ever makes the reconcile succeed,
    /// this test fails loudly and the premise gets re-examined.
    async fn desync_repair(&self) -> Result<()> {
        // A directory, not a file: deletePath only chmods directories, and
        // the chmod (fchmodat) on read-only lower entries is the syscall
        // the original failure mode came from.
        let seed = self.env.home.join("desync-dir");
        std::fs::create_dir_all(&seed)?;
        std::fs::write(seed.join("inner"), "desync\n")?;
        let (predicted, physical) = self.predict_add_path(&seed, "desync").await?;

        // Plant the files in the host store dir, bypassing every db.
        run_cmd(
            Command::new("cp")
                .arg("-a")
                .arg(&physical)
                .arg(&self.env.store),
        )
        .await?;
        run_cmd_fail(self.guest_cmd("nix").args(["path-info", &predicted])).await?;

        let stderr = run_cmd_fail(self.guest_cmd("nix-store").arg("--add").arg(&seed)).await?;
        ensure!(
            stderr.contains("cannot unlink"),
            "expected the reconcile to fail deleting the impostor, got: {stderr}"
        );

        // The planted lower files are untouched, the path is still
        // invalid, and the daemon still serves queries.
        let base = Path::new(&predicted).file_name().unwrap();
        ensure!(
            self.env.store.join(base).join("inner").exists(),
            "failed repair damaged the lower store"
        );
        run_cmd_fail(self.guest_cmd("nix").args(["path-info", &predicted])).await?;
        run_cmd(self.guest_cmd("nix").args(["path-info", &self.host_path])).await?;
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
            guest_upper: root.join("guest/upper"),
            guest_state: root.join("guest/var/nix"),
            guest_socket: root.join("guest/var/nix/daemon-socket/socket"),
            guest_ns: root.join("guest-ns.sh"),
        };

        std::fs::create_dir_all(&root)?;
        mount(
            Some("tmpfs"),
            &root,
            Some("tmpfs"),
            MsFlags::empty(),
            None::<&str>,
        )
        .context("mount tmpfs")?;

        // Read-only bind mount of the host store, the overlay's lowerdir —
        // stands in for the `/nix/store:ro` volume in the real setup.
        let guest_lower = root.join("guest/lower");
        let guest_work = root.join("guest/work");
        let guest_merged = root.join("guest/root/nix/store");
        for dir in [
            &env.store,
            &env.home,
            &env.conf,
            &env.host_state,
            &guest_lower,
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
            "experimental-features = nix-command local-overlay-store\n\
             sandbox = false\n\
             build-users-group =\n\
             require-drop-supplementary-groups = false\n\
             substituters =\n\
             trusted-users =\n",
        )?;

        mount(
            Some(&env.store),
            &guest_lower,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .context("bind mount lower store")?;
        mount(
            None::<&str>,
            &guest_lower,
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY,
            None::<&str>,
        )
        .context("remount lower store read-only")?;

        let overlay_opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            guest_lower.display(),
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

        std::fs::write(
            &env.guest_ns,
            format!(
                "#!/bin/sh\nset -e\nmount --bind {} {}\nexec \"$@\"\n",
                guest_merged.display(),
                env.store.display(),
            ),
        )?;
        std::fs::set_permissions(
            &env.guest_ns,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;

        Ok(env)
    }

    /// Common nix process environment: clean slate, shared store dir and
    /// config, per-side state dir and store URI.
    fn nix_cmd(&self, program: &str, state: &Path, remote: &str) -> Command {
        let mut cmd = Command::new(program);
        self.apply_nix_env(&mut cmd, state, remote);
        cmd
    }

    /// Like `nix_cmd`, but run inside the guest mount namespace (see
    /// `guest_ns`), where the merged overlay shadows the logical store dir.
    fn guest_ns_cmd(&self, program: &str, state: &Path, remote: &str) -> Command {
        let mut cmd = Command::new("unshare");
        cmd.args(["--mount", "--"]).arg(&self.guest_ns).arg(program);
        self.apply_nix_env(&mut cmd, state, remote);
        cmd
    }

    fn apply_nix_env(&self, cmd: &mut Command, state: &Path, remote: &str) {
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("NIX_STORE_DIR", &self.store)
            .env("NIX_CONF_DIR", &self.conf)
            .env("NIX_USER_CONF_FILES", "")
            .env("NIX_STATE_DIR", state)
            .env("NIX_LOG_DIR", state.join("log"))
            .env("NIX_REMOTE", remote);
    }

    async fn spawn_daemon(&self, mut cmd: Command, socket: &Path) -> Result<Child> {
        eprintln!("+ {}", fmt_cmd(&cmd));
        let mut child = cmd.spawn().context("spawn nix-daemon")?;

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

/// Run a command that is expected to fail; returns its stderr.
async fn run_cmd_fail(cmd: &mut Command) -> Result<String> {
    let display = fmt_cmd(cmd);
    eprintln!("+ {display} (expecting failure)");
    let out = cmd
        .output()
        .await
        .with_context(|| format!("run {display}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    eprint!("{stderr}");
    ensure!(
        !out.status.success(),
        "command unexpectedly succeeded: {display}"
    );
    Ok(stderr)
}

async fn run_cmd(cmd: &mut Command) -> Result<String> {
    let display = fmt_cmd(cmd);
    eprintln!("+ {display}");
    let out = cmd
        .output()
        .await
        .with_context(|| format!("run {display}"))?;
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
