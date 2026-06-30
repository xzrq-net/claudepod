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

Before changing guest Nix, `OUR_VERSION`, `OUR_FEATURES`, or the allowlist,
re-audit every parsed payload in `src/proxy/handshake.rs`,
`src/proxy/session.rs`, `src/proxy/ops.rs`, `src/proxy/stderr.rs`, and
`src/proxy/host_client.rs` against Nix `local-overlay-store.cc`,
`worker-protocol.cc`, and `daemon.cc`.

`SetOptions` is intentionally parsed for framing and swallowed, not forwarded:
forwarding would let guest-chosen client settings reach the host daemon. Its
`ClientSettings` layout is frozen only as long as the advertised
protocol/features do not add fields.
