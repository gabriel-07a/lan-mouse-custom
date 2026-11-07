{
  lib,
  rustPlatform,
  pkg-config,
  libxkbcommon,
  wayland,
  libX11,
  libxcb,
  fontconfig,
  freetype,
  dbus,
  libGL,
  vulkan-loader,
  git,
}:
let
  cargoToml = fromTOML (builtins.readFile ../lan-mouse-gpui/Cargo.toml);
  pname = cargoToml.package.name;
  version = cargoToml.package.version;
in
rustPlatform.buildRustPackage {
  inherit pname version;

  # The frontend is a standalone package (own Cargo.lock) so it can keep its
  # own zbus/ksni feature resolution and tray implementation.
  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../lan-mouse-gpui/Cargo.lock;
  buildAndTestSubdir = "lan-mouse-gpui";
  doCheck = false;

  nativeBuildInputs = [
    pkg-config
    git
  ];

  buildInputs = [
    libxkbcommon
    wayland
    libX11
    libxcb
    fontconfig
    freetype
    dbus
    libGL
    vulkan-loader
  ];

  meta = with lib; {
    description = "GPUI frontend for Lan Mouse";
    mainProgram = pname;
    platforms = platforms.linux;
    license = licenses.gpl3Plus;
  };
}
