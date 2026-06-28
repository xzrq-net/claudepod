# Followups

## 1:1 project path mapping

Revisit host-to-guest project paths. The clean shape is to have podman mount the
selected host root at a static staging path, then have the entry process
bind-mount it into the same absolute path used on the host, refusing reserved or
conflicting targets.

## nix-proxy on-demand fill for pinned nixpkgs roots

Goal: if the guest asks the lower store whether a path is valid and the host
does not currently have it, allow the proxy to fill that host path only when it
is an evaluated top-level root from the claudepod-pinned nixpkgs. This is an
authorization set for substitution, not a pre-realized closure.

Runtime shape:

- The feature is optional. Normal `claudepod` startup must not evaluate all of
  nixpkgs, build the manifest, or fail when the manifest is absent.
- Cache manifests under the host XDG cache dir, keyed by policy version, guest
  system, pinned nixpkgs source hash, and guest toplevel hash, for example:

  ```text
  $XDG_CACHE_HOME/claudepod/nix-run-roots/v1/<guest-system>/<nixpkgs-hash>/<toplevel-hash>.txt
  ```

- If the cache file exists, `claudepod-start` passes it to
  `claudepod-nix-proxy` and on-demand fills are enabled. If it is missing,
  print a concise message and start with current proxy behavior:

  ```text
  nix run root manifest missing; proxy fills disabled
  run: claudepod --build-nix-run-roots
  ```

- Add an explicit launcher flag to build the missing cache artifact, e.g.
  `--build-nix-run-roots`. Rust should orchestrate this by spawning pinned `nix`
  commands and atomically writing the cache file; Rust should not implement Nix
  evaluation.
- Host launchers should pass the minimal identity/execution inputs needed for
  cache lookup and manifest generation:

  ```text
  CLAUDEPOD_TOPLEVEL=/nix/store/...-nixos-system-...
  CLAUDEPOD_GUEST_SYSTEM=x86_64-linux
  CLAUDEPOD_NIXPKGS=/nix/store/...-source
  CLAUDEPOD_NIX=/nix/store/...-nix-.../bin/nix
  ```

  Optional diagnostic/header values are fine, e.g. `CLAUDEPOD_NIXPKGS_REV` and
  `CLAUDEPOD_NIXPKGS_NAR_HASH`, but the cache key should use store path hash
  parts and explicit policy version/config, not human-readable rev strings.
- Nested claudepods are out of scope for this feature. Do not propagate manifests
  through `/run`, store layers, or nested launcher fallbacks. A nested launch with
  no host-side cache-visible manifest simply runs with fills disabled.

Pieces:

- Build a manifest artifact from the same nixpkgs input and guest system used for
  the guest:

  ```nix
  pkgs = import nixpkgs {
    system = guestSystem;
    config = {
      allowUnfree = true;
      allowAliases = false;
    };
  };
  ```

  Candidate policy: all top-level `pkgs` attrs where `lib.isDerivation value`
  and `tryEval (toString value)` succeeds. In the 2026-06-28 locked nixpkgs
  this was about 23.6k attrs, roughly 1.5 MiB as sorted newline-separated store
  paths. A stricter `meta.mainProgram != null` policy was about 14.5k unique
  paths, but the broader set is still small enough and catches things like
  `postgresql`.

  Output size is not the same as generation cost. Collecting these paths forces
  Nix evaluation of many top-level nixpkgs attributes so their output store paths
  are known. A naive `attrNames pkgs` / `tryEval (toString value)` pass timed out
  after 180s on 2026-06-28; a later exploratory run did complete. Time and
  optimize the generator after the core wiring exists. Manifest generation
  belongs only behind the explicit build flag or another opt-in maintenance
  command, not in devshell, home-manager rebuild, package build, or normal
  launcher startup.

- `claudepod-start` computes the expected cache path from policy version,
  `CLAUDEPOD_GUEST_SYSTEM`, `CLAUDEPOD_NIXPKGS`, and the selected guest toplevel.
  If the file exists, use it; if absent and `--build-nix-run-roots` was passed,
  build it; if absent without the flag, print the disabled-feature message and
  continue.

- `claudepod-start` passes the cache file path to `claudepod-nix-proxy`, e.g.
  `--nix-run-roots $XDG_CACHE_HOME/claudepod/nix-run-roots/v1/<guest-system>/<nixpkgs-hash>/<toplevel-hash>.txt`.

- The proxy loads the manifest once at startup. A sorted `Vec<String>` or
  `HashSet<String>` is fine; this path count is not performance-critical. Store
  and check full store paths, not only hash parts. A hash-part index is fine as
  an implementation detail only if full-path equality is still verified.

- Intercept only `IsValidPath` results:

  1. Forward `IsValidPath` to the host daemon as today.
  2. If the host says valid, return valid.
  3. If the host says invalid and the requested full path is not in the
     manifest, return invalid.
  4. If the host says invalid and the path is in the manifest, fill it via a
     separate host daemon connection, then re-check validity. Return true only
     if the re-check says valid.
  5. If fill fails or the re-check is still invalid, return invalid, not a guest
     error. This is cache-fill-miss semantics: the host cache was not populated,
     the guest did nothing wrong, and a later `IsValidPath` may retry. Do not
     negative-cache failures.

  Framing detail: do not relay the first host query's `STDERR_LAST` before the
  possible fill/re-check decision. The guest must see one terminal stderr marker
  followed by the final boolean result. If the first host query ends in
  `STDERR_ERROR`, relay it exactly as today and do not attempt a fill. On fill
  failure, log a proxy warning with the full store path and error; returning
  `false` is enough. A non-terminal `STDERR_NEXT` warning before the final
  `STDERR_LAST` is acceptable but not required.

- Do not add guest-facing `EnsurePath`, `AddToStore*`, or
  `QueryValidPaths(substitute=1)` to the proxy allowlist. The fill is an
  internal proxy action.

Host fill primitive:

- Preferred despite the extra protocol code: implement a small internal
  host-daemon client and send worker op `EnsurePath` (`10`) on a fresh
  connection. Do not reuse the relayed guest<->host connection for this.
  Handshake like the existing upstream leg, send zero obsolete CPU affinity and
  reserve-space fields, drain the daemon greeting, send `EnsurePath` + store
  path, drain stderr to `STDERR_LAST`, read the trailing success word, preserve
  any error/log text needed for the warning path above, then close. This is a
  small stable protocol surface and reuses the proxy's existing handshake, wire,
  and stderr code.
- Simpler fallback/prototype: spawn host `nix-store --realise /nix/store/...`
  against the host daemon. This avoids new worker-protocol code but adds process
  overhead, requires passing an absolute `nix-store` path or carefully sanitized
  host `PATH`, requires explicitly setting the host daemon remote so guest/proxy
  settings cannot leak in, and depends on CLI behavior instead of the daemon
  API.

Keying/caching:

- Key the cache by manifest policy version, guest system, nixpkgs source hash,
  and guest toplevel hash. Do not rely on the toplevel hash alone to imply the
  exact nixpkgs package universe.
- Include the manifest policy/config in the versioned key. Current policy config:
  `allowUnfree = true`, `allowAliases = false`, all top-level derivations whose
  output path can be evaluated.
- Write cache files atomically: generate into a temp path in the same cache dir,
  fsync enough to avoid obvious torn files, then rename into place.

Required config tasks:

- Pass `CLAUDEPOD_GUEST_SYSTEM`, `CLAUDEPOD_NIXPKGS`, and `CLAUDEPOD_NIX` from
  `mkClaudepod`/`mkLauncher` for host launchers. Nested guest launchers should
  leave them unset and run with fills disabled.
- Pin the guest `nixpkgs` registry to the claudepod nixpkgs input so
  `nix run nixpkgs#...` resolves to the same source the manifest authorizes.
- Make nixpkgs policy consistent across this project and the guest:
  `allowUnfree = true`, `allowAliases = false`. This includes project-level
  nixpkgs imports used to build claudepod and the guest systemwide nixpkgs
  config, so unfree flake outputs evaluate without relying on
  `NIXPKGS_ALLOW_UNFREE=1 --impure`.

Required checks:

- Unit-test manifest membership on full store paths, including same-hash-part or
  same-name mismatches that must not authorize a fill.
- Unit-test launcher cache behavior: missing manifest without the build flag
  prints the disabled-feature message and does not pass a manifest to the proxy;
  existing manifest is passed through; explicit build flag writes the expected
  cache file atomically.
- Unit-test `IsValidPath` paths: host-valid pass-through, host-invalid manifest
  miss, manifest hit with fill success and successful re-check, fill failure
  returning invalid plus a warning, and fill success followed by failed re-check.
- Unit-test stderr/result framing for the intercepted `IsValidPath` path: the
  guest receives one terminal stderr marker before the final boolean.
- Keep existing allowlist tests proving guest-facing `EnsurePath`, `AddToStore*`,
  and `QueryValidPaths(substitute=1)` are rejected.
- Add an e2e case where a guest `nix run nixpkgs#...` target is absent from the
  host store, authorized by the manifest, substituted by the proxy fill, and then
  visible to the guest. Also cover a manifest miss that remains a normal invalid
  path.

Implementation slices:

1. Nixpkgs identity/policy plumbing: wrapper env for guest system, pinned nixpkgs
   source, pinned nix binary, guest registry pin, and `allowUnfree = true`;
   `allowAliases = false` project/guest policy.
2. Launcher/cache plumbing: XDG cache path, policy-versioned key
   (`guest-system`/`nixpkgs-hash`/`toplevel-hash`), missing-cache message,
   optional `--build-nix-run-roots` flag stub, and proxy arg only when a manifest
   exists.
3. Manifest generator: opt-in command path that invokes pinned Nix, validates
   output shape, and atomically writes the cache file. This can land before the
   proxy consumes the manifest.
4. Proxy manifest loader: parse sorted newline-separated full store paths, reject
   malformed entries, and expose membership checks. No fill behavior yet.
5. Internal host `EnsurePath` client: fresh host daemon connection, handshake,
   post-handshake, `EnsurePath`, stderr drain, trailing success read, and warning
   capture.
6. Intercepted `IsValidPath`: final-result framing, cache-fill-miss semantics,
   retry-on-next-query behavior, and no guest-facing allowlist expansion.
7. E2E coverage for enabled fill, missing manifest disabled behavior, and
   manifest miss.
