{
  pkgs,
  toplevel,
}: let
  mkdir = "${pkgs.coreutils}/bin/mkdir";
  mount = "${pkgs.util-linux}/bin/mount";
in
  pkgs.writeShellScript "claudepod-entry" ''
    set -euo pipefail

    # Set up nix store overlay, then hand off to NixOS init.

    # Preserve host nix store reference before overlaying
    ${mkdir} -p /nix/.host-nix/nix/store
    ${mount} --bind /nix/store /nix/.host-nix/nix/store

    # Writable overlay for /nix/store backed by tmpfs
    ${mkdir} -p /nix/.rw-store
    ${mount} -t tmpfs -o mode=755 none /nix/.rw-store
    ${mkdir} -p /nix/.rw-store/store /nix/.rw-store/work
    ${mount} -t overlay overlay -o lowerdir=/nix/.host-nix/nix/store,upperdir=/nix/.rw-store/store,workdir=/nix/.rw-store/work,userxattr /nix/store

    # Write runtime config for guest systemd service
    echo "''${CLAUDEPOD_PROJECT_PATH:-/tmp}" > /run/claudepod-project
    echo "''${CLAUDEPOD_MODE:-shell}" > /run/claudepod-mode
    echo "''${CLAUDEPOD_HAS_PROJECT:-false}" > /run/claudepod-has-project
    : > /run/claudepod-command
    if [ "$#" -gt 0 ]; then
      printf '%s\0' "$@" > /run/claudepod-command
    fi

    # Forward selected env vars across the systemd boundary.
    # %q emits bash-quoted output; service reads via `set -a; . file; set +a`.
    : > /run/claudepod-env
    for name in ''${!CLAUDE_CODE_@}; do
      printf '%s=%q\n' "$name" "''${!name}" >> /run/claudepod-env
    done
    if [ -n "''${MAX_THINKING_TOKENS-}" ]; then
      printf '%s=%q\n' MAX_THINKING_TOKENS "$MAX_THINKING_TOKENS" >> /run/claudepod-env
    fi

    if [ -n "''${CLAUDEPOD_VERBOSE-}" ]; then
      ${mkdir} -p /run/systemd/system.conf.d
      printf '[Manager]\nShowStatus=yes\n' > /run/systemd/system.conf.d/50-claudepod-verbose.conf
    fi

    exec ${toplevel}/init
  ''
