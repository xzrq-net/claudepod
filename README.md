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

The container itself is a few hundred kilobytes. It mounts the host Nix store
read-only and uses the Nix daemon's local-overlay feature to consult the host
database. See caveat below.

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
claudepod [-s] [-v path] [-v host:guest]...
gptpod    [-s] [-v path] [-v host:guest]...
```

Options:

- `-s`: start a login shell instead of the default agent mode.
- `-v path`: mount the same host path at the same guest path.
- `-v host:guest`: mount a host path at a specific guest path.

Environment variables:

- `CLAUDEPOD_STATE_DIR`: override the persistent state directory. The guest home
  is mounted from `$CLAUDEPOD_STATE_DIR/home`.
- `CLAUDEPOD_SRC_ROOT`: override the host source root mounted at guest `~/src`.
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

            # Base path "/home/me" defaults to Home Manager home path
            stateDir = "/home/me/.local/share/claudepod"; # directory to mount as container home
            srcRoot = "/home/me/src"; # directory to mount as container ~/src

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
NIX_REMOTE=local-overlay://?lower-store=local%3A%2F%2F%3Froot%3D%2Fnix%2F.host-nix%26read-only%3Dtrue&upper-layer=/nix/.rw-store/store&real=/nix/store&check-mount=false
```

Broken down:

- `local-overlay://`: use Nix's local overlay store implementation.
- `lower-store=local://?root=/nix/.host-nix&read-only=true`: The container
  mounts the host Nix store and database read-only. Use them as the lower layer.
  - `read-only=true`: do not try to write to the host store or database.
- `upper-layer=/nix/.rw-store/store`: put guest-created store paths in the
  writable overlay upper layer. This is backed by tmpfs inside the container.
- `real=/nix/store`: expose the combined lower and upper store at the normal
  `/nix/store` path used by guest processes.
- `check-mount=false`: skip Nix's overlay-store mount validation because it does
  not match the kernel overlayfs format reported in `/proc/mounts`.

### Caveat

`read-only` means the guest Nix daemon only reads the main database file, not
the SQLite WAL log. Recent host database changes may fail to show up with
obscure `fchmodat` errors. To sync things up, run this command on the host:

```shell
sudo sqlite3 /nix/var/nix/db/db.sqlite 'PRAGMA wal_checkpoint(FULL);'
```
