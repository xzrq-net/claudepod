`claudepod` is a Nix Home Manager module that builds and runs a tiny container.
This project is vibe-coded personal infrastructure. Approach it as a showcase of
container primitives, not an off-the-shelf product.

The container is meant to gently prevent a coding agent session from misusing
ambient credentials like dotfiles. Isolation model:

- dedicated container home (including `.claude` and `.codex`) and host `~/src`
  are mounted read-write and shared between instances
- current directory mounted read-write under `/projects/<name>`
- unrestricted Internet and local network access
- passwordless sudo into container root
- leaks host details like mount names and hardware info
- GPT 5.5 couldn't figure out a way to escape isolation
- host Nix store is fully readable (you don't keep secrets in there, do you?)

The container runs from an empty rootfs overlay: podman just manages the
container's scratch layer. At runtime, `claudepod-start` passes
`claudepod-init` and the NixOS toplevel from the host store; the init sets up
the store overlay and hands off to NixOS/systemd. The guest mounts the host
Nix store read-only and uses the Nix daemon's local-overlay feature,
consulting the host daemon for metadata through a read-only proxy.

## Quick Start

Run Claude Code in the current project:

```sh
nix run github:xzrq-net/claudepod
```

Run Codex in the current project:

```sh
nix run github:xzrq-net/claudepod#gptpod
```

## Commands

```text
claudepod [-s] [-V] [-v path] [-v host:guest]... [-- command [arg]...]
gptpod    [-s] [-V] [-v path] [-v host:guest]... [-- command [arg]...]
```

Options:

- `-s`: start a login shell instead of the default agent mode.
- `-V`: verbose mode, shows systemd boot messages in the guest.
- `-v path`: mount the same host path at the same guest path.
- `-v host:guest`: mount a host path at a specific guest path.
- `command [arg]...`: run this command in the project directory instead of the
  default agent or shell.

Environment variables:

- `CLAUDE_CODE_*`: forwarded into the guest and through systemd to the agent
  process.
- `MAX_THINKING_TOKENS`: forwarded the same way when set.

Default paths:

- State directory: `${XDG_DATA_HOME:-$HOME/.local/share}/claudepod`
- Source root: `$HOME/src`

## Home Manager Usage

The flake exposes `homeModules.default`. Enabling it adds the commands to
`home.packages`.

Example:

```nix
{
  inputs.claudepod.url = "github:xzrq-net/claudepod";

  outputs = { claudepod, home-manager, ... }: {
    homeConfigurations.me = home-manager.lib.homeManagerConfiguration {
      modules = [
        claudepod.homeModules.default
        {
          programs.claudepod = {
            enable = true;
            username = "me"; # username in container, defaults to Home Manager username

            # function from guest `pkgs` to extra packages installed in the guest
            extraGuestPackages = pkgs: [
              pkgs.nodePackages.pnpm
            ];
          };
        }
      ];
    };
  };
}
```

## Nix Store Overlay

The guest Nix daemon is configured with:

```text
NIX_REMOTE=local-overlay://?lower-store=unix%3A%2F%2F%2Fnix%2F.host-nix-daemon%2Fsocket&upper-layer=/nix/.rw-store/store&real=/nix/store&check-mount=false
```

Broken down:

- `local-overlay://`: use Nix's local overlay store implementation.
- `lower-store=unix:///nix/.host-nix-daemon/socket`: lower-layer metadata
  queries go to a host-side proxy (`claudepod-nix-proxy`) over a bind-mounted
  socket. The proxy forwards a small set of read-only operations to the
  host nix-daemon and rejects everything else; store *contents* still come
  from the read-only `/nix/store` mount.
- `upper-layer=/nix/.rw-store/store`: put guest-created store paths in the
  writable overlay upper layer. This is backed by tmpfs inside the container.
- `real=/nix/store`: expose the combined lower and upper store at the normal
  `/nix/store` path used by guest processes.
- `check-mount=false`: skip Nix's overlay-store mount validation because it does
  not match the kernel overlayfs format reported in `/proc/mounts`.
