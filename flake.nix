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

    defaultUsername = "user";

    mkClaudepod = {
      pkgs,
      username,
      guestSystem ? pkgs.stdenv.hostPlatform.system,
      extraGuestPackages ? (_: []),
    }: let
      guest = nixpkgs.lib.nixosSystem {
        system = guestSystem;
        modules = [
          guestModule
          {claudepod = {inherit username extraGuestPackages;};}
        ];
      };
      craneLib = crane.mkLib pkgs;
      rust = craneLib.buildPackage {src = craneLib.cleanCargoSource ./.;};
      image = pkgs.dockerTools.streamLayeredImage {
        name = "claudepod";
        tag = "latest";
        contents = [];
        includeStorePaths = false;
        config.Entrypoint = [
          "${import ./entrypoint.nix {
            inherit pkgs;
            toplevel = guest.config.system.build.toplevel;
          }}"
        ];
      };
    in
      pkgs.runCommand "claudepod" {
        nativeBuildInputs = [pkgs.makeWrapper];
        passthru = {inherit rust image;};
      } ''
        mkdir -p $out/bin
        makeWrapper ${rust}/bin/claudepod-start $out/bin/claudepod \
          --inherit-argv0 \
          --set CLAUDEPOD_USERNAME ${pkgs.lib.escapeShellArg username} \
          --set CLAUDEPOD_IMAGE ${image} \
          --set CLAUDEPOD_PODMAN ${pkgs.podman}/bin/podman
        ln -s claudepod $out/bin/gptpod
      '';

    claudepod = mkClaudepod {
      inherit pkgs;
      username = defaultUsername;
    };
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
      CLAUDEPOD_USERNAME = defaultUsername;
      CLAUDEPOD_IMAGE = "${claudepod.image}";
      CLAUDEPOD_PODMAN = "${pkgs.podman}/bin/podman";

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
