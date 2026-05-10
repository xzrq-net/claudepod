{
  pkgs,
  username,
  toplevel,
  stateDir,
  srcRoot,
}: let
  entryScript = import ./entry.nix {inherit pkgs toplevel;};
  defaultStateDir = if stateDir == null then "" else pkgs.lib.escapeShellArg stateDir;
  defaultSrcRoot = if srcRoot == null then "" else pkgs.lib.escapeShellArg srcRoot;

  image = pkgs.dockerTools.streamLayeredImage {
    name = "claudepod";
    tag = "latest";
    contents = [];
    includeStorePaths = false;
    config.Entrypoint = ["${entryScript}"];
  };

  mkPodCommand = {
    name,
    defaultMode,
  }:
    pkgs.writeShellScriptBin name ''
      set -euo pipefail

      MODE=${defaultMode}
      DEFAULT_STATE_DIR=${defaultStateDir}
      DEFAULT_SRC_ROOT=${defaultSrcRoot}

      if [ -n "''${CLAUDEPOD_STATE_DIR-}" ]; then
        STATE_DIR="''${CLAUDEPOD_STATE_DIR}"
      elif [ -n "$DEFAULT_STATE_DIR" ]; then
        STATE_DIR="$DEFAULT_STATE_DIR"
      else
        STATE_DIR="''${XDG_DATA_HOME:-$HOME/.local/share}/claudepod"
      fi

      if [ -n "''${CLAUDEPOD_SRC_ROOT-}" ]; then
        SRC_ROOT="''${CLAUDEPOD_SRC_ROOT}"
      elif [ -n "$DEFAULT_SRC_ROOT" ]; then
        SRC_ROOT="$DEFAULT_SRC_ROOT"
      else
        SRC_ROOT="$HOME/src"
      fi
      SRC_ROOT="''${SRC_ROOT%/}"
      EXTRA_VOLUMES=()
      while getopts "sv:" opt; do
        case "$opt" in
          s) MODE=shell ;;
          v) EXTRA_VOLUMES+=("$OPTARG") ;;
          *) echo "Usage: ${name} [-s] [-v path] [-v host:guest] ..." >&2; exit 1 ;;
        esac
      done

      PROJECT_DIR="$(pwd)"
      SRC_PREFIX="$SRC_ROOT/"

      ${pkgs.coreutils}/bin/mkdir -p "$STATE_DIR/home"

      # Determine guest landing path
      if [[ "$PROJECT_DIR" == "$SRC_PREFIX"* ]]; then
        REL_PATH="''${PROJECT_DIR#$SRC_PREFIX}"
        GUEST_PATH="/home/${username}/src/$REL_PATH"
        NEED_PROJECT_SHARE=false
      else
        PROJECT_NAME="$(${pkgs.coreutils}/bin/basename "$PROJECT_DIR")"
        GUEST_PATH="/projects/$PROJECT_NAME"
        NEED_PROJECT_SHARE=true
      fi

      # Load container image
      ${image} | ${pkgs.podman}/bin/podman load -q 2>/dev/null

      VOLUMES=(
        -v /nix/store:/nix/store:ro
        -v /nix/var/nix/db:/nix/.host-nix/nix/var/nix/db:ro
        -v "$STATE_DIR/home:/home/${username}"
        -v "$SRC_ROOT:/home/${username}/src"
      )

      if [ "$NEED_PROJECT_SHARE" = true ]; then
        VOLUMES+=(-v "$PROJECT_DIR:$GUEST_PATH")
      fi

      for spec in "''${EXTRA_VOLUMES[@]}"; do
        if [[ "$spec" == *:* ]]; then
          VOLUMES+=(-v "$spec")
        else
          VOLUMES+=(-v "$spec:$spec")
        fi
      done

      ENV_ARGS=()
      for name in "''${!CLAUDE_CODE_@}"; do
        ENV_ARGS+=(-e "$name")
      done
      if [ -n "''${MAX_THINKING_TOKENS-}" ]; then
        ENV_ARGS+=(-e MAX_THINKING_TOKENS)
      fi

      echo "Starting ${name}..."
      echo "  Host path: $PROJECT_DIR"
      echo "  Guest path: $GUEST_PATH"
      echo ""

      exec ${pkgs.podman}/bin/podman run \
        --rm -it \
        --userns=keep-id --user 0:0 \
        --cap-add=SYS_ADMIN \
        --cap-add=NET_RAW \
        --cap-add=SYS_PTRACE \
        --device /dev/fuse \
        --systemd=always \
        --no-hostname \
        --no-hosts \
        --dns=none \
        --pids-limit=-1 \
        --security-opt unmask=ALL \
        "''${VOLUMES[@]}" \
        "''${ENV_ARGS[@]}" \
        -e CLAUDEPOD_PROJECT_PATH="$GUEST_PATH" \
        -e CLAUDEPOD_MODE="$MODE" \
        -e CLAUDEPOD_HAS_PROJECT="$NEED_PROJECT_SHARE" \
        claudepod:latest
    '';
in
  pkgs.symlinkJoin {
    name = "claudepod";
    paths = [
      (mkPodCommand {
        name = "claudepod";
        defaultMode = "claude";
      })
      (mkPodCommand {
        name = "gptpod";
        defaultMode = "codex";
      })
    ];
  }
