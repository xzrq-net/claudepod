# nix-proxy: key decisions

A host-side proxy that speaks the nix daemon wire protocol and forwards a
small allowlist of read-only operations to the host nix-daemon. It replaces
the guest's direct (read-only) access to the host sqlite db, which is the
source of the WAL staleness problem (see README "Caveats").

## Architecture

- The guest keeps its `local-overlay://` store, but the lower store changes
  from `local://?root=/nix/.host-nix&read-only=true` to a `unix://` daemon
  store pointing at the proxy socket. This is statically valid: the lower
  store only needs to cast to `LocalFSStore` (nix
  `local-overlay-store.cc:50`), and `UDSRemoteStore` is one via
  `IndirectRootStore` (`uds-remote-store.hh:62`, `indirect-root-store.hh:39`).
- Store *contents* still come from the existing `/nix/store:ro` bind mount via
  overlayfs; only *metadata* queries go over the wire. The
  `/nix/var/nix/db:ro` mount and the WAL checkpoint hack are removed.
- The proxy is a filtering tee, not a daemon reimplementation: per client
  connection it dials the host daemon, relays the handshake, then per op
  either parses-and-forwards or rejects.
- Reject = synthesize `STDERR_ERROR` with a descriptive message, then close
  the connection. Closing avoids ever having to drain framed payloads of
  mutating ops, and an unexpected op from the lower store is a bug we want
  loud. SetOptions is special-cased: parsed for framing, swallowed, answered
  with a synthetic empty success. None of the allowed ops depend on client
  settings, and forwarding would apply guest-chosen settings unclamped on
  the host daemon whenever the invoking user is in `trusted-users`
  (daemon.cc `ClientSettings::apply`).
- Hand-rolled wire format, no nix-wire dependency. The allowed subset is
  primitives only (u64 LE, padded length-prefixed strings, string sets); all
  protocol complexity (framed sources, NAR streaming, build results) lives in
  ops we reject. nix-wire (~/src/nix-wire) stays useful as debug tooling
  (`record`/`decode` of live sessions) and as a cross-check, not as a dep.
- Not a security boundary. The guest already has the full store mounted
  read-only; the proxy exists for correctness (WAL) and to keep guest hands
  off host daemon mutations. The proxy connects to the host daemon as the
  invoking user (untrusted client), which is sufficient for all allowed ops.

## Limits

The guest is untrusted; the proxy bounds what it can cost the host. The
bounds are derived from the protocol where possible, generous everywhere —
they exist to stop abuse, not to police a healthy client.

- Concurrency: at most 32 sessions (`MAX_SESSIONS`), backpressure not
  rejection. The permit is taken before accept, so excess connections wait
  in the kernel listen backlog (no proxy fds); a connect flood ends up
  blocking guest-side. This caps host daemon forks; sessions map 1:1 to
  upstream connections.
- The host daemon is dialed only after the guest sends valid protocol
  magic, so a connect-and-stall client costs the host nothing.
- Guest strings cap at 64 KiB (`MAX_GUEST_STRING`; a real store path is at
  most store dir + 245 bytes — nix caps the name component at 211), feature
  lists at 256 entries. The feature list is the only place guest bytes
  accumulate; everything else streams. Host-leg strings keep a coarse
  16 MiB cap (`MAX_HOST_STRING`) — the host daemon is trusted.
- Timeouts: 60s per guest handshake step and per-op argument transfer. None
  at op boundaries, where pooled guest connections idle legitimately, and
  none on host reads (trusted, and queries can take a while under load).

## Protocol version policy

- Handshake order: upstream (host daemon) first, take the negotiated version,
  advertise exactly that downstream to the guest. Both legs then agree and
  responses relay verbatim with no cross-version translation.
- Hard floor (~1.35); refuse handshakes below it. Guest nix is pinned by this
  flake, host nix is whatever the host runs — the floor turns "host daemon
  too old" into a clear error instead of a desync. Between floor and current,
  the allowed ops have almost no serialization drift; the one known gate is
  QueryRealisation (realisation-with-path feature, ≥1.38 feature exchange).

## Ops

Complete demand set, enumerated from every `lowerStore->` call site in nix
`local-overlay-store.cc` plus `RemoteStore::initConnection`:

| Op | # | Notes |
|---|---|---|
| SetOptions | 19 | connection setup; parse for framing, swallow (never forwarded) |
| IsValidPath | 1 | |
| QueryReferrers | 6 | |
| QueryPathInfo | 26 | result is ValidPathInfo (all primitives) |
| QueryPathFromHashPart | 29 | |
| QueryValidPaths | 31 | guest always sends substitute=false (default `NoSubstitute`); reject if flag set |
| QueryValidDerivers | 33 | |
| QueryRealisation | 43 | version-gated result format |

Everything else → reject. Notably *not* needed: NarFromPath (contents come
through the mount), AddTempRoot (local-overlay never calls it on the lower
store), QueryMissing, QuerySubstitutable*. This set is valid for the pinned
guest nix; a nixpkgs bump that changes local-overlay's lower-store usage
shows up as a loud rejection, not silent breakage.

## Module breakdown

Single binary, `src/bin/claudepod-nix-proxy.rs`, plus a small lib so the test
binary can drive it in-process:

- `wire` — u64/string/string-set read+write over tokio streams.
- `handshake` — both legs: client-side toward host daemon, server-side toward
  guest; version pinning logic; feature-set relay.
- `ops` — op enum, per-op arg parse/copy, per-op result parse/copy, the
  allowlist policy.
- `stderr` — daemon→client message loop relay (`Next`, `StartActivity`,
  `StopActivity`, `Result`, terminal `Last`/`Error`); synthetic error
  writer. `STDERR_READ`/`WRITE` are protocol violations here (streaming ops
  are rejected).
- `session` — per-connection state machine: dial upstream, handshakes, op
  loop dispatching through policy.
- `main` — clap (`--listen <socket>`, `--upstream`, default
  `/nix/var/nix/daemon-socket/socket`), accept loop, one tokio task per
  connection. The guest daemon uses a connection pool, so concurrent
  connections are normal; each maps 1:1 to an upstream connection.

New deps: `tokio`. Built like the other binaries via crane in flake.nix.

## Lifecycle and invocation

- `claudepod-start` currently `exec`s podman (`claudepod-start.rs:126`).
  It changes to spawn-and-wait: create a per-instance runtime dir (XDG
  runtime dir), start the proxy listening there, run podman as a child, wait,
  then kill the proxy and remove the socket. Proxy lifetime == container
  lifetime; one proxy per pod instance, so concurrent pods don't share state.
- Proxy crash mid-session: guest daemon gets connection errors on lower-store
  queries and surfaces them to the nix client; nothing corrupts. No automatic
  restart in v1.

## Container protocol (TBD)

Probable shape: bind-mount the runtime dir (or just the socket) into the
container as a podman volume, e.g. `…/nix-proxy.sock:/nix/.host-nix-daemon/socket`,
and set the guest daemon's lower store to
`unix:///nix/.host-nix-daemon/socket` in `guest-module.nix` (NIX_REMOTE,
currently line 166). Open detail: whether the `unix://` store needs explicit
`root`/`real` params so its `realStoreDir` matches the overlay's expectations
(`check-mount=false` today makes this moot, but verify against `toUpperPath`
and friends). Socket permissions are simple — both ends run as the same user
in a rootless setup.

## Testing

End-to-end test as a regular binary (`cargo run --bin claudepod-e2e`, not the cargo
test harness — legible sequential setup, no test concurrency) that re-execs
itself under new user+mount+pid namespaces via util-linux `unshare
--map-root-user` ("am I pid 1?" is the in-namespace marker), so it runs
unprivileged, can mount, and the pid namespace reaps the daemons on exit:

1. Set up two stores on tmpfs: a "host" store and an overlayfs-merged
   "container" store (upper on tmpfs, lower = host store), mirroring the real
   mount layout.
2. Run a real `nix-daemon` against the host store.
3. Run the proxy in-process (thread/tokio task) pointed at the host daemon's
   socket — in-process so test failures have backtraces and coverage.
4. Run a second real `nix-daemon` with `NIX_REMOTE=local-overlay://…` whose
   lower store is the proxy socket — this is the "container" daemon.
   Everything guest-side (daemon, clients, builders) runs in an extra mount
   namespace where the merged overlay is bound over the logical store dir,
   mirroring the container's view of /nix/store: nix clients are
   LocalFSStores that read store files directly at the logical path, so a
   daemon socket alone is not enough. (Beware `root=`: with it set, `real`
   defaults to `<root>/nix/store` regardless of the logical store dir.)
5. Issue requests through the container daemon and assert results.

Key scenarios, one test each in `claudepod-e2e.rs` (doc comments there state
the layer friction each one pins):

- Closure sync: querying a host closure root through the guest pulls the
  whole reference chain into the upper db (`local-overlay-store.cc`).
- Guest build whose output references host paths — the
  allowlist-completeness test: any unexpected lower-store demand fails the
  build loudly.
- Build dedup: a guest rebuild of a drv the host already has resolves to the
  lower path without copying anything into the upper layer.
- Invalid-then-valid: a path queried before the host registers it is visible
  immediately after — no stale negative in the daemon/proxy/daemon chain.
- Demand sweep: a battery of innocuous read-only commands; success means
  local-overlay's lower-store demand stayed within the allowlist.
- Guest GC: drops unrooted upper paths, leaves lower paths alone.
- Desync repair: lower files present but unregistered (the README "fchmodat"
  condition, manufactured directly). The guest's delete-and-re-add reconcile
  fails with EIO — the lower layer changed under the live overlay mount,
  which overlayfs treats as undefined — pinned together with the invariants
  that the lower store stays intact and the daemon survives.

Direct-wire rejection (disallowed op → `STDERR_ERROR` + close) is covered at
the unit level in `ops`/`session`; e2e exercises rejection only implicitly,
via builds and the demand sweep.

Unit tests for `wire`/`handshake`/`ops` against byte fixtures; optionally a
recorded real session (via nix-wire's `record`) replayed against the parser.

## Cleanup once landed

- Drop `/nix/var/nix/db` mount from `claudepod-start.rs`.
- Drop the WAL checkpoint sudo hack and its README caveat (README:114-122).
- Update README store-architecture section for the new lower store.
