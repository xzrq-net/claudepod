//! Host-side nix daemon proxy: forwards the read-only metadata queries that
//! the guest's local-overlay store makes against its lower store, and
//! loudly rejects everything else. See docs/nix-proxy.md.

use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::net::{UnixListener, UnixStream};

pub mod handshake;
pub mod ops;
pub mod session;
pub mod stderr;
pub mod wire;

/// Accept loop. One relay session per connection; the guest daemon uses a
/// connection pool, so concurrent sessions are normal.
pub async fn serve(listener: UnixListener, upstream: PathBuf) -> Result<()> {
    loop {
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
        let upstream = upstream.clone();
        tokio::spawn(async move {
            if let Err(err) = relay(guest, &upstream).await {
                eprintln!("claudepod-nix-proxy: session failed: {err:#}");
            }
        });
    }
}

async fn relay(guest: UnixStream, upstream: &Path) -> Result<()> {
    let host = UnixStream::connect(upstream).await?;
    let (guest_r, guest_w) = guest.into_split();
    let (host_r, host_w) = host.into_split();
    session::run(guest_r, guest_w, host_r, host_w).await
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
