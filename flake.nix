{
  description = "Rootless Podman NixOS container for agent CLIs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    crane.url = "github:ipetkov/crane";

    nix-index-database = {
      url = "github:nix-community/nix-index-database";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    nix-index-database,
    ...
  }: let
    pkgs = import nixpkgs {
      system = "x86_64-linux";
    };

    guestModule = import ./guest-module.nix {inherit nix-index-database;};

    mkRust = pkgs: let
      craneLib = crane.mkLib pkgs;
    in
      craneLib.buildPackage {src = craneLib.cleanCargoSource ./.;};

    mkLauncher = {
      pkgs,
      rust,
      toplevel ? null,
    }: let
      fuseOverlayfs = "${pkgs.fuse-overlayfs}/bin/fuse-overlayfs";
      # The host launcher bakes in an explicit toplevel store path. The in-guest
      # launcher omits it: inside a pod claudepod-start reads the path the parent
      # booted with from /run/claudepod-toplevel instead.
      toplevelArg = pkgs.lib.optionalString (toplevel != null) "--set CLAUDEPOD_TOPLEVEL ${toplevel}";
    in
      pkgs.runCommand "claudepod" {
        nativeBuildInputs = [pkgs.makeWrapper];
        passthru = {inherit rust toplevel fuseOverlayfs;};
      } ''
        mkdir -p $out/bin
        makeWrapper ${rust}/bin/claudepod-start $out/bin/claudepod \
          --inherit-argv0 \
          ${toplevelArg} \
          --set CLAUDEPOD_PODMAN ${pkgs.podman}/bin/podman \
          --set CLAUDEPOD_FUSE_OVERLAYFS ${fuseOverlayfs}
        ln -s claudepod $out/bin/gptpod
      '';

    mkGuestLauncher = pkgs:
      mkLauncher {
        inherit pkgs;
        rust = mkRust pkgs;
      };

    mkClaudepod = {
      pkgs,
      guestSystem ? pkgs.stdenv.hostPlatform.system,
      extraGuestPackages ? (_: []),
    }: let
      rust = mkRust pkgs;
      guest = nixpkgs.lib.nixosSystem {
        system = guestSystem;
        modules = [
          guestModule
          ({
            pkgs,
            ...
          }: {
            claudepod = {
              inherit extraGuestPackages;
              launcherPackage = mkGuestLauncher pkgs;
            };
          })
        ];
      };
      toplevel = guest.config.system.build.toplevel;
    in
      mkLauncher {inherit pkgs rust toplevel;};

    claudepod = mkClaudepod {inherit pkgs;};
  in {
    nixosModules.default = guestModule;
    homeModules.default = import ./home-manager-module.nix {inherit mkClaudepod;};

    packages.x86_64-linux = {
      default = claudepod;
      inherit claudepod;
    };

    apps.x86_64-linux = {
      default = self.apps.x86_64-linux.claudepod;
      claudepod = {
        type = "app";
        program = "${claudepod}/bin/claudepod";
      };
      gptpod = {
        type = "app";
        program = "${claudepod}/bin/gptpod";
      };
    };

    checks.x86_64-linux = {
      claudepod-rust = claudepod.rust;
    };

    devShells.x86_64-linux.default = pkgs.mkShell {
      CLAUDEPOD_TOPLEVEL = "${claudepod.toplevel}";
      CLAUDEPOD_PODMAN = "${pkgs.podman}/bin/podman";
      CLAUDEPOD_FUSE_OVERLAYFS = claudepod.fuseOverlayfs;
      RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

      packages = [
        pkgs.cargo
        pkgs.clippy
        pkgs.rust-analyzer
        pkgs.rustc
        pkgs.rustfmt
      ];
    };
  };
}
