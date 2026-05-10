{
  description = "Rootless Podman NixOS container for agent CLIs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    nix-index-database = {
      url = "github:nix-community/nix-index-database";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {
    self,
    nixpkgs,
    nix-index-database,
    ...
  }: let
    guestModule = import ./modules/guest.nix {inherit nix-index-database;};

    claudepodLib = rec {
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
        stateDir ? "/tmp/claudepod",
        srcRoot ? "/tmp",
        guestSystem ? pkgs.stdenv.hostPlatform.system,
        extraGuestPackages ? (_: []),
        guestModules ? [],
      }: let
        guest = mkGuest {
          system = guestSystem;
          inherit username extraGuestPackages;
          modules = guestModules;
        };
      in
        import ./package.nix {
          inherit pkgs username stateDir srcRoot;
          toplevel = guest.config.system.build.toplevel;
        };
    };
  in
    {
      lib = claudepodLib;
      nixosModules.default = guestModule;
      homeModules.default = import ./modules/home.nix {inherit self;};
    }
    // {
      packages.x86_64-linux = let
        system = "x86_64-linux";
        pkgs = import nixpkgs {inherit system;};
        claudepod = claudepodLib.mkPackage {inherit pkgs;};
      in {
        default = claudepod;
        claudepod = claudepod;
      };
    };
}
