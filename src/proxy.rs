use std::path::PathBuf;

use anyhow::Result;
use tokio::net::UnixListener;

/// Stub: accepts connections and immediately drops them, so a client sees EOF
/// instead of a hang.
pub async fn serve(listener: UnixListener, _upstream: PathBuf) -> Result<()> {
    loop {
        let (_conn, _) = listener.accept().await?;
    }
}
