{nix-index-database}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.claudepod;

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

    if [ "''${#COMMAND[@]}" -gt 0 ]; then
      exec ${pkgs.bashInteractive}/bin/bash --login -c 'cd "$1" && { eval "$(${pkgs.direnv}/bin/direnv export bash)" || true; } && shift && exec "$@"' claudepod "$PROJECT" "''${COMMAND[@]}"
    fi

    case "$MODE" in
      shell)
        exec ${pkgs.bashInteractive}/bin/bash --login
        ;;
      claude)
        exec ${pkgs.bashInteractive}/bin/bash --login -c 'cd "$1" && { eval "$(${pkgs.direnv}/bin/direnv export bash)" || true; } && exec claude --dangerously-skip-permissions' claudepod "$PROJECT"
        ;;
      codex)
        exec ${pkgs.bashInteractive}/bin/bash --login -c 'cd "$1" && { eval "$(${pkgs.direnv}/bin/direnv export bash)" || true; } && exec ${pkgs.nodejs}/bin/npx -y @openai/codex --sandbox danger-full-access --ask-for-approval never' claudepod "$PROJECT"
        ;;
      *)
        echo "Unknown claudepod mode: $MODE" >&2
        exit 1
        ;;
    esac
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
  };

  config = {
    system.stateVersion = lib.trivial.release;

    boot.isNspawnContainer = true;
    # Boot only the session target claudepod needs, while keeping basic/logind
    # for sockets, tmpfiles/wrappers, nix-daemon, and the pam_systemd session.
    systemd.defaultUnit = "claudepod.target";
    systemd.settings.Manager.ShowStatus = "no";
    # The rootfs is ephemeral; committing transient machine-id writes overlay
    # data for no persistent benefit and can delay shutdown.
    systemd.suppressedSystemUnits = [
      "systemd-machine-id-commit.service"
    ];
    # Ship a valid machine-id in the read-only store. systemd reads it as
    # already-initialized and skips the first-boot path that would otherwise
    # create /etc/machine-id on podman's fuse-overlayfs rootfs and fsync() it
    # synchronously before boot can continue.
    environment.etc."machine-id".text = "4ecb2502507f468986747b937d700a13\n";
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
        coreutils
        curl
        direnv
        fd
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
        (pkgs.python3.withPackages (_ps: []))
      ]
      ++ cfg.extraGuestPackages pkgs;

    programs = {
      nix-ld.enable = true;

      nix-index-database.comma.enable = true;

      direnv = {
        enable = true;
        nix-direnv.enable = true;
      };

      bash.interactiveShellInit = ''
        cd "$(${pkgs.coreutils}/bin/cat /run/claudepod-project)"
      '';
    };

    services.logrotate.enable = false;
    documentation.enable = false;
    services.journald.storage = "volatile";

    networking = {
      useDHCP = false;
      firewall.enable = false;
      useHostResolvConf = false;
      resolvconf.enable = false;
    };

    environment.etc."resolv.conf".text = ''
      nameserver 8.8.8.8
      nameserver 8.8.4.4
    '';

    nix.settings.experimental-features = ["nix-command" "flakes" "local-overlay-store"];

    # Only root (the guest nix-daemon) may reach the proxy socket; podman
    # creates the mountpoint parent 0755, which would let any guest uid
    # connect. The socket itself must stay 0666 for host-side uid-mapping
    # reasons (see spawn_nix_proxy in claudepod-start.rs).
    systemd.tmpfiles.rules = [
      "z /nix/.host-nix-daemon 0700 root root - -"
      "d /run/user/1000 0700 1000 100 - -"
    ];

    # check-mount=false: nix overlay store check-mount validation doesn't match kernel overlayfs /proc/mounts format
    # lower-store: host-side nix proxy socket, bind-mounted in by claudepod-start (see docs/nix-proxy.md)
    systemd.services.nix-daemon.environment.NIX_REMOTE = "local-overlay://?lower-store=unix%%3A%%2F%%2F%%2Fnix%%2F.host-nix-daemon%%2Fsocket&upper-layer=/nix/.rw-store/store&real=/nix/store&check-mount=false";
  };
}
