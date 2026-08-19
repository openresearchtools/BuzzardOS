//! Stable physical-key injection for wlroots virtual-keyboard compositors.

use std::ffi::CString;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{bounded, Receiver, Sender};
use wayland_client::protocol::{wl_callback, wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use xkbcommon::xkb;

use self::protocol::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use self::protocol::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;

mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocol/virtual-keyboard-unstable-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocol/virtual-keyboard-unstable-v1.xml");
}

const XKB_KEYMAP: &str = r#"xkb_keymap {
 xkb_keycodes { include "evdev+aliases(qwerty)" };
 xkb_types { include "complete" };
 xkb_compatibility { include "complete" };
 xkb_symbols { include "pc+us+inet(evdev)" };
 xkb_geometry { include "pc(pc105)" };
};
"#;

const KEY_RELEASED: u32 = 0;
const KEY_PRESSED: u32 = 1;
const TEXT_KEYCODE_OFFSET: u32 = 1;
// XKB keycodes are 8..=255. The generated mapping starts at XKB keycode 9,
// leaving at most 247? No: 255 - 9 + 1 = 247 entries would make the rendered
// `maximum` 256 because wtype deliberately includes one unused upper slot.
// Match that representation and cap each keymap at 246 entries.
const MAX_TEXT_KEYMAP_ENTRIES: usize = 246;
const ROUNDTRIP_TIMEOUT: Duration = Duration::from_millis(500);
const CANCELLATION_POLL_SLICE: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
pub(super) enum VirtualKeyboardError {
    #[error("persistent virtual keyboard is unsupported: {0}")]
    Unsupported(&'static str),
    #[error("CUA keyboard operation was cancelled by session teardown")]
    Cancelled,
    #[error("CUA keyboard delivery became ambiguous: {0}")]
    DeliveryAmbiguous(&'static str),
}

/// Serializes complete keyboard transactions, including the interval during
/// which modifiers are held for a pointer gesture. The Wayland object lives on
/// its own thread because `EventQueue` is not `Send`.
static OPERATION_LOCK: Mutex<()> = Mutex::new(());
static TX: OnceLock<Sender<Cmd>> = OnceLock::new();
static SHUTDOWN_EPOCH: AtomicU64 = AtomicU64::new(1);
static ACTIVE_OPERATION: Mutex<Option<Admission>> = Mutex::new(None);
static SESSION_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

/// Runtime-private session admission captured synchronously at a Tool's
/// `invoke` boundary, before consent/focus/other awaits. A recycled public
/// label gets a new runtime-private key from core; an explicitly restarted
/// exact key gets the next generation after its prior EndSession.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admission {
    leases: Vec<crate::core::session::SessionLease>,
    shutdown_epoch: u64,
}

pub(super) fn initialize() {
    SESSION_HOOK_INSTALLED.get_or_init(|| {
        crate::core::session::register_session_end_hook(end_session);
    });
    // Create only the command owner, not a Wayland connection. Publishing an
    // ACTIVE operation is then never observable without a reset-capable owner.
    let _ = tx();
}

pub(super) fn admit(
    trusted_leases: Vec<crate::core::session::SessionLease>,
) -> anyhow::Result<Admission> {
    initialize();
    if trusted_leases
        .iter()
        .any(|lease| !crate::core::session::session_lease_is_current(lease))
    {
        return Err(VirtualKeyboardError::Cancelled.into());
    }
    Ok(Admission {
        leases: trusted_leases,
        shutdown_epoch: SHUTDOWN_EPOCH.load(Ordering::Acquire),
    })
}

enum Cmd {
    Sequence {
        transitions: Vec<KeyTransition>,
        admission: Admission,
        reply: Sender<anyhow::Result<()>>,
    },
    HoldModifiers {
        modifiers: Vec<(u32, u32)>,
        admission: Admission,
        reply: Sender<anyhow::Result<()>>,
    },
    TypeText {
        text: String,
        delay_ms: u64,
        trailing_keysym: Option<String>,
        admission: Admission,
        reply: Sender<anyhow::Result<()>>,
    },
    Reset {
        reply: Sender<anyhow::Result<()>>,
    },
}

#[derive(Default)]
struct State {
    seat: Option<WlSeat>,
    manager: Option<ZwpVirtualKeyboardManagerV1>,
    completed_sync: u64,
}

impl Dispatch<wl_callback::WlCallback, u64> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        generation: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.completed_sync = state.completed_sync.max(*generation);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == WlSeat::interface().name {
                state.seat = Some(registry.bind(name, version.min(7), qh, ()));
            } else if interface == ZwpVirtualKeyboardManagerV1::interface().name {
                state.manager = Some(registry.bind(name, version.min(1), qh, ()));
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub(super) fn hotkey(admission: &Admission, modifiers: &[String], key: &str) -> anyhow::Result<()> {
    let transitions = hotkey_transitions(modifiers, key)?;
    let _operation = OperationGuard::begin(admission)?;
    request_sequence(transitions, admission.clone())
}

/// Press one evdev-mapped synthetic key through the same virtual-keyboard
/// object used for working chords. A separate `wtype` process can successfully
/// bind the protocol yet lose keyboard focus on a nested Sway seat, so named
/// keys such as Enter use this compositor-native route.
pub(super) fn press_key(admission: &Admission, key: &str) -> anyhow::Result<()> {
    if key
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_uppercase())
        && key.chars().count() == 1
    {
        return hotkey(admission, &["shift".to_owned()], &key.to_ascii_lowercase());
    }
    hotkey(admission, &[], key)
}

/// Type Unicode text without replacing the daemon-owned virtual keyboard.
///
/// The generated keymap follows wtype's one-key-per-keysym representation, so
/// every scalar value supported by the pinned libxkbcommon has the same input
/// semantics while the Wayland object remains alive between tool calls.
pub(super) fn type_text(admission: &Admission, text: &str, delay_ms: u64) -> anyhow::Result<()> {
    request_text(admission, text, delay_ms, None)
}

/// Type Unicode text and one named key in the same persistent-keyboard
/// transaction. This preserves the old single-client ordering required by
/// nested wlroots seats without destroying the active input device afterward.
pub(super) fn type_text_then_key(
    admission: &Admission,
    text: &str,
    delay_ms: u64,
    trailing_keysym: &str,
) -> anyhow::Result<()> {
    request_text(admission, text, delay_ms, Some(trailing_keysym))
}

fn request_text(
    admission: &Admission,
    text: &str,
    delay_ms: u64,
    trailing_keysym: Option<&str>,
) -> anyhow::Result<()> {
    if text.is_empty() && trailing_keysym.is_none() {
        return Ok(());
    }
    validate_text_length(text)?;
    let _operation = OperationGuard::begin(admission)?;
    let (reply, receive) = bounded(1);
    tx().send(Cmd::TypeText {
        text: text.to_owned(),
        delay_ms,
        trailing_keysym: trailing_keysym.map(str::to_owned),
        admission: admission.clone(),
        reply,
    })
    .map_err(|error| anyhow::anyhow!("CUA virtual-keyboard worker stopped: {error}"))?;
    receive
        .recv()
        .map_err(|error| anyhow::anyhow!("CUA virtual-keyboard reply closed: {error}"))?
}

fn validate_text_length(text: &str) -> anyhow::Result<usize> {
    let count = text.chars().count();
    if count > crate::contract::MAX_TYPE_TEXT_CHARS {
        anyhow::bail!(
            "type_text contains {count} characters; limit is {}",
            crate::contract::MAX_TYPE_TEXT_CHARS
        );
    }
    Ok(count)
}

/// Keep the requested modifier keys depressed while another input client
/// performs a pointer gesture on the same seat. Wayland modifier state is
/// seat-global, so the separately bound virtual pointer observes this state
/// for the complete press-motion-release transaction.
pub(super) fn with_modifiers_held<T>(
    admission: &Admission,
    modifiers: &[String],
    action: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if modifiers.is_empty() {
        return action();
    }

    let modifier_keys = modifier_keys(modifiers)?;
    let _operation = OperationGuard::begin(admission)?;
    let mut held = HeldModifiers::begin(modifier_keys, admission.clone())?;
    let action_result = action();
    let admission_result = ensure_admitted(admission);
    // Always neutralize the seat before propagating an action error. The guard
    // repeats this best-effort on unwind, so a panic cannot strand Ctrl/Shift.
    let release_result = held.release();
    let value = action_result?;
    admission_result?;
    release_result?;
    Ok(value)
}

struct HeldModifiers {
    active: bool,
}

impl HeldModifiers {
    fn begin(modifiers: Vec<(u32, u32)>, admission: Admission) -> anyhow::Result<Self> {
        let (reply, receive) = bounded(1);
        tx().send(Cmd::HoldModifiers {
            modifiers,
            admission,
            reply,
        })
        .map_err(|error| anyhow::anyhow!("CUA virtual-keyboard worker stopped: {error}"))?;
        receive
            .recv()
            .map_err(|error| anyhow::anyhow!("CUA virtual-keyboard reply closed: {error}"))??;
        Ok(Self { active: true })
    }

    fn release(&mut self) -> anyhow::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        request_reset()
    }
}

impl Drop for HeldModifiers {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            let _ = request_reset();
        }
    }
}

fn request_sequence(transitions: Vec<KeyTransition>, admission: Admission) -> anyhow::Result<()> {
    let (reply, receive) = bounded(1);
    tx().send(Cmd::Sequence {
        transitions,
        admission,
        reply,
    })
    .map_err(|error| anyhow::anyhow!("CUA virtual-keyboard worker stopped: {error}"))?;
    receive
        .recv()
        .map_err(|error| anyhow::anyhow!("CUA virtual-keyboard reply closed: {error}"))?
}

fn request_reset() -> anyhow::Result<()> {
    let (reply, receive) = bounded(1);
    tx().send(Cmd::Reset { reply })
        .map_err(|error| anyhow::anyhow!("CUA virtual-keyboard worker stopped: {error}"))?;
    receive
        .recv()
        .map_err(|error| anyhow::anyhow!("CUA virtual-keyboard reply closed: {error}"))?
}

struct OperationGuard {
    admission: Admission,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl OperationGuard {
    fn begin(admission: &Admission) -> anyhow::Result<Self> {
        // The admission itself was captured by Tool::invoke before any await.
        // Revalidate after waiting for the global keyboard lane so a queued
        // operation cannot start after its own EndSession.
        let lock = OPERATION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_admitted(admission)?;
        let mut active = ACTIVE_OPERATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active = Some(admission.clone());
        // Core tombstones and advances the lease before calling our hook.
        // Rechecking after publication closes validate→ACTIVE: either the hook
        // observes us or this path clears itself without delivering a key.
        if ensure_admitted(admission).is_err() {
            *active = None;
            return Err(VirtualKeyboardError::Cancelled.into());
        }
        drop(active);
        Ok(Self {
            admission: admission.clone(),
            _lock: lock,
        })
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let mut active = ACTIVE_OPERATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.as_ref() == Some(&self.admission) {
            *active = None;
        }
    }
}

fn end_session(session: &str) {
    let active = ACTIVE_OPERATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let active_generation = active
        .as_ref()
        .into_iter()
        .flat_map(|active| &active.leases)
        .filter(|lease| lease.session_id() == session)
        .map(crate::core::session::SessionLease::generation)
        .next();
    drop(active);
    let Some(generation) = active_generation else {
        return;
    };

    const DEADLINE: Duration = Duration::from_secs(2);
    let started = Instant::now();
    let Some(reset_tx) = TX.get() else {
        fail_stop_session_teardown(
            session,
            generation,
            "keyboard owner was absent while its operation was active",
        );
    };
    let (reply, receive) = bounded(1);
    if reset_tx
        .send_timeout(
            Cmd::Reset { reply },
            DEADLINE.saturating_sub(started.elapsed()),
        )
        .is_err()
    {
        fail_stop_session_teardown(session, generation, "neutral reset could not be queued");
    }
    match receive.recv_timeout(DEADLINE.saturating_sub(started.elapsed())) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => fail_stop_session_teardown(
            session,
            generation,
            &format!("neutral reset failed: {error:#}"),
        ),
        Err(error) => fail_stop_session_teardown(
            session,
            generation,
            &format!("neutral reset was not acknowledged: {error}"),
        ),
    }
}

fn fail_stop_session_teardown(session: &str, generation: u64, reason: &str) -> ! {
    tracing::error!(
        session_generation = generation,
        "CUA keyboard fail-stop during session teardown: {reason}; session identifier omitted"
    );
    let _ = session;
    std::process::abort()
}

/// Synchronously neutralize the process-global keyboard during one SDK/runtime
/// shutdown. The owner remains reusable because multiple SDK instances can be
/// constructed sequentially in the same daemon process. Process termination
/// remains the final same-device wlroots release boundary.
pub(super) fn shutdown() -> anyhow::Result<()> {
    const DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
    let Some(shutdown_tx) = TX.get() else {
        return Ok(());
    };
    let started = std::time::Instant::now();
    SHUTDOWN_EPOCH.fetch_add(1, Ordering::AcqRel);
    let remaining = DEADLINE.saturating_sub(started.elapsed());
    let (reply, receive) = bounded(1);
    shutdown_tx
        .send_timeout(Cmd::Reset { reply }, remaining)
        .map_err(|error| anyhow::anyhow!("could not queue CUA keyboard shutdown: {error}"))?;
    receive
        .recv_timeout(DEADLINE.saturating_sub(started.elapsed()))
        .map_err(|error| anyhow::anyhow!("CUA keyboard shutdown was not acknowledged: {error}"))?
}

fn tx() -> &'static Sender<Cmd> {
    TX.get_or_init(|| {
        let (tx, receive) = crossbeam_channel::bounded(32);
        thread::Builder::new()
            .name("cua-virtual-keyboard".into())
            .spawn(move || owner_thread(receive))
            .expect("spawn CUA virtual-keyboard owner thread");
        tx
    })
}

fn owner_thread(receive: Receiver<Cmd>) {
    let mut worker = KeyboardWorker::default();
    while let Ok(command) = receive.recv() {
        let (result, reply) = match command {
            Cmd::Sequence {
                transitions,
                admission,
                reply,
            } => (
                worker.run(|session| session.sequence(&transitions, &admission)),
                Some(reply),
            ),
            Cmd::HoldModifiers {
                modifiers,
                admission,
                reply,
            } => (
                worker.run(|session| session.hold_modifiers(&modifiers, &admission)),
                Some(reply),
            ),
            Cmd::TypeText {
                text,
                delay_ms,
                trailing_keysym,
                admission,
                reply,
            } => (
                worker.run(|session| {
                    session.type_text(&text, delay_ms, trailing_keysym.as_deref(), &admission)
                }),
                Some(reply),
            ),
            Cmd::Reset { reply } => (worker.reset(), Some(reply)),
        };

        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    }
    let _ = worker.shutdown();
}

#[derive(Default)]
struct KeyboardWorker {
    session: Option<KeyboardSession>,
    recovery_required: bool,
    cancelled_teardown_unproven: bool,
}

impl KeyboardWorker {
    fn ensure_session(&mut self) -> anyhow::Result<()> {
        if self.recovery_required {
            return self.recover_neutral();
        }
        if self.session.is_none() {
            self.session = Some(KeyboardSession::connect()?);
        }
        Ok(())
    }

    fn run(
        &mut self,
        operation: impl FnOnce(&mut KeyboardSession) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.ensure_session()?;
        let result = operation(self.session.as_mut().expect("keyboard session initialized"));
        if result.is_err() {
            if result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<VirtualKeyboardError>())
                .is_some_and(|error| matches!(error, VirtualKeyboardError::Cancelled))
            {
                // Prove neutral on the same Wayland client. A sync on a newly
                // connected client cannot order old-client key requests, so a
                // failed same-client barrier makes EndSession fail-stop rather
                // than accepting a merely neutral replacement keyboard.
                let same_client_neutral = self
                    .session
                    .as_mut()
                    .is_some_and(|session| session.restore_fixed_neutral().is_ok());
                if same_client_neutral {
                    self.recovery_required = false;
                } else {
                    self.session.take();
                    self.recovery_required = true;
                    self.cancelled_teardown_unproven = true;
                }
                return result;
            }
            // A failed roundtrip can mean the compositor consumed a key-down
            // but not its matching release. First retry cleanup on the same
            // device. If that connection is unusable, drop it: pinned wlroots'
            // wlr_keyboard_finish emits releases for every pressed key on that
            // same compositor-side keyboard before Sway removes the device.
            // Never replay a press on a replacement keyboard: Sway would treat
            // it as new input and could execute a binding or insert text.
            self.recovery_required = true;
            let same_device_recovered = self
                .session
                .as_mut()
                .is_some_and(|session| session.restore_fixed_neutral().is_ok());
            if same_device_recovered {
                self.recovery_required = false;
            } else {
                self.session.take();
                let _ = self.recover_neutral();
            }
        }
        result
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        if self.cancelled_teardown_unproven {
            return Err(anyhow::anyhow!(
                "cancelled CUA keyboard could not prove same-client neutral delivery"
            ));
        }
        if self.session.is_none() && !self.recovery_required {
            // This process-global owner is eagerly created before tool
            // admission, but the Wayland keyboard itself remains lazy. With
            // no device ever created there is no compositor state to reset.
            return Ok(());
        }
        if self.recovery_required {
            return self.recover_neutral();
        }
        let result = self
            .session
            .as_mut()
            .expect("checked above")
            .restore_fixed_neutral();
        if let Err(original) = result {
            self.recovery_required = true;
            self.session.take();
            return match self.recover_neutral() {
                Ok(()) => Err(anyhow::anyhow!(
                    "resetting the CUA keyboard failed ({original:#}); a replacement keyboard is neutral, but cross-client ordering cannot prove teardown"
                )),
                Err(recovery) => Err(anyhow::anyhow!(
                    "resetting the CUA keyboard failed ({original:#}); neutral reconnect also failed ({recovery:#})"
                )),
            };
        }
        Ok(())
    }

    fn recover_neutral(&mut self) -> anyhow::Result<()> {
        self.recovery_required = true;
        self.session.take();
        let mut session = KeyboardSession::connect()?;
        let recovery = session.restore_fixed_neutral();
        if let Err(error) = recovery {
            self.recovery_required = true;
            drop(session);
            return Err(error).context("restoring a neutral CUA keyboard after disconnect");
        }
        self.session = Some(session);
        self.recovery_required = false;
        self.cancelled_teardown_unproven = false;
        Ok(())
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        let result = if let Some(session) = self.session.as_mut() {
            session.restore_fixed_neutral()
        } else {
            Ok(())
        };
        self.session.take();
        if result.is_err() {
            // Dropping the failed connection is itself the pinned wlroots
            // compositor's same-device release path.
            self.recovery_required = true;
        }
        result
    }
}

#[derive(Debug, Default)]
struct PressedState {
    pressed: Vec<u32>,
    modifier_mask: u32,
}

impl PressedState {
    fn record(&mut self, transition: KeyTransition) {
        if transition.pressed {
            if !self.pressed.contains(&transition.keycode) {
                self.pressed.push(transition.keycode);
            }
        } else if let Some(index) = self
            .pressed
            .iter()
            .rposition(|keycode| *keycode == transition.keycode)
        {
            self.pressed.remove(index);
        }
        self.modifier_mask = transition.modifier_mask;
    }

    fn cleanup_transitions(&self) -> Vec<KeyTransition> {
        self.pressed
            .iter()
            .rev()
            .copied()
            .map(|keycode| KeyTransition {
                keycode,
                pressed: false,
                modifier_mask: 0,
            })
            .collect()
    }
}

struct KeyboardSession {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    keyboard: ZwpVirtualKeyboardV1,
    pressed: PressedState,
    active_keymap: String,
    next_sync: u64,
}

impl KeyboardSession {
    fn connect() -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env()?;
        let mut queue = conn.new_event_queue::<State>();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        bounded_roundtrip(&conn, &mut queue, &qh, &mut state, None, 1)?;
        bounded_roundtrip(&conn, &mut queue, &qh, &mut state, None, 2)?;

        let seat = state.seat.clone().ok_or(VirtualKeyboardError::Unsupported(
            "compositor exposes no wl_seat",
        ))?;
        let manager = state
            .manager
            .clone()
            .ok_or(VirtualKeyboardError::Unsupported(
                "compositor exposes no zwp_virtual_keyboard_manager_v1",
            ))?;
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        let keymap = keymap_file(XKB_KEYMAP)?;
        keyboard.keymap(1, keymap.as_fd(), XKB_KEYMAP.len() as u32 + 1);
        bounded_roundtrip(&conn, &mut queue, &qh, &mut state, None, 3)?;

        Ok(Self {
            conn,
            queue,
            qh,
            state,
            keyboard,
            pressed: PressedState::default(),
            active_keymap: XKB_KEYMAP.to_owned(),
            next_sync: 4,
        })
    }

    fn sequence(
        &mut self,
        transitions: &[KeyTransition],
        admission: &Admission,
    ) -> anyhow::Result<()> {
        ensure_admitted(admission)?;
        self.reset(Some(admission))?;
        for transition in transitions {
            ensure_admitted(admission)?;
            self.emit(*transition, Some(admission))?;
        }
        self.reset(Some(admission))
    }

    fn hold_modifiers(
        &mut self,
        modifiers: &[(u32, u32)],
        admission: &Admission,
    ) -> anyhow::Result<()> {
        ensure_admitted(admission)?;
        self.reset(Some(admission))?;
        let mut modifier_mask = 0;
        for (keycode, mask) in modifiers {
            ensure_admitted(admission)?;
            modifier_mask |= mask;
            self.emit(
                KeyTransition {
                    keycode: *keycode,
                    pressed: true,
                    modifier_mask,
                },
                Some(admission),
            )?;
        }
        Ok(())
    }

    fn type_text(
        &mut self,
        text: &str,
        delay_ms: u64,
        trailing_keysym: Option<&str>,
        admission: &Admission,
    ) -> anyhow::Result<()> {
        ensure_admitted(admission)?;
        let plans = TextKeymap::build_chunks(text, trailing_keysym)?;
        self.reset(Some(admission))?;

        let operation: anyhow::Result<()> = (|| {
            for plan in plans {
                ensure_admitted(admission)?;
                self.install_keymap(&plan.keymap, Some(admission))?;
                for keycode in plan.text_keycodes {
                    ensure_admitted(admission)?;
                    self.emit(
                        KeyTransition {
                            keycode,
                            pressed: true,
                            modifier_mask: 0,
                        },
                        Some(admission),
                    )?;
                    self.emit(
                        KeyTransition {
                            keycode,
                            pressed: false,
                            modifier_mask: 0,
                        },
                        Some(admission),
                    )?;
                    if delay_ms > 0 {
                        cancellable_delay(delay_ms, admission)?;
                    }
                }
                if let Some(keycode) = plan.trailing_keycode {
                    cancellable_delay(50, admission)?;
                    self.emit(
                        KeyTransition {
                            keycode,
                            pressed: true,
                            modifier_mask: 0,
                        },
                        Some(admission),
                    )?;
                    self.emit(
                        KeyTransition {
                            keycode,
                            pressed: false,
                            modifier_mask: 0,
                        },
                        Some(admission),
                    )?;
                }
            }
            Ok(())
        })();

        // Always try to release the dynamic key and restore the fixed evdev
        // keymap before returning. A failure in either step marks the worker
        // dirty, tears down this object, and triggers neutral reconnect.
        let reset = self.reset(Some(admission));
        let restore = if reset.is_ok() {
            Some(self.install_keymap(XKB_KEYMAP, Some(admission)))
        } else {
            None
        };
        let final_reset = restore
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .map(|()| self.reset(Some(admission)));
        operation?;
        reset?;
        if let Some(result) = restore {
            result?;
        }
        if let Some(result) = final_reset {
            result?;
        }
        Ok(())
    }

    fn install_keymap(
        &mut self,
        keymap_text: &str,
        admission: Option<&Admission>,
    ) -> anyhow::Result<()> {
        let keymap = keymap_file(keymap_text)?;
        let size = u32::try_from(keymap_text.len().saturating_add(1))
            .context("CUA keymap is too large for the Wayland protocol")?;
        self.keyboard.keymap(1, keymap.as_fd(), size);
        self.roundtrip(admission)?;
        self.active_keymap.clear();
        self.active_keymap.push_str(keymap_text);
        Ok(())
    }

    fn restore_fixed_neutral(&mut self) -> anyhow::Result<()> {
        self.reset(None)?;
        // Install unconditionally. If the preceding dynamic-keymap sync was
        // interrupted after the compositor consumed its request, the local
        // `active_keymap` value is necessarily uncertain. Only a same-client
        // fixed-keymap request plus sync can prove the compositor-side map.
        self.install_keymap(XKB_KEYMAP, None)?;
        self.reset(None)
    }

    fn emit(
        &mut self,
        transition: KeyTransition,
        admission: Option<&Admission>,
    ) -> anyhow::Result<()> {
        // Record a press before the request is flushed. If the roundtrip fails
        // after the compositor consumed it, Drop still knows to release it.
        // Conversely, retain a key until its release is acknowledged, so a
        // failed release remains in the best-effort teardown set.
        if transition.pressed {
            self.pressed.record(transition);
        }
        self.keyboard.key(
            super::event_time_ms(),
            transition.keycode,
            if transition.pressed {
                KEY_PRESSED
            } else {
                KEY_RELEASED
            },
        );
        self.keyboard.modifiers(transition.modifier_mask, 0, 0, 0);
        self.roundtrip(admission)?;
        if !transition.pressed {
            self.pressed.record(transition);
        }
        std::thread::sleep(std::time::Duration::from_millis(4));
        Ok(())
    }

    fn reset(&mut self, admission: Option<&Admission>) -> anyhow::Result<()> {
        // Continue through the complete release set before reporting the first
        // failure. In particular, one failed release must not strand a second
        // modifier. The final all-zero modifier message also restores a clean
        // seat before control returns to the caller.
        let cleanup = self.pressed.cleanup_transitions();
        let mut first_error = None;
        for transition in cleanup {
            if let Err(error) = self.emit(transition, admission) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        self.pressed.modifier_mask = 0;
        self.keyboard.modifiers(0, 0, 0, 0);
        if let Err(error) = self.roundtrip(admission) {
            if first_error.is_none() {
                first_error = Some(error.into());
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn roundtrip(&mut self, admission: Option<&Admission>) -> anyhow::Result<()> {
        let generation = self.next_sync;
        self.next_sync = self.next_sync.wrapping_add(1).max(1);
        bounded_roundtrip(
            &self.conn,
            &mut self.queue,
            &self.qh,
            &mut self.state,
            admission,
            generation,
        )
    }
}

fn bounded_roundtrip(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    qh: &QueueHandle<State>,
    state: &mut State,
    admission: Option<&Admission>,
    generation: u64,
) -> anyhow::Result<()> {
    conn.display().sync(qh, generation);
    let deadline = Instant::now() + ROUNDTRIP_TIMEOUT;
    loop {
        queue
            .dispatch_pending(state)
            .context("dispatching CUA keyboard Wayland events")?;
        if state.completed_sync >= generation {
            return Ok(());
        }
        if let Some(admission) = admission {
            ensure_admitted(admission)?;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(VirtualKeyboardError::DeliveryAmbiguous(
                "wl_display.sync deadline expired",
            )
            .into());
        }

        let flush_would_block = match queue.flush() {
            Ok(()) => false,
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                true
            }
            Err(error) => return Err(error).context("flushing CUA keyboard Wayland requests"),
        };

        let Some(read_guard) = queue.prepare_read() else {
            continue;
        };
        let wait = remaining.min(CANCELLATION_POLL_SLICE);
        let timeout_ms = i32::try_from(wait.as_millis().max(1)).unwrap_or(i32::MAX);
        let mut descriptor = libc::pollfd {
            fd: read_guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN
                | libc::POLLERR
                | libc::POLLHUP
                | if flush_would_block { libc::POLLOUT } else { 0 },
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            drop(read_guard);
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("polling CUA keyboard Wayland connection");
        }
        if ready == 0 || descriptor.revents & libc::POLLIN == 0 {
            drop(read_guard);
            continue;
        }
        match read_guard.read() {
            Ok(_) => {}
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).context("reading CUA keyboard Wayland events"),
        }
    }
}

fn ensure_admitted(admission: &Admission) -> anyhow::Result<()> {
    if admission.shutdown_epoch != SHUTDOWN_EPOCH.load(Ordering::Acquire) {
        return Err(VirtualKeyboardError::Cancelled.into());
    }
    if admission
        .leases
        .iter()
        .any(|lease| !crate::core::session::session_lease_is_current(lease))
    {
        return Err(VirtualKeyboardError::Cancelled.into());
    }
    Ok(())
}

fn cancellable_delay(delay_ms: u64, admission: &Admission) -> anyhow::Result<()> {
    let mut remaining = std::time::Duration::from_millis(delay_ms);
    while !remaining.is_zero() {
        ensure_admitted(admission)?;
        let slice = remaining.min(CANCELLATION_POLL_SLICE);
        std::thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
    ensure_admitted(admission)
}

pub(super) fn ensure_current(admission: &Admission) -> anyhow::Result<()> {
    ensure_admitted(admission)
}

pub(super) fn sleep_cancellable(admission: &Admission, duration: Duration) -> anyhow::Result<()> {
    let mut remaining = duration;
    while !remaining.is_zero() {
        ensure_admitted(admission)?;
        let slice = remaining.min(CANCELLATION_POLL_SLICE);
        std::thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
    ensure_admitted(admission)
}

impl Drop for KeyboardSession {
    fn drop(&mut self) {
        // Best effort is deliberate in Drop: explicit callers receive reset
        // failures, while unwind/disconnect still sends every release it can.
        for transition in self.pressed.cleanup_transitions() {
            self.keyboard
                .key(super::event_time_ms(), transition.keycode, KEY_RELEASED);
        }
        self.keyboard.modifiers(0, 0, 0, 0);
        self.keyboard.destroy();
        let _ = self.conn.flush();
    }
}

#[derive(Clone, Copy)]
struct TextKeymapEntry {
    character: Option<char>,
    keysym: xkb::Keysym,
}

struct TextKeymap {
    keymap: String,
    text_keycodes: Vec<u32>,
    trailing_keycode: Option<u32>,
}

impl TextKeymap {
    fn build_chunks(text: &str, trailing_keysym: Option<&str>) -> anyhow::Result<Vec<Self>> {
        validate_text_length(text)?;
        let reserve_for_trailing = usize::from(trailing_keysym.is_some());
        let chunk_entry_limit = MAX_TEXT_KEYMAP_ENTRIES - reserve_for_trailing;
        let mut chunks = Vec::new();
        let mut chunk = String::new();
        let mut distinct = Vec::<char>::new();

        for character in text.chars() {
            let is_new = !distinct.contains(&character);
            if is_new && distinct.len() == chunk_entry_limit {
                chunks.push(Self::build(&chunk, None)?);
                chunk.clear();
                distinct.clear();
            }
            if !distinct.contains(&character) {
                distinct.push(character);
            }
            chunk.push(character);
        }

        if !chunk.is_empty() || trailing_keysym.is_some() {
            chunks.push(Self::build(&chunk, trailing_keysym)?);
        }
        if chunks.is_empty() {
            anyhow::bail!("persistent text transaction contains no keys");
        }
        Ok(chunks)
    }

    fn build(text: &str, trailing_keysym: Option<&str>) -> anyhow::Result<Self> {
        let mut entries = Vec::<TextKeymapEntry>::new();
        let mut text_keycodes = Vec::with_capacity(text.chars().count());
        for character in text.chars() {
            let keycode = match entries
                .iter()
                .position(|entry| entry.character == Some(character))
            {
                Some(index) => protocol_keycode(index)?,
                None => {
                    let keysym = keysym_for_character(character);
                    entries.push(TextKeymapEntry {
                        character: Some(character),
                        keysym,
                    });
                    protocol_keycode(entries.len() - 1)?
                }
            };
            text_keycodes.push(keycode);
        }

        let trailing_keycode = trailing_keysym
            .map(|name| {
                let keysym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
                if keysym.raw() == 0 {
                    anyhow::bail!("invalid trailing XKB keysym '{name}'");
                }
                match entries.iter().position(|entry| entry.keysym == keysym) {
                    Some(index) => protocol_keycode(index),
                    None => {
                        entries.push(TextKeymapEntry {
                            character: None,
                            keysym,
                        });
                        protocol_keycode(entries.len() - 1)
                    }
                }
            })
            .transpose()?;

        if entries.is_empty() {
            anyhow::bail!("persistent text transaction contains no keys");
        }
        if entries.len() > MAX_TEXT_KEYMAP_ENTRIES {
            anyhow::bail!(
                "CUA text keymap has {} entries; per-keymap limit is {MAX_TEXT_KEYMAP_ENTRIES}",
                entries.len()
            );
        }
        Ok(Self {
            keymap: render_text_keymap(&entries)?,
            text_keycodes,
            trailing_keycode,
        })
    }
}

fn protocol_keycode(index: usize) -> anyhow::Result<u32> {
    u32::try_from(index)
        .ok()
        .and_then(|value| value.checked_add(TEXT_KEYCODE_OFFSET))
        .ok_or_else(|| anyhow::anyhow!("CUA text keymap has too many distinct keysyms"))
}

fn keysym_for_character(character: char) -> xkb::Keysym {
    let named = match character {
        '\n' => Some("Return"),
        '\t' => Some("Tab"),
        '\u{1b}' => Some("Escape"),
        _ => None,
    };
    named.map_or_else(
        || xkb::utf32_to_keysym(character as u32),
        |name| xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS),
    )
}

fn render_text_keymap(entries: &[TextKeymapEntry]) -> anyhow::Result<String> {
    use std::fmt::Write as _;

    let maximum = entries
        .len()
        .checked_add(9)
        .context("CUA text keymap size overflow")?;
    let mut keymap = String::new();
    writeln!(keymap, "xkb_keymap {{")?;
    writeln!(keymap, "xkb_keycodes \"(unnamed)\" {{")?;
    writeln!(keymap, "minimum = 8;")?;
    writeln!(keymap, "maximum = {maximum};")?;
    for index in 0..entries.len() {
        let ordinal = index + 1;
        let xkb_keycode = index + 9;
        writeln!(keymap, "<K{ordinal}> = {xkb_keycode};")?;
    }
    writeln!(keymap, "}};")?;
    writeln!(
        keymap,
        "xkb_types \"(unnamed)\" {{ include \"complete\" }};"
    )?;
    writeln!(
        keymap,
        "xkb_compatibility \"(unnamed)\" {{ include \"complete\" }};"
    )?;
    writeln!(keymap, "xkb_symbols \"(unnamed)\" {{")?;
    for (index, entry) in entries.iter().enumerate() {
        let ordinal = index + 1;
        let name = xkb::keysym_get_name(entry.keysym);
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'+' | b'-'))
        {
            anyhow::bail!("libxkbcommon returned an unsafe keysym name");
        }
        writeln!(keymap, "key <K{ordinal}> {{[{name}]}};")?;
    }
    writeln!(keymap, "}};")?;
    writeln!(keymap, "}};")?;
    Ok(keymap)
}

fn keymap_file(keymap: &str) -> anyhow::Result<File> {
    let name = CString::new("cua-driver-keymap")?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(keymap.as_bytes())?;
    file.write_all(&[0])?;
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

fn key_to_evdev(key: &str) -> Option<u32> {
    Some(match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => 28,
        "tab" => 15,
        "esc" | "escape" => 1,
        "space" => 57,
        "backspace" => 14,
        "delete" | "del" => 111,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        "home" => 102,
        "end" => 107,
        "pageup" | "page_up" => 104,
        "pagedown" | "page_down" => 109,
        "a" => 30,
        "b" => 48,
        "c" => 46,
        "d" => 32,
        "e" => 18,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "i" => 23,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        "m" => 50,
        "n" => 49,
        "o" => 24,
        "p" => 25,
        "q" => 16,
        "r" => 19,
        "s" => 31,
        "t" => 20,
        "u" => 22,
        "v" => 47,
        "w" => 17,
        "x" => 45,
        "y" => 21,
        "z" => 44,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyTransition {
    keycode: u32,
    pressed: bool,
    modifier_mask: u32,
}

fn hotkey_transitions(modifiers: &[String], key: &str) -> anyhow::Result<Vec<KeyTransition>> {
    let mut modifier_mask = 0;
    let modifier_keys = modifier_keys(modifiers)?;
    let keycode = key_to_evdev(key)
        .ok_or_else(|| anyhow::anyhow!("no evdev keycode mapping for key '{key}'"))?;
    let mut transitions = Vec::with_capacity(modifier_keys.len() * 2 + 2);
    for (modifier_key, mask) in &modifier_keys {
        modifier_mask |= mask;
        transitions.push(KeyTransition {
            keycode: *modifier_key,
            pressed: true,
            modifier_mask,
        });
    }
    transitions.push(KeyTransition {
        keycode,
        pressed: true,
        modifier_mask,
    });
    transitions.push(KeyTransition {
        keycode,
        pressed: false,
        modifier_mask,
    });
    for (modifier_key, mask) in modifier_keys.iter().rev() {
        modifier_mask &= !mask;
        transitions.push(KeyTransition {
            keycode: *modifier_key,
            pressed: false,
            modifier_mask,
        });
    }
    Ok(transitions)
}

fn modifier_keys(modifiers: &[String]) -> anyhow::Result<Vec<(u32, u32)>> {
    let mut keys = Vec::with_capacity(modifiers.len());
    for modifier in modifiers {
        let key = match modifier.as_str() {
            "ctrl" => (29, 4),
            "shift" => (42, 1),
            "alt" => (56, 8),
            "logo" => (125, 64),
            other => anyhow::bail!("unsupported Wayland modifier '{other}'"),
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_reset(state: &mut PressedState) {
        for transition in state.cleanup_transitions() {
            state.record(transition);
        }
        state.modifier_mask = 0;
    }

    fn assert_neutral_for_parent_backspace(state: &PressedState) {
        assert!(state.pressed.is_empty(), "CUA still owns pressed keys");
        assert_eq!(state.modifier_mask, 0, "CUA still owns modifiers");
        // The gateway's nested parent keyboard is a distinct Sway input
        // device. Its evdev Backspace event must remain an ordinary unmodified
        // 14 down/up pair after CUA relinquishes the seat.
        assert_eq!(
            hotkey_transitions(&[], "backspace").unwrap(),
            [
                KeyTransition {
                    keycode: 14,
                    pressed: true,
                    modifier_mask: 0,
                },
                KeyTransition {
                    keycode: 14,
                    pressed: false,
                    modifier_mask: 0,
                },
            ]
        );
    }

    #[test]
    fn chord_uses_evdev_digit7_and_balanced_modifiers() {
        assert_eq!(
            hotkey_transitions(&["ctrl".into(), "shift".into()], "7").unwrap(),
            [
                KeyTransition {
                    keycode: 29,
                    pressed: true,
                    modifier_mask: 4
                },
                KeyTransition {
                    keycode: 42,
                    pressed: true,
                    modifier_mask: 5
                },
                KeyTransition {
                    keycode: 8,
                    pressed: true,
                    modifier_mask: 5
                },
                KeyTransition {
                    keycode: 8,
                    pressed: false,
                    modifier_mask: 5
                },
                KeyTransition {
                    keycode: 42,
                    pressed: false,
                    modifier_mask: 4
                },
                KeyTransition {
                    keycode: 29,
                    pressed: false,
                    modifier_mask: 0
                },
            ]
        );
    }

    #[test]
    fn unmodified_enter_has_balanced_evdev_transitions() {
        assert_eq!(
            hotkey_transitions(&[], "enter").unwrap(),
            [
                KeyTransition {
                    keycode: 28,
                    pressed: true,
                    modifier_mask: 0
                },
                KeyTransition {
                    keycode: 28,
                    pressed: false,
                    modifier_mask: 0
                },
            ]
        );
    }

    #[test]
    fn interrupted_ctrl_l_cleanup_releases_l_and_ctrl_then_zeros_modifiers() {
        let transitions = hotkey_transitions(&["ctrl".into()], "l").unwrap();
        let mut pressed = PressedState::default();
        pressed.record(transitions[0]); // Ctrl down.
        pressed.record(transitions[1]); // L down; interruption before releases.

        assert_eq!(
            pressed.cleanup_transitions(),
            [
                KeyTransition {
                    keycode: 38,
                    pressed: false,
                    modifier_mask: 0,
                },
                KeyTransition {
                    keycode: 29,
                    pressed: false,
                    modifier_mask: 0,
                },
            ]
        );
    }

    #[test]
    fn ctrl_l_text_enter_then_parent_backspace_starts_from_neutral_state() {
        let mut pressed = PressedState::default();
        for transition in hotkey_transitions(&["ctrl".into()], "l").unwrap() {
            pressed.record(transition);
        }
        // Text and Enter use the same persistent CUA keyboard and finish as
        // balanced transactions before parent input resumes.
        for transition in hotkey_transitions(&[], "enter").unwrap() {
            pressed.record(transition);
        }
        assert_neutral_for_parent_backspace(&pressed);
    }

    #[test]
    fn interrupted_ctrl_l_reset_then_parent_backspace_is_neutral() {
        let transitions = hotkey_transitions(&["ctrl".into()], "l").unwrap();
        let mut pressed = PressedState::default();
        pressed.record(transitions[0]);
        pressed.record(transitions[1]);
        apply_reset(&mut pressed);
        assert_neutral_for_parent_backspace(&pressed);
    }

    #[test]
    fn session_end_reset_then_parent_backspace_is_neutral() {
        let mut pressed = PressedState::default();
        pressed.record(KeyTransition {
            keycode: 29,
            pressed: true,
            modifier_mask: 4,
        });
        apply_reset(&mut pressed);
        assert_neutral_for_parent_backspace(&pressed);
    }

    #[test]
    fn disconnected_after_key_down_uses_same_device_destroy_releases_only() {
        #[derive(Default)]
        struct ClientModel {
            pressed: Vec<u32>,
            modifier_mask: u32,
        }

        impl ClientModel {
            fn apply(&mut self, transition: KeyTransition) {
                if transition.pressed {
                    if !self.pressed.contains(&transition.keycode) {
                        self.pressed.push(transition.keycode);
                    }
                } else {
                    self.pressed
                        .retain(|keycode| *keycode != transition.keycode);
                }
                self.modifier_mask = transition.modifier_mask;
            }
        }

        let ctrl_l = hotkey_transitions(&["ctrl".into()], "l").unwrap();
        let mut failed_device = PressedState::default();
        let mut client = ClientModel::default();
        for transition in &ctrl_l[..2] {
            failed_device.record(*transition);
            client.apply(*transition);
        }

        // The failed operation can leave both keys client-visible until the
        // compositor observes the dead virtual-keyboard connection.
        assert_eq!(client.pressed, [29, 38]);
        assert_eq!(client.modifier_mask, 4);

        // Pinned wlroots wlr_keyboard_finish walks its compositor-side pressed
        // set and emits releases on the *same* keyboard before the Sway device
        // listener is removed. Model those events. In particular there is no
        // replacement-keyboard press, which could trigger a binding or insert
        // a character.
        let destroy_events = failed_device.cleanup_transitions();
        assert!(destroy_events.iter().all(|transition| !transition.pressed));
        for transition in destroy_events {
            client.apply(transition);
        }
        assert!(client.pressed.is_empty());
        assert_eq!(client.modifier_mask, 0);
    }

    #[test]
    fn persistent_text_keymap_compiles_and_reuses_unicode_keys() {
        let plan = TextKeymap::build("é😀\n\té", Some("Return")).unwrap();
        assert_eq!(plan.text_keycodes[0], plan.text_keycodes[4]);
        assert!(plan.trailing_keycode.is_some());
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        assert!(xkb::Keymap::new_from_string(
            &context,
            plan.keymap,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .is_some());
    }

    #[test]
    fn persistent_text_chunks_more_than_xkb_keycode_space_on_one_owner() {
        let text: String = (0x100..0x100 + 300).filter_map(char::from_u32).collect();
        assert_eq!(text.chars().count(), 300);
        let plans = TextKeymap::build_chunks(&text, Some("Return")).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(
            plans
                .iter()
                .map(|plan| plan.text_keycodes.len())
                .sum::<usize>(),
            300
        );
        assert!(plans[..plans.len() - 1]
            .iter()
            .all(|plan| plan.trailing_keycode.is_none()));
        assert!(plans.last().unwrap().trailing_keycode.is_some());

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        for plan in plans {
            assert!(xkb::Keymap::new_from_string(
                &context,
                plan.keymap,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .is_some());
        }
    }

    #[test]
    fn persistent_text_rejects_oversized_requests_before_wayland() {
        let text = "x".repeat(crate::contract::MAX_TYPE_TEXT_CHARS + 1);
        let error = match TextKeymap::build_chunks(&text, None) {
            Ok(_) => panic!("oversized text unexpectedly passed validation"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("limit is"));
    }

    #[test]
    fn queued_waiter_with_core_lease_cannot_run_after_session_end() {
        let session = "__cua_runtime_keyboard-test:queued-waiter";
        let lease = crate::core::session::capture_session_lease(session).unwrap();
        let admission = admit(vec![lease]).unwrap();
        assert!(ensure_admitted(&admission).is_ok());
        assert!(crate::core::session::fire_session_end(session));
        assert!(ensure_admitted(&admission)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        crate::core::session::forget_ended_sessions_with_prefix(session);
    }

    #[test]
    fn huge_text_delay_is_cancelled_before_sleeping() {
        let session = "__cua_runtime_keyboard-test:huge-delay";
        let lease = crate::core::session::capture_session_lease(session).unwrap();
        let admission = admit(vec![lease]).unwrap();
        assert!(crate::core::session::fire_session_end(session));
        let started = std::time::Instant::now();
        let error = cancellable_delay(u64::MAX, &admission).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        crate::core::session::forget_ended_sessions_with_prefix(session);
    }

    #[test]
    fn recovery_contains_no_sacrificial_semantic_key_primer() {
        let source = include_str!("virtual_keyboard.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("fn neutral_primer"));
        assert!(!production.contains("fn prime(&mut self)"));
    }

    #[test]
    fn duplicate_modifiers_do_not_create_unbalanced_duplicate_presses() {
        assert_eq!(
            modifier_keys(&["ctrl".into(), "ctrl".into(), "shift".into()]).unwrap(),
            [(29, 4), (42, 1)]
        );
    }
}
