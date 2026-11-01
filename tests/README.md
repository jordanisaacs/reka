# Private native integration tests

`integration_keyboard.py` and `integration_floating.py` start a new River
headless backend and a clean pgtk Emacs with the compiled native module. They
remove inherited compositor sockets, display/authentication variables and the
session bus. Synthetic devices connect only to the new private Wayland socket;
they never use uinput, the running desktop, or a physical input backend.

Build the GTK/virtual-pointer fixtures and pinned patched River tools:

```console
nix build --impure --expr 'let f = builtins.getFlake (toString ./.); pkgs = f.inputs.nixpkgs.legacyPackages.x86_64-linux; in import ./tests/integration-tools.nix { inherit pkgs; }' --out-link /tmp/reka-integration-tools
nix build .#native-module --out-link /tmp/reka-native-module
env PATH=/tmp/reka-integration-tools/bin:$PATH python3 -B tests/integration_floating.py --module /tmp/reka-native-module/lib/libreka.so --output-dir /tmp/reka-floating-result
```

Choose a fresh output directory per run. Logs, final state, and real compositor
screenshots are kept there for failure diagnosis. Without `--output-dir`, a
temporary directory is removed after the run. `--module` also accepts a locally
built `target/debug/libreka.so`. The existing keyboard integration can use the
same tools/module arguments, with `wev` additionally available on PATH.
`--lisp-dir` can select the matching packaged or development Lisp directory
when testing a module built from another checkout.

The floating integration uses two real GTK3 applications, one with a transient
dialog and native min/max size hints. Its small fixture clients request normal
xdg-toplevel operations; they cannot change Reka's internal state. Screenshot
pixel regions verify that the actual surfaces are visible/hidden, alongside
the native geometry delivered to Lisp and the client's configured dimensions.
It first checks that the native ready hook follows asynchronous frame startup,
rather than running during `reka-enable`, and fires only once across later
window and tab changes.

It checks automatic dialog floating without parent retiling or a reserved empty
tile; native Super+f tile/float/tile transitions, retained keyboard focus and
saved geometry; Super+left move and Super+right resize; GTK client-requested move
and northwest resize, including both size bounds and the fixed opposite corner;
virtual keyboard delivery to a detached float; return to ordinary Emacs editing
and a real minibuffer without leaking keys to clients; initial title/app-id
delivery across asynchronous buffer creation; tab visibility; and destruction
of a parent's transient while another app survives.

The virtual-pointer helper additionally refuses runtime paths outside the
`reka-floating-` fixture namespace. Its primary-seat binding and GTK gesture
requests exercise the compositor protocol, rather than replacing native calls
with Lisp or Rust stubs. A passing unit suite alone does not establish that
these interaction paths work.
