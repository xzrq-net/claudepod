`claudepod` is a Nix Home Manager module that builds and runs a tiny sandbox
container for the purpose of running coding agents.

# Agent notes

## nix-proxy maintenance

Before changing guest Nix, `OUR_VERSION`, `OUR_FEATURES`, or nix-proxy allowed
ops, read `docs/nix-proxy.md` and re-audit the parsed wire payloads against Nix
upstream.
