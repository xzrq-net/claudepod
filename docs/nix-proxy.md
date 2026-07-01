# nix-proxy maintenance

The proxy avoids direct guest reads of the host Nix sqlite DB/WAL. Store
contents still come from the read-only `/nix/store` mount; lower-store metadata
queries go through a `unix://` daemon store backed by `claudepod-nix-proxy`.

## Ops

The allowlist is the audited lower-store demand of Nix `local-overlay-store.cc`
plus remote-store connection setup, not a generic set of read-only-looking ops:

- `SetOptions`
- `IsValidPath`
- `QueryReferrers`
- `QueryPathInfo`
- `QueryPathFromHashPart`
- `QueryValidPaths`
- `QueryValidDerivers`
- `QueryRealisation`

Everything else should remain a loud rejection, not opportunistically forwarded.

## On-demand fills

`IsValidPath` is intercepted, not blindly relayed. When the host daemon says
invalid and the path appears in the run-roots manifest (`--nix-run-roots`,
built by `claudepod --build-nix-run-roots` from the pinned nixpkgs package
universe), the proxy issues `EnsurePath` on a fresh proxy-owned host daemon
connection, re-checks validity, and only then answers the guest. This is the
one host-store mutation a guest can trigger: substituting a manifest-listed
path into the host store. Guests never reach `EnsurePath` directly; fill
targets must parse as direct `<hash>-<name>` store paths and be present in
the manifest.

Before changing guest Nix, `OUR_VERSION`, `OUR_FEATURES`, or the allowlist,
re-audit every parsed payload in `src/proxy/handshake.rs`,
`src/proxy/session.rs`, `src/proxy/ops.rs`, `src/proxy/stderr.rs`, and
`src/proxy/host_client.rs` against Nix `local-overlay-store.cc`,
`worker-protocol.cc`, and `daemon.cc`.

`SetOptions` is intentionally parsed for framing and swallowed, not forwarded:
forwarding would let guest-chosen client settings reach the host daemon. Its
`ClientSettings` layout is frozen only as long as the advertised
protocol/features do not add fields.
