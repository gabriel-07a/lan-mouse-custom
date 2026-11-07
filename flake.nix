{
  description = "Nix Flake for lan-mouse";
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-26.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs =
    {
      nixpkgs,
      rust-overlay,
      self,
      ...
    }:
    let
      inherit (nixpkgs) lib;
      forEachPkgs =
        f:
        lib.genAttrs
          [
            "aarch64-linux"
            "x86_64-linux"
          ]
          (
            system:
            let
              pkgs = import nixpkgs {
                inherit system;
                overlays = [
                  rust-overlay.overlays.default
                ];
              };
              # Default toolchain for devshell
              rustToolchain = pkgs.rust-bin.stable.latest.default.override {
                extensions = [
                  # includes already:
                  # rustc
                  # cargo
                  # rust-std
                  # rust-docs
                  # rustfmt-preview
                  # clippy-preview
                  "rust-analyzer"
                  "rust-src"
                ];
              };
              # Minimal toolchain for builds (rustc + cargo + rust-std only)
              rustToolchainForBuild = pkgs.rust-bin.stable.latest.minimal;
            in
            f { inherit pkgs rustToolchain rustToolchainForBuild; }
          );
    in
    {
      packages = forEachPkgs (
        { pkgs, rustToolchainForBuild, ... }:
        let
          customRustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchainForBuild;
            rustc = rustToolchainForBuild;
          };
          main = pkgs.callPackage ./nix { rustPlatform = customRustPlatform; };
          gpui = pkgs.callPackage ./nix/gpui.nix { rustPlatform = customRustPlatform; };
          lan-mouse = pkgs.symlinkJoin {
            name = "lan-mouse";
            paths = [ main gpui ];
          };
        in
        {
          default = lan-mouse;
          inherit lan-mouse;
        }
      );
      devShells = forEachPkgs (
        { pkgs, rustToolchain, ... }:
        {
          default = pkgs.mkShell {
            packages =
              with pkgs;
              [
                rustToolchain
                pkg-config
                libxkbcommon
                libxcb
                wayland
                fontconfig
                freetype
                dbus
                libGL
                vulkan-loader
              ]
              ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                libX11
                libXtst
              ];
            env.RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          };
        }
      );
      homeManagerModules.default = import ./nix/hm-module.nix self;
    };
}
