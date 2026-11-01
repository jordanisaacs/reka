// SPDX-License-Identifier: GPL-3.0-or-later

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::{Context, anyhow, bail};
use emacs::{Env, FromLisp, IntoLisp, Result, Value, Vector, defun, use_symbols};
use nix::poll::PollTimeout;
use nix::sys::eventfd::{EfdFlags, EventFd};
use wayland_client::{
    Connection, Dispatch,
    protocol::{wl_callback, wl_registry},
};
use xkbcommon::xkb;

mod floating;
use floating::{Placement, Rect, ResizeEdges, SizeHints, StackEntry, stacking_order};

use crate::river::{
    river_layer_shell_output_v1::RiverLayerShellOutputV1,
    river_layer_shell_seat_v1::RiverLayerShellSeatV1,
    river_layer_shell_v1::RiverLayerShellV1,
    river_output_v1::RiverOutputV1,
    river_seat_v1::Modifiers,
    river_window_manager_v1::RiverWindowManagerV1,
    river_window_v1::{Edges, RiverWindowV1},
    river_xkb_binding_v1::RiverXkbBindingV1,
    river_xkb_bindings_v1::RiverXkbBindingsV1,
};

mod virtual_keyboard;

mod river {
    pub extern crate wayland_client;
    pub use wayland_client::protocol::wl_surface;

    mod interfaces {
        pub(super) mod wm {
            pub use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("./protocol/river-window-management-v1.xml");
        }

        pub(super) mod xkb {
            use super::wm::*;
            wayland_scanner::generate_interfaces!("./protocol/river-xkb-bindings-v1.xml");
        }

        pub(super) mod layer_shell {
            use super::wm::*;
            wayland_scanner::generate_interfaces!("./protocol/river-layer-shell-v1.xml");
        }
    }

    use self::interfaces::layer_shell::*;
    use self::interfaces::wm::*;
    use self::interfaces::xkb::*;
    wayland_scanner::generate_client_code!("./protocol/river-window-management-v1.xml");
    wayland_scanner::generate_client_code!("./protocol/river-xkb-bindings-v1.xml");
    wayland_scanner::generate_client_code!("./protocol/river-layer-shell-v1.xml");
}

emacs::plugin_is_GPL_compatible!();

use_symbols!(
    cons
    list
    key_event
    new_window
    window_closed
    minimize_requested
    focused
    title_change
    app_id_change
    frame_request
    discard_frame
    toggle_fullscreen
    toggle_floating
    window_geometry
    parent_changed
    frame_focused
    frame_parameter
    frame_name_parameter => "name"
    message
    ready
    reka_get_window => "reka--get-window"
    reka_create_buffer => "reka--create-buffer"
    reka_list_buffers => "reka--list-buffers"
);

macro_rules! dispatch_log_only {
    ($proxy_ty:ty) => {
        impl Dispatch<$proxy_ty, ()> for Reka {
            fn event(
                _: &mut Self,
                _: &$proxy_ty,
                event: <$proxy_ty as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &wayland_client::QueueHandle<Self>,
            ) {
                log::debug!("{} event: {:?}", stringify!($proxy_ty), event);
            }
        }
    };
}

#[emacs::module(name = "libreka", defun_prefix = "reka")]
fn init(_: &Env) -> Result<()> {
    Ok(())
}

#[derive(Clone, Debug)]
enum KeyEvent {
    KeyCode(u32),
    Symbol(String),
}

impl<'e> FromLisp<'e> for KeyEvent {
    fn from_lisp(value: Value<'e>) -> Result<Self> {
        if let Ok(key_code) = u32::from_lisp(value) {
            return Ok(KeyEvent::KeyCode(key_code));
        }

        if let Ok(symbol_name) = String::from_lisp(value) {
            return Ok(KeyEvent::Symbol(symbol_name));
        }

        bail!("could not decode key event, reka bug?");
    }
}

impl<'e> IntoLisp<'e> for KeyEvent {
    fn into_lisp(self, env: &'e Env) -> Result<Value<'e>> {
        match self {
            KeyEvent::KeyCode(c) => c.into_lisp(env),
            KeyEvent::Symbol(name) => env.intern(&name),
        }
    }
}

#[derive(Debug)]
enum Command {
    /// Always forward the binding to Emacs
    Forward(KeyEvent),

    /// Toggle fullscreen for the selected window
    ToggleFullscreen,
    ToggleFloating,
}

fn command_from_lisp<'e>(command: Value<'e>, event: KeyEvent) -> Result<Command> {
    if command.is_not_nil() {
        if toggle_fullscreen.eq(&command) {
            return Ok(Command::ToggleFullscreen);
        }
        if toggle_floating.eq(&command) {
            return Ok(Command::ToggleFloating);
        }

        return Err(anyhow!("unknown builtin command"));
    }

    Ok(Command::Forward(event))
}

#[derive(Debug)]
enum FloatingCommand {
    Toggle,
    Set(bool),
    Move(i32, i32),
    Resize(i32, i32),
    Center,
}

#[derive(Debug)]
enum FromEmacs {
    RegisterPrefix(XKBPrefix, Command),
    SimulateKeys(virtual_keyboard::Request),
    ResetVirtualKeyboard,
    FocusWindow(RiverWindowV1),
    FocusFrame(String),
    CloseWindow(RiverWindowV1),
    BufferCreated(RiverWindowV1),
    UpdateParameters(HashMap<RiverWindowV1, WindowParameters>),
    Floating(RiverWindowV1, FloatingCommand),
    FloatingDefaultSize(i32, i32),
}

#[derive(Debug)]
enum ToEmacs {
    Ready,
    KeyEvent(KeyEvent),
    NewWindow(RiverWindowV1, bool, Option<RiverWindowV1>),
    WindowClosed(RiverWindowV1),
    ParentChanged(RiverWindowV1, Option<RiverWindowV1>),
    FrameFocused(String),
    Focused(RiverWindowV1),
    TitleChange(RiverWindowV1, String),
    AppIDChange(RiverWindowV1, String),
    MinimizeRequested(RiverWindowV1),
    WindowGeometry(RiverWindowV1, bool, String, Rect),
    RequestFrame,
    DiscardFrame(String),
    Message(String),
}

impl<'e> IntoLisp<'e> for ToEmacs {
    fn into_lisp(self, env: &'e Env) -> Result<Value<'e>> {
        match self {
            ToEmacs::Ready => ready.into_lisp(env),
            ToEmacs::KeyEvent(event) => env.cons(key_event, event),
            ToEmacs::NewWindow(win, floating, parent) => {
                let parent = match parent {
                    Some(parent) => RefCell::new(parent).into_lisp(env)?,
                    None => ().into_lisp(env)?,
                };
                env.call(list, (new_window, RefCell::new(win), floating, parent))
            }
            ToEmacs::ParentChanged(win, parent) => {
                let parent = match parent {
                    Some(parent) => RefCell::new(parent).into_lisp(env)?,
                    None => ().into_lisp(env)?,
                };
                env.call(list, (parent_changed, RefCell::new(win), parent))
            }
            ToEmacs::FrameFocused(name) => env.cons(frame_focused, name),
            ToEmacs::WindowClosed(win) => env.call(cons, (window_closed, RefCell::new(win))),
            ToEmacs::MinimizeRequested(win) => env.cons(minimize_requested, RefCell::new(win)),
            ToEmacs::Focused(win) => env.call(cons, (focused, RefCell::new(win))),
            ToEmacs::AppIDChange(win, app_id) => {
                env.call(list, (app_id_change, RefCell::new(win), app_id))
            }
            ToEmacs::TitleChange(win, title) => {
                env.call(list, (title_change, RefCell::new(win), title))
            }
            ToEmacs::WindowGeometry(window, floating, frame_name, rect) => env.call(
                list,
                (
                    window_geometry,
                    RefCell::new(window),
                    floating,
                    frame_name,
                    rect.x,
                    rect.y,
                    rect.w,
                    rect.h,
                ),
            ),
            ToEmacs::RequestFrame => frame_request.into_lisp(env),
            ToEmacs::DiscardFrame(frame_name) => env.call(cons, (discard_frame, frame_name)),
            ToEmacs::Message(text) => env.cons(message, text),
        }
    }
}

struct Handle {
    fd: Arc<EventFd>,
    tx: Sender<FromEmacs>,
    rx: Receiver<ToEmacs>,
    virtual_keyboard_ready: Arc<AtomicBool>,
}

impl Handle {
    fn send(&self, msg: FromEmacs) -> Result<()> {
        self.tx.send(msg)?;
        self.fd.write(1)?;
        Ok(())
    }
}

/// Request closing of the Wayland surface WINDOW through HANDLE. Note that
/// window refers to the Wayland side, not to an Emacs window.
///
/// This function should not need to be called manually.
#[defun]
fn close_window<'e>(env: &'e Env, handle: &Handle, window: &RiverWindowV1) -> Result<Value<'e>> {
    handle.send(FromEmacs::CloseWindow(window.clone()))?;
    ().into_lisp(env)
}

/// Whether race-free simulation is available for the active, unlocked seat.
/// Older compositors without river-virtual-keyboard-v1 remain supported.
#[defun]
fn virtual_keyboard_available_p(handle: &Handle) -> Result<bool> {
    Ok(handle.virtual_keyboard_ready.load(Ordering::Acquire))
}

/// Discard queued simulations and recreate the active seat's virtual keyboard.
/// This can recover a simulation client without restarting the window manager.
#[defun]
fn reset_virtual_keyboard<'e>(env: &'e Env, handle: &Handle) -> Result<Value<'e>> {
    handle
        .virtual_keyboard_ready
        .store(false, Ordering::Release);
    handle.send(FromEmacs::ResetVirtualKeyboard)?;
    ().into_lisp(env)
}

/// Send a bounded vector of [KEY MODIFIERS] strokes to an explicit managed
/// WINDOW. KEY is a Unicode character or XKB keysym name; modifiers use River's
/// bit masks. Public Lisp callers should use `reka-send-keys` instead.
#[defun]
fn inject_key_sequence<'e>(
    env: &'e Env,
    handle: &Handle,
    window: &RiverWindowV1,
    events: Vector<'e>,
) -> Result<Value<'e>> {
    if !handle.virtual_keyboard_ready.load(Ordering::Acquire) {
        bail!(
            "keyboard simulation unavailable: requires River 0.5, an active seat and an unlocked session"
        );
    }
    if events.len() == 0 || events.len() > virtual_keyboard::MAX_STROKES {
        bail!("simulation requires between 1 and 64 keys");
    }
    let mut strokes = Vec::with_capacity(events.len());
    for event in events {
        let stroke: Vector<'e> = event.into_rust()?;
        if stroke.len() != 2 {
            bail!("each simulated key requires a keysym and modifier bits");
        }
        let key: Value<'e> = stroke.get(0)?;
        let keysym = if let Ok(character) = key.into_rust::<u32>() {
            xkb::utf32_to_keysym(character)
        } else {
            let name: String = key.into_rust()?;
            xkb::keysym_from_name(&name, xkb::KEYSYM_CASE_INSENSITIVE)
        };
        let modifiers: u32 = stroke.get(1)?;
        strokes.push(virtual_keyboard::Stroke {
            keysym: keysym.into(),
            modifiers,
        });
    }
    let batch = virtual_keyboard::Batch::prepare(&strokes)?;
    handle.send(FromEmacs::SimulateKeys(virtual_keyboard::Request {
        target: window.clone(),
        batch,
    }))?;
    ().into_lisp(env)
}

/// Set whether WINDOW floats independently of its Emacs layout rectangle.
#[defun]
fn set_window_floating<'e>(
    env: &'e Env,
    handle: &Handle,
    window: &RiverWindowV1,
    floating: Value<'e>,
) -> Result<Value<'e>> {
    handle.send(FromEmacs::Floating(
        window.clone(),
        FloatingCommand::Set(floating.is_not_nil()),
    ))?;
    ().into_lisp(env)
}

/// Toggle WINDOW between tiled and floating placement.
#[defun]
fn toggle_window_floating<'e>(
    env: &'e Env,
    handle: &Handle,
    window: &RiverWindowV1,
) -> Result<Value<'e>> {
    handle.send(FromEmacs::Floating(window.clone(), FloatingCommand::Toggle))?;
    ().into_lisp(env)
}

/// Move a floating WINDOW by DX/DY logical pixels, keeping it in the work area.
#[defun]
fn move_floating_window<'e>(
    env: &'e Env,
    handle: &Handle,
    window: &RiverWindowV1,
    dx: i32,
    dy: i32,
) -> Result<Value<'e>> {
    handle.send(FromEmacs::Floating(
        window.clone(),
        FloatingCommand::Move(dx, dy),
    ))?;
    ().into_lisp(env)
}

/// Resize a floating WINDOW by DW/DH logical pixels, respecting client hints.
#[defun]
fn resize_floating_window<'e>(
    env: &'e Env,
    handle: &Handle,
    window: &RiverWindowV1,
    dw: i32,
    dh: i32,
) -> Result<Value<'e>> {
    handle.send(FromEmacs::Floating(
        window.clone(),
        FloatingCommand::Resize(dw, dh),
    ))?;
    ().into_lisp(env)
}

/// Center WINDOW over its parent, or over its output's usable area.
#[defun]
fn center_floating_window<'e>(
    env: &'e Env,
    handle: &Handle,
    window: &RiverWindowV1,
) -> Result<Value<'e>> {
    handle.send(FromEmacs::Floating(window.clone(), FloatingCommand::Center))?;
    ().into_lisp(env)
}

/// Configure the initial size of manually floated windows.
#[defun]
fn configure_floating<'e>(
    env: &'e Env,
    handle: &Handle,
    width: i32,
    height: i32,
) -> Result<Value<'e>> {
    if width <= 0 || height <= 0 {
        bail!("floating window dimensions must be positive");
    }
    handle.send(FromEmacs::FloatingDefaultSize(width, height))?;
    ().into_lisp(env)
}

type EmacsChannel = Box<dyn std::io::Write>;

/// Start the reka window manager process. This should not be called manually,
/// instead see `reka-enable` for the user-facing function.
#[defun(user_ptr)]
fn start_wm(env: &Env, pipe: Value<'_>) -> Result<Handle> {
    let channel_file = Box::new(env.open_channel(pipe)?);

    let (tx, rx) = channel::<FromEmacs>();
    let (tx_e, rx_e) = channel::<ToEmacs>();

    let virtual_keyboard_ready = Arc::new(AtomicBool::new(false));
    let ready_for_wm = virtual_keyboard_ready.clone();
    let emacs_fd = Arc::new(EventFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK)?);
    let emacs_fd_wmside = emacs_fd.clone();

    std::thread::spawn(move || {
        let result = wm_loop(rx, tx_e, channel_file, emacs_fd_wmside, ready_for_wm);
        if let Err(e) = result {
            log::error!("reka window manager thread crashed: {:?}", e);
        }
    });

    env.message("launched reka window manager! have fun ...")?;
    Ok(Handle {
        fd: emacs_fd,
        tx,
        rx: rx_e,
        virtual_keyboard_ready,
    })
}

#[derive(Clone, Debug, PartialEq)]
struct WindowParameters {
    window: RiverWindowV1,
    frame_name: String,
    x: i32,
    y: i32,
    h: i32,
    w: i32,
}

/// Construct an opaque window parameters object describing the display position
/// and dimensions for a given Wayland surface. This is an internal reka
/// function.
#[defun(user_ptr)]
fn make_window_parameters(
    _env: &Env,
    window: &RiverWindowV1,
    frame_name: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<WindowParameters> {
    let params = WindowParameters {
        window: window.clone(),
        frame_name,
        x,
        y,
        w,
        h,
    };

    Ok(params)
}

/// Update the window parameters for all currently visible surfaces. This is
/// called regularly by various Emacs hooks, e.g. when reacting to layout
/// changes.
///
/// If you end up having to call this manually, please file a reka issue.
#[defun]
fn update_window_parameters<'e>(
    env: &'e Env,
    handle: &Handle,
    per_frame: Vector<'e>,
) -> Result<Value<'e>> {
    let mut new_params = HashMap::new();
    for elem in per_frame.into_iter() {
        let params: Vector<'e> = elem.into_rust()?;
        for p in params.into_iter() {
            let wp: &RefCell<WindowParameters> = p.into_rust()?;
            let params = wp.borrow().clone();
            new_params.insert(params.window.clone(), params);
        }
    }

    handle.send(FromEmacs::UpdateParameters(new_params))?;
    ().into_lisp(env)
}

/// Requests that focus be given to the Wayland surface WINDOW on the compositor
/// side.
///
/// This is an internal function.
#[defun]
fn set_focus_request<'e>(env: &'e Env, handle: &Handle, window: Value<'e>) -> Result<Value<'e>> {
    if window.is_not_nil() {
        let cell: &RefCell<RiverWindowV1> = window.into_rust()?;
        handle.send(FromEmacs::FocusWindow(cell.borrow().clone()))?;
    } else {
        let frame_name: String = env
            .call(frame_parameter, ((), frame_name_parameter))?
            .into_rust()?;
        handle.send(FromEmacs::FocusFrame(frame_name))?;
    }

    ().into_lisp(env)
}

/// Confirm that the Emacs-side buffer for a given window was created.
///
/// This is an internal function.
#[defun]
fn notify_buffer_created<'e>(
    env: &'e Env,
    handle: &Handle,
    w: &RiverWindowV1,
) -> Result<Value<'e>> {
    handle.send(FromEmacs::BufferCreated(w.clone()))?;
    ().into_lisp(env)
}

/// Compare two Wayland surface objects (i.e. River windows) for equality.
#[defun]
fn window_equal<'e>(env: &'e Env, a: &RiverWindowV1, b: &RiverWindowV1) -> Result<Value<'e>> {
    a.eq(b).into_lisp(env)
}

/// Register the given key prefix for global use in reka. This function is
/// internal and difficult to call correctly, please see
/// `reka-push-intercept-prefix` for the user-facing version.
#[defun]
fn register_xkb_prefix<'e>(
    env: &'e Env,
    handle: &Handle,
    event: KeyEvent,
    key: Value<'e>,
    modifiers_bits: u32,
    command: Value<'e>,
) -> Result<Value<'e>> {
    // Emacs hands us either an int (Unicode codepoint for the key), or a string
    // with a "key name". The later is stuff like "XF86AudioRaiseVolume",
    // "Return", etc. which we map back to a keysym with XKB here.
    let keysym = if let Ok(codepoint) = key.into_rust::<i64>() {
        xkb::utf32_to_keysym(codepoint as u32)
    } else {
        let name: String = key.into_rust()?;
        xkb::keysym_from_name(&name, xkb::KEYSYM_CASE_INSENSITIVE)
    };

    if keysym == xkb::keysyms::KEY_NoSymbol.into() {
        return Err(anyhow!("could not resolve XKB keysym for key"));
    }

    let modifiers = Modifiers::from_bits(modifiers_bits).context("unknown modifier bits")?;

    let prefix = XKBPrefix {
        keysym: u32::from(keysym),
        modifiers,
    };

    let command = command_from_lisp(command, event)?;
    handle.send(FromEmacs::RegisterPrefix(prefix, command))?;

    ().into_lisp(env)
}

/// Receive the next reka->emacs command that should be handled. This is an
/// internal function.
#[defun]
fn get_next_command<'e>(env: &'e Env, handle: &Handle) -> Result<Value<'e>> {
    if let Ok(cmd) = handle.rx.try_recv() {
        log::debug!("emacs received command {:?}", cmd);
        return cmd.into_lisp(env);
    }

    ().into_lisp(env)
}

fn wm_loop(
    rx: Receiver<FromEmacs>,
    tx: Sender<ToEmacs>,
    to_emacs: EmacsChannel,
    emacs_fd: Arc<EventFd>,
    keyboard_ready: Arc<AtomicBool>,
) -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    let pid = std::process::id() as i32;

    log::info!("starting reka window manager thread (PID {}) ...", pid);
    let conn = Connection::connect_to_env().context("setting up Wayland connection")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let mut wm = Reka {
        pid,
        rx,
        tx,
        to_emacs,
        from_emacs: emacs_fd,
        seat: None,
        focus: Focus {
            dirty: false,
            state: FocusState::Lost,
        },
        frames: HashMap::new(),
        outputs: HashMap::new(),
        windows: HashMap::new(),
        river_wm: None,
        xkb_bindings: None,
        layer_shell: None,
        prefixes: Default::default(),
        pending_frames: 0,
        virtual_keyboard: virtual_keyboard::VirtualKeyboard::new(keyboard_ready),
        float_size: (640, 480),
        stacking_serial: 0,
        pointer_window: None,
        pointer_position: None,
        pointer_operation: None,
        pointer_bindings: Vec::new(),
        readiness: Readiness::Waiting,
    };
    let _registry = display.get_registry(&qh, ());

    loop {
        event_queue.flush()?;
        log::debug!("awaiting event from emacs or river");
        let guard = match event_queue.prepare_read() {
            Some(g) => g,
            None => {
                event_queue.dispatch_pending(&mut wm)?;
                continue;
            }
        };

        let river_fd = guard.connection_fd();

        let (emacs_ready, river_ready) = {
            let mut polls = [
                nix::poll::PollFd::new(wm.from_emacs.as_fd(), nix::poll::PollFlags::POLLIN),
                nix::poll::PollFd::new(river_fd, nix::poll::PollFlags::POLLIN),
            ];

            let timeout: u16 = 10_000; // TODO nicely named const
            let count = loop {
                match nix::poll::poll(&mut polls, PollTimeout::from(timeout)) {
                    Ok(c) => break c,
                    Err(nix::errno::Errno::EINTR) => {
                        log::debug!("poll interrupted, continuing");
                        continue;
                    }
                    Err(e) => return Err(e).context("while polling emacs/river"),
                }
            };

            if count == 0 {
                continue;
            }

            (
                polls[0]
                    .revents()
                    .expect("emacs_fd::revents() should not return None")
                    .contains(nix::poll::PollFlags::POLLIN),
                polls[1]
                    .revents()
                    .expect("river_fd::revents() should not return None")
                    .contains(nix::poll::PollFlags::POLLIN),
            )
        };

        if emacs_ready {
            wm.handle_emacs(&qh);
        }

        if river_ready {
            log::debug!("river has events ready");
            guard.read()?;
            event_queue.dispatch_pending(&mut wm)?;
        }
    }
}

enum FullscreenState {
    None,
    Requested {
        new: RiverWindowV1,
        previous: Option<RiverWindowV1>,
    },
    Fullscreen(RiverWindowV1),
    Exiting(RiverWindowV1),
}

struct Output {
    proxy: RiverOutputV1,
    ls_output: RiverLayerShellOutputV1,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    fullscreen: FullscreenState,
}

impl Drop for Output {
    fn drop(&mut self) {
        self.proxy.destroy();
        self.ls_output.destroy();
    }
}

struct Frame {
    name: Option<String>,
    displayed_on: Option<RiverOutputV1>,
    node: river::river_node_v1::RiverNodeV1,
    proxy: RiverWindowV1,
}

impl Drop for Frame {
    fn drop(&mut self) {
        // do NOT destroy displayed_on, output drop handles this
        self.proxy.destroy();
        self.node.destroy();
    }
}

#[derive(Debug, PartialEq)]
enum WindowState {
    Starting,
    Active,
    Killed,
}

#[derive(Debug)]
struct Window {
    proxy: RiverWindowV1,
    node: river::river_node_v1::RiverNodeV1,
    state: WindowState,
    params: Option<WindowParameters>,
    actual_width_height: Option<(i32, i32)>,
    placement: Placement,
    parent: Option<RiverWindowV1>,
    hints: SizeHints,
    stacking_order: u64,
    reported_geometry: Option<(bool, String, Rect)>,
    announced: bool,
    title: Option<String>,
    app_id: Option<String>,
}

impl Drop for Window {
    fn drop(&mut self) {
        self.proxy.destroy();
        self.node.destroy();
    }
}

#[derive(Clone, Copy, Debug)]
enum PointerAction {
    Move,
    Resize(ResizeEdges),
}

struct PointerOperation {
    window: RiverWindowV1,
    seat: river::river_seat_v1::RiverSeatV1,
    action: PointerAction,
    initial: Rect,
    start_size: Option<(i32, i32)>,
    started: bool,
    released: bool,
}

struct PointerBinding {
    proxy: river::river_pointer_binding_v1::RiverPointerBindingV1,
    action: PointerAction,
    enabled: bool,
}

impl Drop for PointerBinding {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

#[derive(Clone)]
struct DisplayPlacement {
    frame: RiverWindowV1,
    frame_name: String,
    output: RiverOutputV1,
    area: Rect,
    rect: Rect,
}

struct Seat {
    proxy: river::river_seat_v1::RiverSeatV1,
    #[allow(dead_code)] // TODO(tazjin): do I *need* to store this?
    ls_seat: RiverLayerShellSeatV1,
}

impl Drop for Seat {
    fn drop(&mut self) {
        self.proxy.destroy();
        self.ls_seat.destroy();
    }
}

#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy)]
struct XKBPrefix {
    keysym: u32,
    modifiers: Modifiers,
}

#[derive(Debug)]
enum BindingState {
    Requested,
    Registered(RiverXkbBindingV1),
    Enabled(RiverXkbBindingV1),
}

impl Drop for BindingState {
    fn drop(&mut self) {
        if let BindingState::Enabled(binding) = self {
            binding.destroy();
        }
    }
}

#[derive(Debug)]
struct Binding {
    #[allow(dead_code)]
    command: Command,
    state: BindingState,
}

enum FocusState {
    Lost,
    Frame(RiverWindowV1),
    Window {
        window: RiverWindowV1,
        on_frame: RiverWindowV1,
    },
}

struct Focus {
    dirty: bool,
    state: FocusState,
}

impl Focus {
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn is_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    fn lost_focus(&self) -> bool {
        matches!(self.state, FocusState::Lost)
    }

    fn current_focus_and_frame(&self) -> Option<(&RiverWindowV1, &RiverWindowV1)> {
        match &self.state {
            FocusState::Lost => None,
            FocusState::Frame(frame) => Some((frame, frame)),
            FocusState::Window { window, on_frame } => Some((window, on_frame)),
        }
    }

    /// invalidate a window, meaning it can not be focused anymore (e.g. was closed)
    fn invalidate(&mut self, target: &RiverWindowV1) {
        match &self.state {
            FocusState::Frame(w) | FocusState::Window { on_frame: w, .. } if w.eq(target) => {
                self.state = FocusState::Lost;
            }
            FocusState::Window { window, on_frame } if window.eq(target) => {
                self.state = FocusState::Frame(on_frame.clone());
            }
            _ => {}
        }
    }

    fn focus_window(&mut self, window: RiverWindowV1, on_frame: RiverWindowV1) {
        self.state = FocusState::Window { window, on_frame };
    }

    fn focus_frame(&mut self, frame: RiverWindowV1) {
        self.state = FocusState::Frame(frame);
    }

    fn switch_to_frame(&mut self) {
        if let FocusState::Window { on_frame, .. } = &self.state {
            self.focus_frame(on_frame.clone());
        }
    }
}

#[derive(Default, PartialEq)]
enum Readiness {
    #[default]
    Waiting,
    Pending,
    Ready,
}

struct Reka {
    pid: i32,

    // Emacs-related state
    rx: Receiver<FromEmacs>,
    tx: Sender<ToEmacs>,
    to_emacs: EmacsChannel,
    from_emacs: Arc<EventFd>,
    pending_frames: usize,

    // river-related state
    river_wm: Option<RiverWindowManagerV1>,
    xkb_bindings: Option<RiverXkbBindingsV1>,
    layer_shell: Option<RiverLayerShellV1>,
    virtual_keyboard: virtual_keyboard::VirtualKeyboard,

    seat: Option<Seat>,
    focus: Focus,

    frames: HashMap<RiverWindowV1, Frame>,
    outputs: HashMap<RiverOutputV1, Output>,
    windows: HashMap<RiverWindowV1, Window>,
    prefixes: HashMap<XKBPrefix, Binding>,
    float_size: (i32, i32),
    stacking_serial: u64,
    pointer_window: Option<RiverWindowV1>,
    pointer_position: Option<(i32, i32)>,
    pointer_operation: Option<PointerOperation>,
    pointer_bindings: Vec<PointerBinding>,
    readiness: Readiness,
}

impl Drop for Reka {
    fn drop(&mut self) {
        // Destroy everything else before tearing down the WM objects.
        self.virtual_keyboard.reset();
        self.pointer_bindings.clear();
        self.prefixes.clear();
        self.windows.clear();
        self.frames.clear();
        self.outputs.clear();
        self.seat.take();

        if let Some(bindings) = self.xkb_bindings.take() {
            bindings.destroy();
        }

        if let Some(layer_shell) = self.layer_shell.take() {
            layer_shell.destroy();
        }

        if let Some(wm) = self.river_wm.take() {
            wm.destroy();
        }
    }
}

impl Reka {
    fn has_usable_frame(&self) -> bool {
        self.seat.is_some()
            && self.xkb_bindings.is_some()
            && self.frames.values().any(|frame| {
                frame
                    .displayed_on
                    .as_ref()
                    .and_then(|output| self.outputs.get(output))
                    .is_some_and(|output| output.width > 0 && output.height > 0)
            })
    }

    fn send(&mut self, cmd: ToEmacs) {
        if let Err(err) = self.tx.send(cmd) {
            log::error!("could not send message to Emacs: {}", err);
        }
        if let Err(err) = self.to_emacs.write_all(&[1]) {
            log::error!("could not notify Emacs: {}", err);
        }
    }

    fn announce_windows(&mut self) {
        let mut announcements = Vec::new();
        for window in self.windows.values_mut() {
            if !window.announced {
                window.announced = true;
                announcements.push((
                    window.stacking_order,
                    ToEmacs::NewWindow(
                        window.proxy.clone(),
                        window.placement.floating(),
                        window.parent.clone(),
                    ),
                ));
            }
        }
        announcements.sort_by_key(|(order, _)| *order);
        for (_, announcement) in announcements {
            self.send(announcement);
        }
    }

    fn window_is_fullscreen(&self, proxy: &RiverWindowV1) -> bool {
        self.outputs
            .values()
            .any(|output| match &output.fullscreen {
                FullscreenState::Requested { new, .. } => new == proxy,
                FullscreenState::Fullscreen(window) => window == proxy,
                FullscreenState::None | FullscreenState::Exiting(_) => false,
            })
    }

    fn display_placement(&self, proxy: &RiverWindowV1) -> Option<DisplayPlacement> {
        self.display_placement_inner(proxy, 0)
    }

    fn display_placement_inner(
        &self,
        proxy: &RiverWindowV1,
        depth: usize,
    ) -> Option<DisplayPlacement> {
        // River guarantees an acyclic parent tree; also bound traversal locally.
        if depth > self.windows.len() {
            return None;
        }
        let window = self.windows.get(proxy)?;
        if window.state != WindowState::Active || window.params.is_none() {
            return None;
        }
        let mut display = if let Some(parent) = &window.parent {
            if self.windows.contains_key(parent) {
                self.display_placement_inner(parent, depth + 1)?
            } else {
                let frame = self.frames.get(parent)?;
                let output_proxy = frame.displayed_on.as_ref()?;
                let output = self.outputs.get(output_proxy)?;
                let area = Rect {
                    x: 0,
                    y: 0,
                    w: output.width.max(1),
                    h: output.height.max(1),
                };
                DisplayPlacement {
                    frame: parent.clone(),
                    frame_name: frame.name.clone()?,
                    output: output_proxy.clone(),
                    area,
                    rect: area,
                }
            }
        } else {
            let params = window.params.as_ref()?;
            let frame = self.frame_by_name(&params.frame_name)?;
            let output_proxy = frame.displayed_on.as_ref()?;
            let output = self.outputs.get(output_proxy)?;
            DisplayPlacement {
                frame: frame.proxy.clone(),
                frame_name: params.frame_name.clone(),
                output: output_proxy.clone(),
                area: Rect {
                    x: 0,
                    y: 0,
                    w: output.width.max(1),
                    h: output.height.max(1),
                },
                rect: Rect {
                    x: params.x,
                    y: params.y,
                    w: params.w.max(1),
                    h: params.h.max(1),
                },
            }
        };
        let tile = window
            .params
            .as_ref()
            .map(|params| Rect {
                x: params.x,
                y: params.y,
                w: params.w.max(1),
                h: params.h.max(1),
            })
            .unwrap_or(display.rect);
        display.rect = window.placement.geometry(tile);
        if window.placement.floating() {
            display.rect = display.rect.fit(display.area, window.hints);
        }
        Some(display)
    }

    fn raise_window(&mut self, proxy: &RiverWindowV1) {
        let mut root = proxy.clone();
        for _ in 0..self.windows.len() {
            let Some(parent) = self
                .windows
                .get(&root)
                .and_then(|window| window.parent.as_ref())
            else {
                break;
            };
            if !self.windows.contains_key(parent) {
                break;
            }
            root = parent.clone();
        }
        let mut family = vec![root];
        for index in 0..self.windows.len() {
            if index >= family.len() {
                break;
            }
            let parent = family[index].clone();
            let mut children: Vec<_> = self
                .windows
                .iter()
                .filter(|(_, window)| window.parent.as_ref() == Some(&parent))
                .map(|(id, window)| (window.stacking_order, id.clone()))
                .collect();
            children.sort_by_key(|(order, _)| *order);
            family.extend(children.into_iter().map(|(_, id)| id));
        }
        for id in family {
            self.stacking_serial += 1;
            if let Some(window) = self.windows.get_mut(&id) {
                window.stacking_order = self.stacking_serial;
            }
        }
    }

    fn center_reference(&self, proxy: &RiverWindowV1, display: &DisplayPlacement) -> Rect {
        self.windows
            .get(proxy)
            .and_then(|window| window.parent.as_ref())
            .and_then(|parent| self.display_placement(parent))
            .map(|parent| parent.rect)
            .unwrap_or(display.area)
    }

    fn floating_command(&mut self, proxy: &RiverWindowV1, command: FloatingCommand) {
        let Some(display) = self.display_placement(proxy) else {
            return;
        };
        // Fullscreen owns the current dimensions. Leave the saved float alone.
        if self.window_is_fullscreen(proxy) {
            return;
        }
        let reference = self.center_reference(proxy, &display);
        let Some(window) = self.windows.get_mut(proxy) else {
            return;
        };
        let was_floating = window.placement.floating();
        let should_float = match command {
            FloatingCommand::Toggle => !was_floating,
            FloatingCommand::Set(value) => value,
            _ => true,
        };
        window.placement.explicit = Some(should_float);
        if !should_float {
            if let Some(operation) = &mut self.pointer_operation
                && operation.window == *proxy
            {
                operation.released = true;
            }
            return;
        }
        let initial = window.placement.geometry.unwrap_or_else(|| {
            Rect::centered(
                self.float_size.0,
                self.float_size.1,
                reference,
                display.area,
                window.hints,
            )
        });
        window.placement.geometry = Some(match command {
            FloatingCommand::Move(dx, dy) => initial.moved(dx, dy, display.area, window.hints),
            FloatingCommand::Resize(dw, dh) => initial.resized(
                dw,
                dh,
                ResizeEdges {
                    right: true,
                    bottom: true,
                    ..Default::default()
                },
                display.area,
                window.hints,
            ),
            FloatingCommand::Center => {
                Rect::centered(initial.w, initial.h, reference, display.area, window.hints)
            }
            _ => initial.fit(display.area, window.hints),
        });
        self.raise_window(proxy);
    }

    fn initialize_floating_geometry(&mut self) {
        // Parents are created before children. Resolve those first so dialogs
        // center over the parent's actual box rather than an Emacs tile.
        let mut needs_configure = false;
        let mut windows: Vec<_> = self
            .windows
            .iter()
            .map(|(proxy, window)| (window.stacking_order, proxy.clone()))
            .collect();
        windows.sort_by_key(|(order, _)| *order);
        for (_, proxy) in windows {
            if self.window_is_fullscreen(&proxy) {
                continue;
            }
            let Some(display) = self.display_placement(&proxy) else {
                continue;
            };
            let reference = self.center_reference(&proxy, &display);
            let window = self.windows.get_mut(&proxy).unwrap();
            if !window.placement.floating() {
                continue;
            }
            if let Some(geometry) = window.placement.geometry {
                window.placement.geometry = Some(geometry.fit(display.area, window.hints));
            } else if let Some((width, height)) = window.actual_width_height {
                window.placement.geometry = Some(Rect::centered(
                    width,
                    height,
                    reference,
                    display.area,
                    window.hints,
                ));
                if let Some(geometry) = window.placement.geometry {
                    needs_configure |= (geometry.w, geometry.h) != (width, height);
                }
            }
        }
        if needs_configure && let Some(manager) = &self.river_wm {
            manager.manage_dirty();
        }
    }

    fn begin_pointer_operation(
        &mut self,
        proxy: &RiverWindowV1,
        seat: river::river_seat_v1::RiverSeatV1,
        action: PointerAction,
    ) {
        if self.pointer_operation.is_some()
            || self.window_is_fullscreen(proxy)
            || self.seat.as_ref().map(|current| &current.proxy) != Some(&seat)
        {
            return;
        }
        let Some(display) = self.display_placement(proxy) else {
            return;
        };
        let Some(window) = self.windows.get_mut(proxy) else {
            return;
        };
        let from_tiled = !window.placement.floating();
        let start_size = if from_tiled && matches!(action, PointerAction::Move) {
            Some(
                window
                    .placement
                    .geometry
                    .map(|rect| (rect.w, rect.h))
                    .unwrap_or(self.float_size),
            )
        } else {
            None
        };
        window.placement.explicit = Some(true);
        window.placement.geometry = Some(display.rect);
        self.raise_window(proxy);
        self.focus.focus_window(proxy.clone(), display.frame);
        self.focus.mark_dirty();
        self.pointer_operation = Some(PointerOperation {
            window: proxy.clone(),
            seat,
            action,
            initial: display.rect,
            start_size,
            started: false,
            released: false,
        });
    }

    fn pointer_delta(&mut self, dx: i32, dy: i32) {
        let Some(operation) = &self.pointer_operation else {
            return;
        };
        if operation.released {
            return;
        }
        let Some(display) = self.display_placement(&operation.window) else {
            return;
        };
        let window = self.windows.get_mut(&operation.window).unwrap();
        window.placement.geometry = Some(match operation.action {
            PointerAction::Move => operation.initial.moved(dx, dy, display.area, window.hints),
            PointerAction::Resize(edges) => {
                operation
                    .initial
                    .resized(dx, dy, edges, display.area, window.hints)
            }
        });
    }

    fn reconcile_pointer_operation(&mut self) {
        if let Some(operation) = &self.pointer_operation {
            let valid = self.display_placement(&operation.window).is_some()
                && self
                    .windows
                    .get(&operation.window)
                    .is_some_and(|window| window.placement.floating())
                && !self.window_is_fullscreen(&operation.window);
            if !valid {
                self.pointer_operation.as_mut().unwrap().released = true;
            }
        }
        // PointerPosition may follow the initiating button/client event in
        // the same batch. Use its final value at manage_start, not stale input.
        let start_geometry = self.pointer_operation.as_ref().and_then(|operation| {
            let (width, height) = operation.start_size?;
            if operation.started || operation.released {
                return None;
            }
            let display = self.display_placement(&operation.window)?;
            let hints = self.windows.get(&operation.window)?.hints;
            let geometry = if let Some((x, y)) = self.pointer_position {
                let output = self.outputs.get(&display.output)?;
                operation.initial.grabbed(
                    width,
                    height,
                    (x.saturating_sub(output.x), y.saturating_sub(output.y)),
                    display.area,
                    hints,
                )
            } else {
                Rect::centered(width, height, display.area, display.area, hints)
            };
            Some(geometry)
        });
        if let Some(geometry) = start_geometry
            && let Some(operation) = &mut self.pointer_operation
        {
            operation.initial = geometry;
            operation.start_size = None;
            if let Some(window) = self.windows.get_mut(&operation.window) {
                window.placement.geometry = Some(geometry);
            }
        }
        if let Some(operation) = &mut self.pointer_operation {
            if operation.released {
                if operation.started {
                    if matches!(operation.action, PointerAction::Resize(_))
                        && self.windows.contains_key(&operation.window)
                    {
                        operation.window.inform_resize_end();
                    }
                    operation.seat.op_end();
                }
                self.pointer_operation = None;
            } else if !operation.started {
                operation.seat.op_start_pointer();
                if matches!(operation.action, PointerAction::Resize(_)) {
                    operation.window.inform_resize_start();
                }
                operation.started = true;
            }
        }
        for binding in &mut self.pointer_bindings {
            if !binding.enabled {
                binding.proxy.enable();
                binding.enabled = true;
            }
        }
    }

    fn render_windows(&mut self) {
        self.initialize_floating_geometry();
        let order = stacking_order(
            self.windows
                .iter()
                .map(|(id, window)| StackEntry {
                    id: id.clone(),
                    parent: window.parent.clone(),
                    floating: window.placement.floating(),
                    order: window.stacking_order,
                })
                .collect(),
        );
        let mut notifications = Vec::new();
        for proxy in order {
            let Some(display) = self.display_placement(&proxy) else {
                proxy.hide();
                continue;
            };
            let output = &self.outputs[&display.output];
            let window = self.windows.get_mut(&proxy).unwrap();
            proxy.show();
            window.node.set_position(
                display.rect.x.saturating_add(output.x),
                display.rect.y.saturating_add(output.y),
            );
            window.node.place_top();
            if window.placement.floating() {
                // Retain native decorations, shadows and subsurfaces on floats.
                proxy.set_clip_box(0, 0, 0, 0);
            } else {
                let (width, height) = window
                    .actual_width_height
                    .unwrap_or((display.rect.w, display.rect.h));
                proxy.set_clip_box(0, 0, width, height);
            }
            let report = (
                window.placement.floating(),
                display.frame_name.clone(),
                display.rect,
            );
            if window.reported_geometry.as_ref() != Some(&report) {
                window.reported_geometry = Some(report.clone());
                notifications.push(ToEmacs::WindowGeometry(
                    proxy.clone(),
                    report.0,
                    report.1,
                    report.2,
                ));
            }
        }
        for notification in notifications {
            self.send(notification);
        }
    }

    fn frame_by_output(&self, output: &RiverOutputV1) -> Option<&Frame> {
        self.frames
            .iter()
            .find(|(_, f)| f.displayed_on.as_ref() == Some(output))
            .map(|x| x.1)
    }

    fn frame_by_name(&self, name: &str) -> Option<&Frame> {
        self.frames
            .iter()
            .find(|(_, f)| f.name.as_deref() == Some(name))
            .map(|x| x.1)
    }

    fn displaying_frame(&self, window: &RiverWindowV1) -> Option<&Frame> {
        self.display_placement(window)
            .and_then(|display| self.frames.get(&display.frame))
    }

    fn binding_by_proxy(&self, proxy: &RiverXkbBindingV1) -> Option<(&XKBPrefix, &Binding)> {
        self.prefixes.iter().find(|(_, v)| {
            if let BindingState::Enabled(this) = &v.state {
                this.eq(proxy)
            } else {
                false
            }
        })
    }

    // reconcile_frames ensures that each output gets one maximized Emacs frame.
    fn reconcile_frames(&mut self) {
        let mut frame_requests: usize = 0;

        'outputs: for (output_proxy, output) in &self.outputs {
            if let Some(f) = self.frame_by_output(output_proxy) {
                f.proxy.propose_dimensions(output.width, output.height);
                continue;
            }

            // try to find an existing frame that is minimised and reuse it
            for f in self.frames.values_mut() {
                if f.displayed_on.is_some() {
                    continue;
                }

                f.displayed_on = Some(output_proxy.clone());

                // always propose dimensions to keep frame sized to output
                f.proxy.propose_dimensions(output.width, output.height);
                f.proxy.inform_maximized();
                f.proxy.set_tiled(Edges::all());
                continue 'outputs;
            }

            // no frame found -> might need a new frame
            frame_requests += 1;
        }

        // Request new frames (if we haven't already fired a request). This
        // accounting is necessary because there might be additional manage
        // sequences before a frame shows up.
        if frame_requests > self.pending_frames {
            for _ in 0..(frame_requests - self.pending_frames) {
                self.send(ToEmacs::RequestFrame);
                self.pending_frames += 1;
            }
        }
    }

    // reconcile_focus updates the seat's focus based on pending_focus (set by
    // events)
    fn reconcile_focus(&mut self) {
        let Some(seat) = &mut self.seat else {
            log::warn!("no seat present, something might have gone wrong");
            return;
        };

        if self.focus.lost_focus() {
            // recover focus by focusing *any* displayed frame
            let Some(frame) = self
                .frames
                .iter()
                .find(|(_, f)| f.displayed_on.is_some())
                .map(|(proxy, _)| proxy.clone())
            else {
                log::error!("no displayed frames -> can not focus anything!");
                return;
            };

            self.focus.focus_frame(frame);
        }

        let is_dirty = self.focus.is_dirty();
        let (target, frame) = self.focus.current_focus_and_frame().unwrap();

        seat.proxy.focus_window(target);

        if is_dirty {
            let output = self.frames[frame]
                .displayed_on
                .as_ref()
                .expect("reka bug: inactive frame focused");
            self.outputs[output].ls_output.set_default();

            // don't notify emacs about focused frames
            if target != frame {
                self.send(ToEmacs::Focused(target.clone()));
            }
        }
    }

    // A manage sequence commits focus and one destination's keys atomically.
    // Never combine destinations: River dispatches queued keys to final focus.
    fn flush_virtual_keys(&mut self) {
        let Some(request) = self.virtual_keyboard.next() else {
            return;
        };
        let frame = self
            .windows
            .get(&request.target)
            .filter(|window| window.state == WindowState::Active)
            .and_then(|_| self.displaying_frame(&request.target))
            .filter(|frame| frame.displayed_on.is_some())
            .map(|frame| frame.proxy.clone());
        let (Some(frame), Some(seat)) = (frame, self.seat.as_ref()) else {
            self.send(ToEmacs::Message(
                "keyboard simulation canceled: destination is no longer displayed".into(),
            ));
            return;
        };
        self.focus.focus_window(request.target.clone(), frame);
        seat.proxy.focus_window(&request.target);
        if let Err(error) = self.virtual_keyboard.emit(request.batch) {
            self.send(ToEmacs::Message(format!(
                "keyboard simulation failed: {error}"
            )));
        }
    }

    // reconcile_windows closes killed windows and removes them from the list
    fn reconcile_windows(&mut self) {
        for (proxy, window) in &self.windows {
            match window.state {
                WindowState::Killed => proxy.close(),
                WindowState::Starting => {}
                WindowState::Active => {
                    proxy.set_capabilities(
                        river::river_window_v1::Capabilities::from_bits_truncate(4 | 8),
                    );
                    if self.window_is_fullscreen(proxy) {
                        continue;
                    }
                    if let Some(display) = self.display_placement(proxy) {
                        if window.placement.floating() {
                            proxy.set_tiled(Edges::empty());
                            proxy.use_csd();
                            if window.placement.geometry.is_some() {
                                proxy.propose_dimensions(display.rect.w, display.rect.h);
                            } else {
                                // Give new dialogs their natural initial dimensions.
                                proxy.propose_dimensions(0, 0);
                            }
                        } else {
                            proxy.set_tiled(Edges::all());
                            proxy.use_ssd();
                            proxy.propose_dimensions(display.rect.w, display.rect.h);
                        }
                    }
                }
            }
        }
    }

    fn reconcile_bindings(&mut self) {
        let mut to_enable = vec![];
        {
            for v in self.prefixes.values_mut() {
                if let BindingState::Registered(b) = &v.state {
                    to_enable.push(b.clone());
                    v.state = BindingState::Enabled(b.clone());
                    log::info!("enabling new XKB binding");
                }
            }
        };

        for binding in to_enable.into_iter() {
            binding.enable();
        }
    }

    fn reconcile_fullscreen(&mut self) {
        for output in self.outputs.values_mut() {
            match &output.fullscreen {
                FullscreenState::Requested { new, previous } => {
                    if let Some(previous) = previous {
                        previous.inform_not_fullscreen();
                        previous.exit_fullscreen();
                    }

                    new.inform_fullscreen();
                    new.fullscreen(&output.proxy);
                    output.fullscreen = FullscreenState::Fullscreen(new.clone());
                }
                FullscreenState::Exiting(win) => {
                    win.inform_not_fullscreen();
                    win.exit_fullscreen();
                    output.fullscreen = FullscreenState::None;
                }
                _ => {}
            }
        }
    }

    fn handle_emacs(&mut self, qh: &wayland_client::QueueHandle<Self>) {
        log::debug!("emacs signalled available data");
        let mut buf = [0u8; 8];
        let _ = nix::unistd::read(&self.from_emacs, &mut buf);

        let mut needs_manage = false;
        while let Ok(msg) = self.rx.try_recv() {
            log::debug!("message from emacs: {:?}", msg);
            match msg {
                FromEmacs::ResetVirtualKeyboard => {
                    self.virtual_keyboard.restart(qh);
                }
                FromEmacs::SimulateKeys(request) => {
                    if let Err(error) = self.virtual_keyboard.enqueue(request) {
                        self.send(ToEmacs::Message(error.to_string()));
                    } else {
                        needs_manage = true;
                    }
                }
                FromEmacs::RegisterPrefix(prefix, command) => {
                    needs_manage = true;
                    log::debug!("registering new prefix with command {:?}", command);
                    match self.prefixes.entry(prefix) {
                        std::collections::hash_map::Entry::Occupied(mut entry) => {
                            entry.get_mut().command = command;
                        }
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(Binding {
                                command,
                                state: BindingState::Requested,
                            });
                        }
                    }
                }
                FromEmacs::FocusWindow(window) => {
                    needs_manage = true;
                    let Some(frame) = self.displaying_frame(&window) else {
                        log::error!("can not focus window that is not displayed!");
                        continue;
                    };
                    let frame = frame.proxy.clone();
                    self.raise_window(&window);
                    self.focus.focus_window(window, frame);
                }
                FromEmacs::FocusFrame(frame_name) => {
                    needs_manage = true;
                    let frame = self
                        .frame_by_name(&frame_name)
                        .filter(|frame| frame.displayed_on.is_some())
                        .map(|frame| frame.proxy.clone());
                    if let Some(frame) = frame {
                        self.focus.focus_frame(frame);
                    } else {
                        self.focus.switch_to_frame();
                    }
                }
                FromEmacs::CloseWindow(window) => {
                    if let Some(w) = self.windows.get_mut(&window) {
                        w.state = WindowState::Killed;
                        needs_manage = true;
                    }
                }
                FromEmacs::BufferCreated(window) => {
                    let metadata = if let Some(w) = self.windows.get_mut(&window) {
                        needs_manage = true;
                        w.state = WindowState::Active;
                        Some((w.app_id.clone(), w.title.clone()))
                    } else {
                        None
                    };
                    // Initial metadata often precedes Emacs buffer creation.
                    // Replay after its acknowledgement, with app-id first so
                    // the subsequent title produces the correct buffer name.
                    if let Some((app_id, title)) = metadata {
                        if let Some(app_id) = app_id {
                            self.send(ToEmacs::AppIDChange(window.clone(), app_id));
                        }
                        if let Some(title) = title {
                            self.send(ToEmacs::TitleChange(window.clone(), title));
                        }
                    }
                }
                FromEmacs::Floating(proxy, command) => {
                    self.floating_command(&proxy, command);
                    needs_manage = true;
                }
                FromEmacs::FloatingDefaultSize(width, height) => {
                    self.float_size = (width, height);
                }
                FromEmacs::UpdateParameters(mut new_params) => {
                    for (proxy, w) in self.windows.iter_mut() {
                        let result = new_params.remove(proxy);
                        if w.params != result {
                            w.params = result;
                            needs_manage = true;
                        }
                    }
                }
            }
        }

        // All commands from Emacs need a manage sequence.
        if let Some(river_wm) = &self.river_wm
            && needs_manage
        {
            river_wm.manage_dirty();
        }

        let register_prefixes = self
            .prefixes
            .iter()
            .filter_map(|(k, v)| match v.state {
                BindingState::Requested => Some(*k),
                _ => None,
            })
            .collect::<Vec<_>>();

        if !register_prefixes.is_empty()
            && let (Some(xkb), Some(seat)) = (&self.xkb_bindings, &self.seat)
        {
            let seat = seat.proxy.clone();
            let mut bindings = vec![];

            for p in &register_prefixes {
                let b = xkb.get_xkb_binding(&seat, p.keysym, p.modifiers, qh, ());
                bindings.push(b);
            }

            for (p, b) in register_prefixes.into_iter().zip(bindings) {
                let binding = self
                    .prefixes
                    .get_mut(&p)
                    .expect("binding magically disappeared");
                binding.state = BindingState::Registered(b);
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Reka {
    fn event(
        state: &mut Self,
        proxy: &wl_registry::WlRegistry,
        event: <wl_registry::WlRegistry as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        if let wl_registry::Event::GlobalRemove { name } = event {
            state.virtual_keyboard.global_removed(name);
            return;
        }
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state
                .virtual_keyboard
                .global(proxy, name, &interface, version, qhandle);
            if interface == "river_window_manager_v1" {
                let wm = proxy.bind::<RiverWindowManagerV1, _, _>(name, 4, qhandle, ());
                state.river_wm = Some(wm);
                log::debug!("registering river window manager ...");
            } else if interface == "river_xkb_bindings_v1" {
                let xkb = proxy.bind::<RiverXkbBindingsV1, _, _>(name, 2, qhandle, ());
                state.xkb_bindings = Some(xkb);
                log::debug!("registering river xkb bindings ...");
            } else if interface == "river_layer_shell_v1" {
                let layer_shell = proxy.bind::<RiverLayerShellV1, _, _>(name, 1, qhandle, ());
                state.layer_shell = Some(layer_shell);
                log::debug!("registering river layer shell ... ");
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Reka {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event
            && state.readiness == Readiness::Pending
        {
            if state.has_usable_frame() {
                state.readiness = Readiness::Ready;
                state.send(ToEmacs::Ready);
            } else {
                state.readiness = Readiness::Waiting;
            }
        }
    }
}

impl Dispatch<RiverWindowManagerV1, ()> for Reka {
    fn event(
        state: &mut Self,
        proxy: &RiverWindowManagerV1,
        event: <RiverWindowManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        conn: &Connection,
        qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            // manage sequence: window dimensions, fullscreen state, keyboard
            // focus, decorations, capabilities ...
            river::river_window_manager_v1::Event::ManageStart => {
                log::debug!("manage sequence started");

                state.announce_windows();
                state.reconcile_frames();
                state.reconcile_pointer_operation();
                state.reconcile_windows();
                state.reconcile_bindings();
                state.reconcile_fullscreen();
                state.reconcile_focus();
                state.flush_virtual_keys();

                proxy.manage_finish();
                if state.virtual_keyboard.has_pending() {
                    proxy.manage_dirty();
                }
            }

            // render sequence: positions, z-order, borders, visibility (?), clipping
            river::river_window_manager_v1::Event::RenderStart => {
                // reconcile frame display state
                for (frame_proxy, frame) in state.frames.iter() {
                    match frame.displayed_on.as_ref() {
                        None => frame_proxy.hide(),
                        Some(output_proxy) => {
                            frame_proxy.show();
                            frame.node.place_bottom();

                            // position frame at its output's origin
                            if let Some(output) = state.outputs.get(output_proxy) {
                                frame.node.set_position(output.x, output.y);
                            }
                        }
                    }
                }

                state.render_windows();

                proxy.render_finish();

                if state.readiness == Readiness::Waiting && state.has_usable_frame() {
                    state.readiness = Readiness::Pending;
                    // Wait until River has processed the render requests before
                    // telling the session manager that the desktop is usable.
                    conn.display().sync(qhandle, ());
                }
            }

            river::river_window_manager_v1::Event::Output { id } => {
                log::debug!("RiverWindowManagerV1::Event::Output received: id={:?}", id);
                let ls_output = state
                    .layer_shell
                    .as_ref()
                    .expect("layer shell not registered")
                    .get_output(&id, qhandle, ());

                state.outputs.insert(
                    id.clone(),
                    Output {
                        proxy: id,
                        ls_output,
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                        fullscreen: FullscreenState::None,
                    },
                );
            }

            river::river_window_manager_v1::Event::Seat { id } => {
                log::debug!("RiverWindowManagerV1::Event::Seat received: id={:?}", id);
                let ls_seat = state
                    .layer_shell
                    .as_ref()
                    .expect("layer shell not bound")
                    .get_seat(&id, qhandle, ());
                if state.seat.is_none() {
                    let move_binding = id.get_pointer_binding(
                        0x110,
                        Modifiers::from_bits_truncate(64),
                        qhandle,
                        (),
                    );
                    let resize_binding = id.get_pointer_binding(
                        0x111,
                        Modifiers::from_bits_truncate(64),
                        qhandle,
                        (),
                    );
                    state.pointer_bindings.push(PointerBinding {
                        proxy: move_binding,
                        action: PointerAction::Move,
                        enabled: false,
                    });
                    state.pointer_bindings.push(PointerBinding {
                        proxy: resize_binding,
                        action: PointerAction::Resize(ResizeEdges {
                            right: true,
                            bottom: true,
                            ..Default::default()
                        }),
                        enabled: false,
                    });
                    state.seat = Some(Seat { proxy: id, ls_seat });
                } else {
                    log::error!(
                        "seat is already taken, reka does not support multi-seats at the moment"
                    );
                }
            }

            river::river_window_manager_v1::Event::Unavailable => {
                state.virtual_keyboard.reset();
                log::info!("RiverWindowManagerV1::Event::Unavailable received");
            }
            river::river_window_manager_v1::Event::Finished => {
                state.virtual_keyboard.reset();
                log::info!("RiverWindowManagerV1::Event::Finished received");
            }
            river::river_window_manager_v1::Event::SessionLocked => {
                state.virtual_keyboard.lock(true);
                log::info!("RiverWindowManagerV1::Event::SessionLocked received");
            }
            river::river_window_manager_v1::Event::SessionUnlocked => {
                state.virtual_keyboard.lock(false);
                log::info!("RiverWindowManagerV1::Event::SessionUnlocked received");
                // TODO: do we still need to recover focus here?
            }
            river::river_window_manager_v1::Event::Window { id } => {
                log::debug!("RiverWindowManagerV1::Event::Window received: id={:?}", id);
                // *not* creating the window object here to avoid Drop complications
            }
        }
    }

    wayland_client::event_created_child!(Reka, RiverWindowManagerV1, [
        river::river_window_manager_v1::EVT_WINDOW_OPCODE => (river::river_window_v1::RiverWindowV1, ()),
        river::river_window_manager_v1::EVT_OUTPUT_OPCODE => (river::river_output_v1::RiverOutputV1, ()),
        river::river_window_manager_v1::EVT_SEAT_OPCODE => (river::river_seat_v1::RiverSeatV1, ()),
    ]);
}

impl Dispatch<RiverOutputV1, ()> for Reka {
    fn event(
        state: &mut Self,
        proxy: &RiverOutputV1,
        event: <RiverOutputV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        log::debug!("RiverOutputV1 event received: {:?}", event);

        match event {
            river::river_output_v1::Event::Removed => {
                log::info!("an output was disconnected, removing");
                let Some(output) = state.outputs.remove(proxy) else {
                    log::warn!("unknown output disconnected");
                    return;
                };

                let Some(frame_name) = state
                    .frame_by_output(&output.proxy)
                    .and_then(|f| f.name.clone())
                else {
                    log::warn!("disconnected output had no frame");
                    return;
                };

                state.send(ToEmacs::DiscardFrame(frame_name));
            }
            river::river_output_v1::Event::Position { x, y } => {
                log::debug!("output position: x={}, y={}", x, y);
                if let Some(output) = state.outputs.get_mut(proxy) {
                    output.x = x;
                    output.y = y;
                }
            }
            river::river_output_v1::Event::Dimensions { width, height } => {
                log::debug!("output dimensions: {}x{}", width, height);
                if let Some(output) = state.outputs.get_mut(proxy) {
                    output.width = width;
                    output.height = height;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverLayerShellOutputV1, ()> for Reka {
    fn event(
        state: &mut Self,
        proxy: &RiverLayerShellOutputV1,
        event: <RiverLayerShellOutputV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            river::river_layer_shell_output_v1::Event::NonExclusiveArea {
                x,
                y,
                width,
                height,
            } => {
                let Some(output_proxy) = state
                    .outputs
                    .iter()
                    .find(|(_, o)| o.ls_output.eq(proxy))
                    .map(|(p, _)| p.clone())
                else {
                    log::warn!("could not find matching output for layer-shell output");
                    return;
                };

                if let Some(output) = state.outputs.get_mut(&output_proxy) {
                    output.x = x;
                    output.y = y;
                    output.width = width;
                    output.height = height;
                }
            }
        }
    }
}

impl Dispatch<river::river_seat_v1::RiverSeatV1, ()> for Reka {
    fn event(
        state: &mut Self,
        _proxy: &river::river_seat_v1::RiverSeatV1,
        event: <river::river_seat_v1::RiverSeatV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        log::debug!("RiverSeatV1 event received: {:?}", event);

        // Reka selects one primary seat. Secondary seat events must not
        // disturb that seat's focus, pointer operation or simulated keyboard.
        if state.seat.as_ref().is_none_or(|seat| seat.proxy != *_proxy) {
            return;
        }

        match &event {
            river::river_seat_v1::Event::WlSeat { name }
                if state
                    .seat
                    .as_ref()
                    .is_some_and(|seat| seat.proxy == *_proxy) =>
            {
                state.virtual_keyboard.set_seat(*name, qhandle);
            }
            river::river_seat_v1::Event::Removed
                if state
                    .seat
                    .as_ref()
                    .is_some_and(|seat| seat.proxy == *_proxy) =>
            {
                state.virtual_keyboard.remove_seat();
                state.seat.take();
            }
            _ => {}
        }
        match event {
            river::river_seat_v1::Event::WindowInteraction { window } => {
                if let Some(frame) = state.frames.get(&window) {
                    if let Some(name) = &frame.name {
                        state.send(ToEmacs::FrameFocused(name.clone()));
                    }
                    state.focus.focus_frame(window);
                    return;
                }
                let Some(frame) = state
                    .displaying_frame(&window)
                    .map(|frame| frame.proxy.clone())
                else {
                    return;
                };
                state.raise_window(&window);
                state.focus.focus_window(window, frame);
                state.focus.mark_dirty();
            }
            river::river_seat_v1::Event::PointerEnter { window } => {
                state.pointer_window = Some(window);
            }
            river::river_seat_v1::Event::PointerLeave => {
                state.pointer_window = None;
            }
            river::river_seat_v1::Event::PointerPosition { x, y } => {
                state.pointer_position = Some((x, y));
            }
            river::river_seat_v1::Event::OpDelta { dx, dy } => state.pointer_delta(dx, dy),
            river::river_seat_v1::Event::OpRelease => {
                if let Some(operation) = &mut state.pointer_operation {
                    operation.released = true;
                }
            }
            river::river_seat_v1::Event::Removed => {
                state.pointer_operation = None;
                state.pointer_window = None;
                state.pointer_position = None;
                state.pointer_bindings.clear();
                state.seat = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverWindowV1, ()> for Reka {
    fn event(
        state: &mut Self,
        proxy: &RiverWindowV1,
        event: <RiverWindowV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        log::debug!("RiverWindowV1 event received: {:?}", event);

        match event {
            river::river_window_v1::Event::UnreliablePid { unreliable_pid } => {
                let node = proxy.get_node(qhandle, ());

                if unreliable_pid == state.pid {
                    log::info!("discovered new Emacs frame ...");
                    state.frames.insert(
                        proxy.clone(),
                        Frame {
                            name: None,
                            displayed_on: None,
                            proxy: proxy.clone(),
                            node,
                        },
                    );

                    if state.pending_frames > 0 {
                        state.pending_frames -= 1;
                    } else {
                        // users might (accidentally?) create new frames
                        log::warn!("new frame was not requested by WM");
                    }
                    return;
                }

                log::info!("discovered new window (PID: {}) ...", unreliable_pid);
                state.stacking_serial += 1;
                state.windows.insert(
                    proxy.clone(),
                    Window {
                        proxy: proxy.clone(),
                        node,
                        state: WindowState::Starting,
                        params: None,
                        actual_width_height: None,
                        placement: Placement::default(),
                        parent: None,
                        hints: SizeHints::default(),
                        stacking_order: state.stacking_serial,
                        reported_geometry: None,
                        announced: false,
                        title: None,
                        app_id: None,
                    },
                );
            }
            river::river_window_v1::Event::AppId {
                app_id: Some(app_id),
            } => {
                if app_id.is_empty() {
                    return;
                }
                if let Some(window) = state.windows.get_mut(proxy) {
                    window.app_id = Some(app_id.clone());
                }
                state.send(ToEmacs::AppIDChange(proxy.clone(), app_id));
            }
            river::river_window_v1::Event::Title { title: Some(title) } => {
                if title.is_empty() {
                    return;
                }

                if title.starts_with("reka-frame-")
                    && let Some(f) = state.frames.get_mut(proxy)
                {
                    f.name = Some(title);
                    return;
                }
                if let Some(window) = state.windows.get_mut(proxy) {
                    window.title = Some(title.clone());
                }
                state.send(ToEmacs::TitleChange(proxy.clone(), title.clone()));

                if !state.windows.contains_key(proxy) {
                    log::warn!("received title for unknown window, orphan frame?");
                }
            }
            river::river_window_v1::Event::Parent { parent } => {
                let mut notify = false;
                if let Some(window) = state.windows.get_mut(proxy) {
                    if window.parent != parent {
                        if parent.is_none() && window.parent.is_some() {
                            window.placement.detach_parent(window.hints.fixed());
                        } else if window.placement.explicit.is_none() {
                            // Only automatic dialogs adopt a new centering reference.
                            window.placement.geometry = None;
                        }
                        window.parent = parent.clone();
                        notify = window.announced;
                    }
                    window.placement.automatic = window.parent.is_some() || window.hints.fixed();
                }
                if notify {
                    state.send(ToEmacs::ParentChanged(proxy.clone(), parent));
                }
            }
            river::river_window_v1::Event::DimensionsHint {
                min_width,
                min_height,
                max_width,
                max_height,
            } => {
                if let Some(window) = state.windows.get_mut(proxy) {
                    window.hints = SizeHints {
                        min_w: min_width,
                        min_h: min_height,
                        max_w: max_width,
                        max_h: max_height,
                    };
                    window.placement.automatic = window.parent.is_some() || window.hints.fixed();
                }
            }
            river::river_window_v1::Event::PointerMoveRequested { seat } => {
                state.begin_pointer_operation(proxy, seat, PointerAction::Move);
            }
            river::river_window_v1::Event::PointerResizeRequested {
                seat,
                edges: wayland_client::WEnum::Value(edges),
            } => {
                let bits = edges.bits();
                state.begin_pointer_operation(
                    proxy,
                    seat,
                    PointerAction::Resize(ResizeEdges {
                        top: bits & 1 != 0,
                        bottom: bits & 2 != 0,
                        left: bits & 4 != 0,
                        right: bits & 8 != 0,
                    }),
                );
            }
            river::river_window_v1::Event::Dimensions { width, height } => {
                if let Some(w) = state.windows.get_mut(proxy) {
                    w.actual_width_height = Some((width, height));
                }
            }
            river::river_window_v1::Event::Closed => {
                state.send(ToEmacs::WindowClosed(proxy.clone()));
                state.focus.invalidate(proxy);
                if state.pointer_window.as_ref() == Some(proxy) {
                    state.pointer_window = None;
                }
                let mut orphans = Vec::new();
                for window in state.windows.values_mut() {
                    if window.parent.as_ref() == Some(proxy) {
                        window.parent = None;
                        // Keep orphaned dialogs usable without moving or tiling them.
                        window.placement.detach_parent(window.hints.fixed());
                        orphans.push(window.proxy.clone());
                    }
                }
                for orphan in orphans {
                    state.send(ToEmacs::ParentChanged(orphan, None));
                }

                for output in state.outputs.values_mut() {
                    match &output.fullscreen {
                        FullscreenState::Requested { new: win, .. }
                        | FullscreenState::Fullscreen(win)
                        | FullscreenState::Exiting(win)
                            if proxy.eq(win) =>
                        {
                            output.fullscreen = FullscreenState::None;
                        }
                        _ => {}
                    }
                }

                if state.windows.remove(proxy).is_some() {
                    return;
                }

                if state.frames.remove(proxy).is_none() {
                    log::warn!("unknown window closed; reka bug?");
                }
            }
            river::river_window_v1::Event::MinimizeRequested => {
                state.send(ToEmacs::MinimizeRequested(proxy.clone()));
            }
            river::river_window_v1::Event::FullscreenRequested { output: target } => {
                // one of the denser pieces of logic here ... figure out which output to fullscreen on:
                // 1. the requested output
                // 2. the output the window is already on
                // 3. the active output
                let potential_target = target
                    .or_else(|| state.display_placement(proxy).map(|display| display.output))
                    .or_else(|| {
                        state
                            .focus
                            .current_focus_and_frame()
                            .and_then(|(_, fw)| state.frames.get(fw))
                            .and_then(|f| f.displayed_on.clone())
                    });
                let Some(target) = potential_target else {
                    log::warn!("fullscreen requested, but could not find matching output!");
                    return;
                };

                if let Some(output) = state.outputs.get_mut(&target) {
                    let previous = match &output.fullscreen {
                        FullscreenState::Fullscreen(window) | FullscreenState::Exiting(window) => {
                            Some(window.clone())
                        }
                        _ => None,
                    };

                    log::debug!("requesting fullscreen state");
                    output.fullscreen = FullscreenState::Requested {
                        new: proxy.clone(),
                        previous,
                    };
                }
            }

            river::river_window_v1::Event::ExitFullscreenRequested => {
                for output in state.outputs.values_mut() {
                    match &output.fullscreen {
                        FullscreenState::Requested { new: current, .. }
                        | FullscreenState::Fullscreen(current)
                            if current.eq(proxy) =>
                        {
                            log::debug!("exiting fullscreen as per request");
                            output.fullscreen = FullscreenState::Exiting(current.clone());
                            return;
                        }
                        _ => {}
                    };
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverXkbBindingV1, ()> for Reka {
    fn event(
        state: &mut Self,
        proxy: &RiverXkbBindingV1,
        event: <RiverXkbBindingV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        log::debug!("current known bindings: {:?}", state.prefixes);
        log::debug!("RiverXkbBindingV1 event received: {:?}", event);
        if matches!(event, river::river_xkb_binding_v1::Event::Pressed) {
            let Some((_, binding)) = state.binding_by_proxy(proxy) else {
                log::warn!("received keypress event for unknown binding, reka bug?");
                return;
            };

            match &binding.command {
                Command::Forward(event) => {
                    state.send(ToEmacs::KeyEvent(event.clone()));
                    state.focus.switch_to_frame();
                }
                Command::ToggleFloating => {
                    let target = state
                        .focus
                        .current_focus_and_frame()
                        .filter(|(window, frame)| window != frame)
                        .map(|(window, _)| window.clone());
                    if let Some(target) = target {
                        state.floating_command(&target, FloatingCommand::Toggle);
                    }
                }
                Command::ToggleFullscreen => {
                    let Some((focus, frame)) = state.focus.current_focus_and_frame() else {
                        log::warn!("fullscreen requested, but nothing is focused. reka bug?");
                        return;
                    };

                    if focus == frame {
                        state.send(ToEmacs::Message(
                            "can not fullscreen Emacs even more!".into(),
                        ));
                        return;
                    }

                    let Some(output) = &state.frames[frame].displayed_on else {
                        log::warn!("selected frame for fullscreen is not displayed. reka bug?");
                        return;
                    };

                    let new_state = match &state.outputs[output].fullscreen {
                        FullscreenState::None => FullscreenState::Requested {
                            new: focus.clone(),
                            previous: None,
                        },
                        FullscreenState::Fullscreen(current) => {
                            FullscreenState::Exiting(current.clone())
                        }
                        _ => {
                            log::warn!("invalid output state for fullscreen toggle");
                            return;
                        }
                    };

                    state.outputs.get_mut(output).unwrap().fullscreen = new_state;
                }
            }
        }
    }
}

impl Dispatch<river::river_pointer_binding_v1::RiverPointerBindingV1, ()> for Reka {
    fn event(
        state: &mut Self,
        proxy: &river::river_pointer_binding_v1::RiverPointerBindingV1,
        event: <river::river_pointer_binding_v1::RiverPointerBindingV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let river::river_pointer_binding_v1::Event::Pressed = event {
            let action = state
                .pointer_bindings
                .iter()
                .find(|binding| binding.proxy == *proxy)
                .map(|binding| binding.action);
            if let (Some(action), Some(window), Some(seat)) =
                (action, state.pointer_window.clone(), state.seat.as_ref())
            {
                let seat = seat.proxy.clone();
                state.begin_pointer_operation(&window, seat, action);
            }
        }
    }
}

dispatch_log_only!(river::river_node_v1::RiverNodeV1);
dispatch_log_only!(river::river_xkb_bindings_v1::RiverXkbBindingsV1);
dispatch_log_only!(river::river_xkb_bindings_seat_v1::RiverXkbBindingsSeatV1);
dispatch_log_only!(RiverLayerShellV1);
dispatch_log_only!(RiverLayerShellSeatV1); // TODO: focus_none? hmm
