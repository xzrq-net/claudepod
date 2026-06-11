# Agent notes

## nix-proxy protocol version

The proxy pins the nix daemon worker protocol at `OUR_VERSION`
(`src/proxy/handshake.rs`) on both legs, so op serializations cannot drift
underneath it: any new wire field in nix must be gated on a protocol version
or feature the proxy doesn't advertise.

When bumping `OUR_VERSION` (or adding to `OUR_FEATURES`), re-audit every
allowed op's serialization in `src/proxy/ops.rs` against nix's
`worker-protocol.cc` / `daemon.cc` for the new version. In particular
SetOptions: it is parsed for framing and swallowed (never forwarded), and its
`ClientSettings` layout is frozen until exactly such a bump — a new field
shows up as a framing change here, not as a runtime error.
