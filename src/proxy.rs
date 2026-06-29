//! Host-side nix daemon proxy: forwards the read-only metadata queries that
//! the guest's local-overlay store makes against its lower store, and
//! loudly rejects everything else. See docs/nix-proxy.md.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

mod handshake;
mod host_client;
mod ops;
mod run_roots;
mod session;
mod stderr;
mod wire;

pub use run_roots::NixRunRoots;

/// Cap on concurrent sessions, and thereby on host daemon connections (the
/// host daemon forks a child per connection). Backpressure, not rejection:
/// excess connections queue for a slot. Sessions map 1:1 to nix clients
/// inside the guest (one pooled lower-store connection per forked guest
/// daemon child), so a legitimate workload rarely needs more than a few.
const MAX_SESSIONS: usize = 32;

/// Accept loop. One relay session per connection; the guest daemon uses a
/// connection pool, so concurrent sessions are normal.
///
/// `on_first_accept` runs after the first successful accept. claudepod-start
/// uses it to unlink the listening socket: podman's bind mount into the
/// container pins the inode, and a connection proves the mount is up, so the
/// host-side name is no longer needed.
pub async fn serve(
    listener: UnixListener,
    upstream: PathBuf,
    mut on_first_accept: Option<Box<dyn FnOnce() + Send>>,
) -> Result<()> {
    let limiter = Arc::new(Semaphore::new(MAX_SESSIONS));
    loop {
        // Backpressure before accept: at capacity, new connections wait in
        // the kernel backlog where they cost this process no fds; once the
        // backlog fills, connect() blocks or fails guest-side. Accepting
        // first and queueing on the permit would let a connect flood
        // exhaust the proxy's fd limit. (Never closed, so acquire can't
        // fail.)
        let permit = limiter.clone().acquire_owned().await.unwrap();
        // Accept errors (e.g. transient EMFILE) must not take down the
        // sessions already running.
        let guest = match listener.accept().await {
            Ok((guest, _)) => guest,
            Err(err) => {
                eprintln!("claudepod-nix-proxy: accept failed: {err}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        if let Some(hook) = on_first_accept.take() {
            hook();
        }
        let upstream = upstream.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(err) = relay(guest, &upstream).await {
                eprintln!("claudepod-nix-proxy: session failed: {err:#}");
            }
        });
    }
}

async fn relay(guest: UnixStream, upstream: &Path) -> Result<()> {
    let (guest_r, guest_w) = guest.into_split();
    session::run(guest_r, guest_w, || async {
        let host = UnixStream::connect(upstream).await?;
        Ok(host.into_split())
    })
    .await
}

#[cfg(test)]
pub(crate) mod testutil {
    pub fn put_u64(buf: &mut Vec<u8>, v: u64) {
        buf.extend(v.to_le_bytes());
    }

    pub fn put_str(buf: &mut Vec<u8>, s: &[u8]) {
        put_u64(buf, s.len() as u64);
        buf.extend(s);
        if !s.len().is_multiple_of(8) {
            buf.extend(&[0u8; 8][..8 - s.len() % 8]);
        }
    }
}
