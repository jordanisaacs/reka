# Keyboard simulation

Reka can send application keys through River 0.5's
`river-virtual-keyboard-v1` extension. This supplements the standard
`zwp_virtual_keyboard_v1` protocol with a synchronized manage sequence: focus
and injected events are committed together. No external input-injection tool
or access to `/dev/uinput` is required.

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

On River 0.4, Reka continues to manage windows normally, but simulation reports
that it is unavailable. `reka-virtual-keyboard-available-p` checks availability
for the active, unlocked seat.

The protocol XML files were copied from River 0.5.0, retaining their MIT
licenses. The standard virtual-keyboard XML's SPDX comment was moved inside
the protocol element for compatibility with `wayland-scanner`'s parser.

Tests:

```console
cargo test
emacs -Q --batch -L lisp -l tests/simulation.el
```
