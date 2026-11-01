# Keyboard simulation

Reka can send application keys through River 0.5's
`river-virtual-keyboard-v1` extension. This supplements the standard
`zwp_virtual_keyboard_v1` protocol with a synchronized manage sequence: focus
and injected events are committed together. No external input-injection tool
or access to `/dev/uinput` is required.

River 0.5.0 needs the fixes in `patches/river-synced-keyboard-focus.patch` before
using this feature: its stock implementation crashes when focus changes after
an injection and mishandles synchronized-keyboard cleanup during idle destruction or
window-manager disconnect.
`nix/patched-river.nix` applies both fixes to an existing River derivation.
They preserve physical keyboard binding filtering, correctly forward a
synchronized keyboard's own key and modifier state, and detach callbacks before
freeing keyboard resources. The fixes were checked
against upstream main `65a908430dddc6728a5f71c89974b786e48302fb`.

Load the optional Lisp library after starting Reka:

```elisp
(require 'reka-simulation)
(reka-define-simulation-key "C-f" "<right>")
(reka-define-simulation-key "C-b" "<left>")
(reka-define-simulation-key "C-y" "C-v")
```

`reka-send-keys` accepts a `kbd` string or keyboard-event vector, or reads a
sequence interactively. It targets the application represented by the selected
Reka buffer. These definitions change Reka application buffers; ordinary Emacs
text buffers retain their own bindings. Loading the library alone does not
register any interceptions.

Each call supports 1–64 keys. Every key is pressed and released and its
simulated modifiers are reset. Keysyms describe the intended character,
independently of the physical keyboard layout, including Unicode. Shifted
letters are uppercased. For shifted punctuation, send the character (for
example `!`); explicit `S-1` instead means the literal keysym `1` with Shift.

Only one destination's batch is sent in each manage sequence. A hidden or
closed destination is rejected, and a later focus change cannot redirect the
keys to an unrelated application. Seat removal and session locking discard
pending simulations. Requests do not modify physical-device settings or grant
other clients access to the injection protocol. Simulated text is omitted
from debug messages.

On otherwise compatible River versions without the optional simulation
protocol, Reka continues to manage windows normally, but simulation reports
that it is unavailable. `reka-virtual-keyboard-available-p` checks availability
for the active, unlocked seat.

`reka-reset-virtual-keyboard` discards queued simulations and recreates the
active seat's injection keyboard. It does not restart the window manager.

The protocol XML files were copied from River 0.5.0, retaining their MIT
licenses. The standard virtual-keyboard XML's SPDX comment was moved inside
the protocol element for compatibility with `wayland-scanner`'s parser.

Tests:

```console
cargo test
emacs -Q --batch -L lisp -l tests/simulation.el
python tests/integration_keyboard.py
```

The integration test needs the patched River, Emacs PGTK, `wev`, `wtype`, and
coreutils on `PATH`. It starts a separate headless compositor in a private
runtime directory, supplies only synthetic input, and tests real event
delivery, balanced releases, separate destinations, focus changes and repeated
keyboard destruction/recreation without descriptor leaks. It never connects
to the existing desktop or physical input devices.
