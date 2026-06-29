//! Host-side proxy speaking the nix daemon wire protocol; forwards an
//! allowlist of read-only metadata queries to the host daemon. See
//! docs/nix-proxy.md.
//!
//! Internal helper spawned by claudepod-start, which binds the listener and
//! passes it as an inherited fd — accepting before podman starts, so there
//! is no readiness race. The socket path (recovered from the fd) is
//! unlinked on the first accepted connection: the container's bind mount
//! pins the inode, and connections can only arrive through that mount, so
//! the first accept proves the host-side name is dead weight. Shutdown
//! unlinks it if no connection ever arrives.

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use nix::sys::signal::Signal;
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};

#[derive(Parser)]
struct Args {
    /// Already-bound listening socket fd inherited from claudepod-start.
    #[arg(long)]
    listen_fd: RawFd,
    /// Exit (SIGTERM via PR_SET_PDEATHSIG) when the process with this pid
    /// dies. claudepod-start passes its own pid, then execs podman — exec
    /// keeps the pid, so proxy lifetime == container session lifetime.
    #[arg(long)]
    parent_pid: i32,
    /// Host nix daemon socket to forward to.
    #[arg(long, default_value = "/nix/var/nix/daemon-socket/socket")]
    upstream: PathBuf,
    /// Nix run-root manifest authorizing future on-demand fills.
    #[arg(long, value_name = "PATH")]
    nix_run_roots: Option<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    // Safety: claudepod-start passes a freshly bound listener fd that
    // nothing else owns.
    let listener = unsafe { StdUnixListener::from_raw_fd(args.listen_fd) };
    let socket_path = listener
        .local_addr()
        .ok()
        .and_then(|addr| addr.as_pathname().map(PathBuf::from));
    let result = run(args, listener, socket_path.clone()).await;
    // Backstop for the no-connection case; a no-op after the first-accept
    // hook has already unlinked.
    if let Some(path) = &socket_path {
        unlink(path);
    }
    result
}

async fn run(args: Args, listener: StdUnixListener, socket_path: Option<PathBuf>) -> Result<()> {
    // Handlers go in before PR_SET_PDEATHSIG: a parent death at any later
    // point lands here (and unlinks the socket) instead of hitting the
    // default disposition mid-startup.
    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;

    nix::sys::prctl::set_pdeathsig(Signal::SIGTERM).context("PR_SET_PDEATHSIG")?;
    // The parent may have died between spawning us and the prctl above, in
    // which case the signal will never fire.
    if nix::unistd::getppid().as_raw() != args.parent_pid {
        bail!("parent {} died before proxy startup", args.parent_pid);
    }

    let _nix_run_roots = args
        .nix_run_roots
        .as_deref()
        .map(claudepod::proxy::NixRunRoots::load)
        .transpose()?;

    listener
        .set_nonblocking(true)
        .context("set listener nonblocking")?;
    let listener = UnixListener::from_std(listener).context("adopt listener fd")?;

    let on_first_accept =
        socket_path.map(|path| Box::new(move || unlink(&path)) as Box<dyn FnOnce() + Send>);

    tokio::select! {
        result = claudepod::proxy::serve(listener, args.upstream, on_first_accept) => result,
        _ = sigterm.recv() => Ok(()),
        _ = sigint.recv() => Ok(()),
    }
}

/// Remove the socket file. NotFound is expected: both the first-accept hook
/// and shutdown unlink, whichever comes second hits nothing.
fn unlink(path: &Path) {
    if let Err(err) = std::fs::remove_file(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("claudepod-nix-proxy: unlink {}: {err}", path.display());
    }
}
