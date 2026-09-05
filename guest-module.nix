{
  nix-index-database,
  nixpkgs,
  nixpkgsConfig,
}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.claudepod;
  nixIndexPackages = import nix-index-database {inherit pkgs;};
  nixLocateFull = pkgs.runCommand "nix-locate-full-db" {} ''
    mkdir -p $out/bin
    ln -s ${nixIndexPackages.nix-index-with-db}/bin/nix-locate $out/bin/nix-locate
  '';

  # Per-command devshell loader: re-evaluates direnv against the current cwd
  # before each agent shell command, so devshell changes land without
  # restarting the agent session. Wired into Claude Code via the SessionStart
  # hook in /etc/claude-code/managed-settings.json (sourced through
  # CLAUDE_ENV_FILE) and into codex via /etc/profile (codex runs `bash -lc`).
  # Failure modes are loud on stderr but never fail the shell.
  #
  # The agent process itself is started without direnv loaded (see
  # claudepodShell), so every command is a fresh load from the shell's own
  # env. direnv snapshots the env it first sees and reverts to it on reload;
  # a snapshot taken before the agent started would drop whatever the agent
  # adds to its shells afterwards, e.g. Claude Code's plugin bin/ dirs. A
  # fresh load costs ~30 ms with a warm nix-direnv cache.
  agentDevshell = pkgs.writeText "agent-devshell.sh" ''
    if command -v direnv >/dev/null 2>&1; then
      _de_out=$(timeout 60 direnv export bash 2>/dev/null); _de_rc=$?
      eval "$_de_out"
      if [ "$_de_rc" -eq 124 ]; then
        echo "[devshell] direnv eval timed out (60s); run 'direnv reload' to rebuild the env" >&2
      elif [ "$_de_rc" -ne 0 ]; then
        echo "[devshell] direnv failed; running with base env (blocked .envrc? run 'direnv allow'; or broken shellHook)" >&2
      elif [ -n "''${NIX_DIRENV_DID_FALLBACK:-}" ]; then
        echo "[devshell] flake eval FAILED; using last good devshell env — fix flake.nix" >&2
      fi
      unset _de_out _de_rc
    fi
    true
  '';

  claudepodShell = pkgs.writeShellScript "claudepod-shell" ''
    set -euo pipefail

    MODE=$(${pkgs.coreutils}/bin/cat /run/claudepod-mode)
    PROJECT=$(${pkgs.coreutils}/bin/cat /run/claudepod-project)
    COMMAND=()
    if [ -s /run/claudepod-command ]; then
      while IFS= read -r -d "" arg; do
        COMMAND+=("$arg")
      done < /run/claudepod-command
    fi

    set -a
    . /run/claudepod-env
    set +a

    if [ "''${#COMMAND[@]}" -eq 0 ]; then
      case "$MODE" in
        shell)
          cd "$PROJECT"
          exec ${pkgs.bashInteractive}/bin/bash --login
          ;;
        claude)
          COMMAND=(claude --dangerously-skip-permissions)
          ;;
        codex)
          COMMAND=(${pkgs.nodejs}/bin/npx -y @openai/codex --sandbox danger-full-access --ask-for-approval never)
          ;;
        *)
          echo "Unknown claudepod mode: $MODE" >&2
          exit 1
          ;;
      esac
    fi

    # Deliberately no direnv here: the agent must not inherit loaded direnv
    # state (see agentDevshell). Give it devshell tools for its own process,
    # e.g. MCP servers, by wrapping those commands in `direnv exec`.
    exec ${pkgs.bashInteractive}/bin/bash --login -c '
      cd "$1"
      shift
      exec "$@"
    ' claudepod "$PROJECT" "''${COMMAND[@]}"
  '';

  claudepodRuntimeUser = pkgs.writeShellScript "claudepod-runtime-user" ''
    set -eu

    IFS= read -r username < /run/claudepod-username
    home=/home/$username

    ${pkgs.shadow}/bin/useradd \
      --no-create-home \
      --no-user-group \
      --uid 1000 \
      --gid 100 \
      --groups wheel \
      --home-dir "$home" \
      --shell /run/current-system/sw/bin/bash \
      -- \
      "$username"

    ${pkgs.coreutils}/bin/rm -f /etc/subuid /etc/subgid
    ${pkgs.coreutils}/bin/install -m 0644 -o root -g root /run/claudepod-subuid /etc/subuid
    ${pkgs.coreutils}/bin/install -m 0644 -o root -g root /run/claudepod-subgid /etc/subgid
  '';
in {
  imports = [
    nix-index-database.nixosModules.nix-index
  ];

  options.claudepod = {
    extraGuestPackages = lib.mkOption {
      type = lib.types.functionTo (lib.types.listOf lib.types.package);
      default = _guestPkgs: [];
      description = "Function from guest pkgs to extra guest packages.";
    };

    launcherPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "claudepod launcher package installed in the guest.";
    };
  };

  config = {
    system.stateVersion = lib.trivial.release;
    nixpkgs.config = nixpkgsConfig;

    boot.isNspawnContainer = true;
    # Boot only the session target claudepod needs, while keeping basic/logind
    # for sockets, tmpfiles/wrappers, nix-daemon, and the pam_systemd session.
    systemd.defaultUnit = "claudepod.target";
    systemd.settings.Manager.ShowStatus = "no";
    # Ship a valid machine-id in the read-only store. systemd reads it as
    # already-initialized and skips the first-boot path that would otherwise
    # create /etc/machine-id on podman's fuse-overlayfs rootfs and fsync() it
    # synchronously before boot can continue.
    environment.etc."machine-id".text = "4ecb2502507f468986747b937d700a13\n";
    environment.sessionVariables.CLAUDE_CODE_DISABLE_BG_SHELL_PRESSURE_REAP = "1";
    networking.hostName = "claudepod";

    systemd.services.console-getty.enable = false;

    users.groups.users.gid = 100;

    security.sudo.wheelNeedsPassword = false;
    security.pam.services.claudepod = {
      startSession = true;
      setLoginUid = false;
      rootOK = true;
      unixAuth = false;
      pamMount = false;
    };

    virtualisation.podman.enable = true;

    systemd.services.claudepod-runtime-user = {
      description = "Create claudepod runtime user";
      before = ["claudepod-shell.service"];
      unitConfig.FailureAction = "poweroff";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${claudepodRuntimeUser}";
      };
    };

    systemd.services.claudepod-shell = {
      description = "Claudepod interactive shell";
      requires = ["claudepod-runtime-user.service"];
      after = ["basic.target" "systemd-logind.service" "claudepod-runtime-user.service"];
      wantedBy = ["claudepod.target"];
      unitConfig = {
        SuccessAction = "poweroff";
        FailureAction = "poweroff";
      };
      serviceConfig = {
        Type = "simple";
        User = "1000";
        Group = "100";
        Environment = "container=podman";
        PAMName = "claudepod";
        StandardInput = "tty";
        StandardOutput = "tty";
        TTYPath = "/dev/console";
        TTYReset = true;
        TTYVHangup = true;
        ExecStart = "${claudepodShell}";
      };
    };

    systemd.targets.claudepod = {
      description = "Claudepod session";
      requires = ["basic.target" "systemd-logind.service"];
      after = ["basic.target" "systemd-logind.service"];
      unitConfig.AllowIsolate = true;
    };

    environment.systemPackages =
      (with pkgs; [
        bashInteractive
        bubblewrap
        bun
        coreutils
        curl
        direnv
        fd
        file
        findutils
        gawk
        git
        gnugrep
        gnused
        jq
        jujutsu
        less
        nodejs
        ripgrep
        tmux
        tree
        unzip
        util-linux
        vim
        wget
      ])
      ++ [
        (lib.hiPrio nixLocateFull)
        (pkgs.python3.withPackages (_ps: []))
      ]
      ++ lib.optional (cfg.launcherPackage != null) cfg.launcherPackage
      ++ cfg.extraGuestPackages pkgs;

    programs = {
      # codex runs each command with `bash -lc`, so /etc/profile is its
      # per-command hook point. Claude Code's Bash tool shells are non-login
      # and are covered by the CLAUDE_ENV_FILE hook instead.
      bash.loginShellInit = "source /etc/agent-devshell.sh";

      nix-ld.enable = true;
      nix-index.package = nixIndexPackages.nix-index-with-small-db;

      nix-index-database.comma.enable = true;

      direnv = {
        enable = true;
        nix-direnv.enable = true;
      };
    };

    services.logrotate.enable = false;
    documentation.enable = false;
    services.journald.storage = "volatile";

    networking = {
      useDHCP = false;
      firewall.enable = false;
      useHostResolvConf = false;
      resolvconf.enable = false;
      # Drops "::1 localhost" from /etc/hosts: pasta splices --host-port on
      # ::1 too, but host services usually listen only on 127.0.0.1, so
      # clients that resolve localhost to ::1 first hang up instead of
      # falling back to IPv4.
      enableIPv6 = false;
    };

    environment.etc."agent-devshell.sh".source = agentDevshell;

    # Claude Code sources $CLAUDE_ENV_FILE before each Bash tool command;
    # pointing it at the loader gives per-command direnv. Managed settings so
    # it holds regardless of what lives in the mutable guest home.
    environment.etc."claude-code/managed-settings.json".text = builtins.toJSON {
      hooks.SessionStart = [
        {
          hooks = [
            {
              type = "command";
              command = "echo 'source /etc/agent-devshell.sh' >> \"$CLAUDE_ENV_FILE\"";
            }
          ];
        }
      ];
    };

    environment.etc."resolv.conf".text = ''
      nameserver 8.8.8.8
      nameserver 8.8.4.4
    '';

    nix = {
      registry.nixpkgs.flake = nixpkgs;
      settings.experimental-features = ["nix-command" "flakes" "local-overlay-store"];
    };

    # Only root (the guest nix-daemon) may reach the proxy socket; podman
    # creates the mountpoint parent 0755, which would let any guest uid
    # connect. The socket itself must stay 0666 for host-side uid-mapping
    # reasons (see spawn_nix_proxy in claudepod-start.rs).
    systemd.tmpfiles.rules = [
      "z /nix/.host-nix-daemon 0700 root root - -"
      "d /run/user/1000 0700 1000 100 - -"
    ];

    # local-overlay store parameters:
    # - real=/nix/store: merged overlay mounted by claudepod-entry
    # - upper-layer=/nix/.rw-store/store: tmpfs-backed writable layer
    # - lower-store=...: host-side nix proxy socket bind-mounted by claudepod-start
    # - check-mount=false: nix's overlay check does not match kernel overlayfs /proc/mounts
    systemd.services.nix-daemon.environment.NIX_REMOTE = "local-overlay://?lower-store=unix%%3A%%2F%%2F%%2Fnix%%2F.host-nix-daemon%%2Fsocket&upper-layer=/nix/.rw-store/store&real=/nix/store&check-mount=false";
  };
}
