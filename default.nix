{
  pkgs ? import <nixpkgs> { },
  depot ? { },
  ...
}:

let
  emacsBuilder = depot.tools.emacs-pkgs.buildEmacsPackage or pkgs.emacs-pgtk.pkgs.trivialBuild;

  module = pkgs.rustPlatform.buildRustPackage {
    pname = "libreka";
    version = "0.1.0";
    src = pkgs.lib.fileset.toSource {
      root = ./.;
      fileset = pkgs.lib.fileset.unions [
        ./Cargo.toml
        ./Cargo.lock
        ./src
        ./protocol
        ./LICENSE
      ];
    };
    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [
      pkgs.emacs-pgtk
      pkgs.libxkbcommon
    ];
    cargoLock.lockFile = ./Cargo.lock;

    postInstall = ''
      mkdir -p $out/share/emacs/site-lisp
      ln -s $out/lib/libreka.so $out/share/emacs/site-lisp/libreka.so
    '';
  };
in
emacsBuilder {
  pname = "reka";
  version = "0.1.0";
  src = ./lisp;
  packageRequires = [ module ];
  doInstallCheck = true;
  installCheckPhase = ''
    runHook preInstallCheck
    ${pkgs.python3}/bin/python ${./tests/run_ert.py} \
      ${pkgs.emacs-pgtk}/bin/emacs "$out" "${module}" "${./tests}"
    runHook postInstallCheck
  '';
  passthru.nativeModule = module;
  meta = {
    description = "Emacs-based window manager for River";
    homepage = "https://github.com/jordanisaacs/reka";
    license = pkgs.lib.licenses.gpl3Plus;
    platforms = pkgs.lib.platforms.linux;
  };
}
