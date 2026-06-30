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

/// Stderr drained without relaying. `Error` means the daemon sent a
/// structured `STDERR_ERROR` and the connection is still at an op boundary.
#[derive(Debug, PartialEq, Eq)]
pub enum Drained {
    Last,
    Error(String),
}

/// Relay stderr messages host-to-guest until a terminal message.
pub async fn relay<R, W>(host: &mut R, guest: &mut W) -> Result<Terminal>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    relay_inner(host, guest, true).await
}

/// Relay stderr messages host-to-guest until a terminal message, but hold
/// back `STDERR_LAST`. Used when the proxy must inspect the result payload
/// before deciding what final result to send to the guest. `STDERR_ERROR`
/// is still relayed exactly.
pub async fn relay_without_last<R, W>(host: &mut R, guest: &mut W) -> Result<Terminal>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    relay_inner(host, guest, false).await
}

async fn relay_inner<R, W>(host: &mut R, guest: &mut W, forward_last: bool) -> Result<Terminal>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        // Don't forward until the whole message is known to be relayable, so
        // a policy violation never leaves the guest mid-message.
        match read_message(host).await? {
            Message::Last => {
                if forward_last {
                    wire::write_u64(guest, STDERR_LAST).await?;
                }
                return Ok(Terminal::Last);
            }
            Message::Error(error) => {
                wire::write_u64(guest, STDERR_ERROR).await?;
                write_error_payload(guest, &error).await?;
                return Ok(Terminal::Error);
            }
            Message::Next(text) => {
                wire::write_u64(guest, STDERR_NEXT).await?;
                wire::write_string(guest, &text).await?;
            }
            Message::StartActivity {
                id,
                verbosity,
                activity_type,
                text,
                fields,
                parent,
            } => {
                wire::write_u64(guest, STDERR_START_ACTIVITY).await?;
                wire::write_u64(guest, id).await?;
                wire::write_u64(guest, verbosity).await?;
                wire::write_u64(guest, activity_type).await?;
                wire::write_string(guest, &text).await?;
                write_fields(guest, &fields).await?;
                wire::write_u64(guest, parent).await?;
            }
            Message::StopActivity { id } => {
                wire::write_u64(guest, STDERR_STOP_ACTIVITY).await?;
                wire::write_u64(guest, id).await?;
            }
            Message::Result {
                id,
                result_type,
                fields,
            } => {
                wire::write_u64(guest, STDERR_RESULT).await?;
                wire::write_u64(guest, id).await?;
                wire::write_u64(guest, result_type).await?;
                write_fields(guest, &fields).await?;
            }
            Message::Streaming(msg) => {
                let err =
                    format!("host daemon sent streaming stderr message {msg:#x} during a query op");
                let _ = write_error(guest, &err).await;
                bail!(err);
            }
            Message::Unknown(msg) => {
                let err = format!("unknown stderr message {msg:#x} from host daemon");
                let _ = write_error(guest, &err).await;
                bail!(err);
            }
        }
        // Keep log lines live instead of buffering until the op completes.
        guest.flush().await?;
    }
}

/// Drain stderr messages from the host daemon without relaying them. Used by
/// proxy-owned host operations; `STDERR_ERROR` becomes an error containing the
/// daemon error plus log text seen before it.
pub async fn drain_to_last<R>(host: &mut R) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    match drain_to_terminal(host).await? {
        Drained::Last => Ok(()),
        Drained::Error(message) => bail!("{message}"),
    }
}

pub async fn drain_to_terminal<R>(host: &mut R) -> Result<Drained>
where
    R: AsyncRead + Unpin,
{
    let mut logs = Vec::new();
    loop {
        match read_message(host).await? {
            Message::Last => return Ok(Drained::Last),
            Message::Error(error) => {
                let mut message = error.daemon_message();
                if !logs.is_empty() {
                    message.push_str("; logs: ");
                    message.push_str(&logs.join(" | "));
                }
                return Ok(Drained::Error(message));
            }
            Message::Next(text) => {
                push_log(&mut logs, &text);
            }
            Message::StartActivity { text, fields, .. } => {
                push_log(&mut logs, &text);
                push_field_logs(&mut logs, &fields);
            }
            Message::StopActivity { .. } => {}
            Message::Result { fields, .. } => push_field_logs(&mut logs, &fields),
            Message::Streaming(msg) => {
                bail!("host daemon sent streaming stderr message {msg:#x} during proxy-owned op");
            }
            Message::Unknown(msg) => bail!("unknown stderr message {msg:#x} from host daemon"),
        }
    }
}

enum Message {
    Last,
    Error(ErrorPayload),
    Next(Vec<u8>),
    StartActivity {
        id: u64,
        verbosity: u64,
        activity_type: u64,
        text: Vec<u8>,
        fields: Vec<Field>,
        parent: u64,
    },
    StopActivity {
        id: u64,
    },
    Result {
        id: u64,
        result_type: u64,
        fields: Vec<Field>,
    },
    Streaming(u64),
    Unknown(u64),
}

enum Field {
    Int(u64),
    String(Vec<u8>),
}

struct ErrorPayload {
    type_name: Vec<u8>,
    verbosity: u64,
    name: Vec<u8>,
    message: Vec<u8>,
    traces: Vec<Vec<u8>>,
}

impl ErrorPayload {
    fn daemon_message(&self) -> String {
        let mut message = format!("host daemon error: {}", text(&self.message));
        if !self.traces.is_empty() {
            message.push_str("; traces: ");
            message.push_str(
                &self
                    .traces
                    .iter()
                    .map(|trace| text(trace))
                    .collect::<Vec<_>>()
                    .join(" | "),
            );
        }
        message
    }
}

async fn read_message<R>(host: &mut R) -> Result<Message>
where
    R: AsyncRead + Unpin,
{
    let msg = wire::read_u64(host).await?;
    match msg {
        STDERR_LAST => Ok(Message::Last),
        STDERR_ERROR => Ok(Message::Error(read_error_payload(host).await?)),
        STDERR_NEXT => Ok(Message::Next(
            wire::read_string(host, wire::MAX_HOST_STRING).await?,
        )),
        STDERR_START_ACTIVITY => Ok(Message::StartActivity {
            id: wire::read_u64(host).await?,
            verbosity: wire::read_u64(host).await?,
            activity_type: wire::read_u64(host).await?,
            text: wire::read_string(host, wire::MAX_HOST_STRING).await?,
            fields: read_fields(host).await?,
            parent: wire::read_u64(host).await?,
        }),
        STDERR_STOP_ACTIVITY => Ok(Message::StopActivity {
            id: wire::read_u64(host).await?,
        }),
        STDERR_RESULT => Ok(Message::Result {
            id: wire::read_u64(host).await?,
            result_type: wire::read_u64(host).await?,
            fields: read_fields(host).await?,
        }),
        STDERR_READ | STDERR_WRITE => Ok(Message::Streaming(msg)),
        _ => Ok(Message::Unknown(msg)),
    }
}

/// Error payload (serialise.cc `operator<<(Sink &, const Error &)`),
/// structured format only — guaranteed above the version floor.
async fn read_error_payload<R>(host: &mut R) -> Result<ErrorPayload>
where
    R: AsyncRead + Unpin,
{
    let type_name = wire::read_string(host, wire::MAX_HOST_STRING).await?;
    let verbosity = wire::read_u64(host).await?;
    let name = wire::read_string(host, wire::MAX_HOST_STRING).await?;
    let message = wire::read_string(host, wire::MAX_HOST_STRING).await?;
    let have_pos = wire::read_u64(host).await?;
    ensure!(have_pos == 0, "error positions are not supported");
    let trace_count = wire::read_u64(host).await?;
    ensure!(
        trace_count <= wire::MAX_COUNT,
        "trace count {trace_count} exceeds limit"
    );
    let mut traces = Vec::with_capacity(trace_count as usize);
    for _ in 0..trace_count {
        let have_pos = wire::read_u64(host).await?;
        ensure!(have_pos == 0, "error positions are not supported");
        traces.push(wire::read_string(host, wire::MAX_HOST_STRING).await?);
    }
    Ok(ErrorPayload {
        type_name,
        verbosity,
        name,
        message,
        traces,
    })
}

async fn write_error_payload<W>(guest: &mut W, error: &ErrorPayload) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    wire::write_string(guest, &error.type_name).await?;
    wire::write_u64(guest, error.verbosity).await?;
    wire::write_string(guest, &error.name).await?;
    wire::write_string(guest, &error.message).await?;
    wire::write_u64(guest, 0).await?;
    wire::write_u64(guest, error.traces.len() as u64).await?;
    for trace in &error.traces {
        wire::write_u64(guest, 0).await?;
        wire::write_string(guest, trace).await?;
    }
    Ok(())
}

/// Typed logger fields (worker-protocol-connection.cc readFields).
async fn read_fields<R>(host: &mut R) -> Result<Vec<Field>>
where
    R: AsyncRead + Unpin,
{
    let count = wire::read_u64(host).await?;
    ensure!(
        count <= wire::MAX_COUNT,
        "field count {count} exceeds limit"
    );
    let mut fields = Vec::with_capacity(count as usize);
    for _ in 0..count {
        fields.push(match wire::read_u64(host).await? {
            0 => Field::Int(wire::read_u64(host).await?),
            1 => Field::String(wire::read_string(host, wire::MAX_HOST_STRING).await?),
            t => bail!("unsupported logger field type {t}"),
        });
    }
    Ok(fields)
}

async fn write_fields<W>(guest: &mut W, fields: &[Field]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    wire::write_u64(guest, fields.len() as u64).await?;
    for field in fields {
        match field {
            Field::Int(value) => {
                wire::write_u64(guest, 0).await?;
                wire::write_u64(guest, *value).await?;
            }
            Field::String(value) => {
                wire::write_u64(guest, 1).await?;
                wire::write_string(guest, value).await?;
            }
        }
    }
    Ok(())
}

fn push_log(logs: &mut Vec<String>, raw: &[u8]) {
    let text = String::from_utf8_lossy(raw).trim_end().to_string();
    if !text.is_empty() {
        logs.push(text);
    }
}

fn push_field_logs(logs: &mut Vec<String>, fields: &[Field]) {
    for field in fields {
        if let Field::String(raw) = field {
            push_log(logs, raw);
        }
    }
}

fn text(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
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
