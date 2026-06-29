//! Per-connection relay state machine: handshakes, post-handshake exchange,
//! then the op loop.

use std::ffi::OsStr;
use std::future::Future;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

use super::{NixRunRoots, handshake, ops, stderr, wire};

/// A healthy guest streams continuously through handshake steps and op
/// arguments; only at op boundaries may a (pooled) connection idle, so
/// these can be aggressive without ever killing a healthy connection.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
const ARGS_TIMEOUT: Duration = Duration::from_secs(60);

/// Drive one guest connection against one host daemon connection until the
/// guest hangs up or a protocol violation kills the session. `connect`
/// dials the host daemon; it runs only after the guest has sent valid
/// protocol magic, so a stalling client never costs the host a daemon fork.
pub async fn run<HR, HW, ConnectFut, FillFut>(
    guest_r: impl AsyncRead + Unpin,
    guest_w: impl AsyncWrite + Unpin,
    connect: impl FnOnce() -> ConnectFut,
    nix_run_roots: Option<Arc<NixRunRoots>>,
    fill: impl Fn(PathBuf) -> FillFut,
    warn: impl Fn(String),
) -> Result<()>
where
    HR: AsyncRead + Unpin,
    HW: AsyncWrite + Unpin,
    ConnectFut: Future<Output = Result<(HR, HW)>>,
    FillFut: Future<Output = Result<()>>,
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
        if op == ops::Op::IsValidPath {
            intercept_is_valid_path(
                nix_run_roots.as_deref(),
                &fill,
                &warn,
                &mut gr,
                &mut hr,
                &mut hw,
                &mut gw,
            )
            .await?;
            gw.flush().await?;
            continue;
        }

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

async fn intercept_is_valid_path<FillFut>(
    nix_run_roots: Option<&NixRunRoots>,
    fill: &impl Fn(PathBuf) -> FillFut,
    warn: &impl Fn(String),
    guest_r: &mut (impl AsyncRead + Unpin),
    host_r: &mut (impl AsyncRead + Unpin),
    host_w: &mut (impl AsyncWrite + Unpin),
    guest_w: &mut (impl AsyncWrite + Unpin),
) -> Result<()>
where
    FillFut: Future<Output = Result<()>>,
{
    let store_path = relay_is_valid_path_args(guest_r, host_w, guest_w).await?;
    host_w.flush().await?;

    let terminal = stderr::relay_without_last(host_r, guest_w)
        .await
        .context("IsValidPath stderr")?;
    if terminal == stderr::Terminal::Error {
        return Ok(());
    }

    let host_valid = wire::read_u64(host_r).await.context("IsValidPath result")? != 0;
    if host_valid {
        write_is_valid_path_result(guest_w, true).await?;
        return Ok(());
    }

    if !nix_run_roots.is_some_and(|roots| roots.contains(&store_path)) {
        write_is_valid_path_result(guest_w, false).await?;
        return Ok(());
    }

    if let Err(err) = fill(store_path.clone()).await {
        warn_on_demand_fill(warn, &store_path, format!("fill failed: {err:#}"));
        write_is_valid_path_result(guest_w, false).await?;
        return Ok(());
    }

    match recheck_is_valid_path(host_r, host_w, &store_path).await? {
        Recheck::Valid => write_is_valid_path_result(guest_w, true).await?,
        Recheck::Invalid => {
            warn_on_demand_fill(warn, &store_path, "re-check still invalid".to_string());
            write_is_valid_path_result(guest_w, false).await?;
        }
        Recheck::DaemonError(message) => {
            warn_on_demand_fill(warn, &store_path, format!("re-check failed: {message}"));
            write_is_valid_path_result(guest_w, false).await?;
        }
    }

    Ok(())
}

async fn relay_is_valid_path_args(
    guest_r: &mut (impl AsyncRead + Unpin),
    host_w: &mut (impl AsyncWrite + Unpin),
    guest_w: &mut (impl AsyncWrite + Unpin),
) -> Result<PathBuf> {
    match within(ARGS_TIMEOUT, async {
        let raw = wire::read_string(guest_r, wire::MAX_GUEST_STRING).await?;
        wire::write_string(host_w, &raw).await?;
        Ok(PathBuf::from(OsStr::from_bytes(&raw)))
    })
    .await
    {
        Ok(path) => Ok(path),
        Err(err) => {
            let err = err.context("IsValidPath arguments");
            let _ = stderr::write_error(guest_w, &format!("{err:#}")).await;
            Err(err)
        }
    }
}

enum Recheck {
    Valid,
    Invalid,
    DaemonError(String),
}

async fn recheck_is_valid_path(
    host_r: &mut (impl AsyncRead + Unpin),
    host_w: &mut (impl AsyncWrite + Unpin),
    store_path: &Path,
) -> Result<Recheck> {
    wire::write_u64(host_w, ops::Op::IsValidPath as u64).await?;
    wire::write_string(host_w, store_path.as_os_str().as_bytes()).await?;
    host_w.flush().await?;

    match stderr::drain_to_terminal(host_r)
        .await
        .context("IsValidPath re-check stderr")?
    {
        stderr::Drained::Last => {
            let valid = wire::read_u64(host_r)
                .await
                .context("IsValidPath re-check result")?
                != 0;
            Ok(if valid {
                Recheck::Valid
            } else {
                Recheck::Invalid
            })
        }
        stderr::Drained::Error(message) => Ok(Recheck::DaemonError(message)),
    }
}

async fn write_is_valid_path_result(
    guest_w: &mut (impl AsyncWrite + Unpin),
    valid: bool,
) -> Result<()> {
    wire::write_u64(guest_w, stderr::STDERR_LAST).await?;
    wire::write_u64(guest_w, u64::from(valid)).await?;
    Ok(())
}

fn warn_on_demand_fill(warn: &impl Fn(String), store_path: &Path, reason: String) {
    warn(format!(
        "claudepod-nix-proxy: warning: on-demand fill for {}: {reason}",
        store_path.display()
    ));
}

/// Copy an op's arguments guest-to-`dest` under the args timeout. On
/// failure, nothing has been sent guestward for this op yet, so the guest
/// still gets a parseable synthetic error before the session dies.
async fn relay_args(
    op: ops::Op,
    negotiated: &handshake::Negotiated,
    guest: &mut (impl AsyncRead + Unpin),
    dest: &mut (impl AsyncWrite + Unpin),
    guest_w: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
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
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::proxy::handshake::{WORKER_MAGIC_1, WORKER_MAGIC_2};
    use crate::proxy::testutil::{put_str, put_u64};

    const VERSION_1_38: u64 = 0x126;

    const STORE_PATH: &[u8] = b"/nix/store/aaa111-one";
    const OTHER_STORE_PATH: &[u8] = b"/nix/store/bbb222-two";

    /// Everything the guest sends before the op loop.
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

    fn put_is_valid_path(buf: &mut Vec<u8>, path: &[u8]) {
        put_u64(buf, ops::Op::IsValidPath as u64);
        put_str(buf, path);
    }

    fn put_is_valid_path_result(buf: &mut Vec<u8>, valid: bool) {
        put_u64(buf, stderr::STDERR_LAST);
        put_u64(buf, u64::from(valid));
    }

    fn put_daemon_error(buf: &mut Vec<u8>, msg: &[u8]) {
        put_u64(buf, stderr::STDERR_ERROR);
        put_str(buf, b"Error");
        put_u64(buf, 0);
        put_str(buf, b"Error");
        put_str(buf, msg);
        put_u64(buf, 0); // no position
        put_u64(buf, 0); // no traces
    }

    async fn run_session(guest_in: &[u8], host_in: &[u8]) -> (Result<()>, Vec<u8>, Vec<u8>) {
        let (result, guest_out, host_out, _, _) =
            run_session_with_fill(guest_in, host_in, None, []).await;
        (result, guest_out, host_out)
    }

    async fn run_session_with_roots(
        guest_in: &[u8],
        host_in: &[u8],
        roots: &[&[u8]],
    ) -> (Result<()>, Vec<u8>, Vec<u8>, Vec<PathBuf>, Vec<String>) {
        let roots = roots
            .iter()
            .map(|path| PathBuf::from(OsStr::from_bytes(path)));
        run_session_with_fill(
            guest_in,
            host_in,
            Some(Arc::new(NixRunRoots::from_paths(roots))),
            [],
        )
        .await
    }

    async fn run_session_with_fill<const N: usize>(
        guest_in: &[u8],
        host_in: &[u8],
        roots: Option<Arc<NixRunRoots>>,
        fill_results: [&str; N],
    ) -> (Result<()>, Vec<u8>, Vec<u8>, Vec<PathBuf>, Vec<String>) {
        let mut guest_out = Vec::new();
        let mut host_out = Vec::new();
        let fills = Arc::new(Mutex::new(Vec::new()));
        let fill_results = Arc::new(Mutex::new(VecDeque::from(fill_results.map(str::to_owned))));
        let warnings = Arc::new(Mutex::new(Vec::new()));

        let fill_fills = fills.clone();
        let fill_results_for_hook = fill_results.clone();
        let warn_warnings = warnings.clone();
        let result = run(
            &mut &guest_in[..],
            &mut guest_out,
            || async { Ok((host_in, &mut host_out)) },
            roots,
            move |store_path| {
                let fills = fill_fills.clone();
                let fill_results = fill_results_for_hook.clone();
                async move {
                    fills.lock().unwrap().push(store_path);
                    let Some(result) = fill_results.lock().unwrap().pop_front() else {
                        anyhow::bail!("unexpected fill");
                    };
                    if result.is_empty() {
                        Ok(())
                    } else {
                        anyhow::bail!("{result}");
                    }
                }
            },
            move |message| warn_warnings.lock().unwrap().push(message),
        )
        .await;
        let fills = fills.lock().unwrap().clone();
        let warnings = warnings.lock().unwrap().clone();
        (result, guest_out, host_out, fills, warnings)
    }

    fn expected_host_with_is_valid_path_ops(paths: &[&[u8]]) -> Vec<u8> {
        let mut expected = Vec::new();
        put_u64(&mut expected, WORKER_MAGIC_1);
        put_u64(&mut expected, VERSION_1_38);
        put_u64(&mut expected, 0); // our (empty) feature list
        put_u64(&mut expected, 0); // obsolete CPU affinity
        put_u64(&mut expected, 0); // obsolete reserveSpace
        for path in paths {
            put_is_valid_path(&mut expected, path);
        }
        expected
    }

    fn expected_guest_prefix() -> Vec<u8> {
        let mut expected = Vec::new();
        put_u64(&mut expected, WORKER_MAGIC_2);
        put_u64(&mut expected, VERSION_1_38);
        put_u64(&mut expected, 0); // negotiated (empty) feature list
        put_str(&mut expected, b"2.34.7");
        put_u64(&mut expected, 2);
        put_u64(&mut expected, stderr::STDERR_LAST); // greeting
        expected
    }

    #[tokio::test]
    async fn is_valid_path_host_valid_pass_through() {
        let mut guest_in = guest_script();
        put_is_valid_path(&mut guest_in, STORE_PATH);
        // Guest EOF after the op: clean shutdown.

        let mut host_in = host_script();
        put_is_valid_path_result(&mut host_in, true);

        let (result, guest_out, host_out) = run_session(&guest_in, &host_in).await;
        result.unwrap();

        assert_eq!(
            host_out,
            expected_host_with_is_valid_path_ops(&[STORE_PATH])
        );

        // Toward the guest: our handshake, relayed greeting, relayed result.
        let mut expected_guest = expected_guest_prefix();
        put_is_valid_path_result(&mut expected_guest, true);
        assert_eq!(guest_out, expected_guest);
    }

    #[tokio::test]
    async fn is_valid_path_host_invalid_manifest_miss_returns_false() {
        let mut guest_in = guest_script();
        put_is_valid_path(&mut guest_in, STORE_PATH);

        let mut host_in = host_script();
        put_is_valid_path_result(&mut host_in, false);

        let (result, guest_out, host_out, fills, warnings) =
            run_session_with_roots(&guest_in, &host_in, &[OTHER_STORE_PATH]).await;
        result.unwrap();

        assert_eq!(
            host_out,
            expected_host_with_is_valid_path_ops(&[STORE_PATH])
        );
        let mut expected_guest = expected_guest_prefix();
        put_is_valid_path_result(&mut expected_guest, false);
        assert_eq!(guest_out, expected_guest);
        assert!(fills.is_empty());
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn is_valid_path_daemon_error_is_relayed_without_fill() {
        let mut guest_in = guest_script();
        put_is_valid_path(&mut guest_in, STORE_PATH);

        let mut host_in = host_script();
        put_daemon_error(&mut host_in, b"daemon-side failure");

        let roots = Some(Arc::new(NixRunRoots::from_paths([PathBuf::from(
            OsStr::from_bytes(STORE_PATH),
        )])));
        let (result, guest_out, _, fills, warnings) =
            run_session_with_fill(&guest_in, &host_in, roots, []).await;
        result.unwrap();

        let mut expected_guest = expected_guest_prefix();
        put_daemon_error(&mut expected_guest, b"daemon-side failure");
        assert_eq!(guest_out, expected_guest);
        assert!(fills.is_empty());
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn is_valid_path_manifest_hit_fill_success_recheck_valid_returns_true() {
        let mut guest_in = guest_script();
        put_is_valid_path(&mut guest_in, STORE_PATH);

        let mut host_in = host_script();
        put_is_valid_path_result(&mut host_in, false);
        put_is_valid_path_result(&mut host_in, true);

        let roots = Some(Arc::new(NixRunRoots::from_paths([PathBuf::from(
            OsStr::from_bytes(STORE_PATH),
        )])));
        let (result, guest_out, host_out, fills, warnings) =
            run_session_with_fill(&guest_in, &host_in, roots, [""]).await;
        result.unwrap();

        assert_eq!(
            host_out,
            expected_host_with_is_valid_path_ops(&[STORE_PATH, STORE_PATH])
        );
        let mut expected_guest = expected_guest_prefix();
        put_is_valid_path_result(&mut expected_guest, true);
        assert_eq!(guest_out, expected_guest);
        assert_eq!(fills, [PathBuf::from(OsStr::from_bytes(STORE_PATH))]);
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn is_valid_path_fill_failure_returns_false_and_warns() {
        let mut guest_in = guest_script();
        put_is_valid_path(&mut guest_in, STORE_PATH);

        let mut host_in = host_script();
        put_is_valid_path_result(&mut host_in, false);

        let roots = Some(Arc::new(NixRunRoots::from_paths([PathBuf::from(
            OsStr::from_bytes(STORE_PATH),
        )])));
        let (result, guest_out, host_out, fills, warnings) =
            run_session_with_fill(&guest_in, &host_in, roots, ["substitution failed"]).await;
        result.unwrap();

        assert_eq!(
            host_out,
            expected_host_with_is_valid_path_ops(&[STORE_PATH])
        );
        let mut expected_guest = expected_guest_prefix();
        put_is_valid_path_result(&mut expected_guest, false);
        assert_eq!(guest_out, expected_guest);
        assert_eq!(fills, [PathBuf::from(OsStr::from_bytes(STORE_PATH))]);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("/nix/store/aaa111-one"),
            "{warnings:?}"
        );
        assert!(warnings[0].contains("substitution failed"), "{warnings:?}");
    }

    #[tokio::test]
    async fn is_valid_path_fill_success_recheck_invalid_returns_false_and_warns() {
        let mut guest_in = guest_script();
        put_is_valid_path(&mut guest_in, STORE_PATH);

        let mut host_in = host_script();
        put_is_valid_path_result(&mut host_in, false);
        put_is_valid_path_result(&mut host_in, false);

        let roots = Some(Arc::new(NixRunRoots::from_paths([PathBuf::from(
            OsStr::from_bytes(STORE_PATH),
        )])));
        let (result, guest_out, host_out, fills, warnings) =
            run_session_with_fill(&guest_in, &host_in, roots, [""]).await;
        result.unwrap();

        assert_eq!(
            host_out,
            expected_host_with_is_valid_path_ops(&[STORE_PATH, STORE_PATH])
        );
        let mut expected_guest = expected_guest_prefix();
        put_is_valid_path_result(&mut expected_guest, false);
        assert_eq!(guest_out, expected_guest);
        assert_eq!(fills, [PathBuf::from(OsStr::from_bytes(STORE_PATH))]);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("/nix/store/aaa111-one"),
            "{warnings:?}"
        );
        assert!(warnings[0].contains("still invalid"), "{warnings:?}");
    }

    #[tokio::test]
    async fn is_valid_path_recheck_protocol_error_fails_session() {
        let mut guest_in = guest_script();
        put_is_valid_path(&mut guest_in, STORE_PATH);

        let mut host_in = host_script();
        put_is_valid_path_result(&mut host_in, false);
        put_u64(&mut host_in, 0xdead_beef);

        let roots = Some(Arc::new(NixRunRoots::from_paths([PathBuf::from(
            OsStr::from_bytes(STORE_PATH),
        )])));
        let (result, guest_out, host_out, fills, warnings) =
            run_session_with_fill(&guest_in, &host_in, roots, [""]).await;
        let err = result.unwrap_err();
        let err = format!("{err:#}");

        assert!(err.contains("unknown stderr message"), "{err}");
        assert_eq!(
            host_out,
            expected_host_with_is_valid_path_ops(&[STORE_PATH, STORE_PATH])
        );
        assert_eq!(guest_out, expected_guest_prefix());
        assert_eq!(fills, [PathBuf::from(OsStr::from_bytes(STORE_PATH))]);
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn intercepted_is_valid_path_emits_one_terminal_marker() {
        let mut guest_in = guest_script();
        put_is_valid_path(&mut guest_in, STORE_PATH);

        let mut host_in = host_script();
        put_u64(&mut host_in, stderr::STDERR_NEXT);
        put_str(&mut host_in, b"initial invalid check\n");
        put_is_valid_path_result(&mut host_in, false);
        put_is_valid_path_result(&mut host_in, true);

        let roots = Some(Arc::new(NixRunRoots::from_paths([PathBuf::from(
            OsStr::from_bytes(STORE_PATH),
        )])));
        let (result, guest_out, _, _, warnings) =
            run_session_with_fill(&guest_in, &host_in, roots, [""]).await;
        result.unwrap();

        let mut expected_guest = expected_guest_prefix();
        put_u64(&mut expected_guest, stderr::STDERR_NEXT);
        put_str(&mut expected_guest, b"initial invalid check\n");
        put_is_valid_path_result(&mut expected_guest, true);
        assert_eq!(guest_out, expected_guest);
        assert!(warnings.is_empty());
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
