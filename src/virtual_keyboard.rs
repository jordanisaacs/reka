//! Race-free keyboard simulation for the active River window manager.
//!
//! A batch has one explicit destination. River commits focus and synchronized
//! key events together, so changes in physical focus cannot redirect its keys.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use nix::fcntl::{FcntlArg, SealFlag, fcntl};
use nix::sys::memfd::{MFdFlags, memfd_create};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    protocol::{wl_registry, wl_seat},
};
use xkbcommon::xkb;

use crate::river::river_window_v1::RiverWindowV1;

mod protocol {
    pub extern crate wayland_client;
    pub use wayland_client::protocol::wl_seat;

    mod interfaces {
        pub(super) mod keyboard {
            pub use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!(
                "./protocol/upstream/virtual-keyboard-unstable-v1.xml"
            );
        }
        pub(super) mod river {
            use super::keyboard::*;
            wayland_scanner::generate_interfaces!("./protocol/river-virtual-keyboard-v1.xml");
        }
    }
    use interfaces::keyboard::*;
    use interfaces::river::*;
    wayland_scanner::generate_client_code!("./protocol/upstream/virtual-keyboard-unstable-v1.xml");
    wayland_scanner::generate_client_code!("./protocol/river-virtual-keyboard-v1.xml");
}

use protocol::{
    river_virtual_keyboard_manager_v1::RiverVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

pub(crate) const MAX_STROKES: usize = 64;
const MAX_QUEUED_BATCHES: usize = 16;
const MODIFIERS: [(u32, &str); 5] = [
    (1, "Shift"),
    (4, "Control"),
    (8, "Mod1"),
    (64, "Mod4"),
    (128, "Mod5"),
];

#[derive(Clone, Copy)]
pub(crate) struct Stroke {
    pub keysym: u32,
    pub modifiers: u32,
}

#[derive(Debug)]
struct MappedStroke {
    keycode: u32,
    modifiers: u32,
}

pub(crate) struct Batch {
    keymap: String,
    strokes: Vec<MappedStroke>,
}

impl Batch {
    pub(crate) fn prepare(strokes: &[Stroke]) -> Result<Self> {
        if strokes.is_empty() || strokes.len() > MAX_STROKES {
            bail!("simulation requires between 1 and {MAX_STROKES} keys");
        }
        let supported = MODIFIERS.iter().fold(0, |bits, (bit, _)| bits | bit);
        let mut symbols = Vec::new();
        for stroke in strokes {
            if stroke.keysym == 0 || stroke.keysym > 0x1fff_ffff {
                bail!("invalid simulation keysym");
            }
            if stroke.modifiers & !supported != 0 {
                bail!("unsupported simulation modifier bits");
            }
            if !symbols.contains(&stroke.keysym) {
                symbols.push(stroke.keysym);
            }
        }
        // The caller supplies final keysyms, independently of the physical
        // layout. Unicode and shifted letters therefore retain their meaning.
        let codes = symbols
            .iter()
            .enumerate()
            .map(|(index, _)| format!("<K{index:03}> = {};", index + 9))
            .collect::<String>();
        let keys = symbols
            .iter()
            .enumerate()
            .map(|(index, symbol)| {
                format!("key <K{index:03}> {{ type=\"ONE_LEVEL\", [0x{symbol:x}] }};")
            })
            .collect::<String>();
        let source = format!(
            r#"xkb_keymap {{
            xkb_keycodes "reka" {{ minimum=8; maximum=255; {codes} }};
            xkb_types "reka" {{ type "ONE_LEVEL" {{ modifiers=None; map[None]=Level1; }}; }};
            xkb_compatibility "reka" {{ }};
            xkb_symbols "reka" {{ {keys} }};
        }};"#
        );
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            source,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .context("could not construct simulation keymap")?;
        let mut mapped = Vec::with_capacity(strokes.len());
        for stroke in strokes {
            let keycode = symbols
                .iter()
                .position(|symbol| *symbol == stroke.keysym)
                .unwrap() as u32
                + 1;
            let mut modifiers = 0;
            for (bit, name) in MODIFIERS {
                if stroke.modifiers & bit != 0 {
                    let index = keymap.mod_get_index(name);
                    if index >= 32 {
                        bail!("simulation keymap lacks an expected modifier");
                    }
                    modifiers |= 1 << index;
                }
            }
            mapped.push(MappedStroke { keycode, modifiers });
        }
        Ok(Self {
            keymap: keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1),
            strokes: mapped,
        })
    }

    fn keymap_file(&self) -> Result<File> {
        let fd = memfd_create(
            c"reka-keymap",
            MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING,
        )?;
        let mut file = File::from(fd);
        file.write_all(self.keymap.as_bytes())?;
        file.write_all(&[0])?;
        fcntl(
            &file,
            FcntlArg::F_ADD_SEALS(
                SealFlag::F_SEAL_SHRINK
                    | SealFlag::F_SEAL_GROW
                    | SealFlag::F_SEAL_WRITE
                    | SealFlag::F_SEAL_SEAL,
            ),
        )?;
        Ok(file)
    }
}

pub(crate) struct Request {
    pub target: RiverWindowV1,
    pub batch: Batch,
}

// Existing WM debug logging must never print the simulated text/keysyms.
impl fmt::Debug for Request {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SimulateKeys")
            .field("keys", &self.batch.strokes.len())
            .finish_non_exhaustive()
    }
}

pub(crate) struct VirtualKeyboard {
    manager: Option<(u32, RiverVirtualKeyboardManagerV1)>,
    standard_manager: Option<(u32, ZwpVirtualKeyboardManagerV1)>,
    seats: HashMap<u32, wl_seat::WlSeat>,
    active_seat: Option<u32>,
    keyboard: Option<ZwpVirtualKeyboardV1>,
    pending: VecDeque<Request>,
    ready: Arc<AtomicBool>,
    locked: bool,
    clock: Instant,
}

impl VirtualKeyboard {
    pub(crate) fn new(ready: Arc<AtomicBool>) -> Self {
        Self {
            manager: None,
            standard_manager: None,
            seats: HashMap::new(),
            active_seat: None,
            keyboard: None,
            pending: VecDeque::new(),
            ready,
            locked: false,
            clock: Instant::now(),
        }
    }

    pub(crate) fn global(
        &mut self,
        registry: &wl_registry::WlRegistry,
        name: u32,
        interface: &str,
        version: u32,
        queue: &QueueHandle<crate::Reka>,
    ) {
        match interface {
            "river_virtual_keyboard_manager_v1" => {
                self.manager = Some((name, registry.bind(name, 1, queue, ())));
            }
            "zwp_virtual_keyboard_manager_v1" => {
                self.standard_manager = Some((name, registry.bind(name, 1, queue, ())));
            }
            "wl_seat" => {
                self.seats
                    .insert(name, registry.bind(name, version.min(7), queue, ()));
            }
            _ => return,
        }
        self.initialize(queue);
    }

    pub(crate) fn global_removed(&mut self, name: u32) {
        if self.active_seat == Some(name) {
            self.remove_seat();
        }
        if let Some(seat) = self.seats.remove(&name)
            && seat.version() >= 5
        {
            seat.release();
        }
        if self
            .manager
            .as_ref()
            .is_some_and(|(global, _)| *global == name)
        {
            self.clear_keyboard();
            if let Some((_, manager)) = self.manager.take() {
                manager.destroy();
            }
        }
        if self
            .standard_manager
            .as_ref()
            .is_some_and(|(global, _)| *global == name)
        {
            self.clear_keyboard();
            self.standard_manager.take();
        }
    }

    pub(crate) fn set_seat(&mut self, name: u32, queue: &QueueHandle<crate::Reka>) {
        if self.active_seat != Some(name) {
            self.remove_seat();
            self.active_seat = Some(name);
        }
        self.initialize(queue);
    }

    fn initialize(&mut self, queue: &QueueHandle<crate::Reka>) {
        if self.keyboard.is_none()
            && let (Some((_, manager)), Some((_, standard)), Some(seat)) = (
                &self.manager,
                &self.standard_manager,
                self.active_seat.and_then(|name| self.seats.get(&name)),
            )
        {
            let keyboard = standard.create_virtual_keyboard(seat, queue, ());
            // River requires synchronization BEFORE the first keymap request.
            manager.sync_keyboard(&keyboard);
            self.keyboard = Some(keyboard);
        }
        self.ready
            .store(self.keyboard.is_some() && !self.locked, Ordering::Release);
    }

    fn clear_keyboard(&mut self) {
        self.ready.store(false, Ordering::Release);
        self.pending.clear();
        if let Some(keyboard) = self.keyboard.take() {
            keyboard.destroy();
        }
    }

    pub(crate) fn restart(&mut self, queue: &QueueHandle<crate::Reka>) {
        self.clear_keyboard();
        self.initialize(queue);
    }

    pub(crate) fn remove_seat(&mut self) {
        self.clear_keyboard();
        self.active_seat = None;
    }

    pub(crate) fn reset(&mut self) {
        self.remove_seat();
        if let Some((_, manager)) = self.manager.take() {
            manager.destroy();
        }
        self.standard_manager.take();
    }

    pub(crate) fn lock(&mut self, locked: bool) {
        self.locked = locked;
        if locked {
            self.pending.clear();
        }
        self.ready
            .store(self.keyboard.is_some() && !locked, Ordering::Release);
    }

    pub(crate) fn enqueue(&mut self, request: Request) -> Result<()> {
        if !self.ready.load(Ordering::Acquire) {
            bail!(
                "keyboard simulation unavailable: requires River 0.5, an active seat and an unlocked session"
            );
        }
        if self.pending.len() >= MAX_QUEUED_BATCHES {
            bail!("too many pending keyboard simulations");
        }
        self.pending.push_back(request);
        Ok(())
    }

    pub(crate) fn next(&mut self) -> Option<Request> {
        if self.locked {
            return None;
        }
        self.pending.pop_front()
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) fn emit(&self, batch: Batch) -> Result<()> {
        let keyboard = self
            .keyboard
            .as_ref()
            .context("simulation seat was removed")?;
        let file = batch.keymap_file()?;
        keyboard.keymap(1, file.as_fd(), (batch.keymap.len() + 1).try_into()?);
        for stroke in batch.strokes {
            let time = self.clock.elapsed().as_millis() as u32;
            keyboard.modifiers(stroke.modifiers, 0, 0, 0);
            keyboard.key(time, stroke.keycode, 1);
            keyboard.key(time, stroke.keycode, 0);
            keyboard.modifiers(0, 0, 0, 0);
        }
        Ok(())
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        self.reset();
        for (_, seat) in self.seats.drain() {
            if seat.version() >= 5 {
                seat.release();
            }
        }
    }
}

macro_rules! no_events {
    ($proxy:ty) => {
        impl Dispatch<$proxy, ()> for crate::Reka {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: <$proxy as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}
no_events!(RiverVirtualKeyboardManagerV1);
no_events!(ZwpVirtualKeyboardManagerV1);
no_events!(ZwpVirtualKeyboardV1);
no_events!(wl_seat::WlSeat);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keymap_preserves_unicode_modifiers_and_evdev_offset() {
        let strokes = [
            Stroke {
                keysym: 0x61,
                modifiers: 4,
            },
            Stroke {
                keysym: 0x0101_f642,
                modifiers: 0,
            },
            Stroke {
                keysym: 0x61,
                modifiers: 1 | 8,
            },
        ];
        let batch = Batch::prepare(&strokes).unwrap();
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map = xkb::Keymap::new_from_string(
            &context,
            batch.keymap.clone(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        for (stroke, mapped) in strokes.iter().zip(batch.strokes.iter()) {
            assert_eq!(
                map.key_get_syms_by_level(xkb::Keycode::new(mapped.keycode + 8), 0, 0),
                &[xkb::Keysym::new(stroke.keysym)]
            );
            assert_eq!(mapped.modifiers, stroke.modifiers);
        }
        assert_eq!(batch.strokes[0].keycode, batch.strokes[2].keycode);
        let file = batch.keymap_file().unwrap();
        assert_eq!(
            file.metadata().unwrap().len(),
            batch.keymap.len() as u64 + 1
        );
        assert!(fcntl(&file, FcntlArg::F_GET_SEALS).unwrap() & SealFlag::F_SEAL_WRITE.bits() != 0);
    }

    #[test]
    fn malformed_and_unbounded_sequences_are_rejected() {
        assert!(Batch::prepare(&[]).is_err());
        assert!(
            Batch::prepare(&[Stroke {
                keysym: 0,
                modifiers: 0
            }])
            .is_err()
        );
        assert!(
            Batch::prepare(&[Stroke {
                keysym: 0x61,
                modifiers: 0x100
            }])
            .is_err()
        );
        assert!(
            Batch::prepare(&vec![
                Stroke {
                    keysym: 0x61,
                    modifiers: 0
                };
                MAX_STROKES + 1
            ])
            .is_err()
        );
    }
}
