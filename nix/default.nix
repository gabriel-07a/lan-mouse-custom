{
  stdenv,
  rustPlatform,
  lib,
  pkg-config,
  libX11,
  libXtst,
  git,
}:
let
  cargoToml = fromTOML (builtins.readFile ../Cargo.toml);
  pname = cargoToml.package.name;
  version = cargoToml.package.version;
in
rustPlatform.buildRustPackage {
  inherit pname;
  inherit version;

  nativeBuildInputs = [
    pkg-config
    git
  ];

  buildInputs = lib.optionals stdenv.hostPlatform.isLinux [
    libX11
    libXtst
  ];

  src = builtins.path {
    name = pname;
    path = lib.cleanSource ../.;
  };

  cargoLock.lockFile = ../Cargo.lock;

  # Set Environment Variables
  RUST_BACKTRACE = "full";

  postInstall = ''
    install -Dm444 *.desktop -t $out/share/applications
    install -Dm444 lan-mouse-gtk/resources/*.svg -t $out/share/icons/hicolor/scalable/apps
  '';

  meta = with lib; {
    description = "Lan Mouse is a mouse and keyboard sharing software";
    longDescription = ''
      Lan Mouse is a mouse and keyboard sharing software similar to universal-control on Apple devices. It allows for using multiple pcs with a single set of mouse and keyboard. This is also known as a Software KVM switch.
      The supported platforms are Linux (Wayland and X11) and Windows; macOS is no longer built, tested or supported by this fork.
    '';
    mainProgram = pname;
    platforms = platforms.all;
  };
}
