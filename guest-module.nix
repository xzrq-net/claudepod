{nix-index-database}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.claudepod;

  claudepodStart = pkgs.writeShellScript "claudepod-start" ''
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
in {
  imports = [
    nix-index-database.nixosModules.nix-index
  ];

  options.claudepod = {
    username = lib.mkOption {
      type = lib.types.str;
      description = "Normal user created inside the claudepod guest.";
    };

    extraGuestPackages = lib.mkOption {
      type = lib.types.functionTo (lib.types.listOf lib.types.package);
      default = _guestPkgs: [];
      description = "Function from guest pkgs to extra guest packages.";
    };
  };

  config = {
    system.stateVersion = lib.trivial.release;

    boot.isNspawnContainer = true;
    systemd.settings.Manager.ShowStatus = "no";
    networking.hostName = "claudepod";

    systemd.services.console-getty.enable = false;

    users.groups.users.gid = 100;

    users.users.${cfg.username} = {
      isNormalUser = true;
      home = "/home/${cfg.username}";
      group = "users";
      extraGroups = ["wheel"];
      initialHashedPassword = "";
      uid = 1000;
    };

    security.sudo.wheelNeedsPassword = false;

    systemd.services.claudepod-shell = {
      description = "Claudepod interactive shell";
      after = ["multi-user.target"];
      wantedBy = ["multi-user.target"];
      serviceConfig = {
        Type = "simple";
        StandardInput = "tty";
        StandardOutput = "tty";
        TTYPath = "/dev/console";
        TTYReset = true;
        TTYVHangup = true;
        User = cfg.username;
        ExecStart = "${claudepodStart}";
        ExecStopPost = "+" + "${pkgs.util-linux}/bin/kill -SIGRTMIN+14 1";
      };
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

    nix.settings.experimental-features = ["nix-command" "flakes" "local-overlay-store" "read-only-local-store"];

    # check-mount=false: nix overlay store check-mount validation doesn't match kernel overlayfs /proc/mounts format
    systemd.services.nix-daemon.environment.NIX_REMOTE = "local-overlay://?lower-store=local%%3A%%2F%%2F%%3Froot%%3D%%2Fnix%%2F.host-nix%%26read-only%%3Dtrue&upper-layer=/nix/.rw-store/store&real=/nix/store&check-mount=false";
  };
}
