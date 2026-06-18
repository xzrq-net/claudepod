`claudepod` is a Nix Home Manager module that builds and runs a tiny container.
This project is vibe-coded personal infrastructure. Treat it as a showcase of
container primitives, not an off-the-shelf product.

The container is meant to gently prevent a coding agent session from misusing
ambient credentials like dotfiles. It is not a security boundary. Isolation
model:

- dedicated container home (including `.claude` and `.codex`) and, by default,
  host `~/src` are mounted read-write and shared between instances
- `--sandbox-home DIR` uses a caller-selected home backing directory and skips
  the host `~/src` mount
- current directory mounted read-write under `/projects/<name>`
- unrestricted Internet and local network access
- passwordless sudo into container root
- leaks host details like mount names and hardware info
- GPT 5.5 couldn't figure out a way to escape isolation
- host Nix store is fully readable (you don't keep secrets in there, do you?)

The overhead is ~20 MB RAM in systemd detritus and <1MB on disk for NixOS.
Everything else comes from the host Nix store. Time to start, run `true` and
shut down is around 1.2s on my machine.

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
claudepod [-s] [-V] [--sandbox-home DIR] [-v path] [-v host:guest]... [--] [command [arg]...]
gptpod    [-s] [-V] [--sandbox-home DIR] [-v path] [-v host:guest]... [--] [command [arg]...]
```

Options:

- `-s`: start a login shell instead of the default agent mode.
- `-V`: verbose mode, shows systemd boot messages in the guest.
- `--sandbox-home DIR`: use `DIR` as the guest home backing directory and do not
  mount host `~/src`. The current directory is still mounted as the project.
- `-v path`: mount the same host path at the same guest path.
- `-v host:guest`: mount a host path at a specific guest path.
- `command [arg]...`: run this command in the project directory instead of the
  default agent or shell.

Environment variables:

- `CLAUDE_CODE_*`: forwarded into the guest session process.
- `MAX_THINKING_TOKENS`: forwarded the same way when set.

Builtin paths:

- State directory: `${XDG_DATA_HOME:-$HOME/.local/share}/claudepod`
- Default guest home backing directory: state directory + `/home`
- Default source root mounted into the guest: `$HOME/src`, unless
  `--sandbox-home` is used

## Home Manager Usage

The flake exposes `homeModules.default`. Enabling it adds the `claudepod` and
`gptpod` commands to `home.packages`.

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

            # Function from guest `pkgs` to extra packages installed in the guest.
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
  socket. The proxy forwards a small set of read-only operations to the host
  nix-daemon and rejects everything else. Store _contents_ still come from the
  read-only `/nix/store` mount.
- `upper-layer=/nix/.rw-store/store`: put guest-created store paths in the
  writable overlay upper layer. This is backed by tmpfs inside the container.
- `real=/nix/store`: expose the combined lower and upper store at the normal
  `/nix/store` path used by guest processes.
- `check-mount=false`: skip Nix's overlay-store mount validation because it does
  not match the kernel overlayfs format reported in `/proc/mounts`.

## Bonus: fun facts

- Nested claudepods are supported up to half of kernel userns nesting limit. To
  avoid kernel overlayfs nesting limits, each level's Nix store is a standalone
  overlay listing all parents as lowerdir layers.
- systemd is actively hostile to fast startup and requires workarounds
  - SYSTEMD_DEFAULT_MOUNT_RATE_LIMIT_BURST=1000. Otherwise, systemd mounts a lot
    of file systems, notices the mounts came in faster than 5/s and stalls boot.
  - Static /etc/machine-id to avoid a ~1 s stall on fsync after writing a new
    ID.
