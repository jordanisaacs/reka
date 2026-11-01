{
  description = "Reka: an Emacs window manager for River";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/1c89083e451e891152365b799595cf7790a43325";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      packagesFor = system: nixpkgs.legacyPackages.${system};
      rekaFor = system: import ./. { pkgs = packagesFor system; };
      emacsFor =
        system:
        let
          pkgs = packagesFor system;
        in
        (pkgs.emacsPackagesFor pkgs.emacs-pgtk).emacsWithPackages (_: [ (rekaFor system) ]);
      nativeCheck =
        system: kind:
        let
          pkgs = packagesFor system;
          tools = import ./tests/integration-tools.nix { inherit pkgs; };
        in
        pkgs.runCommand "reka-native-${kind}"
          {
            nativeBuildInputs = [ tools ];
            FONTCONFIG_FILE = pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; };
          }
          ''
            export HOME="$TMPDIR/home"
            mkdir -p "$HOME"
            python -B ${./.}/tests/integration_${kind}.py \
              --module ${(rekaFor system).nativeModule}/lib/libreka.so \
              --output-dir "$out"
          '';
    in
    {
      overlays.default = final: _: {
        reka = import ./. { pkgs = final; };
        reka-emacs = (final.emacsPackagesFor final.emacs-pgtk).emacsWithPackages (_: [ final.reka ]);
        river-reka = import ./nix/patched-river.nix { inherit (final) river; };
      };
      packages = eachSystem (system: {
        default = rekaFor system;
        reka = rekaFor system;
        native-module = (rekaFor system).nativeModule;
        emacs = emacsFor system;
        river = import ./nix/patched-river.nix { inherit (packagesFor system) river; };
      });
      apps = eachSystem (system: {
        default = {
          type = "app";
          program = "${emacsFor system}/bin/emacs";
          meta.description = "Emacs with the Reka native module and Lisp installed";
        };
      });
      checks = eachSystem (system: {
        reka = rekaFor system;
        native-keyboard = nativeCheck system "keyboard";
        native-floating = nativeCheck system "floating";
      });
      formatter = eachSystem (system: (packagesFor system).nixfmt-tree);
      devShells = eachSystem (
        system:
        let
          pkgs = packagesFor system;
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              pkg-config
              emacs-pgtk
              python3
              ruff
              nixfmt
            ];
            buildInputs = [ pkgs.libxkbcommon ];
          };
        }
      );
    };
}
