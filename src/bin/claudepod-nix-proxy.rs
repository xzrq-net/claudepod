//! Host-side proxy speaking the nix daemon wire protocol; forwards an
//! allowlist of read-only metadata queries to the host daemon. See
//! docs/nix-proxy.md.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::UnixListener;

#[derive(Parser)]
struct Args {
    /// Unix socket to listen on.
    #[arg(long)]
    listen: PathBuf,
    /// Host nix daemon socket to forward to.
    #[arg(long, default_value = "/nix/var/nix/daemon-socket/socket")]
    upstream: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let listener = UnixListener::bind(&args.listen)
        .with_context(|| format!("failed to listen on {}", args.listen.display()))?;
    claudepod::proxy::serve(listener, args.upstream).await
}
