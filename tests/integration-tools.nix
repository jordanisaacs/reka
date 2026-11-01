{ pkgs }:
let
  clients = pkgs.stdenv.mkDerivation {
    pname = "reka-integration-clients";
    version = "1";
    dontUnpack = true;
    nativeBuildInputs = [
      pkgs.pkg-config
      pkgs.wayland-scanner
    ];
    buildInputs = [
      pkgs.gtk3
      pkgs.wayland
    ];
    buildPhase = ''
      wayland-scanner client-header ${pkgs.wlr-protocols}/share/wlr-protocols/unstable/wlr-virtual-pointer-unstable-v1.xml virtual-pointer-client.h
      wayland-scanner private-code ${pkgs.wlr-protocols}/share/wlr-protocols/unstable/wlr-virtual-pointer-unstable-v1.xml virtual-pointer-code.c
      $CC -Wall -Wextra -Wno-unused-parameter ${./gtk_floating_fixture.c} $(pkg-config --cflags --libs gtk+-3.0) -o reka-gtk-fixture
      $CC -Wall -Wextra -Wno-unused-parameter -I. ${./virtual_pointer_fixture.c} virtual-pointer-code.c $(pkg-config --cflags --libs wayland-client) -o reka-pointer-fixture
    '';
    installPhase = ''
      mkdir -p "$out/bin"
      cp reka-gtk-fixture reka-pointer-fixture "$out/bin/"
    '';
  };
in
pkgs.symlinkJoin {
  name = "reka-integration-tools";
  paths = [
    clients
    (import ../nix/patched-river.nix { inherit (pkgs) river; })
    pkgs.emacs-pgtk
    pkgs.wtype
    pkgs.grim
    pkgs.coreutils
    (pkgs.python3.withPackages (ps: [ ps.pillow ]))
  ];
}
