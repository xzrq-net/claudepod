//! The op allowlist and per-op payload relay.
//!
//! The allowed set is the complete demand of nix's local-overlay store on
//! its lower store (see docs/nix-proxy.md "Ops"). Payloads are parsed only
//! for field boundaries and copied verbatim — both legs run the same
//! negotiated protocol version, so no translation is ever needed.

use anyhow::{Context, Result, ensure};
use tokio::io::{AsyncRead, AsyncWrite};

use super::handshake::Negotiated;
use super::wire;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    IsValidPath,
    QueryReferrers,
    SetOptions,
    QueryPathInfo,
    QueryPathFromHashPart,
    QueryValidPaths,
    QueryValidDerivers,
    QueryRealisation,
}

impl Op {
    /// The allowlist: op word to allowed op.
    pub fn allowed(word: u64) -> Option<Op> {
        Some(match word {
            1 => Op::IsValidPath,
            6 => Op::QueryReferrers,
            19 => Op::SetOptions,
            26 => Op::QueryPathInfo,
            29 => Op::QueryPathFromHashPart,
            31 => Op::QueryValidPaths,
            33 => Op::QueryValidDerivers,
            43 => Op::QueryRealisation,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Op::IsValidPath => "IsValidPath",
            Op::QueryReferrers => "QueryReferrers",
            Op::SetOptions => "SetOptions",
            Op::QueryPathInfo => "QueryPathInfo",
            Op::QueryPathFromHashPart => "QueryPathFromHashPart",
            Op::QueryValidPaths => "QueryValidPaths",
            Op::QueryValidDerivers => "QueryValidDerivers",
            Op::QueryRealisation => "QueryRealisation",
        }
    }
}

/// Name of any op the nix protocol defines, for rejection messages.
pub fn op_name(word: u64) -> &'static str {
    match word {
        1 => "IsValidPath",
        6 => "QueryReferrers",
        7 => "AddToStore",
        8 => "AddTextToStore",
        9 => "BuildPaths",
        10 => "EnsurePath",
        11 => "AddTempRoot",
        12 => "AddIndirectRoot",
        13 => "SyncWithGC",
        14 => "FindRoots",
        18 => "QueryDeriver",
        19 => "SetOptions",
        20 => "CollectGarbage",
        21 => "QuerySubstitutablePathInfo",
        22 => "QueryDerivationOutputs",
        23 => "QueryAllValidPaths",
        26 => "QueryPathInfo",
        28 => "QueryDerivationOutputNames",
        29 => "QueryPathFromHashPart",
        30 => "QuerySubstitutablePathInfos",
        31 => "QueryValidPaths",
        32 => "QuerySubstitutablePaths",
        33 => "QueryValidDerivers",
        34 => "OptimiseStore",
        35 => "VerifyStore",
        36 => "BuildDerivation",
        37 => "AddSignatures",
        38 => "NarFromPath",
        39 => "AddToStoreNar",
        40 => "QueryMissing",
        41 => "QueryDerivationOutputMap",
        42 => "RegisterDrvOutput",
        43 => "QueryRealisation",
        44 => "AddMultipleToStore",
        45 => "AddBuildLog",
        46 => "BuildPathsWithResults",
        47 => "AddPermRoot",
        _ => "unknown",
    }
}

/// Relay an op's arguments guest-to-host. The op word itself has already
/// been forwarded by the caller.
pub async fn copy_args<R, W>(
    op: Op,
    negotiated: &Negotiated,
    guest: &mut R,
    host: &mut W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match op {
        // A single store path (or hash part for QueryPathFromHashPart).
        Op::IsValidPath
        | Op::QueryReferrers
        | Op::QueryPathInfo
        | Op::QueryValidDerivers
        | Op::QueryPathFromHashPart => wire::copy_string(guest, host).await?,
        Op::QueryValidPaths => {
            wire::copy_string_list(guest, host).await?;
            // The substitute flag would make the host daemon fetch paths on
            // our behalf — a mutation. The local-overlay store never asks
            // for it (default NoSubstitute).
            let substitute = wire::read_u64(guest).await?;
            ensure!(
                substitute == 0,
                "QueryValidPaths with substitution is not allowed"
            );
            wire::write_u64(host, 0).await?;
        }
        Op::SetOptions => {
            // ClientSettings (daemon.cc): 12 scalar fields, then counted
            // name/value override pairs. Parsed for framing only — the
            // session swallows SetOptions instead of forwarding it. New
            // fields must be protocol-gated, so this layout is frozen until
            // OUR_VERSION moves (see AGENTS.md).
            for _ in 0..12 {
                wire::copy_u64(guest, host).await?;
            }
            let overrides = wire::copy_u64(guest, host).await?;
            ensure!(
                overrides <= wire::MAX_COUNT,
                "override count {overrides} exceeds limit"
            );
            for _ in 0..overrides {
                wire::copy_string(guest, host).await?; // name
                wire::copy_string(guest, host).await?; // value
            }
        }
        Op::QueryRealisation => {
            if negotiated.realisation_with_path() {
                // DrvOutput: derivation store path + output name.
                wire::copy_string(guest, host).await?;
                wire::copy_string(guest, host).await?;
            } else {
                // Rendered "drvhash!output" id.
                wire::copy_string(guest, host).await?;
            }
        }
    }
    Ok(())
}

/// Relay an op's result host-to-guest (the payload after `STDERR_LAST`).
pub async fn copy_result<R, W>(
    op: Op,
    negotiated: &Negotiated,
    host: &mut R,
    guest: &mut W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match op {
        // Swallowed by the session, never forwarded; nothing to relay even
        // if it were (the result payload is empty).
        Op::SetOptions => {}
        Op::IsValidPath => {
            wire::copy_u64(host, guest).await?;
        }
        Op::QueryReferrers | Op::QueryValidPaths | Op::QueryValidDerivers => {
            wire::copy_string_list(host, guest).await?;
        }
        // Optional store path; empty string means not found.
        Op::QueryPathFromHashPart => wire::copy_string(host, guest).await?,
        Op::QueryPathInfo => {
            let valid = wire::copy_u64(host, guest).await?;
            if valid != 0 {
                copy_path_info(host, guest).await.context("ValidPathInfo")?;
            }
        }
        Op::QueryRealisation => {
            if negotiated.realisation_with_path() {
                // optional<UnkeyedRealisation>
                let tag = wire::copy_u64(host, guest).await?;
                ensure!(tag <= 1, "invalid optional tag {tag}");
                if tag == 1 {
                    wire::copy_string(host, guest).await?; // output store path
                    wire::copy_string_list(host, guest).await?; // signatures
                }
            } else {
                // Pre-feature daemons return a set of strings (realisations
                // as JSON from 1.31, bare store paths before that).
                wire::copy_string_list(host, guest).await?;
            }
        }
    }
    Ok(())
}

/// UnkeyedValidPathInfo (worker-protocol.cc), protocol >= 1.16 fields
/// included — always present above the version floor.
async fn copy_path_info<R, W>(host: &mut R, guest: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    wire::copy_string(host, guest).await?; // deriver (optional store path)
    wire::copy_string(host, guest).await?; // narHash (base16)
    wire::copy_string_list(host, guest).await?; // references
    wire::copy_u64(host, guest).await?; // registrationTime
    wire::copy_u64(host, guest).await?; // narSize
    wire::copy_u64(host, guest).await?; // ultimate
    wire::copy_string_list(host, guest).await?; // sigs
    wire::copy_string(host, guest).await?; // ca (optional)
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::proxy::testutil::{put_str, put_u64};

    fn negotiated(features: &[&str]) -> Negotiated {
        Negotiated {
            version: super::super::handshake::OUR_VERSION,
            features: features
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>(),
        }
    }

    #[test]
    fn allowlist() {
        for (word, name) in [
            (1, "IsValidPath"),
            (6, "QueryReferrers"),
            (19, "SetOptions"),
            (26, "QueryPathInfo"),
            (29, "QueryPathFromHashPart"),
            (31, "QueryValidPaths"),
            (33, "QueryValidDerivers"),
            (43, "QueryRealisation"),
        ] {
            assert_eq!(Op::allowed(word).unwrap().name(), name);
        }
        for word in [0, 7, 9, 11, 38, 40, 44, 48, u64::MAX] {
            assert_eq!(Op::allowed(word), None, "op {word} must not be allowed");
        }
    }

    async fn assert_args_copy(op: Op, neg: &Negotiated, args: &[u8]) {
        let mut out = Vec::new();
        let mut input = args;
        copy_args(op, neg, &mut input, &mut out).await.unwrap();
        assert_eq!(out, args, "copy must be byte-identical");
        assert!(input.is_empty(), "copy must consume all args");
    }

    async fn assert_result_copy(op: Op, neg: &Negotiated, result: &[u8]) {
        let mut out = Vec::new();
        let mut input = result;
        copy_result(op, neg, &mut input, &mut out).await.unwrap();
        assert_eq!(out, result, "copy must be byte-identical");
        assert!(input.is_empty(), "copy must consume the whole result");
    }

    #[tokio::test]
    async fn path_args() {
        let mut args = Vec::new();
        put_str(&mut args, b"/nix/store/abc-foo");
        for op in [
            Op::IsValidPath,
            Op::QueryReferrers,
            Op::QueryPathInfo,
            Op::QueryValidDerivers,
        ] {
            assert_args_copy(op, &negotiated(&[]), &args).await;
        }
    }

    #[tokio::test]
    async fn query_valid_paths_args() {
        let mut args = Vec::new();
        put_u64(&mut args, 2);
        put_str(&mut args, b"/nix/store/abc-foo");
        put_str(&mut args, b"/nix/store/def-bar");
        put_u64(&mut args, 0); // NoSubstitute
        assert_args_copy(Op::QueryValidPaths, &negotiated(&[]), &args).await;
    }

    #[tokio::test]
    async fn query_valid_paths_rejects_substitution() {
        let mut args = Vec::new();
        put_u64(&mut args, 0);
        put_u64(&mut args, 1); // Substitute
        let mut out = Vec::new();
        let err = copy_args(
            Op::QueryValidPaths,
            &negotiated(&[]),
            &mut args.as_slice(),
            &mut out,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("substitution"), "{err}");
    }

    #[tokio::test]
    async fn set_options_args() {
        let mut args = Vec::new();
        for v in [0, 0, 0, 5, 16, 0, 1, 0, 0, 0, 0, 1] {
            put_u64(&mut args, v);
        }
        put_u64(&mut args, 2);
        put_str(&mut args, b"max-jobs");
        put_str(&mut args, b"4");
        put_str(&mut args, b"warn-dirty");
        put_str(&mut args, b"false");
        assert_args_copy(Op::SetOptions, &negotiated(&[]), &args).await;
        assert_result_copy(Op::SetOptions, &negotiated(&[]), &[]).await;
    }

    #[tokio::test]
    async fn query_realisation_formats() {
        let neg_new = negotiated(&["realisation-with-path"]);
        let mut args = Vec::new();
        put_str(&mut args, b"/nix/store/abc-foo.drv");
        put_str(&mut args, b"out");
        assert_args_copy(Op::QueryRealisation, &neg_new, &args).await;

        let mut result = Vec::new();
        put_u64(&mut result, 1);
        put_str(&mut result, b"/nix/store/def-foo");
        put_u64(&mut result, 1);
        put_str(&mut result, b"cache.example.org:sig");
        assert_result_copy(Op::QueryRealisation, &neg_new, &result).await;

        let neg_old = negotiated(&[]);
        let mut args = Vec::new();
        put_str(&mut args, b"sha256:abcd!out");
        assert_args_copy(Op::QueryRealisation, &neg_old, &args).await;

        let mut result = Vec::new();
        put_u64(&mut result, 1);
        put_str(&mut result, b"{\"json\":\"realisation\"}");
        assert_result_copy(Op::QueryRealisation, &neg_old, &result).await;
    }

    #[tokio::test]
    async fn is_valid_path_result() {
        let mut result = Vec::new();
        put_u64(&mut result, 1);
        assert_result_copy(Op::IsValidPath, &negotiated(&[]), &result).await;
    }

    #[tokio::test]
    async fn path_set_results() {
        let mut result = Vec::new();
        put_u64(&mut result, 1);
        put_str(&mut result, b"/nix/store/abc-foo");
        for op in [
            Op::QueryReferrers,
            Op::QueryValidPaths,
            Op::QueryValidDerivers,
        ] {
            assert_result_copy(op, &negotiated(&[]), &result).await;
        }
    }

    #[tokio::test]
    async fn query_path_info_result() {
        let mut result = Vec::new();
        put_u64(&mut result, 1); // valid
        put_str(&mut result, b"/nix/store/abc-foo.drv"); // deriver
        put_str(&mut result, b"abcd0123"); // narHash
        put_u64(&mut result, 1); // references
        put_str(&mut result, b"/nix/store/def-bar");
        put_u64(&mut result, 1700000000); // registrationTime
        put_u64(&mut result, 4096); // narSize
        put_u64(&mut result, 0); // ultimate
        put_u64(&mut result, 1); // sigs
        put_str(&mut result, b"cache.example.org:sig");
        put_str(&mut result, b""); // ca
        assert_result_copy(Op::QueryPathInfo, &negotiated(&[]), &result).await;

        let mut invalid = Vec::new();
        put_u64(&mut invalid, 0);
        assert_result_copy(Op::QueryPathInfo, &negotiated(&[]), &invalid).await;
    }
}
