{self}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.claudepod;
in {
  options.programs.claudepod = {
    enable = lib.mkEnableOption "claudepod";

    username = lib.mkOption {
      type = lib.types.str;
      default = config.home.username;
      defaultText = "config.home.username";
      description = "User name to create inside the claudepod guest.";
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.xdg.dataHome}/claudepod";
      defaultText = "\${config.xdg.dataHome}/claudepod";
      description = "Host state directory for persistent claudepod data.";
    };

    srcRoot = lib.mkOption {
      type = lib.types.str;
      default = "${config.home.homeDirectory}/src";
      defaultText = "\${config.home.homeDirectory}/src";
      description = "Host source tree mounted at ~/src inside the claudepod guest.";
    };

    guestSystem = lib.mkOption {
      type = lib.types.str;
      default = pkgs.stdenv.hostPlatform.system;
      defaultText = "pkgs.stdenv.hostPlatform.system";
      description = "System used for the claudepod NixOS guest.";
    };

    extraGuestPackages = lib.mkOption {
      type = lib.types.functionTo (lib.types.listOf lib.types.package);
      default = _guestPkgs: [];
      description = ''
        Function from the guest NixOS package set to extra packages installed in
        the claudepod guest.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [
      (self.lib.mkPackage {
        inherit pkgs;
        inherit (cfg) username stateDir srcRoot guestSystem extraGuestPackages;
      })
    ];
  };
}
