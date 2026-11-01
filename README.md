reka
====

reka lets emacs' logic just flow into [river](https://codeberg.org/river/river).
It is a window manager inside of Emacs for the Wayland world.

![screenshot of reka](https://tazj.in/blobs/reka_0228.png)

## Background

I spent many years using [EXWM](https://github.com/ch11ng/exwm/), it was great
but quite wonky. At some point Wayland became unavoidable (I really need
per-monitor fractional scaling!) and I switched to
[niri](https://github.com/YaLTeR/niri), which is one of the highest quality
pieces of software in the world.

At some point my friend [ezemtsov](https://codeberg.org/ezemtsov/) wrote
[EWM](https://codeberg.org/ezemtsov/ewm), which is a full Emacs compositor
running as a dynamic module inside of Emacs. It's a great project, but I wanted
to see if we can avoid the complexity of a full compositor implementation with
the [river WM
protocol](https://isaacfreund.com/docs/wayland/river-window-management-v1/), and
classic *brain-coding*.

What else ... the name? Oh, it's obvious to Russian speakers.

## Status

This fork supports tiled and floating application windows. Floating buffers
belong to an Emacs frame and tab without occupying a tiled Emacs window. There
may still be bugs and rough edges outside of the tested configurations.

There is **no** simulation key support, this will need a [new
protocol](https://codeberg.org/river/river/pulls/1412) on the river side.

Reka does not expose any input or output management to Emacs Lisp right now, and
I'm not sure that it will. River has external tools
([channel](https://codeberg.org/Sivecano/channel) for input management, and
[kanshi](https://gitlab.freedesktop.org/emersion/kanshi) for output management)
that are sufficient for me.

If you want something more fully featured, look in the direction of EWM instead!

## Floating windows

Press **Super+f** to toggle the focused application's floating state. Floating
windows keep their geometry when tiled again and restore it when re-floated;
their tiled geometry follows the current Emacs layout. Client dialogs with a
parent, and windows declaring a fixed size, float automatically. Dialogs center
over their parent and follow its visibility and output. Closing a parent leaves
an orphaned dialog floating and usable. Native `xdg_popup` menus remain the
compositor's responsibility.

**Super+left-drag** moves a window and **Super+right-drag** resizes from its
bottom-right corner. Client title-bar and resize-handle requests use River's
native pointer operations too. Resizing respects client minimum/maximum sizes
and the output's usable area. Moving or resizing a tiled application first
makes it floating. Fullscreen geometry is owned by River; exiting fullscreen
restores the saved normal placement.

The existing intercepted `C-x` prefix provides these commands in application
buffers, and while a detached floating application is focused:

| Key | Action |
| --- | --- |
| `C-x w f` | Toggle floating |
| `C-x w c` | Center over parent or output |
| `C-x w` then an arrow | Move by `reka-floating-step` |
| `C-x w` then Shift+arrow | Shrink/grow width or height |

The same operations are available as `M-x reka-floating-toggle`,
`reka-floating-center`, `reka-floating-move`, and `reka-floating-resize`.
Configure `reka-floating-default-size` (default `(640 . 480)`),
`reka-floating-step` (default `32` logical pixels), and
`reka-floating-toggle-key` (default `"s-f"`, or nil to disable its registration)
before calling `reka-enable`.

Floating state is readable through the application buffer's `reka-floating`,
`reka-floating-geometry` (`X Y WIDTH HEIGHT`, relative to its output's usable
area), and `reka-floating-frame-name` variables. The
`reka-window-created-hook`, `reka-window-geometry-change-hook`, and
`reka-floating-state-change-hook` run in the corresponding application buffer.
Ordinary Emacs window/buffer selection clears native floating focus; explicitly
selecting a floating application's buffer brings it to that frame and tab.

For asynchronous session startup, add your finalization function to
`reka-ready-hook` before calling `reka-enable`. The hook runs once, with
`reka-ready` set to non-nil, after River has processed the first rendered Emacs
frame on a usable output with input and key bindings available. The synchronous
`reka-enable-hook` only indicates that native startup has been launched; it is
too early to declare the desktop ready to a session manager.

## Using this

You should be a sufficiently committed person to use this. There is no
hand-holding!

First, get the source either from TVL directly, or from the mirror:

```
# clone directly from TVL:
git clone https://code.tvl.fyi/depot.git:/tools/emacs-pkgs/reka.git

# or clone from Codeberg mirror:
git clone https://codeberg.org/tazjin/reka.git
```

After that, build the Rust project (you need `pkg-config` and `libxkbcommon`),
and make yourself a script that launches Emacs approximately like this:

```
emacs --directory $reka_src/target/release --directory $reka_src/lisp ...
```

and then pass that script to river's `-c` flag on launch. In your Emacs config,
make sure to `(require 'reka)` and then `(reka-enable)`.

## Contributing

The upstream code location is in [the TVL
monorepo](https://code.tvl.fyi/about/tools/emacs-pkgs/reka). Issues can be
reported to the TVL bug tracker for those that have an account here, or on the
[Codeberg mirror](https://codeberg.org/tazjin/reka).

Reka follows the standard TVL [contribution
guidelines](https://code.tvl.fyi/tree/docs/CONTRIBUTING.md). Patches must be
written by real humans using fleshy brains.
