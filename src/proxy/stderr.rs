//! The daemon-to-client stderr message loop: relay between op arguments and
//! op result, plus the synthetic error used to reject ops.
//!
//! `STDERR_READ`/`STDERR_WRITE` only occur for ops that stream data through
//! the client connection; every such op is rejected by policy, so here they
//! are protocol violations.

use anyhow::{Result, bail, ensure};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::wire;

pub const STDERR_NEXT: u64 = 0x6f6c6d67;
pub const STDERR_READ: u64 = 0x64617461;
pub const STDERR_WRITE: u64 = 0x64617416;
pub const STDERR_LAST: u64 = 0x616c7473;
pub const STDERR_ERROR: u64 = 0x63787470;
pub const STDERR_START_ACTIVITY: u64 = 0x53545254;
pub const STDERR_STOP_ACTIVITY: u64 = 0x53544f50;
pub const STDERR_RESULT: u64 = 0x52534c54;

/// How a relayed stderr exchange ended. After `Last` the op's result
/// payload follows; after `Error` the op is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminal {
    Last,
    Error,
}

/// Relay stderr messages host-to-guest until a terminal message.
pub async fn relay<R, W>(host: &mut R, guest: &mut W) -> Result<Terminal>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        // Don't forward the message word until it's known to be relayable,
        // so a violation never leaves the guest mid-message.
        let msg = wire::read_u64(host).await?;
        match msg {
            STDERR_LAST => {
                wire::write_u64(guest, msg).await?;
                return Ok(Terminal::Last);
            }
            STDERR_ERROR => {
                wire::write_u64(guest, msg).await?;
                copy_error(host, guest).await?;
                return Ok(Terminal::Error);
            }
            STDERR_NEXT => {
                wire::write_u64(guest, msg).await?;
                wire::copy_string(host, guest, wire::MAX_HOST_STRING).await?;
            }
            STDERR_START_ACTIVITY => {
                wire::write_u64(guest, msg).await?;
                wire::copy_u64(host, guest).await?; // activity id
                wire::copy_u64(host, guest).await?; // verbosity
                wire::copy_u64(host, guest).await?; // activity type
                wire::copy_string(host, guest, wire::MAX_HOST_STRING).await?; // text
                copy_fields(host, guest).await?;
                wire::copy_u64(host, guest).await?; // parent activity id
            }
            STDERR_STOP_ACTIVITY => {
                wire::write_u64(guest, msg).await?;
                wire::copy_u64(host, guest).await?; // activity id
            }
            STDERR_RESULT => {
                wire::write_u64(guest, msg).await?;
                wire::copy_u64(host, guest).await?; // activity id
                wire::copy_u64(host, guest).await?; // result type
                copy_fields(host, guest).await?;
            }
            // The violating word has not been forwarded, so the guest is at
            // a message boundary and can still parse a synthetic error.
            // Failures further down (mid-payload) must NOT synthesize one.
            STDERR_READ | STDERR_WRITE => {
                let err =
                    format!("host daemon sent streaming stderr message {msg:#x} during a query op");
                let _ = write_error(guest, &err).await;
                bail!(err);
            }
            _ => {
                let err = format!("unknown stderr message {msg:#x} from host daemon");
                let _ = write_error(guest, &err).await;
                bail!(err);
            }
        }
        // Keep log lines live instead of buffering until the op completes.
        guest.flush().await?;
    }
}

/// Typed logger fields (worker-protocol-connection.cc readFields).
async fn copy_fields<R, W>(host: &mut R, guest: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let count = wire::copy_u64(host, guest).await?;
    ensure!(
        count <= wire::MAX_COUNT,
        "field count {count} exceeds limit"
    );
    for _ in 0..count {
        match wire::copy_u64(host, guest).await? {
            0 => {
                wire::copy_u64(host, guest).await?;
            }
            1 => wire::copy_string(host, guest, wire::MAX_HOST_STRING).await?,
            t => bail!("unsupported logger field type {t}"),
        }
    }
    Ok(())
}

/// Error payload (serialise.cc `operator<<(Sink &, const Error &)`),
/// structured format only — guaranteed above the version floor.
async fn copy_error<R, W>(host: &mut R, guest: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    wire::copy_string(host, guest, wire::MAX_HOST_STRING).await?; // type, always "Error"
    wire::copy_u64(host, guest).await?; // verbosity level
    wire::copy_string(host, guest, wire::MAX_HOST_STRING).await?; // name (removed field)
    wire::copy_string(host, guest, wire::MAX_HOST_STRING).await?; // message
    let have_pos = wire::copy_u64(host, guest).await?;
    ensure!(have_pos == 0, "error positions are not supported");
    let traces = wire::copy_u64(host, guest).await?;
    ensure!(
        traces <= wire::MAX_COUNT,
        "trace count {traces} exceeds limit"
    );
    for _ in 0..traces {
        let have_pos = wire::copy_u64(host, guest).await?;
        ensure!(have_pos == 0, "error positions are not supported");
        wire::copy_string(host, guest, wire::MAX_HOST_STRING).await?; // trace message
    }
    Ok(())
}

/// Synthesize a `STDERR_ERROR` toward the guest. Used to reject ops; the
/// caller closes the connection afterwards.
pub async fn write_error<W>(guest: &mut W, msg: &str) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    wire::write_u64(guest, STDERR_ERROR).await?;
    wire::write_string(guest, b"Error").await?;
    wire::write_u64(guest, 0).await?; // lvlError
    wire::write_string(guest, b"Error").await?; // name (removed field)
    wire::write_string(guest, format!("claudepod-nix-proxy: {msg}").as_bytes()).await?;
    wire::write_u64(guest, 0).await?; // no position
    wire::write_u64(guest, 0).await?; // no traces
    guest.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::testutil::{put_str, put_u64};

    fn put_error(buf: &mut Vec<u8>, msg: &[u8]) {
        put_u64(buf, STDERR_ERROR);
        put_str(buf, b"Error");
        put_u64(buf, 0);
        put_str(buf, b"Error");
        put_str(buf, msg);
        put_u64(buf, 0); // no position
        put_u64(buf, 1); // one trace
        put_u64(buf, 0); // no trace position
        put_str(buf, b"while doing something");
    }

    #[tokio::test]
    async fn relays_until_last() {
        let mut script = Vec::new();
        put_u64(&mut script, STDERR_NEXT);
        put_str(&mut script, b"a log line\n");
        put_u64(&mut script, STDERR_START_ACTIVITY);
        put_u64(&mut script, 42); // id
        put_u64(&mut script, 5); // verbosity
        put_u64(&mut script, 100); // type
        put_str(&mut script, b"querying info");
        put_u64(&mut script, 2); // two fields
        put_u64(&mut script, 0); // int field
        put_u64(&mut script, 7);
        put_u64(&mut script, 1); // string field
        put_str(&mut script, b"/nix/store/abc-foo");
        put_u64(&mut script, 0); // parent
        put_u64(&mut script, STDERR_RESULT);
        put_u64(&mut script, 42);
        put_u64(&mut script, 101);
        put_u64(&mut script, 0); // no fields
        put_u64(&mut script, STDERR_STOP_ACTIVITY);
        put_u64(&mut script, 42);
        put_u64(&mut script, STDERR_LAST);

        let mut out = Vec::new();
        let terminal = relay(&mut script.as_slice(), &mut out).await.unwrap();
        assert_eq!(terminal, Terminal::Last);
        assert_eq!(out, script);
    }

    #[tokio::test]
    async fn relays_error() {
        let mut script = Vec::new();
        put_u64(&mut script, STDERR_NEXT);
        put_str(&mut script, b"warning\n");
        put_error(&mut script, b"path is not valid");

        let mut out = Vec::new();
        let terminal = relay(&mut script.as_slice(), &mut out).await.unwrap();
        assert_eq!(terminal, Terminal::Error);
        assert_eq!(out, script);
    }

    /// A violating message must not be forwarded; the guest gets a clean
    /// synthetic error instead.
    async fn assert_violation(script: &[u8], expected_msg: &str) {
        let mut out = Vec::new();
        let err = relay(&mut &script[..], &mut out).await.unwrap_err();
        assert!(err.to_string().contains(expected_msg), "{err}");
        let mut expected = Vec::new();
        write_error(&mut expected, &err.to_string()).await.unwrap();
        assert_eq!(out, expected);
    }

    #[tokio::test]
    async fn rejects_streaming_messages() {
        for msg in [STDERR_READ, STDERR_WRITE] {
            let mut script = Vec::new();
            put_u64(&mut script, msg);
            assert_violation(&script, "streaming").await;
        }
    }

    #[tokio::test]
    async fn rejects_unknown_message() {
        let mut script = Vec::new();
        put_u64(&mut script, 0xdeadbeef);
        assert_violation(&script, "unknown stderr message").await;
    }

    /// The synthetic error must parse with the same shape `readError`
    /// expects; check it against our own relay parser, which mirrors it.
    #[tokio::test]
    async fn synthetic_error_is_well_formed() {
        let mut out = Vec::new();
        write_error(&mut out, "rejected op AddToStore (7)")
            .await
            .unwrap();

        let mut relayed = Vec::new();
        let terminal = relay(&mut out.as_slice(), &mut relayed).await.unwrap();
        assert_eq!(terminal, Terminal::Error);
        assert_eq!(relayed, out);

        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(
            text.contains("claudepod-nix-proxy: rejected op AddToStore (7)"),
            "{text}"
        );
    }
}
