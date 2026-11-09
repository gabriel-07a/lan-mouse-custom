# Nix Flake Usage

These instructions target the unofficial fork
(`github:gabriel-07a/lan-mouse-custom`). The package installs both binaries
(`lan-mouse` and `lan-mouse-gpui`); the service looks for the frontend next to
itself.

## Run

```bash
nix run github:gabriel-07a/lan-mouse-custom

# With params
nix run github:gabriel-07a/lan-mouse-custom -- --help

```

## Home-manager module

Add input:

```nix
inputs = {
    lan-mouse.url = "github:gabriel-07a/lan-mouse-custom";
}
```

Optional: add [our binary cache](https://app.cachix.org/cache/lan-mouse) to allow a faster package install.

```nix
nixConfig = {
    extra-substituters = [
        "https://lan-mouse.cachix.org/"
    ];
    extra-trusted-public-keys = [
      "lan-mouse.cachix.org-1:KlE2AEZUgkzNKM7BIzMQo8w9yJYqUpor1CAUNRY6OyM="
    ];
};
```

Enable lan-mouse:

``` nix
{
  inputs,
  ...
}: {
  # Add the Home Manager module
  imports = [inputs.lan-mouse.homeManagerModules.default];

  programs.lan-mouse = {
    enable = true;
    # systemd = false;
    # package = inputs.lan-mouse.packages.${pkgs.stdenv.hostPlatform.system}.default
    # Optional configuration in nix syntax, see config.toml for available options
    # settings = { };
    };
  };
}

```
