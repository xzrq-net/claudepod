{
  description = "Rootless Podman NixOS container for agent CLIs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    nix-index-database = {
      url = "github:nix-community/nix-index-database";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {
    self,
    nixpkgs,
    rust-overlay,
    crane,
    nix-index-database,
    ...
  }: let
    pkgs = import nixpkgs {
      system = "x86_64-linux";
      overlays = [rust-overlay.overlays.default];
    };
    mkRustToolchain = pkgs:
      pkgs.rust-bin.stable.latest.default.override {
        extensions = ["clippy" "rust-src" "rustfmt"];
      };
    mkCraneLib = pkgs:
      (crane.mkLib pkgs).overrideToolchain (mkRustToolchain pkgs);

    guestModule = import ./guest-module.nix {inherit nix-index-database;};

    claudepodLib = rec {
      mkImage = {
        pkgs,
        toplevel,
      }: let
        entryScript = import ./entrypoint.nix {inherit pkgs toplevel;};
      in
        pkgs.dockerTools.streamLayeredImage {
          name = "claudepod";
          tag = "latest";
          contents = [];
          includeStorePaths = false;
          config.Entrypoint = ["${entryScript}"];
        };

      mkGuest = {
        system ? "x86_64-linux",
        username ? "user",
        extraGuestPackages ? (_: []),
        modules ? [],
        specialArgs ? {},
      }:
        nixpkgs.lib.nixosSystem {
          inherit system specialArgs;
          modules =
            [
              guestModule
              {
                claudepod = {
                  inherit username extraGuestPackages;
                };
              }
            ]
            ++ modules;
        };

      mkPackage = {
        pkgs,
        username ? "user",
        guestSystem ? pkgs.stdenv.hostPlatform.system,
        extraGuestPackages ? (_: []),
        guestModules ? [],
      }: let
        craneLib = mkCraneLib pkgs;
        guest = mkGuest {
          system = guestSystem;
          inherit username extraGuestPackages;
          modules = guestModules;
        };
        image = mkImage {
          inherit pkgs;
          toplevel = guest.config.system.build.toplevel;
        };
      in
        import ./package.nix {
          inherit pkgs craneLib username image;
        };

      mkRustPackage = {pkgs}: let
        craneLib = mkCraneLib pkgs;
      in
        craneLib.buildPackage {
          src = craneLib.cleanCargoSource ./.;
        };
    };
  in
    {
      lib = claudepodLib;
      nixosModules.default = guestModule;
      homeModules.default = import ./home-manager-module.nix {inherit self;};
    }
    // {
      packages.x86_64-linux = let
        claudepod = claudepodLib.mkPackage {inherit pkgs;};
        claudepodRust = claudepodLib.mkRustPackage {inherit pkgs;};
      in {
        default = claudepod;
        claudepod = claudepod;
        claudepod-rust = claudepodRust;
      };

      apps.x86_64-linux = let
        claudepodPackage = self.packages.x86_64-linux.claudepod;
      in rec {
        default = claudepod;
        claudepod = {
          type = "app";
          program = "${claudepodPackage}/bin/claudepod";
        };
        gptpod = {
          type = "app";
          program = "${claudepodPackage}/bin/gptpod";
        };
      };

      checks.x86_64-linux = {
        claudepod-rust = self.packages.x86_64-linux.claudepod-rust;
      };

      devShells.x86_64-linux.default = let
        rustToolchain = mkRustToolchain pkgs;
        devUsername = "user";
        devGuest = claudepodLib.mkGuest {
          system = pkgs.stdenv.hostPlatform.system;
          username = devUsername;
        };
        devImage = claudepodLib.mkImage {
          inherit pkgs;
          toplevel = devGuest.config.system.build.toplevel;
        };
      in
        pkgs.mkShell {
          CLAUDEPOD_USERNAME = devUsername;
          CLAUDEPOD_IMAGE = "${devImage}";
          CLAUDEPOD_PODMAN = "${pkgs.podman}/bin/podman";

          packages = [
            rustToolchain
            pkgs.rust-bin.stable.latest.rust-analyzer
          ];
        };
    };
}
