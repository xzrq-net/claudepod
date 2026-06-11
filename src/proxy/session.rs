//! Per-connection relay state machine: handshakes, post-handshake exchange,
//! then the op loop.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

use super::{handshake, ops, stderr, wire};

/// A healthy guest streams continuously through handshake steps and op
/// arguments; only at op boundaries may a (pooled) connection idle, so
/// these can be aggressive without ever killing a healthy connection.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
const ARGS_TIMEOUT: Duration = Duration::from_secs(60);

/// Drive one guest connection against one host daemon connection until the
/// guest hangs up or a protocol violation kills the session. `connect`
/// dials the host daemon; it runs only after the guest has sent valid
/// protocol magic, so a stalling client never costs the host a daemon fork.
pub async fn run<GR, GW, HR, HW, C, F>(guest_r: GR, guest_w: GW, connect: C) -> Result<()>
where
    GR: AsyncRead + Unpin,
    GW: AsyncWrite + Unpin,
    HR: AsyncRead + Unpin,
    HW: AsyncWrite + Unpin,
    C: FnOnce() -> F,
    F: Future<Output = Result<(HR, HW)>>,
{
    let mut gr = BufReader::new(guest_r);
    let mut gw = BufWriter::new(guest_w);

    within(HANDSHAKE_TIMEOUT, handshake::guest_magic(&mut gr))
        .await
        .context("guest handshake")?;

    let (host_r, host_w) = connect().await.context("dial host daemon")?;
    let mut hr = BufReader::new(host_r);
    let mut hw = BufWriter::new(host_w);

    let negotiated = handshake::upstream(&mut hr, &mut hw)
        .await
        .context("host daemon handshake")?;
    within(
        HANDSHAKE_TIMEOUT,
        handshake::downstream(&mut gr, &mut gw, &negotiated),
    )
    .await
    .context("guest handshake")?;

    // Client post-handshake fields: obsolete CPU affinity (a nonzero word is
    // followed by the affinity value), obsolete reserveSpace.
    within(HANDSHAKE_TIMEOUT, async {
        if wire::copy_u64(&mut gr, &mut hw).await? != 0 {
            wire::copy_u64(&mut gr, &mut hw).await?;
        }
        wire::copy_u64(&mut gr, &mut hw).await
    })
    .await
    .context("guest post-handshake")?;
    hw.flush().await?;

    // Daemon handshake info (nix version string, trusted flag), then the
    // greeting stderr exchange.
    wire::copy_string(&mut hr, &mut gw, wire::MAX_HOST_STRING).await?;
    wire::copy_u64(&mut hr, &mut gw).await?;
    stderr::relay(&mut hr, &mut gw).await.context("greeting")?;
    gw.flush().await?;

    loop {
        // No timeout here: the guest daemon pools connections, and a pooled
        // connection legitimately idles between ops indefinitely.
        let Some(word) = wire::read_u64_or_eof(&mut gr).await? else {
            return Ok(()); // guest hung up between ops
        };
        let Some(op) = ops::Op::allowed(word) else {
            // A loud failure: the guest's lower store should never reach for
            // anything outside the allowlist (see docs/nix-proxy.md).
            let msg = format!(
                "rejected op {} ({word}): not in the read-only allowlist",
                ops::op_name(word)
            );
            stderr::write_error(&mut gw, &msg).await?;
            bail!(msg);
        };

        if op == ops::Op::SetOptions {
            // Swallowed, not forwarded: parse for framing, then synthesize
            // the empty success the guest expects. None of the allowed ops
            // depend on client settings, and forwarding would hand the guest
            // unclamped host daemon settings whenever the invoking user is
            // in trusted-users (daemon.cc `ClientSettings::apply`).
            relay_args(op, &negotiated, &mut gr, &mut tokio::io::sink(), &mut gw).await?;
            wire::write_u64(&mut gw, stderr::STDERR_LAST).await?;
            gw.flush().await?;
            continue;
        }

        wire::write_u64(&mut hw, word).await?;
        relay_args(op, &negotiated, &mut gr, &mut hw, &mut gw).await?;
        hw.flush().await?;

        // No synthetic error on failure: relay() may have left the guest
        // mid-message (it synthesizes one itself at the points where the
        // guest is known to be message-aligned).
        let terminal = stderr::relay(&mut hr, &mut gw)
            .await
            .with_context(|| format!("{} stderr", op.name()))?;
        if terminal == stderr::Terminal::Last {
            // No synthetic error on failure here: the guest may have
            // consumed a partial result, so the stream is unsalvageable.
            // Closing makes it error out instead.
            ops::copy_result(op, &negotiated, &mut hr, &mut gw)
                .await
                .with_context(|| format!("{} result", op.name()))?;
        }
        gw.flush().await?;
    }
}

/// Copy an op's arguments guest-to-`dest` under the args timeout. On
/// failure, nothing has been sent guestward for this op yet, so the guest
/// still gets a parseable synthetic error before the session dies.
async fn relay_args<R, W, G>(
    op: ops::Op,
    negotiated: &handshake::Negotiated,
    guest: &mut R,
    dest: &mut W,
    guest_w: &mut G,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    G: AsyncWrite + Unpin,
{
    match within(ARGS_TIMEOUT, ops::copy_args(op, negotiated, guest, dest)).await {
        Ok(()) => Ok(()),
        Err(err) => {
            let err = err.context(format!("{} arguments", op.name()));
            let _ = stderr::write_error(guest_w, &format!("{err:#}")).await;
            Err(err)
        }
    }
}

/// `tokio::time::timeout` flattened into the session's error type.
async fn within<T>(limit: Duration, fut: impl Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(limit, fut).await {
        Ok(result) => result,
        Err(_) => bail!("timed out after {}s", limit.as_secs()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::handshake::{WORKER_MAGIC_1, WORKER_MAGIC_2};
    use crate::proxy::testutil::{put_str, put_u64};

    const VERSION_1_38: u64 = 0x126;

    /// Everything the guest sends, through one IsValidPath op.
    fn guest_script() -> Vec<u8> {
        let mut buf = Vec::new();
        put_u64(&mut buf, WORKER_MAGIC_1);
        put_u64(&mut buf, VERSION_1_38);
        put_u64(&mut buf, 1); // client features
        put_str(&mut buf, b"disable-set-options");
        put_u64(&mut buf, 0); // obsolete CPU affinity
        put_u64(&mut buf, 0); // obsolete reserveSpace
        buf
    }

    /// Everything the host daemon sends, through the greeting.
    fn host_script() -> Vec<u8> {
        let mut buf = Vec::new();
        put_u64(&mut buf, WORKER_MAGIC_2);
        put_u64(&mut buf, VERSION_1_38);
        put_u64(&mut buf, 0); // daemon features
        put_str(&mut buf, b"2.34.7"); // daemon nix version
        put_u64(&mut buf, 2); // trusted: NotTrusted
        put_u64(&mut buf, stderr::STDERR_LAST);
        buf
    }

    async fn run_session(guest_in: &[u8], host_in: &[u8]) -> (Result<()>, Vec<u8>, Vec<u8>) {
        let mut guest_out = Vec::new();
        let mut host_out = Vec::new();
        let result = run(&mut &guest_in[..], &mut guest_out, || async {
            Ok((host_in, &mut host_out))
        })
        .await;
        (result, guest_out, host_out)
    }

    #[tokio::test]
    async fn full_session_is_valid_path() {
        let mut guest_in = guest_script();
        put_u64(&mut guest_in, 1); // IsValidPath
        put_str(&mut guest_in, b"/nix/store/abc-foo");
        // Guest EOF after the op: clean shutdown.

        let mut host_in = host_script();
        put_u64(&mut host_in, stderr::STDERR_LAST);
        put_u64(&mut host_in, 1); // result: valid

        let (result, guest_out, host_out) = run_session(&guest_in, &host_in).await;
        result.unwrap();

        // Toward the host: our handshake, relayed post-handshake fields,
        // relayed op.
        let mut expected_host = Vec::new();
        put_u64(&mut expected_host, WORKER_MAGIC_1);
        put_u64(&mut expected_host, VERSION_1_38);
        put_u64(&mut expected_host, 0); // our (empty) feature list
        put_u64(&mut expected_host, 0);
        put_u64(&mut expected_host, 0);
        put_u64(&mut expected_host, 1);
        put_str(&mut expected_host, b"/nix/store/abc-foo");
        assert_eq!(host_out, expected_host);

        // Toward the guest: our handshake, relayed greeting, relayed result.
        let mut expected_guest = Vec::new();
        put_u64(&mut expected_guest, WORKER_MAGIC_2);
        put_u64(&mut expected_guest, VERSION_1_38);
        put_u64(&mut expected_guest, 0); // negotiated (empty) feature list
        put_str(&mut expected_guest, b"2.34.7");
        put_u64(&mut expected_guest, 2);
        put_u64(&mut expected_guest, stderr::STDERR_LAST);
        put_u64(&mut expected_guest, stderr::STDERR_LAST);
        put_u64(&mut expected_guest, 1);
        assert_eq!(guest_out, expected_guest);
    }

    #[tokio::test]
    async fn set_options_is_swallowed() {
        let mut guest_in = guest_script();
        put_u64(&mut guest_in, 19); // SetOptions
        for _ in 0..12 {
            put_u64(&mut guest_in, 0); // scalar fields
        }
        put_u64(&mut guest_in, 1); // one override
        put_str(&mut guest_in, b"substituters");
        put_str(&mut guest_in, b"https://evil.example");
        put_u64(&mut guest_in, 1); // IsValidPath, to prove the loop survives
        put_str(&mut guest_in, b"/nix/store/abc-foo");

        let mut host_in = host_script();
        // The host script only answers IsValidPath; SetOptions never gets there.
        put_u64(&mut host_in, stderr::STDERR_LAST);
        put_u64(&mut host_in, 1);

        let (result, guest_out, host_out) = run_session(&guest_in, &host_in).await;
        result.unwrap();

        // Toward the host: handshake and IsValidPath, no trace of SetOptions.
        let mut expected_host = Vec::new();
        put_u64(&mut expected_host, WORKER_MAGIC_1);
        put_u64(&mut expected_host, VERSION_1_38);
        put_u64(&mut expected_host, 0); // our (empty) feature list
        put_u64(&mut expected_host, 0);
        put_u64(&mut expected_host, 0);
        put_u64(&mut expected_host, 1);
        put_str(&mut expected_host, b"/nix/store/abc-foo");
        assert_eq!(host_out, expected_host);

        // Toward the guest: a synthetic empty success for SetOptions,
        // then the real IsValidPath exchange.
        let mut expected_guest = Vec::new();
        put_u64(&mut expected_guest, WORKER_MAGIC_2);
        put_u64(&mut expected_guest, VERSION_1_38);
        put_u64(&mut expected_guest, 0); // negotiated (empty) feature list
        put_str(&mut expected_guest, b"2.34.7");
        put_u64(&mut expected_guest, 2);
        put_u64(&mut expected_guest, stderr::STDERR_LAST); // greeting
        put_u64(&mut expected_guest, stderr::STDERR_LAST); // synthetic SetOptions success
        put_u64(&mut expected_guest, stderr::STDERR_LAST); // IsValidPath stderr
        put_u64(&mut expected_guest, 1); // IsValidPath result
        assert_eq!(guest_out, expected_guest);
    }

    #[tokio::test]
    async fn rejects_disallowed_op() {
        let mut guest_in = guest_script();
        put_u64(&mut guest_in, 7); // AddToStore

        let (result, guest_out, host_out) = run_session(&guest_in, &host_script()).await;
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("rejected op AddToStore (7)"),
            "{err}"
        );

        // Nothing op-related reaches the host (only handshake + post-handshake).
        // magic, version, empty feature list, 2 obsolete fields
        let handshake_len = 8 * 5;
        assert_eq!(host_out.len(), handshake_len);

        // The guest gets a parseable synthetic error after the greeting.
        let text = String::from_utf8_lossy(&guest_out).into_owned();
        assert!(
            text.contains("claudepod-nix-proxy: rejected op AddToStore"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn daemon_error_is_relayed_and_session_continues() {
        let mut guest_in = guest_script();
        put_u64(&mut guest_in, 26); // QueryPathInfo
        put_str(&mut guest_in, b"/nix/store/abc-foo");
        put_u64(&mut guest_in, 1); // IsValidPath, to prove the loop survives
        put_str(&mut guest_in, b"/nix/store/abc-foo");

        let mut host_in = host_script();
        // QueryPathInfo fails with a daemon-side error...
        put_u64(&mut host_in, stderr::STDERR_ERROR);
        put_str(&mut host_in, b"Error");
        put_u64(&mut host_in, 0);
        put_str(&mut host_in, b"Error");
        put_str(&mut host_in, b"path '/nix/store/abc-foo' is not valid");
        put_u64(&mut host_in, 0);
        put_u64(&mut host_in, 0);
        // ...then IsValidPath succeeds.
        put_u64(&mut host_in, stderr::STDERR_LAST);
        put_u64(&mut host_in, 0);

        let (result, guest_out, _) = run_session(&guest_in, &host_in).await;
        result.unwrap();
        let text = String::from_utf8_lossy(&guest_out).into_owned();
        assert!(text.contains("is not valid"), "{text}");
    }
}
