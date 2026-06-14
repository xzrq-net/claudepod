# Followups

## Recursive nix store overlays

Nested claudepods can mount one overlay on top of an existing overlay, but the
kernel filesystem stack depth limit stops the naive approach after one nested
container. True recursive nesting needs a flattened `lowerdir=` stack: each
child gets the parent's writable upper layer plus all inherited lower layers as
separate read-only bind mounts, then mounts its own tmpfs upper over that list.

This is O(depth) bind mounts / podman args and needs an explicit layer-stack
protocol between `claudepod-start` and `claudepod-entry`; short mount paths
matter because the final `lowerdir=` string is finite.

## 1:1 project path mapping

Revisit host-to-guest project paths. The clean shape is to have podman mount the
selected host root at a static staging path, then have the entry process
bind-mount it into the same absolute path used on the host, refusing reserved or
conflicting targets.

## Reformat anyhow context / fix import

Unnecessary verbosity like "failed to" prefixes

## OsString vs String audit for paths / env vars / platform bits

## Audit anyhow:: fully qualified names in rust code
