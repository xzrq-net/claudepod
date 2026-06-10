//! Both legs of the daemon handshake (nix `worker-protocol-connection.cc`).
//!
//! Order matters: the upstream (host daemon) handshake runs first, and the
//! resulting version and feature set are advertised verbatim to the guest.
//! Both legs then agree on every version- and feature-gated serialization,
//! so op payloads relay without translation. A guest that would negotiate
//! anything lower is refused.

use std::collections::BTreeSet;
use std::fmt;

use anyhow::{Context, Result, ensure};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::wire;

pub const WORKER_MAGIC_1: u64 = 0x6e697863;
pub const WORKER_MAGIC_2: u64 = 0x6478696f;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
}

impl Version {
    pub const fn new(major: u8, minor: u8) -> Self {
        Version { major, minor }
    }

    fn from_wire(v: u64) -> Self {
        Version {
            major: (v >> 8) as u8,
            minor: v as u8,
        }
    }

    fn to_wire(self) -> u64 {
        (self.major as u64) << 8 | self.minor as u64
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Highest protocol version this proxy understands (nix 2.34).
pub const OUR_VERSION: Version = Version::new(1, 38);
/// Oldest host daemon we accept. The allowed ops have stable serializations
/// from here up; an older host daemon fails loudly instead of desyncing.
pub const FLOOR: Version = Version::new(1, 35);
/// Protocol features we advertise upstream. A feature negotiated upstream
/// becomes mandatory for the guest (see `downstream`), so a feature may only
/// be listed here once the pinned guest nix ships it. Nix 2.34 ships none;
/// when the guest nix gains `realisation-with-path`, add it here — the op
/// relay already handles both QueryRealisation formats.
pub const OUR_FEATURES: &[&str] = &[];
pub const FEATURE_REALISATION_WITH_PATH: &str = "realisation-with-path";
/// The feature exchange exists from protocol 1.38.
const FEATURE_EXCHANGE: Version = Version::new(1, 38);

/// Version and feature set shared by both legs of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Negotiated {
    pub version: Version,
    pub features: BTreeSet<String>,
}

impl Negotiated {
    /// Gates the `QueryRealisation` wire format.
    pub fn realisation_with_path(&self) -> bool {
        self.features.contains(FEATURE_REALISATION_WITH_PATH)
    }
}

/// Client-side handshake toward the host daemon.
pub async fn upstream<R, W>(r: &mut R, w: &mut W) -> Result<Negotiated>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    wire::write_u64(w, WORKER_MAGIC_1).await?;
    wire::write_u64(w, OUR_VERSION.to_wire()).await?;
    w.flush().await?;

    let magic = wire::read_u64(r).await?;
    ensure!(
        magic == WORKER_MAGIC_2,
        "host daemon sent bad magic {magic:#x}"
    );
    let daemon = Version::from_wire(wire::read_u64(r).await?);
    ensure!(
        daemon.major == 1,
        "unsupported host daemon protocol version {daemon}"
    );
    let version = daemon.min(OUR_VERSION);
    ensure!(
        version >= FLOOR,
        "host daemon protocol version {daemon} is older than the supported floor {FLOOR}"
    );

    let mut features = BTreeSet::new();
    if version >= FEATURE_EXCHANGE {
        wire::write_string_list(w, OUR_FEATURES).await?;
        w.flush().await?;
        let daemon_features = read_features(r).await?;
        features = OUR_FEATURES
            .iter()
            .filter(|f| daemon_features.contains(**f))
            .map(|f| f.to_string())
            .collect();
    }

    Ok(Negotiated { version, features })
}

/// Server-side handshake toward the guest, advertising exactly the
/// upstream-negotiated version and features.
pub async fn downstream<R, W>(r: &mut R, w: &mut W, negotiated: &Negotiated) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let magic = wire::read_u64(r).await?;
    ensure!(magic == WORKER_MAGIC_1, "guest sent bad magic {magic:#x}");
    wire::write_u64(w, WORKER_MAGIC_2).await?;
    wire::write_u64(w, negotiated.version.to_wire()).await?;
    w.flush().await?;

    let client = Version::from_wire(wire::read_u64(r).await?);
    ensure!(
        client >= negotiated.version,
        "guest protocol version {client} is older than the host daemon's {}; \
         the proxy does not translate between versions",
        negotiated.version
    );

    if negotiated.version >= FEATURE_EXCHANGE {
        let client_features = read_features(r).await?;
        wire::write_string_list(w, &negotiated.features).await?;
        w.flush().await?;
        for feature in &negotiated.features {
            ensure!(
                client_features.contains(feature),
                "guest lacks protocol feature '{feature}' negotiated with the host daemon"
            );
        }
    }

    Ok(())
}

async fn read_features<R: AsyncRead + Unpin>(r: &mut R) -> Result<BTreeSet<String>> {
    wire::read_string_list(r)
        .await?
        .into_iter()
        .map(|f| String::from_utf8(f).context("protocol feature name is not UTF-8"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::testutil::{put_str, put_u64};

    fn features(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn negotiated(minor: u8, feats: &[&str]) -> Negotiated {
        Negotiated {
            version: Version::new(1, minor),
            features: features(feats),
        }
    }

    async fn run_upstream(daemon_script: &[u8]) -> (Result<Negotiated>, Vec<u8>) {
        let mut sent = Vec::new();
        let result = upstream(&mut &daemon_script[..], &mut sent).await;
        (result, sent)
    }

    fn daemon_greeting(version: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        put_u64(&mut buf, WORKER_MAGIC_2);
        put_u64(&mut buf, version);
        buf
    }

    #[tokio::test]
    async fn upstream_feature_exchange() {
        let mut script = daemon_greeting(0x126);
        // Daemon features we don't advertise are not negotiated.
        put_u64(&mut script, 2);
        put_str(&mut script, b"delete-dead-specific-referrers");
        put_str(&mut script, b"realisation-with-path");

        let (result, sent) = run_upstream(&script).await;
        assert_eq!(result.unwrap(), negotiated(38, &[]));

        let mut expected = Vec::new();
        put_u64(&mut expected, WORKER_MAGIC_1);
        put_u64(&mut expected, 0x126);
        put_u64(&mut expected, 0); // our (empty) feature list
        assert_eq!(sent, expected);
    }

    #[tokio::test]
    async fn upstream_pre_feature_exchange() {
        let (result, sent) = run_upstream(&daemon_greeting(0x125)).await;
        assert_eq!(result.unwrap(), negotiated(37, &[]));
        // No feature list after magic + version.
        assert_eq!(sent.len(), 16);
    }

    #[tokio::test]
    async fn upstream_caps_at_our_version() {
        let mut script = daemon_greeting(0x12a); // 1.42
        put_u64(&mut script, 0); // no daemon features
        let (result, _) = run_upstream(&script).await;
        assert_eq!(result.unwrap(), negotiated(38, &[]));
    }

    #[tokio::test]
    async fn upstream_rejects_below_floor() {
        let (result, _) = run_upstream(&daemon_greeting(0x122)).await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("floor"), "{err}");
    }

    #[tokio::test]
    async fn upstream_rejects_wrong_major() {
        let (result, _) = run_upstream(&daemon_greeting(0x226)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn upstream_rejects_bad_magic() {
        let mut script = Vec::new();
        put_u64(&mut script, 0xdead);
        put_u64(&mut script, 0x126);
        let (result, _) = run_upstream(&script).await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("magic"), "{err}");
    }

    async fn run_downstream(guest_script: &[u8], neg: &Negotiated) -> (Result<()>, Vec<u8>) {
        let mut sent = Vec::new();
        let result = downstream(&mut &guest_script[..], &mut sent, neg).await;
        (result, sent)
    }

    #[tokio::test]
    async fn downstream_happy_path() {
        let neg = negotiated(38, &["realisation-with-path"]);
        let mut script = Vec::new();
        put_u64(&mut script, WORKER_MAGIC_1);
        put_u64(&mut script, 0x126);
        put_u64(&mut script, 2);
        put_str(&mut script, b"disable-set-options");
        put_str(&mut script, b"realisation-with-path");

        let (result, sent) = run_downstream(&script, &neg).await;
        result.unwrap();

        let mut expected = Vec::new();
        put_u64(&mut expected, WORKER_MAGIC_2);
        put_u64(&mut expected, 0x126);
        put_u64(&mut expected, 1);
        put_str(&mut expected, b"realisation-with-path");
        assert_eq!(sent, expected);
    }

    #[tokio::test]
    async fn downstream_rejects_older_guest() {
        let neg = negotiated(38, &[]);
        let mut script = Vec::new();
        put_u64(&mut script, WORKER_MAGIC_1);
        put_u64(&mut script, 0x125);
        let (result, _) = run_downstream(&script, &neg).await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("older"), "{err}");
    }

    #[tokio::test]
    async fn downstream_rejects_missing_feature() {
        let neg = negotiated(38, &["realisation-with-path"]);
        let mut script = Vec::new();
        put_u64(&mut script, WORKER_MAGIC_1);
        put_u64(&mut script, 0x126);
        put_u64(&mut script, 0); // guest has no features
        let (result, _) = run_downstream(&script, &neg).await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("lacks protocol feature"), "{err}");
    }

    #[tokio::test]
    async fn downstream_newer_guest_is_fine() {
        let neg = negotiated(37, &[]);
        let mut script = Vec::new();
        put_u64(&mut script, WORKER_MAGIC_1);
        put_u64(&mut script, 0x12a);
        let (result, _) = run_downstream(&script, &neg).await;
        result.unwrap();
    }
}
