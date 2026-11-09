# Legacy non-flake entry point (nix-build / nix-shell).
#
# The default build ships both binaries: the service/CLI (nix/default.nix) and
# the GPUI frontend (nix/gpui.nix). The service looks for lan-mouse-gpui next to
# itself, so they must be installed together.
{ pkgs ? import <nixpkgs> { }
}:
let
  main = pkgs.callPackage ./nix/default.nix { };
  gpui = pkgs.callPackage ./nix/gpui.nix { };
in
pkgs.symlinkJoin {
  name = "lan-mouse";
  paths = [
    main
    gpui
  ];
  meta = main.meta;
}
