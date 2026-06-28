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
    nixpkgsConfig = {
      allowUnfree = true;
      allowAliases = false;
    };

    pkgs = import nixpkgs {
      system = "x86_64-linux";
      config = nixpkgsConfig;
    };

    guestModule = import ./guest-module.nix {inherit nix-index-database nixpkgs nixpkgsConfig;};

    mkRust = pkgs: let
      craneLib = crane.mkLib pkgs;
    in
      craneLib.buildPackage {src = craneLib.cleanCargoSource ./.;};

    mkLauncher = {
      pkgs,
      rust,
      guestIdentity ? null,
    }: let
      hostNix = import nixpkgs {
        system = pkgs.stdenv.hostPlatform.system;
        config = nixpkgsConfig;
      };
      toplevel = if guestIdentity == null then null else guestIdentity.toplevel;
      fuseOverlayfs = "${pkgs.fuse-overlayfs}/bin/fuse-overlayfs";
      # Host launchers set guest identity for host-side cache/policy decisions:
      # toplevel boots one NixOS closure; guestSystem evaluates the pinned
      # nixpkgs package universe for manifest/cache generation.
      # Nested launchers leave these unset and read /run/claudepod-toplevel from
      # the parent container instead.
      guestIdentityArgs = pkgs.lib.optionalString (guestIdentity != null) (pkgs.lib.escapeShellArgs [
        "--set"
        "CLAUDEPOD_TOPLEVEL"
        "${guestIdentity.toplevel}"
        "--set"
        "CLAUDEPOD_GUEST_SYSTEM"
        guestIdentity.guestSystem
        "--set"
        "CLAUDEPOD_NIXPKGS"
        "${nixpkgs}"
        "--set"
        "CLAUDEPOD_NIX"
        "${hostNix.nix}/bin/nix"
      ]);
    in
      pkgs.runCommand "claudepod" {
        nativeBuildInputs = [pkgs.makeWrapper];
        passthru = {inherit rust toplevel fuseOverlayfs;};
      } ''
        mkdir -p $out/bin
        makeWrapper ${rust}/bin/claudepod-start $out/bin/claudepod \
          --inherit-argv0 \
          ${guestIdentityArgs} \
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
      mkLauncher {
        inherit pkgs rust;
        guestIdentity = {inherit toplevel guestSystem;};
      };

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
