//! Persistent virtual-pointer for stateful `mouse_button_down` / `mouse_drag` /
//! `mouse_button_up`.
//!
//! The non-persistent path in [`crate::platform::wayland::click`] opens its own
//! `ZwlrVirtualPointerV1`, presses, releases, drops the connection — useful
//! for one-shot clicks but useless for held-button drags: each tool call
//! emits a fresh device whose press/release pair is matched by the
//! compositor, so apps that distinguish a real drag (press, motion+, release)
//! from a series of clicks (press, release, press, release, …) miss the
//! drag entirely.
//!
//! This module keeps each gesture's virtual-pointer, connection, event queue,
//! and dispatch state together across batch steps in one CLI invocation. A
//! single owner thread per process owns `cursor_id -> ActivePointer`. It does
//! not survive the CLI process or provide cross-process held input. Commands use a
//! `crossbeam-channel`; replies come back on a per-call reply channel so the
//! caller blocks until the compositor has roundtripped.
//!
//! Lifecycle:
//! - First `press` for a cursor_id binds a fresh `ZwlrVirtualPointerV1`,
//!   optionally activates a foreign-toplevel target (desktop scope does not),
//!   presses the button, adds to the held-button set, roundtrips.
//! - Subsequent `move_to` calls emit `motion_absolute` on the same vptr (no
//!   activate — would steal focus mid-drag) and roundtrip.
//! - `release` emits a button release, removes from the held set; if the
//!   set is empty the vptr is destroyed and the map entry dropped.
//! - Connection and geometry failures return errors to the caller. The
//!   one-shot drag path releases its held button on both success and failure.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender};
use wayland_client::protocol::wl_pointer::ButtonState;

use super::{evdev_pointer_button, open_vptr_session};

/// One in-flight command from the public API to the owner thread.
enum Cmd {
    Press {
        cursor_id: String,
        window_id: Option<u64>,
        x: i32,
        y: i32,
        button: u8,
        reply: Sender<anyhow::Result<()>>,
    },
    MoveTo {
        cursor_id: String,
        x: i32,
        y: i32,
        reply: Sender<anyhow::Result<()>>,
    },
    Release {
        cursor_id: String,
        button: u8,
        reply: Sender<anyhow::Result<()>>,
    },
}

/// State held inside the owner thread for one cursor_id.
struct ActivePointer {
    session: super::VptrSession,
    /// evdev codes of buttons currently held down. When this set becomes
    /// empty the vptr is destroyed and the entry dropped from the map.
    held: HashSet<u32>,
    /// Output extent at session open time — needed for motion_absolute.
    out_w: u32,
    out_h: u32,
    /// Coordinate epoch in which the press was accepted. Zero denotes a
    /// non-Buzzard-OS compositor without generation metadata.
    geometry_generation: u64,
}

/// Process-global command channel into the owner thread. Lazily started on
/// first use.
static TX: OnceLock<Sender<Cmd>> = OnceLock::new();

fn tx() -> &'static Sender<Cmd> {
    TX.get_or_init(|| {
        let (tx, rx) = bounded::<Cmd>(32);
        thread::Builder::new()
            .name("cua-persistent-vptr".into())
            .spawn(move || owner_thread(rx))
            .expect("spawn cua-persistent-vptr thread");
        tx
    })
}

fn owner_thread(rx: Receiver<Cmd>) {
    let mut active: HashMap<String, ActivePointer> = HashMap::new();
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Press {
                cursor_id,
                window_id,
                x,
                y,
                button,
                reply,
            } => {
                let r = handle_press(&mut active, &cursor_id, window_id, x, y, button);
                let _ = reply.send(r);
            }
            Cmd::MoveTo {
                cursor_id,
                x,
                y,
                reply,
            } => {
                let r = handle_move(&mut active, &cursor_id, x, y);
                let _ = reply.send(r);
            }
            Cmd::Release {
                cursor_id,
                button,
                reply,
            } => {
                let r = handle_release(&mut active, &cursor_id, button);
                let _ = reply.send(r);
            }
        }
    }
}

fn handle_press(
    active: &mut HashMap<String, ActivePointer>,
    cursor_id: &str,
    window_id: Option<u64>,
    x: i32,
    y: i32,
    button: u8,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !active.contains_key(cursor_id),
        "cursor '{cursor_id}' already has a held button"
    );
    let output_before = super::read_buzzardos_output_state()?;
    // The same connection, dispatch queue, state and virtual pointer own the
    // entire gesture. Dropping the original queue discarded its input/output
    // events while the previous implementation drove a new, unrelated queue.
    let mut sess = open_vptr_session(window_id)?;
    let (w, h) = (sess.output_w, sess.output_h);
    let px = x.clamp(0, w as i32 - 1) as u32;
    let py = y.clamp(0, h as i32 - 1) as u32;
    let btn = evdev_pointer_button(button);

    sess.vptr
        .motion_absolute(super::event_time_ms(), px, py, w, h);
    sess.vptr.frame();
    sess.queue.roundtrip(&mut sess.state)?;
    // Match the ordinary click path: the compositor acknowledgement is not
    // the application's acknowledgement of the newly advertised pointer.
    // Let its event loop bind wl_pointer before delivering the first press.
    std::thread::sleep(std::time::Duration::from_millis(15));
    sess.vptr
        .button(super::event_time_ms(), btn, ButtonState::Pressed);
    sess.vptr.frame();
    sess.queue.roundtrip(&mut sess.state)?;

    let output_after = super::read_buzzardos_output_state()?;
    if let Err(error) = super::require_same_output_generation(output_before, output_after) {
        // Never leave a compositor grab behind when geometry changes between
        // the press coordinates and its acknowledgement.
        sess.vptr
            .button(super::event_time_ms(), btn, ButtonState::Released);
        sess.vptr.frame();
        let _ = sess.queue.roundtrip(&mut sess.state);
        sess.vptr.destroy();
        return Err(error);
    }

    let mut held = HashSet::new();
    held.insert(btn);
    active.insert(
        cursor_id.to_string(),
        ActivePointer {
            session: sess,
            held,
            out_w: w,
            out_h: h,
            geometry_generation: output_before
                .map(|state| state.geometry_generation)
                .unwrap_or(0),
        },
    );
    Ok(())
}

fn handle_move(
    active: &mut HashMap<String, ActivePointer>,
    cursor_id: &str,
    x: i32,
    y: i32,
) -> anyhow::Result<()> {
    ensure_active_generation(active, cursor_id)?;
    let entry = active.get_mut(cursor_id).ok_or_else(|| {
        anyhow::anyhow!(
            "no held mouse button for cursor '{cursor_id}'; call mouse_button_down first"
        )
    })?;
    let px = x.clamp(0, entry.out_w as i32 - 1) as u32;
    let py = y.clamp(0, entry.out_h as i32 - 1) as u32;
    entry
        .session
        .vptr
        .motion_absolute(super::event_time_ms(), px, py, entry.out_w, entry.out_h);
    entry.session.vptr.frame();
    entry.session.queue.roundtrip(&mut entry.session.state)?;
    ensure_active_generation(active, cursor_id)
}

fn handle_release(
    active: &mut HashMap<String, ActivePointer>,
    cursor_id: &str,
    button: u8,
) -> anyhow::Result<()> {
    ensure_active_generation(active, cursor_id)?;
    let output_before = super::read_buzzardos_output_state()?;
    let btn = evdev_pointer_button(button);
    let drop_entry = {
        let entry = active
            .get_mut(cursor_id)
            .ok_or_else(|| anyhow::anyhow!("no held mouse button for cursor '{cursor_id}'"))?;
        entry
            .session
            .vptr
            .button(super::event_time_ms(), btn, ButtonState::Released);
        entry.session.vptr.frame();
        entry.session.queue.roundtrip(&mut entry.session.state)?;
        entry.held.remove(&btn);
        entry.held.is_empty()
    };
    if drop_entry {
        if let Some(mut p) = active.remove(cursor_id) {
            p.session.vptr.destroy();
            p.session.queue.roundtrip(&mut p.session.state)?;
        }
    }
    let output_after = super::read_buzzardos_output_state()?;
    super::require_same_output_generation(output_before, output_after)
}

/// If a held press belongs to an older coordinate epoch, emit releases for
/// every held button before rejecting the next operation. This makes a
/// cross-call drag fail closed without stranding Sway's seat grab.
fn ensure_active_generation(
    active: &mut HashMap<String, ActivePointer>,
    cursor_id: &str,
) -> anyhow::Result<()> {
    let Some(expected) = active
        .get(cursor_id)
        .map(|pointer| pointer.geometry_generation)
    else {
        return Ok(());
    };
    let current = super::read_buzzardos_output_state()
        .map(|state| state.map(|state| state.geometry_generation).unwrap_or(0));
    if current.as_ref().is_ok_and(|current| *current == expected) {
        return Ok(());
    }

    if let Some(mut pointer) = active.remove(cursor_id) {
        for button in &pointer.held {
            pointer
                .session
                .vptr
                .button(super::event_time_ms(), *button, ButtonState::Released);
        }
        pointer.session.vptr.frame();
        let _ = pointer.session.queue.roundtrip(&mut pointer.session.state);
        pointer.session.vptr.destroy();
        let _ = pointer.session.queue.roundtrip(&mut pointer.session.state);
    }
    match current {
        Ok(current) => anyhow::bail!(
            "stale_output_geometry: held pointer generation {expected} does not match current generation {current}; compositor buttons were released"
        ),
        Err(error) => Err(error),
    }
}

// ── public API ────────────────────────────────────────────────────────────

/// Press and HOLD `button` (evdev code) at output coordinates `(x, y)` on the
/// toplevel identified by `window_id`. Subsequent `move_to` / `release` calls
/// targeting the same `cursor_id` reuse the same virtual-pointer device, so
/// the compositor treats the sequence as one logical drag rather than as
/// independent clicks. Errors if `cursor_id` already has a held button.
pub fn press(cursor_id: &str, window_id: u64, x: i32, y: i32, button: u8) -> anyhow::Result<()> {
    let (tx_r, rx_r) = bounded(1);
    tx().send(Cmd::Press {
        cursor_id: cursor_id.to_string(),
        window_id: Some(window_id),
        x,
        y,
        button,
        reply: tx_r,
    })
    .map_err(|e| anyhow::anyhow!("cua-persistent-vptr thread is dead: {e}"))?;
    rx_r.recv()
        .map_err(|e| anyhow::anyhow!("reply channel closed: {e}"))?
}

/// Press and hold at canonical guest-output coordinates without activating a
/// named toplevel. This is the stateful peer of `click_desktop` and keeps the
/// same virtual-pointer device alive for `move_to` and `release`.
pub fn press_desktop(cursor_id: &str, x: i32, y: i32, button: u8) -> anyhow::Result<()> {
    let (tx_r, rx_r) = bounded(1);
    tx().send(Cmd::Press {
        cursor_id: cursor_id.to_string(),
        window_id: None,
        x,
        y,
        button,
        reply: tx_r,
    })
    .map_err(|e| anyhow::anyhow!("cua-persistent-vptr thread is dead: {e}"))?;
    rx_r.recv()
        .map_err(|e| anyhow::anyhow!("reply channel closed: {e}"))?
}

/// Emit motion_absolute on the held cursor's virtual-pointer. Errors if there
/// is no held button for `cursor_id`.
pub fn move_to(cursor_id: &str, x: i32, y: i32) -> anyhow::Result<()> {
    let (tx_r, rx_r) = bounded(1);
    tx().send(Cmd::MoveTo {
        cursor_id: cursor_id.to_string(),
        x,
        y,
        reply: tx_r,
    })
    .map_err(|e| anyhow::anyhow!("cua-persistent-vptr thread is dead: {e}"))?;
    rx_r.recv()
        .map_err(|e| anyhow::anyhow!("reply channel closed: {e}"))?
}

/// Release `button` on the held cursor. If no other buttons remain held the
/// virtual-pointer is destroyed and its Wayland connection torn down.
pub fn release(cursor_id: &str, button: u8) -> anyhow::Result<()> {
    let (tx_r, rx_r) = bounded(1);
    tx().send(Cmd::Release {
        cursor_id: cursor_id.to_string(),
        button,
        reply: tx_r,
    })
    .map_err(|e| anyhow::anyhow!("cua-persistent-vptr thread is dead: {e}"))?;
    rx_r.recv()
        .map_err(|e| anyhow::anyhow!("reply channel closed: {e}"))?
}
