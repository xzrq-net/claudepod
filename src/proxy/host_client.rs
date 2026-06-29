//! Internal host-daemon client operations used by proxy-owned fills.
//!
//! These ops are never accepted from guest connections. They run on fresh
//! host daemon connections so the relayed guest session framing stays intact.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;

use super::{handshake, stderr, wire};

const OP_ENSURE_PATH: u64 = 10;

#[allow(dead_code)] // Used by the later IsValidPath interception slice.
pub(crate) async fn ensure_path(upstream: &Path, store_path: &Path) -> Result<()> {
    let host = UnixStream::connect(upstream)
        .await
        .with_context(|| format!("connect host daemon {}", upstream.display()))?;
    let (host_r, host_w) = host.into_split();
    ensure_path_on_connection(host_r, host_w, store_path).await
}

async fn ensure_path_on_connection<R, W>(host_r: R, host_w: W, store_path: &Path) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    crate::store_path::validate_direct(store_path)
        .with_context(|| format!("EnsurePath target {}", store_path.display()))?;

    let mut hr = BufReader::new(host_r);
    let mut hw = BufWriter::new(host_w);

    let _ = handshake::upstream(&mut hr, &mut hw)
        .await
        .context("host daemon handshake")?;

    // Client post-handshake fields above the proxy's version floor:
    // obsolete CPU affinity, obsolete reserveSpace.
    wire::write_u64(&mut hw, 0).await?;
    wire::write_u64(&mut hw, 0).await?;
    hw.flush().await?;

    // Daemon handshake info, then greeting stderr.
    wire::read_string(&mut hr, wire::MAX_HOST_STRING)
        .await
        .context("daemon version")?;
    wire::read_u64(&mut hr)
        .await
        .context("daemon trusted flag")?;
    stderr::drain_to_last(&mut hr)
        .await
        .context("daemon greeting")?;

    wire::write_u64(&mut hw, OP_ENSURE_PATH).await?;
    wire::write_string(&mut hw, store_path.as_os_str().as_bytes()).await?;
    hw.flush().await?;

    if let Err(err) = stderr::drain_to_last(&mut hr).await {
        bail!("EnsurePath stderr: {err:#}");
    }
    let success = wire::read_u64(&mut hr).await.context("EnsurePath result")?;
    ensure!(
        success == 1,
        "EnsurePath returned unexpected success word {success}"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::proxy::handshake::{WORKER_MAGIC_1, WORKER_MAGIC_2};
    use crate::proxy::testutil::{put_str, put_u64};

    const VERSION_1_38: u64 = 0x126;

    fn host_greeting() -> Vec<u8> {
        let mut buf = Vec::new();
        put_u64(&mut buf, WORKER_MAGIC_2);
        put_u64(&mut buf, VERSION_1_38);
        put_u64(&mut buf, 0); // daemon features
        put_str(&mut buf, b"2.34.7");
        put_u64(&mut buf, 2); // trusted: NotTrusted
        put_u64(&mut buf, stderr::STDERR_LAST);
        buf
    }

    fn put_error(buf: &mut Vec<u8>, msg: &[u8]) {
        put_u64(buf, stderr::STDERR_ERROR);
        put_str(buf, b"Error");
        put_u64(buf, 0);
        put_str(buf, b"Error");
        put_str(buf, msg);
        put_u64(buf, 0); // no position
        put_u64(buf, 0); // no traces
    }

    #[tokio::test]
    async fn ensure_path_success_sends_expected_wire() {
        let path = Path::new("/nix/store/aaa111-one");
        let mut script = host_greeting();
        put_u64(&mut script, stderr::STDERR_LAST);
        put_u64(&mut script, 1);

        let mut sent = Vec::new();
        let mut input = script.as_slice();
        ensure_path_on_connection(&mut input, &mut sent, path)
            .await
            .unwrap();

        let mut expected = Vec::new();
        put_u64(&mut expected, WORKER_MAGIC_1);
        put_u64(&mut expected, VERSION_1_38);
        put_u64(&mut expected, 0); // our empty feature list
        put_u64(&mut expected, 0); // obsolete CPU affinity
        put_u64(&mut expected, 0); // obsolete reserveSpace
        put_u64(&mut expected, OP_ENSURE_PATH);
        put_str(&mut expected, path.as_os_str().as_bytes());
        assert_eq!(sent, expected);
    }

    #[tokio::test]
    async fn ensure_path_daemon_error_becomes_err_with_log_text() {
        let mut script = host_greeting();
        put_u64(&mut script, stderr::STDERR_NEXT);
        put_str(&mut script, b"copying missing path\n");
        put_error(&mut script, b"cannot build missing path");

        let mut sent = Vec::new();
        let mut input = script.as_slice();
        let err =
            ensure_path_on_connection(&mut input, &mut sent, Path::new("/nix/store/aaa111-one"))
                .await
                .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("cannot build missing path"), "{text}");
        assert!(text.contains("copying missing path"), "{text}");
    }
}
