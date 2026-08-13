// SPDX-License-Identifier: AGPL-3.0-or-later

//! Typed, asynchronous control of the guest-private PipeWire-Pulse graph.
//!
//! The PulseAudio mainloop and every audio stream live on one dedicated
//! worker thread. GTK only submits bounded commands and reads cloned state;
//! it never blocks on the sound server. Merely starting this service performs
//! introspection and installs subscriptions. A recording stream is created
//! only by [`SoundController::start_microphone_test`] and is disconnected on
//! stop, timeout, backend failure, or service drop.

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::introspect::{
    ServerInfo, SinkInfo, SinkInputInfo, SourceInfo, SourceOutputInfo,
};
use pulse::context::subscribe::InterestMaskSet;
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::def::{BufferAttr, SinkState, SourceState};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::proplist::{Proplist, properties};
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet as StreamFlagSet, PeekResult, State as StreamState, Stream};
use pulse::volume::{ChannelVolumes, Volume, VolumeLinear};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use thiserror::Error;

const COMMAND_QUEUE_CAPACITY: usize = 64;
const WORKER_TICK: Duration = Duration::from_millis(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DEVICES: usize = 256;
const MAX_STREAMS: usize = 2_048;
const MAX_COMPLETED_SNAPSHOTS: usize = 4;
const MAX_DIAGNOSTICS: usize = 16;
const MAX_PENDING_CALLBACK_OPERATIONS: usize = 128;
const MAX_PENDING_REFRESH_OPERATIONS: usize = 128;
const MAX_SERVER_NAME_BYTES: usize = 512;
const MAX_DISPLAY_TEXT_BYTES: usize = 512;
const CALLBACK_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const TEST_RATE: u32 = 48_000;
const SPEAKER_TEST_HARD_LIMIT: Duration = Duration::from_secs(8);
const MICROPHONE_TEST_HARD_LIMIT: Duration = Duration::from_secs(30);
const MICROPHONE_FRAGMENT_BYTES: u32 = 9_600;
const MAX_MICROPHONE_FRAGMENTS_PER_TICK: usize = 4;
const APPLICATION_NAME: &str = "Buzzard OS Settings";
const APPLICATION_ID: &str = "org.openresearchtools.WildBuzzard.Settings1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundConnection {
    Connecting,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceActivity {
    Running,
    Idle,
    Suspended,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId {
    index: u32,
    name: String,
}

impl DeviceId {
    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SoundDevice {
    pub id: DeviceId,
    pub description: String,
    pub volume_raw: u32,
    pub volume_percent: f64,
    pub channels: u8,
    pub muted: bool,
    pub activity: DeviceActivity,
    pub active_port: Option<String>,
    pub monitor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SoundStreamId(pub u32);

#[derive(Debug, Clone, PartialEq)]
pub struct SoundStreamInfo {
    pub id: SoundStreamId,
    pub name: String,
    pub application_name: Option<String>,
    pub route_device_index: u32,
    pub volume_percent: Option<f64>,
    pub muted: bool,
    pub corked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MicrophoneLevel {
    /// Root-mean-square sample amplitude in the range 0.0 through 1.0.
    pub rms: f64,
    /// Root-mean-square amplitude in dBFS, floored at -96 dBFS.
    pub dbfs: f64,
    /// Accessible meter position, mapping -60 dBFS through 0 dBFS to 0..1.
    pub meter_fraction: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundTestState {
    Idle,
    Starting,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundOperationKind {
    Refresh,
    SetDefaultOutput,
    SetOutputVolume,
    SetOutputMute,
    SetDefaultInput,
    SetInputVolume,
    SetInputMute,
    StartSpeakerTest,
    StopSpeakerTest,
    StartMicrophoneTest,
    StopMicrophoneTest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundOperationStatus {
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SoundRequestId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundOperationFeedback {
    pub request_id: SoundRequestId,
    pub operation: SoundOperationKind,
    pub status: SoundOperationStatus,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SoundState {
    pub generation: u64,
    pub connection: SoundConnection,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    pub default_output_name: Option<String>,
    pub default_input_name: Option<String>,
    pub outputs: Vec<SoundDevice>,
    pub inputs: Vec<SoundDevice>,
    pub playback_streams: Vec<SoundStreamInfo>,
    pub recording_streams: Vec<SoundStreamInfo>,
    pub subscription_active: bool,
    pub speaker_test: SoundTestState,
    pub microphone_test: SoundTestState,
    pub microphone_level: Option<MicrophoneLevel>,
    pub last_operation: Option<SoundOperationFeedback>,
    pub diagnostic: Option<String>,
}

impl Default for SoundState {
    fn default() -> Self {
        Self {
            generation: 0,
            connection: SoundConnection::Connecting,
            server_name: None,
            server_version: None,
            default_output_name: None,
            default_input_name: None,
            outputs: Vec::new(),
            inputs: Vec::new(),
            playback_streams: Vec::new(),
            recording_streams: Vec::new(),
            subscription_active: false,
            speaker_test: SoundTestState::Idle,
            microphone_test: SoundTestState::Idle,
            microphone_level: None,
            last_operation: None,
            diagnostic: Some("Connecting to the guest-private PipeWire-Pulse service.".into()),
        }
    }
}

impl SoundState {
    pub fn default_output(&self) -> Option<&SoundDevice> {
        let name = self.default_output_name.as_deref()?;
        self.outputs.iter().find(|device| device.id.name == name)
    }

    pub fn default_input(&self) -> Option<&SoundDevice> {
        let name = self.default_input_name.as_deref()?;
        self.inputs.iter().find(|device| device.id.name == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserVolumePercent(u16);

impl UserVolumePercent {
    pub const MAX: u16 = 150;

    pub fn new(percent: u16) -> Result<Self, SoundClientError> {
        if percent <= Self::MAX {
            Ok(Self(percent))
        } else {
            Err(SoundClientError::InvalidVolume(percent))
        }
    }

    pub fn get(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SoundClientError {
    #[error("volume {0}% is outside the supported 0% through 150% range")]
    InvalidVolume(u16),
    #[error("the sound request counter is exhausted")]
    RequestCounterExhausted,
    #[error("the sound backend is busy; try again after its current requests complete")]
    QueueFull,
    #[error("the sound backend has stopped")]
    BackendStopped,
}

#[derive(Debug, Clone)]
pub struct SoundController {
    sender: SyncSender<Command>,
    state: Arc<Mutex<SoundState>>,
    next_request: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
}

impl SoundController {
    pub fn state(&self) -> SoundState {
        lock_state(&self.state).clone()
    }

    pub fn refresh(&self) -> Result<SoundRequestId, SoundClientError> {
        self.submit(SoundOperationKind::Refresh, Action::Refresh)
    }

    pub fn set_default_output(
        &self,
        device: &DeviceId,
    ) -> Result<SoundRequestId, SoundClientError> {
        self.submit(
            SoundOperationKind::SetDefaultOutput,
            Action::SetDefaultOutput(device.clone()),
        )
    }

    pub fn set_output_volume(
        &self,
        device: &DeviceId,
        volume: UserVolumePercent,
    ) -> Result<SoundRequestId, SoundClientError> {
        self.submit(
            SoundOperationKind::SetOutputVolume,
            Action::SetOutputVolume(device.clone(), volume),
        )
    }

    pub fn set_output_mute(
        &self,
        device: &DeviceId,
        muted: bool,
    ) -> Result<SoundRequestId, SoundClientError> {
        self.submit(
            SoundOperationKind::SetOutputMute,
            Action::SetOutputMute(device.clone(), muted),
        )
    }

    pub fn set_default_input(&self, device: &DeviceId) -> Result<SoundRequestId, SoundClientError> {
        self.submit(
            SoundOperationKind::SetDefaultInput,
            Action::SetDefaultInput(device.clone()),
        )
    }

    pub fn set_input_volume(
        &self,
        device: &DeviceId,
        volume: UserVolumePercent,
    ) -> Result<SoundRequestId, SoundClientError> {
        self.submit(
            SoundOperationKind::SetInputVolume,
            Action::SetInputVolume(device.clone(), volume),
        )
    }

    pub fn set_input_mute(
        &self,
        device: &DeviceId,
        muted: bool,
    ) -> Result<SoundRequestId, SoundClientError> {
        self.submit(
            SoundOperationKind::SetInputMute,
            Action::SetInputMute(device.clone(), muted),
        )
    }

    pub fn start_speaker_test(
        &self,
        device: Option<&DeviceId>,
    ) -> Result<SoundRequestId, SoundClientError> {
        self.submit(
            SoundOperationKind::StartSpeakerTest,
            Action::StartSpeakerTest(device.cloned()),
        )
    }

    pub fn stop_speaker_test(&self) -> Result<SoundRequestId, SoundClientError> {
        self.submit(SoundOperationKind::StopSpeakerTest, Action::StopSpeakerTest)
    }

    pub fn start_microphone_test(
        &self,
        device: Option<&DeviceId>,
    ) -> Result<SoundRequestId, SoundClientError> {
        self.submit(
            SoundOperationKind::StartMicrophoneTest,
            Action::StartMicrophoneTest(device.cloned()),
        )
    }

    pub fn stop_microphone_test(&self) -> Result<SoundRequestId, SoundClientError> {
        self.submit(
            SoundOperationKind::StopMicrophoneTest,
            Action::StopMicrophoneTest,
        )
    }

    fn submit(
        &self,
        operation: SoundOperationKind,
        action: Action,
    ) -> Result<SoundRequestId, SoundClientError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(SoundClientError::BackendStopped);
        }
        let request_id = self
            .next_request
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map(SoundRequestId)
            .map_err(|_| SoundClientError::RequestCounterExhausted)?;
        match self.sender.try_send(Command {
            request_id,
            operation,
            action,
        }) {
            Ok(()) => Ok(request_id),
            Err(TrySendError::Full(_)) => Err(SoundClientError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(SoundClientError::BackendStopped),
        }
    }
}

#[derive(Debug)]
pub struct SoundService {
    controller: SoundController,
    worker: Option<JoinHandle<()>>,
}

impl SoundService {
    pub fn spawn() -> std::io::Result<Self> {
        let (sender, receiver) = sync_channel(COMMAND_QUEUE_CAPACITY);
        let state = Arc::new(Mutex::new(SoundState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let controller = SoundController {
            sender,
            state: Arc::clone(&state),
            next_request: Arc::new(AtomicU64::new(1)),
            shutdown: Arc::clone(&shutdown),
        };
        let worker = thread::Builder::new()
            .name("wildbuzzard-sound".into())
            .spawn(move || Worker::new(receiver, state, shutdown).run())?;
        Ok(Self {
            controller,
            worker: Some(worker),
        })
    }

    pub fn controller(&self) -> SoundController {
        self.controller.clone()
    }
}

impl Drop for SoundService {
    fn drop(&mut self) {
        self.controller.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Debug)]
struct Command {
    request_id: SoundRequestId,
    operation: SoundOperationKind,
    action: Action,
}

#[derive(Debug)]
enum Action {
    Refresh,
    SetDefaultOutput(DeviceId),
    SetOutputVolume(DeviceId, UserVolumePercent),
    SetOutputMute(DeviceId, bool),
    SetDefaultInput(DeviceId),
    SetInputVolume(DeviceId, UserVolumePercent),
    SetInputMute(DeviceId, bool),
    StartSpeakerTest(Option<DeviceId>),
    StopSpeakerTest,
    StartMicrophoneTest(Option<DeviceId>),
    StopMicrophoneTest,
}

struct Worker {
    receiver: Receiver<Command>,
    state: Arc<Mutex<SoundState>>,
    shutdown: Arc<AtomicBool>,
    session: Option<Session>,
    reconnect_at: Instant,
}

impl Worker {
    fn new(
        receiver: Receiver<Command>,
        state: Arc<Mutex<SoundState>>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            receiver,
            state,
            shutdown,
            session: None,
            reconnect_at: Instant::now(),
        }
    }

    fn run(mut self) {
        while !self.shutdown.load(Ordering::Acquire) {
            self.ensure_session();

            for _ in 0..COMMAND_QUEUE_CAPACITY {
                match self.receiver.try_recv() {
                    Ok(command) => self.handle_command(command),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.shutdown.store(true, Ordering::Release);
                        break;
                    }
                }
            }

            let failure = self.session.as_mut().and_then(Session::tick);
            if let Some(reason) = failure {
                if let Some(mut session) = self.session.take() {
                    session.disconnect_tests(&reason);
                    session.context.disconnect();
                }
                update_connection(
                    &self.state,
                    SoundConnection::Unavailable,
                    false,
                    Some(reason),
                );
                self.reconnect_at = Instant::now() + RECONNECT_DELAY;
            }

            thread::sleep(WORKER_TICK);
        }

        if let Some(mut session) = self.session.take() {
            session.shutdown();
        }
    }

    fn ensure_session(&mut self) {
        if self.session.is_some() || Instant::now() < self.reconnect_at {
            return;
        }
        update_connection(
            &self.state,
            SoundConnection::Connecting,
            false,
            Some("Connecting to the guest-private PipeWire-Pulse service.".into()),
        );
        match Session::new(Arc::clone(&self.state)) {
            Ok(session) => self.session = Some(session),
            Err(error) => {
                update_connection(
                    &self.state,
                    SoundConnection::Unavailable,
                    false,
                    Some(error),
                );
                self.reconnect_at = Instant::now() + RECONNECT_DELAY;
            }
        }
    }

    fn handle_command(&mut self, command: Command) {
        let Some(session) = self.session.as_mut() else {
            if matches!(command.action, Action::Refresh) {
                self.reconnect_at = Instant::now();
            }
            finish_operation(
                &self.state,
                command.request_id,
                command.operation,
                false,
                "The guest-private PipeWire-Pulse service is unavailable.",
            );
            return;
        };
        if session.context.get_state() != ContextState::Ready || !session.ready {
            finish_operation(
                &self.state,
                command.request_id,
                command.operation,
                false,
                "The guest-private PipeWire-Pulse service is still connecting.",
            );
            return;
        }
        session.handle_command(command);
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingCallbackOperation {
    request_id: SoundRequestId,
    operation: SoundOperationKind,
    started: Instant,
}

struct Session {
    mainloop: Mainloop,
    context: Context,
    state: Arc<Mutex<SoundState>>,
    connect_started: Instant,
    ready: bool,
    dirty: Rc<Cell<bool>>,
    subscription_result: Rc<Cell<Option<bool>>>,
    completed_snapshots: Rc<RefCell<VecDeque<Snapshot>>>,
    snapshot_in_flight: Option<(u64, Instant)>,
    next_snapshot: u64,
    refresh_feedback: VecDeque<(SoundRequestId, SoundOperationKind)>,
    callback_operations: Rc<RefCell<VecDeque<PendingCallbackOperation>>>,
    speaker: Option<SpeakerTest>,
    microphone: Option<MicrophoneTest>,
}

impl Session {
    fn new(state: Arc<Mutex<SoundState>>) -> Result<Self, String> {
        let mainloop = Mainloop::new().ok_or("Cannot allocate the PulseAudio mainloop.")?;
        let mut proplist = Proplist::new().ok_or("Cannot allocate PulseAudio properties.")?;
        set_property(
            &mut proplist,
            properties::APPLICATION_NAME,
            APPLICATION_NAME,
        )?;
        set_property(&mut proplist, properties::APPLICATION_ID, APPLICATION_ID)?;
        set_property(
            &mut proplist,
            properties::APPLICATION_ICON_NAME,
            "wildbuzzard-settings",
        )?;
        let mut context = Context::new_with_proplist(&mainloop, APPLICATION_NAME, &proplist)
            .ok_or("Cannot create a PulseAudio context.")?;
        context
            .connect(None, ContextFlagSet::NOAUTOSPAWN, None)
            .map_err(|error| format!("Cannot connect to PipeWire-Pulse: {error}"))?;
        Ok(Self {
            mainloop,
            context,
            state,
            connect_started: Instant::now(),
            ready: false,
            dirty: Rc::new(Cell::new(false)),
            subscription_result: Rc::new(Cell::new(None)),
            completed_snapshots: Rc::new(RefCell::new(VecDeque::new())),
            snapshot_in_flight: None,
            next_snapshot: 1,
            refresh_feedback: VecDeque::new(),
            callback_operations: Rc::new(RefCell::new(VecDeque::new())),
            speaker: None,
            microphone: None,
        })
    }

    /// Returns a diagnostic when the complete connection must be rebuilt.
    fn tick(&mut self) -> Option<String> {
        match self.mainloop.iterate(false) {
            IterateResult::Err(error) => {
                return Some(format!("PipeWire-Pulse mainloop failed: {error}"));
            }
            IterateResult::Quit(_) => {
                return Some("PipeWire-Pulse mainloop stopped unexpectedly.".into());
            }
            IterateResult::Success(_) => {}
        }

        match self.context.get_state() {
            ContextState::Ready => {
                if !self.ready {
                    self.become_ready();
                }
            }
            ContextState::Failed => {
                return Some(format!(
                    "PipeWire-Pulse connection failed: {}",
                    self.context.errno()
                ));
            }
            ContextState::Terminated => {
                return Some("PipeWire-Pulse connection terminated.".into());
            }
            ContextState::Unconnected
            | ContextState::Connecting
            | ContextState::Authorizing
            | ContextState::SettingName => {
                if connection_wait_expired(self.connect_started.elapsed()) {
                    return Some("PipeWire-Pulse did not become ready within five seconds.".into());
                }
                return None;
            }
        }

        if let Some(success) = self.subscription_result.take() {
            let mut state = lock_state(&self.state);
            state.subscription_active = success;
            state.generation = state.generation.saturating_add(1);
            if !success {
                state.diagnostic =
                    Some("Connected, but live sound-server subscriptions were rejected.".into());
            }
        }

        self.accept_completed_snapshot();
        if self
            .snapshot_in_flight
            .is_some_and(|(_, started)| snapshot_wait_expired(started.elapsed()))
        {
            self.snapshot_in_flight = None;
            finish_refresh_feedback(
                &self.state,
                &mut self.refresh_feedback,
                false,
                "Sound-server introspection timed out; the last confirmed state is retained.",
            );
            set_diagnostic(
                &self.state,
                "Sound-server introspection timed out; the last confirmed state is retained.",
            );
        }
        if self.dirty.get() && self.snapshot_in_flight.is_none() {
            self.dirty.set(false);
            self.request_snapshot();
        }
        expire_callback_operations(&self.state, &self.callback_operations);

        self.tick_speaker();
        self.tick_microphone();
        None
    }

    fn become_ready(&mut self) {
        self.ready = true;
        let dirty = Rc::clone(&self.dirty);
        self.context
            .set_subscribe_callback(Some(Box::new(move |_, _, _| dirty.set(true))));
        let subscription_result = Rc::clone(&self.subscription_result);
        self.context.subscribe(
            InterestMaskSet::SERVER
                | InterestMaskSet::SINK
                | InterestMaskSet::SOURCE
                | InterestMaskSet::SINK_INPUT
                | InterestMaskSet::SOURCE_OUTPUT,
            move |success| subscription_result.set(Some(success)),
        );
        update_connection(
            &self.state,
            SoundConnection::Ready,
            false,
            Some("Connected; reading guest-private sound devices and streams.".into()),
        );
        self.dirty.set(true);
    }

    fn register_callback_operation(
        &self,
        request_id: SoundRequestId,
        operation: SoundOperationKind,
    ) -> bool {
        let mut pending = self.callback_operations.borrow_mut();
        if pending.len() >= MAX_PENDING_CALLBACK_OPERATIONS {
            drop(pending);
            finish_operation(
                &self.state,
                request_id,
                operation,
                false,
                "Too many sound-server changes are awaiting confirmation; try again after they finish.",
            );
            return false;
        }
        pending.push_back(PendingCallbackOperation {
            request_id,
            operation,
            started: Instant::now(),
        });
        true
    }

    fn handle_command(&mut self, command: Command) {
        begin_operation(
            &self.state,
            command.request_id,
            command.operation,
            "Request sent to the guest-private sound server.",
        );
        match command.action {
            Action::Refresh => {
                if self.refresh_feedback.len() >= MAX_PENDING_REFRESH_OPERATIONS {
                    finish_operation(
                        &self.state,
                        command.request_id,
                        command.operation,
                        false,
                        "Too many sound refreshes are awaiting one bounded server snapshot.",
                    );
                    return;
                }
                self.refresh_feedback
                    .push_back((command.request_id, command.operation));
                self.dirty.set(true);
            }
            Action::SetDefaultOutput(device) => {
                if !self.output_exists(&device) {
                    self.reject_stale_device(command.request_id, command.operation, "output");
                    return;
                }
                if !self.register_callback_operation(command.request_id, command.operation) {
                    return;
                }
                let state = Arc::clone(&self.state);
                let dirty = Rc::clone(&self.dirty);
                let pending = Rc::clone(&self.callback_operations);
                self.context
                    .set_default_sink(device.name(), move |success| {
                        if complete_callback_operation(&pending, command.request_id) {
                            finish_operation(
                                &state,
                                command.request_id,
                                command.operation,
                                success,
                                if success {
                                    "Default output changed."
                                } else {
                                    "The sound server rejected the default output change."
                                },
                            );
                        }
                        dirty.set(true);
                    });
            }
            Action::SetOutputVolume(device, volume) => {
                let Some(channels) = self.output_channels(&device) else {
                    self.reject_stale_device(command.request_id, command.operation, "output");
                    return;
                };
                let values = channel_volumes(channels, volume);
                if !self.register_callback_operation(command.request_id, command.operation) {
                    return;
                }
                let state = Arc::clone(&self.state);
                let dirty = Rc::clone(&self.dirty);
                let pending = Rc::clone(&self.callback_operations);
                self.context.introspect().set_sink_volume_by_index(
                    device.index(),
                    &values,
                    Some(Box::new(move |success| {
                        if complete_callback_operation(&pending, command.request_id) {
                            finish_operation(
                                &state,
                                command.request_id,
                                command.operation,
                                success,
                                if success {
                                    "Output volume changed."
                                } else {
                                    "The sound server rejected the output volume change."
                                },
                            );
                        }
                        dirty.set(true);
                    })),
                );
            }
            Action::SetOutputMute(device, muted) => {
                if !self.output_exists(&device) {
                    self.reject_stale_device(command.request_id, command.operation, "output");
                    return;
                }
                if !self.register_callback_operation(command.request_id, command.operation) {
                    return;
                }
                let state = Arc::clone(&self.state);
                let dirty = Rc::clone(&self.dirty);
                let pending = Rc::clone(&self.callback_operations);
                self.context.introspect().set_sink_mute_by_index(
                    device.index(),
                    muted,
                    Some(Box::new(move |success| {
                        if complete_callback_operation(&pending, command.request_id) {
                            finish_operation(
                                &state,
                                command.request_id,
                                command.operation,
                                success,
                                if success {
                                    "Output mute changed."
                                } else {
                                    "The sound server rejected the output mute change."
                                },
                            );
                        }
                        dirty.set(true);
                    })),
                );
            }
            Action::SetDefaultInput(device) => {
                if !self.input_exists(&device) {
                    self.reject_stale_device(command.request_id, command.operation, "input");
                    return;
                }
                if !self.register_callback_operation(command.request_id, command.operation) {
                    return;
                }
                let state = Arc::clone(&self.state);
                let dirty = Rc::clone(&self.dirty);
                let pending = Rc::clone(&self.callback_operations);
                self.context
                    .set_default_source(device.name(), move |success| {
                        if complete_callback_operation(&pending, command.request_id) {
                            finish_operation(
                                &state,
                                command.request_id,
                                command.operation,
                                success,
                                if success {
                                    "Default input changed."
                                } else {
                                    "The sound server rejected the default input change."
                                },
                            );
                        }
                        dirty.set(true);
                    });
            }
            Action::SetInputVolume(device, volume) => {
                let Some(channels) = self.input_channels(&device) else {
                    self.reject_stale_device(command.request_id, command.operation, "input");
                    return;
                };
                let values = channel_volumes(channels, volume);
                if !self.register_callback_operation(command.request_id, command.operation) {
                    return;
                }
                let state = Arc::clone(&self.state);
                let dirty = Rc::clone(&self.dirty);
                let pending = Rc::clone(&self.callback_operations);
                self.context.introspect().set_source_volume_by_index(
                    device.index(),
                    &values,
                    Some(Box::new(move |success| {
                        if complete_callback_operation(&pending, command.request_id) {
                            finish_operation(
                                &state,
                                command.request_id,
                                command.operation,
                                success,
                                if success {
                                    "Input volume changed."
                                } else {
                                    "The sound server rejected the input volume change."
                                },
                            );
                        }
                        dirty.set(true);
                    })),
                );
            }
            Action::SetInputMute(device, muted) => {
                if !self.input_exists(&device) {
                    self.reject_stale_device(command.request_id, command.operation, "input");
                    return;
                }
                if !self.register_callback_operation(command.request_id, command.operation) {
                    return;
                }
                let state = Arc::clone(&self.state);
                let dirty = Rc::clone(&self.dirty);
                let pending = Rc::clone(&self.callback_operations);
                self.context.introspect().set_source_mute_by_index(
                    device.index(),
                    muted,
                    Some(Box::new(move |success| {
                        if complete_callback_operation(&pending, command.request_id) {
                            finish_operation(
                                &state,
                                command.request_id,
                                command.operation,
                                success,
                                if success {
                                    "Input mute changed."
                                } else {
                                    "The sound server rejected the input mute change."
                                },
                            );
                        }
                        dirty.set(true);
                    })),
                );
            }
            Action::StartSpeakerTest(device) => {
                if device.as_ref().is_some_and(|id| !self.output_exists(id)) {
                    self.reject_stale_device(command.request_id, command.operation, "output");
                    return;
                }
                self.stop_speaker_stream(
                    "The preceding speaker test was replaced before it became active.",
                );
                match SpeakerTest::start(&mut self.context, device.as_ref(), command.request_id) {
                    Ok(test) => {
                        self.speaker = Some(test);
                        set_speaker_test(&self.state, SoundTestState::Starting, None);
                    }
                    Err(error) => {
                        set_speaker_test(&self.state, SoundTestState::Failed, Some(&error));
                        finish_operation(
                            &self.state,
                            command.request_id,
                            command.operation,
                            false,
                            &error,
                        );
                    }
                }
            }
            Action::StopSpeakerTest => {
                self.stop_speaker_stream("The speaker test was stopped before it became active.");
                set_speaker_test(&self.state, SoundTestState::Idle, None);
                finish_operation(
                    &self.state,
                    command.request_id,
                    command.operation,
                    true,
                    "Speaker test stopped.",
                );
            }
            Action::StartMicrophoneTest(device) => {
                if device.as_ref().is_some_and(|id| !self.input_exists(id)) {
                    self.reject_stale_device(command.request_id, command.operation, "input");
                    return;
                }
                self.stop_microphone_stream(
                    "The preceding microphone level test was replaced before capture became active.",
                );
                match MicrophoneTest::start(&mut self.context, device.as_ref(), command.request_id)
                {
                    Ok(test) => {
                        self.microphone = Some(test);
                        set_microphone_test(&self.state, SoundTestState::Starting, None, None);
                    }
                    Err(error) => {
                        set_microphone_test(
                            &self.state,
                            SoundTestState::Failed,
                            None,
                            Some(&error),
                        );
                        finish_operation(
                            &self.state,
                            command.request_id,
                            command.operation,
                            false,
                            &error,
                        );
                    }
                }
            }
            Action::StopMicrophoneTest => {
                self.stop_microphone_stream(
                    "The microphone level test was stopped before capture became active.",
                );
                set_microphone_test(&self.state, SoundTestState::Idle, None, None);
                finish_operation(
                    &self.state,
                    command.request_id,
                    command.operation,
                    true,
                    "Microphone level test stopped and its capture stream was released.",
                );
            }
        }
    }

    fn reject_stale_device(
        &self,
        request_id: SoundRequestId,
        operation: SoundOperationKind,
        kind: &str,
    ) {
        finish_operation(
            &self.state,
            request_id,
            operation,
            false,
            &format!("The selected {kind} device is no longer present in the latest server state."),
        );
        self.dirty.set(true);
    }

    fn output_exists(&self, id: &DeviceId) -> bool {
        lock_state(&self.state)
            .outputs
            .iter()
            .any(|device| device.id == *id)
    }

    fn input_exists(&self, id: &DeviceId) -> bool {
        lock_state(&self.state)
            .inputs
            .iter()
            .any(|device| device.id == *id)
    }

    fn output_channels(&self, id: &DeviceId) -> Option<u8> {
        lock_state(&self.state)
            .outputs
            .iter()
            .find(|device| device.id == *id)
            .map(|device| device.channels)
    }

    fn input_channels(&self, id: &DeviceId) -> Option<u8> {
        lock_state(&self.state)
            .inputs
            .iter()
            .find(|device| device.id == *id)
            .map(|device| device.channels)
    }

    fn request_snapshot(&mut self) {
        let request_id = self.next_snapshot;
        self.next_snapshot = self.next_snapshot.saturating_add(1);
        self.snapshot_in_flight = Some((request_id, Instant::now()));
        request_snapshot(
            &self.context,
            request_id,
            Rc::clone(&self.completed_snapshots),
        );
    }

    fn accept_completed_snapshot(&mut self) {
        let Some(expected) = self.snapshot_in_flight.map(|(id, _)| id) else {
            self.completed_snapshots.borrow_mut().clear();
            return;
        };
        let mut accepted = None;
        {
            let mut queue = self.completed_snapshots.borrow_mut();
            while let Some(snapshot) = queue.pop_front() {
                if snapshot.request_id == expected {
                    accepted = Some(snapshot);
                    break;
                }
            }
        }
        let Some(snapshot) = accepted else { return };
        self.snapshot_in_flight = None;
        apply_snapshot(&self.state, snapshot);
        // A subscription or explicit Refresh that arrived while this snapshot
        // was in flight requires one newer snapshot. Only then can every
        // coalesced Refresh truthfully report completion.
        if !self.dirty.get() {
            finish_refresh_feedback(
                &self.state,
                &mut self.refresh_feedback,
                true,
                "Sound devices and streams refreshed.",
            );
        }
    }

    fn tick_speaker(&mut self) {
        let Some(mut test) = self.speaker.take() else {
            return;
        };
        match test.tick() {
            TestTick::Continue => self.speaker = Some(test),
            TestTick::Running => {
                set_speaker_test(&self.state, SoundTestState::Running, None);
                finish_operation(
                    &self.state,
                    test.request_id,
                    SoundOperationKind::StartSpeakerTest,
                    true,
                    "Speaker test is playing through the selected guest output.",
                );
                self.speaker = Some(test);
            }
            TestTick::Completed(message) => {
                test.disconnect();
                set_speaker_test(&self.state, SoundTestState::Completed, None);
                set_diagnostic(&self.state, message);
                self.dirty.set(true);
            }
            TestTick::Failed(error) => {
                test.disconnect();
                set_speaker_test(&self.state, SoundTestState::Failed, Some(&error));
                finish_operation(
                    &self.state,
                    test.request_id,
                    SoundOperationKind::StartSpeakerTest,
                    false,
                    &error,
                );
                self.dirty.set(true);
            }
        }
    }

    fn tick_microphone(&mut self) {
        let Some(mut test) = self.microphone.take() else {
            return;
        };
        match test.tick() {
            MicrophoneTick::Continue => self.microphone = Some(test),
            MicrophoneTick::Running(level) => {
                set_microphone_test(&self.state, SoundTestState::Running, level, None);
                if !test.reported_running {
                    test.reported_running = true;
                    finish_operation(
                        &self.state,
                        test.request_id,
                        SoundOperationKind::StartMicrophoneTest,
                        true,
                        "Microphone level test is active only for this explicit test.",
                    );
                }
                self.microphone = Some(test);
            }
            MicrophoneTick::Completed(message) => {
                test.disconnect();
                set_microphone_test(&self.state, SoundTestState::Completed, None, None);
                set_diagnostic(&self.state, message);
                self.dirty.set(true);
            }
            MicrophoneTick::Failed(error) => {
                test.disconnect();
                set_microphone_test(&self.state, SoundTestState::Failed, None, Some(&error));
                finish_operation(
                    &self.state,
                    test.request_id,
                    SoundOperationKind::StartMicrophoneTest,
                    false,
                    &error,
                );
                self.dirty.set(true);
            }
        }
    }

    fn stop_speaker_stream(&mut self, pending_message: &str) {
        if let Some(mut test) = self.speaker.take() {
            cancel_pending_test_start(
                &self.state,
                test.request_id,
                SoundOperationKind::StartSpeakerTest,
                test.reported_running,
                pending_message,
            );
            test.disconnect();
        }
        self.dirty.set(true);
    }

    fn stop_microphone_stream(&mut self, pending_message: &str) {
        if let Some(mut test) = self.microphone.take() {
            cancel_pending_test_start(
                &self.state,
                test.request_id,
                SoundOperationKind::StartMicrophoneTest,
                test.reported_running,
                pending_message,
            );
            test.disconnect();
        }
        self.dirty.set(true);
    }

    fn disconnect_tests(&mut self, reason: &str) {
        let microphone_was_active = self.microphone.is_some();
        let speaker_was_active = self.speaker.is_some();
        self.stop_microphone_stream(reason);
        self.stop_speaker_stream(reason);
        finish_refresh_feedback(&self.state, &mut self.refresh_feedback, false, reason);
        finish_callback_operations(&self.state, &self.callback_operations, reason);
        if microphone_was_active {
            set_microphone_test(&self.state, SoundTestState::Failed, None, Some(reason));
        }
        if speaker_was_active {
            set_speaker_test(&self.state, SoundTestState::Failed, Some(reason));
        }
    }

    fn shutdown(&mut self) {
        self.stop_microphone_stream(
            "Settings closed before the microphone level test became active.",
        );
        self.stop_speaker_stream("Settings closed before the speaker test became active.");
        finish_refresh_feedback(
            &self.state,
            &mut self.refresh_feedback,
            false,
            "Settings closed before sound refresh completed.",
        );
        finish_callback_operations(
            &self.state,
            &self.callback_operations,
            "Settings closed before the sound server answered the request.",
        );
        self.context.set_subscribe_callback(None);
        self.context.disconnect();
    }
}

#[derive(Default)]
struct SnapshotAccumulator {
    request_id: u64,
    pending: u8,
    server_name: Option<String>,
    server_version: Option<String>,
    default_output_name: Option<String>,
    default_input_name: Option<String>,
    outputs: Vec<SoundDevice>,
    inputs: Vec<SoundDevice>,
    playback_streams: Vec<SoundStreamInfo>,
    recording_streams: Vec<SoundStreamInfo>,
    diagnostics: Vec<String>,
}

struct Snapshot {
    request_id: u64,
    server_name: Option<String>,
    server_version: Option<String>,
    default_output_name: Option<String>,
    default_input_name: Option<String>,
    outputs: Vec<SoundDevice>,
    inputs: Vec<SoundDevice>,
    playback_streams: Vec<SoundStreamInfo>,
    recording_streams: Vec<SoundStreamInfo>,
    diagnostics: Vec<String>,
}

const SERVER_PART: u8 = 1 << 0;
const SINK_PART: u8 = 1 << 1;
const SOURCE_PART: u8 = 1 << 2;
const PLAYBACK_PART: u8 = 1 << 3;
const RECORDING_PART: u8 = 1 << 4;
const ALL_SNAPSHOT_PARTS: u8 =
    SERVER_PART | SINK_PART | SOURCE_PART | PLAYBACK_PART | RECORDING_PART;

fn request_snapshot(
    context: &Context,
    request_id: u64,
    completed: Rc<RefCell<VecDeque<Snapshot>>>,
) {
    let accumulator = Rc::new(RefCell::new(Some(SnapshotAccumulator {
        request_id,
        pending: ALL_SNAPSHOT_PARTS,
        ..SnapshotAccumulator::default()
    })));
    let introspector = context.introspect();

    {
        let accumulator = Rc::clone(&accumulator);
        let completed = Rc::clone(&completed);
        introspector.get_server_info(move |info| {
            record_server(&accumulator, info);
            finish_snapshot_part(&accumulator, &completed, SERVER_PART, None);
        });
    }
    {
        let accumulator = Rc::clone(&accumulator);
        let completed = Rc::clone(&completed);
        introspector.get_sink_info_list(move |result| match result {
            ListResult::Item(info) => record_sink(&accumulator, info),
            ListResult::End => finish_snapshot_part(&accumulator, &completed, SINK_PART, None),
            ListResult::Error => finish_snapshot_part(
                &accumulator,
                &completed,
                SINK_PART,
                Some("Cannot enumerate guest output devices."),
            ),
        });
    }
    {
        let accumulator = Rc::clone(&accumulator);
        let completed = Rc::clone(&completed);
        introspector.get_source_info_list(move |result| match result {
            ListResult::Item(info) => record_source(&accumulator, info),
            ListResult::End => finish_snapshot_part(&accumulator, &completed, SOURCE_PART, None),
            ListResult::Error => finish_snapshot_part(
                &accumulator,
                &completed,
                SOURCE_PART,
                Some("Cannot enumerate guest input devices."),
            ),
        });
    }
    {
        let accumulator = Rc::clone(&accumulator);
        let completed = Rc::clone(&completed);
        introspector.get_sink_input_info_list(move |result| match result {
            ListResult::Item(info) => record_playback_stream(&accumulator, info),
            ListResult::End => finish_snapshot_part(&accumulator, &completed, PLAYBACK_PART, None),
            ListResult::Error => finish_snapshot_part(
                &accumulator,
                &completed,
                PLAYBACK_PART,
                Some("Cannot enumerate active guest playback streams."),
            ),
        });
    }
    {
        let accumulator = Rc::clone(&accumulator);
        let completed = Rc::clone(&completed);
        introspector.get_source_output_info_list(move |result| match result {
            ListResult::Item(info) => record_recording_stream(&accumulator, info),
            ListResult::End => finish_snapshot_part(&accumulator, &completed, RECORDING_PART, None),
            ListResult::Error => finish_snapshot_part(
                &accumulator,
                &completed,
                RECORDING_PART,
                Some("Cannot enumerate active guest recording streams."),
            ),
        });
    }
}

fn finish_snapshot_part(
    accumulator: &Rc<RefCell<Option<SnapshotAccumulator>>>,
    completed: &Rc<RefCell<VecDeque<Snapshot>>>,
    part: u8,
    error: Option<&str>,
) {
    let finished = {
        let mut slot = accumulator.borrow_mut();
        let Some(value) = slot.as_mut() else { return };
        if value.pending & part == 0 {
            return;
        }
        if let Some(error) = error {
            push_diagnostic(&mut value.diagnostics, error);
        }
        value.pending &= !part;
        (value.pending == 0).then(|| slot.take().expect("snapshot accumulator exists"))
    };
    let Some(value) = finished else { return };
    let snapshot = Snapshot {
        request_id: value.request_id,
        server_name: value.server_name,
        server_version: value.server_version,
        default_output_name: value.default_output_name,
        default_input_name: value.default_input_name,
        outputs: value.outputs,
        inputs: value.inputs,
        playback_streams: value.playback_streams,
        recording_streams: value.recording_streams,
        diagnostics: value.diagnostics,
    };
    let mut queue = completed.borrow_mut();
    if queue.len() == MAX_COMPLETED_SNAPSHOTS {
        queue.pop_front();
    }
    queue.push_back(snapshot);
}

fn record_server(accumulator: &Rc<RefCell<Option<SnapshotAccumulator>>>, info: &ServerInfo<'_>) {
    let mut slot = accumulator.borrow_mut();
    let Some(value) = slot.as_mut() else { return };
    value.server_name = info
        .server_name
        .as_deref()
        .map(|name| bounded_display_text(name, "PipeWire-Pulse"));
    value.server_version = info
        .server_version
        .as_deref()
        .map(|version| bounded_display_text(version, "unknown"));
    value.default_output_name = validated_server_name(
        info.default_sink_name.as_deref(),
        "default output",
        &mut value.diagnostics,
    );
    value.default_input_name = validated_server_name(
        info.default_source_name.as_deref(),
        "default input",
        &mut value.diagnostics,
    );
}

fn record_sink(accumulator: &Rc<RefCell<Option<SnapshotAccumulator>>>, info: &SinkInfo<'_>) {
    let mut slot = accumulator.borrow_mut();
    let Some(value) = slot.as_mut() else { return };
    if value.outputs.len() >= MAX_DEVICES {
        push_diagnostic(
            &mut value.diagnostics,
            "The output-device list exceeded the 256-device safety limit.",
        );
        return;
    }
    let Some(name) = validated_server_name(
        info.name.as_deref(),
        "output device",
        &mut value.diagnostics,
    ) else {
        return;
    };
    let volume = info.volume.avg();
    value.outputs.push(SoundDevice {
        id: DeviceId {
            index: info.index,
            name: name.clone(),
        },
        description: info
            .description
            .as_deref()
            .map_or_else(|| name.clone(), |text| bounded_display_text(text, &name)),
        volume_raw: volume.0,
        volume_percent: volume_percent(volume),
        channels: info.volume.len(),
        muted: info.mute,
        activity: sink_activity(info.state),
        active_port: info
            .active_port
            .as_ref()
            .and_then(|port| port.description.as_deref().or(port.name.as_deref()))
            .map(|text| bounded_display_text(text, "Unknown port")),
        monitor: false,
    });
}

fn record_source(accumulator: &Rc<RefCell<Option<SnapshotAccumulator>>>, info: &SourceInfo<'_>) {
    let mut slot = accumulator.borrow_mut();
    let Some(value) = slot.as_mut() else { return };
    if value.inputs.len() >= MAX_DEVICES {
        push_diagnostic(
            &mut value.diagnostics,
            "The input-device list exceeded the 256-device safety limit.",
        );
        return;
    }
    let Some(name) =
        validated_server_name(info.name.as_deref(), "input device", &mut value.diagnostics)
    else {
        return;
    };
    let volume = info.volume.avg();
    value.inputs.push(SoundDevice {
        id: DeviceId {
            index: info.index,
            name: name.clone(),
        },
        description: info
            .description
            .as_deref()
            .map_or_else(|| name.clone(), |text| bounded_display_text(text, &name)),
        volume_raw: volume.0,
        volume_percent: volume_percent(volume),
        channels: info.volume.len(),
        muted: info.mute,
        activity: source_activity(info.state),
        active_port: info
            .active_port
            .as_ref()
            .and_then(|port| port.description.as_deref().or(port.name.as_deref()))
            .map(|text| bounded_display_text(text, "Unknown port")),
        monitor: info.monitor_of_sink.is_some(),
    });
}

fn record_playback_stream(
    accumulator: &Rc<RefCell<Option<SnapshotAccumulator>>>,
    info: &SinkInputInfo<'_>,
) {
    let mut slot = accumulator.borrow_mut();
    let Some(value) = slot.as_mut() else { return };
    if value.playback_streams.len() >= MAX_STREAMS {
        push_diagnostic(
            &mut value.diagnostics,
            "The playback-stream list exceeded the 2048-stream safety limit.",
        );
        return;
    }
    value.playback_streams.push(SoundStreamInfo {
        id: SoundStreamId(info.index),
        name: stream_name(info.name.as_deref(), &info.proplist, "Playback stream"),
        application_name: stream_application_name(&info.proplist),
        route_device_index: info.sink,
        volume_percent: info.has_volume.then(|| volume_percent(info.volume.avg())),
        muted: info.mute,
        corked: info.corked,
    });
}

fn record_recording_stream(
    accumulator: &Rc<RefCell<Option<SnapshotAccumulator>>>,
    info: &SourceOutputInfo<'_>,
) {
    let mut slot = accumulator.borrow_mut();
    let Some(value) = slot.as_mut() else { return };
    if value.recording_streams.len() >= MAX_STREAMS {
        push_diagnostic(
            &mut value.diagnostics,
            "The recording-stream list exceeded the 2048-stream safety limit.",
        );
        return;
    }
    value.recording_streams.push(SoundStreamInfo {
        id: SoundStreamId(info.index),
        name: stream_name(info.name.as_deref(), &info.proplist, "Recording stream"),
        application_name: stream_application_name(&info.proplist),
        route_device_index: info.source,
        volume_percent: info.has_volume.then(|| volume_percent(info.volume.avg())),
        muted: info.mute,
        corked: info.corked,
    });
}

fn apply_snapshot(state: &Arc<Mutex<SoundState>>, mut snapshot: Snapshot) {
    snapshot.outputs.sort_by(|left, right| {
        left.description
            .to_lowercase()
            .cmp(&right.description.to_lowercase())
            .then_with(|| left.id.index.cmp(&right.id.index))
    });
    snapshot.inputs.sort_by(|left, right| {
        left.description
            .to_lowercase()
            .cmp(&right.description.to_lowercase())
            .then_with(|| left.id.index.cmp(&right.id.index))
    });
    snapshot.playback_streams.sort_by_key(|stream| stream.id.0);
    snapshot.recording_streams.sort_by_key(|stream| stream.id.0);
    let mut current = lock_state(state);
    current.server_name = snapshot.server_name;
    current.server_version = snapshot.server_version;
    current.default_output_name = snapshot.default_output_name;
    current.default_input_name = snapshot.default_input_name;
    current.outputs = snapshot.outputs;
    current.inputs = snapshot.inputs;
    current.playback_streams = snapshot.playback_streams;
    current.recording_streams = snapshot.recording_streams;
    current.connection = SoundConnection::Ready;
    current.diagnostic = if snapshot.diagnostics.is_empty() {
        None
    } else {
        Some(snapshot.diagnostics.join(" "))
    };
    current.generation = current.generation.saturating_add(1);
}

struct SpeakerTest {
    request_id: SoundRequestId,
    stream: Stream,
    samples: Vec<u8>,
    cursor: usize,
    created: Instant,
    reported_running: bool,
    drain_started: bool,
    drain_result: Rc<Cell<Option<bool>>>,
}

impl SpeakerTest {
    fn start(
        context: &mut Context,
        device: Option<&DeviceId>,
        request_id: SoundRequestId,
    ) -> Result<Self, String> {
        let spec = Spec {
            format: Format::S16NE,
            channels: 2,
            rate: TEST_RATE,
        };
        if !spec.is_valid() {
            return Err("The fixed speaker-test sample format is invalid.".into());
        }
        let mut properties = stream_properties("Speaker Test")?;
        let mut stream = Stream::new_with_proplist(
            context,
            "Buzzard OS Speaker Test",
            &spec,
            None,
            &mut properties,
        )
        .ok_or("Cannot allocate the speaker-test stream.")?;
        let attributes = BufferAttr {
            maxlength: u32::MAX,
            tlength: 9_600,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: u32::MAX,
        };
        stream
            .connect_playback(
                device.map(DeviceId::name),
                Some(&attributes),
                StreamFlagSet::ADJUST_LATENCY,
                None,
                None,
            )
            .map_err(|error| format!("Cannot start the speaker test: {error}"))?;
        Ok(Self {
            request_id,
            stream,
            samples: speaker_test_samples(),
            cursor: 0,
            created: Instant::now(),
            reported_running: false,
            drain_started: false,
            drain_result: Rc::new(Cell::new(None)),
        })
    }

    fn tick(&mut self) -> TestTick {
        if self.created.elapsed() > SPEAKER_TEST_HARD_LIMIT {
            return TestTick::Failed(
                "Speaker test exceeded its eight-second safety limit and was stopped.".into(),
            );
        }
        match self.stream.get_state() {
            StreamState::Failed | StreamState::Terminated => {
                return TestTick::Failed("The speaker-test stream disconnected.".into());
            }
            StreamState::Unconnected | StreamState::Creating => return TestTick::Continue,
            StreamState::Ready => {}
        }
        let first_running_tick = !self.reported_running;
        self.reported_running = true;
        if self.cursor < self.samples.len() {
            let writable = self.stream.writable_size().unwrap_or(0);
            let remaining = self.samples.len() - self.cursor;
            let write_len = writable.min(remaining).min(65_536) / 4 * 4;
            if write_len > 0 {
                let end = self.cursor + write_len;
                if let Err(error) = self.stream.write_copy(
                    &self.samples[self.cursor..end],
                    0,
                    pulse::stream::SeekMode::Relative,
                ) {
                    return TestTick::Failed(format!(
                        "Cannot write the speaker-test samples: {error}"
                    ));
                }
                self.cursor = end;
            }
        }
        if self.cursor == self.samples.len() && !self.drain_started {
            self.drain_started = true;
            let result = Rc::clone(&self.drain_result);
            self.stream
                .drain(Some(Box::new(move |success| result.set(Some(success)))));
        }
        if let Some(success) = self.drain_result.take() {
            return if success {
                TestTick::Completed("Speaker test completed and released its stream.")
            } else {
                TestTick::Failed("The sound server could not drain the speaker test.".into())
            };
        }
        if first_running_tick {
            TestTick::Running
        } else {
            TestTick::Continue
        }
    }

    fn disconnect(&mut self) {
        let _ = self.stream.disconnect();
    }
}

enum TestTick {
    Continue,
    Running,
    Completed(&'static str),
    Failed(String),
}

struct MicrophoneTest {
    request_id: SoundRequestId,
    stream: Stream,
    created: Instant,
    reported_running: bool,
}

impl MicrophoneTest {
    fn start(
        context: &mut Context,
        device: Option<&DeviceId>,
        request_id: SoundRequestId,
    ) -> Result<Self, String> {
        let spec = Spec {
            format: Format::FLOAT32NE,
            channels: 1,
            rate: TEST_RATE,
        };
        if !spec.is_valid() {
            return Err("The fixed microphone-meter sample format is invalid.".into());
        }
        let mut properties = stream_properties("Microphone Level Test")?;
        let mut stream = Stream::new_with_proplist(
            context,
            "Buzzard OS Microphone Level Test",
            &spec,
            None,
            &mut properties,
        )
        .ok_or("Cannot allocate the microphone level stream.")?;
        let attributes = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: MICROPHONE_FRAGMENT_BYTES,
        };
        stream
            .connect_record(
                device.map(DeviceId::name),
                Some(&attributes),
                StreamFlagSet::ADJUST_LATENCY,
            )
            .map_err(|error| format!("Cannot start the microphone level test: {error}"))?;
        Ok(Self {
            request_id,
            stream,
            created: Instant::now(),
            reported_running: false,
        })
    }

    fn tick(&mut self) -> MicrophoneTick {
        if self.created.elapsed() > MICROPHONE_TEST_HARD_LIMIT {
            return MicrophoneTick::Completed(
                "Microphone level test reached its 30-second safety limit and released capture.",
            );
        }
        match self.stream.get_state() {
            StreamState::Failed | StreamState::Terminated => {
                return MicrophoneTick::Failed(
                    "The microphone level stream disconnected and capture was released.".into(),
                );
            }
            StreamState::Unconnected | StreamState::Creating => {
                return MicrophoneTick::Continue;
            }
            StreamState::Ready => {}
        }

        let mut latest = None;
        for _ in 0..MAX_MICROPHONE_FRAGMENTS_PER_TICK {
            if self.stream.readable_size().unwrap_or(0) == 0 {
                break;
            }
            let (level, discard) = match self.stream.peek() {
                Ok(PeekResult::Data(bytes)) => (microphone_level(bytes), true),
                Ok(PeekResult::Hole(_)) => (None, true),
                Ok(PeekResult::Empty) => (None, false),
                Err(error) => {
                    return MicrophoneTick::Failed(format!(
                        "Cannot read microphone level samples: {error}"
                    ));
                }
            };
            if discard {
                if let Err(error) = self.stream.discard() {
                    return MicrophoneTick::Failed(format!(
                        "Cannot release microphone sample data: {error}"
                    ));
                }
            } else {
                break;
            }
            latest = level.or(latest);
        }
        MicrophoneTick::Running(latest)
    }

    fn disconnect(&mut self) {
        let _ = self.stream.disconnect();
    }
}

enum MicrophoneTick {
    Continue,
    Running(Option<MicrophoneLevel>),
    Completed(&'static str),
    Failed(String),
}

fn stream_properties(media_name: &str) -> Result<Proplist, String> {
    let mut properties = Proplist::new().ok_or("Cannot allocate stream properties.")?;
    set_property(
        &mut properties,
        properties::APPLICATION_NAME,
        APPLICATION_NAME,
    )?;
    set_property(&mut properties, properties::APPLICATION_ID, APPLICATION_ID)?;
    set_property(
        &mut properties,
        properties::APPLICATION_ICON_NAME,
        "wildbuzzard-settings",
    )?;
    set_property(&mut properties, properties::MEDIA_NAME, media_name)?;
    set_property(&mut properties, properties::MEDIA_ROLE, "test")?;
    Ok(properties)
}

fn set_property(properties: &mut Proplist, key: &str, value: &str) -> Result<(), String> {
    properties
        .set_str(key, value)
        .map_err(|()| format!("Cannot set the PulseAudio property {key}."))
}

fn channel_volumes(channels: u8, volume: UserVolumePercent) -> ChannelVolumes {
    let mut values = ChannelVolumes::default();
    values.set(
        channels.max(1),
        Volume::from(VolumeLinear(f64::from(volume.get()) / 100.0)),
    );
    values
}

fn speaker_test_samples() -> Vec<u8> {
    const FRAMES: u32 = TEST_RATE + TEST_RATE / 5;
    const SWITCH_FRAME: u32 = TEST_RATE / 2;
    const SECOND_START: u32 = TEST_RATE * 7 / 10;
    let mut bytes = Vec::with_capacity(FRAMES as usize * 4);
    for frame in 0..FRAMES {
        let (left, right, local_frame) = if frame < SWITCH_FRAME {
            (true, false, frame)
        } else if frame >= SECOND_START {
            (false, true, frame - SECOND_START)
        } else {
            (false, false, 0)
        };
        let tone = if left || right {
            let phase =
                2.0 * std::f64::consts::PI * 440.0 * f64::from(local_frame) / f64::from(TEST_RATE);
            let segment_frames = SWITCH_FRAME;
            let fade_frames = TEST_RATE / 100;
            let fade_in = (local_frame as f64 / f64::from(fade_frames)).clamp(0.0, 1.0);
            let remaining = segment_frames.saturating_sub(local_frame);
            let fade_out = (remaining as f64 / f64::from(fade_frames)).clamp(0.0, 1.0);
            (phase.sin() * 0.18 * fade_in.min(fade_out) * f64::from(i16::MAX)).round() as i16
        } else {
            0
        };
        bytes.extend_from_slice(&(if left { tone } else { 0 }).to_ne_bytes());
        bytes.extend_from_slice(&(if right { tone } else { 0 }).to_ne_bytes());
    }
    bytes
}

fn microphone_level(bytes: &[u8]) -> Option<MicrophoneLevel> {
    let mut sum_squares = 0.0;
    let mut samples = 0_u64;
    for chunk in bytes.chunks_exact(4) {
        let sample = f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if sample.is_finite() {
            let sample = f64::from(sample).clamp(-1.0, 1.0);
            sum_squares += sample * sample;
            samples += 1;
        }
    }
    if samples == 0 {
        return None;
    }
    let rms = (sum_squares / samples as f64).sqrt().clamp(0.0, 1.0);
    let dbfs = if rms == 0.0 {
        -96.0
    } else {
        (20.0 * rms.log10()).clamp(-96.0, 0.0)
    };
    Some(MicrophoneLevel {
        rms,
        dbfs,
        meter_fraction: ((dbfs + 60.0) / 60.0).clamp(0.0, 1.0),
    })
}

fn validated_server_name(
    value: Option<&str>,
    kind: &str,
    diagnostics: &mut Vec<String>,
) -> Option<String> {
    let Some(value) = value else {
        push_diagnostic(diagnostics, &format!("The {kind} has no server name."));
        return None;
    };
    if value.is_empty()
        || value.len() > MAX_SERVER_NAME_BYTES
        || value.chars().any(char::is_control)
    {
        push_diagnostic(
            diagnostics,
            &format!("Ignored an invalid or oversized {kind} server name."),
        );
        return None;
    }
    Some(value.to_owned())
}

fn bounded_display_text(value: &str, fallback: &str) -> String {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return fallback.to_owned();
    }
    let mut end = 0;
    for (offset, character) in normalized.char_indices() {
        let next = offset + character.len_utf8();
        if next > MAX_DISPLAY_TEXT_BYTES {
            break;
        }
        end = next;
    }
    normalized[..end].to_owned()
}

fn push_diagnostic(diagnostics: &mut Vec<String>, diagnostic: &str) {
    if diagnostics.len() >= MAX_DIAGNOSTICS
        || diagnostics.iter().any(|existing| existing == diagnostic)
    {
        return;
    }
    diagnostics.push(bounded_display_text(
        diagnostic,
        "Sound diagnostic unavailable.",
    ));
}

fn stream_name(name: Option<&str>, properties: &Proplist, fallback: &str) -> String {
    properties
        .get_str(properties::MEDIA_NAME)
        .as_deref()
        .or(name)
        .map_or_else(
            || fallback.to_owned(),
            |value| bounded_display_text(value, fallback),
        )
}

fn stream_application_name(properties: &Proplist) -> Option<String> {
    properties
        .get_str(properties::APPLICATION_NAME)
        .map(|value| bounded_display_text(&value, "Unknown application"))
}

fn volume_percent(volume: Volume) -> f64 {
    f64::from(volume.0) * 100.0 / f64::from(Volume::NORMAL.0)
}

fn sink_activity(state: SinkState) -> DeviceActivity {
    match state {
        SinkState::Running => DeviceActivity::Running,
        SinkState::Idle => DeviceActivity::Idle,
        SinkState::Suspended => DeviceActivity::Suspended,
        SinkState::Invalid => DeviceActivity::Unknown,
    }
}

fn source_activity(state: SourceState) -> DeviceActivity {
    match state {
        SourceState::Running => DeviceActivity::Running,
        SourceState::Idle => DeviceActivity::Idle,
        SourceState::Suspended => DeviceActivity::Suspended,
        SourceState::Invalid => DeviceActivity::Unknown,
    }
}

fn lock_state(state: &Arc<Mutex<SoundState>>) -> MutexGuard<'_, SoundState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn update_connection(
    state: &Arc<Mutex<SoundState>>,
    connection: SoundConnection,
    subscription_active: bool,
    diagnostic: Option<String>,
) {
    let mut state = lock_state(state);
    state.connection = connection;
    state.subscription_active = subscription_active;
    state.diagnostic = diagnostic;
    if connection != SoundConnection::Ready {
        state.server_name = None;
        state.server_version = None;
        state.default_output_name = None;
        state.default_input_name = None;
        state.outputs.clear();
        state.inputs.clear();
        state.playback_streams.clear();
        state.recording_streams.clear();
    }
    state.generation = state.generation.saturating_add(1);
}

fn set_diagnostic(state: &Arc<Mutex<SoundState>>, diagnostic: &str) {
    let mut state = lock_state(state);
    state.diagnostic = Some(bounded_display_text(
        diagnostic,
        "Sound diagnostic unavailable.",
    ));
    state.generation = state.generation.saturating_add(1);
}

fn connection_wait_expired(elapsed: Duration) -> bool {
    elapsed >= CONNECT_TIMEOUT
}

fn snapshot_wait_expired(elapsed: Duration) -> bool {
    elapsed >= SNAPSHOT_TIMEOUT
}

fn callback_operation_wait_expired(elapsed: Duration) -> bool {
    elapsed >= CALLBACK_OPERATION_TIMEOUT
}

fn complete_callback_operation(
    pending: &Rc<RefCell<VecDeque<PendingCallbackOperation>>>,
    request_id: SoundRequestId,
) -> bool {
    let mut pending = pending.borrow_mut();
    let Some(position) = pending
        .iter()
        .position(|operation| operation.request_id == request_id)
    else {
        return false;
    };
    pending.remove(position).is_some()
}

fn expire_callback_operations(
    state: &Arc<Mutex<SoundState>>,
    pending: &Rc<RefCell<VecDeque<PendingCallbackOperation>>>,
) {
    let now = Instant::now();
    let expired = {
        let mut pending = pending.borrow_mut();
        let mut retained = VecDeque::with_capacity(pending.len());
        let mut expired = Vec::new();
        while let Some(operation) = pending.pop_front() {
            if callback_operation_wait_expired(now.saturating_duration_since(operation.started)) {
                expired.push(operation);
            } else {
                retained.push_back(operation);
            }
        }
        *pending = retained;
        expired
    };
    for operation in expired {
        finish_operation(
            state,
            operation.request_id,
            operation.operation,
            false,
            "The sound server did not confirm the requested change within five seconds.",
        );
    }
}

fn finish_callback_operations(
    state: &Arc<Mutex<SoundState>>,
    pending: &Rc<RefCell<VecDeque<PendingCallbackOperation>>>,
    message: &str,
) {
    let operations = pending.borrow_mut().drain(..).collect::<Vec<_>>();
    for operation in operations {
        finish_operation(
            state,
            operation.request_id,
            operation.operation,
            false,
            message,
        );
    }
}

fn finish_refresh_feedback(
    state: &Arc<Mutex<SoundState>>,
    pending: &mut VecDeque<(SoundRequestId, SoundOperationKind)>,
    success: bool,
    message: &str,
) {
    while let Some((request_id, operation)) = pending.pop_front() {
        finish_operation(state, request_id, operation, success, message);
    }
}

fn cancel_pending_test_start(
    state: &Arc<Mutex<SoundState>>,
    request_id: SoundRequestId,
    operation: SoundOperationKind,
    reported_running: bool,
    message: &str,
) {
    if !reported_running {
        finish_operation(state, request_id, operation, false, message);
    }
}

fn begin_operation(
    state: &Arc<Mutex<SoundState>>,
    request_id: SoundRequestId,
    operation: SoundOperationKind,
    message: &str,
) {
    set_operation(
        state,
        SoundOperationFeedback {
            request_id,
            operation,
            status: SoundOperationStatus::Pending,
            message: bounded_display_text(message, "Sound request pending."),
        },
    );
}

fn finish_operation(
    state: &Arc<Mutex<SoundState>>,
    request_id: SoundRequestId,
    operation: SoundOperationKind,
    success: bool,
    message: &str,
) {
    set_operation(
        state,
        SoundOperationFeedback {
            request_id,
            operation,
            status: if success {
                SoundOperationStatus::Succeeded
            } else {
                SoundOperationStatus::Failed
            },
            message: bounded_display_text(message, "Sound request finished."),
        },
    );
}

fn set_operation(state: &Arc<Mutex<SoundState>>, feedback: SoundOperationFeedback) {
    let mut state = lock_state(state);
    if state
        .last_operation
        .as_ref()
        .is_none_or(|current| current.request_id <= feedback.request_id)
    {
        state.last_operation = Some(feedback);
        state.generation = state.generation.saturating_add(1);
    }
}

fn set_speaker_test(state: &Arc<Mutex<SoundState>>, status: SoundTestState, error: Option<&str>) {
    let mut state = lock_state(state);
    state.speaker_test = status;
    if let Some(error) = error {
        state.diagnostic = Some(bounded_display_text(error, "Speaker test failed."));
    }
    state.generation = state.generation.saturating_add(1);
}

fn set_microphone_test(
    state: &Arc<Mutex<SoundState>>,
    status: SoundTestState,
    level: Option<MicrophoneLevel>,
    error: Option<&str>,
) {
    let mut state = lock_state(state);
    state.microphone_test = status;
    state.microphone_level = level;
    if let Some(error) = error {
        state.diagnostic = Some(bounded_display_text(error, "Microphone level test failed."));
    }
    state.generation = state.generation.saturating_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_volume_is_typed_and_bounded() {
        assert_eq!(UserVolumePercent::new(0).unwrap().get(), 0);
        assert_eq!(UserVolumePercent::new(150).unwrap().get(), 150);
        assert_eq!(
            UserVolumePercent::new(151),
            Err(SoundClientError::InvalidVolume(151))
        );
    }

    #[test]
    fn initial_state_has_no_capture_or_playback_test() {
        let state = SoundState::default();
        assert_eq!(state.speaker_test, SoundTestState::Idle);
        assert_eq!(state.microphone_test, SoundTestState::Idle);
        assert_eq!(state.microphone_level, None);
        assert!(state.playback_streams.is_empty());
        assert!(state.recording_streams.is_empty());
    }

    #[test]
    fn microphone_level_is_finite_bounded_and_truthful() {
        let mut bytes = Vec::new();
        for sample in [0.5_f32, -0.5, 0.5, -0.5] {
            bytes.extend_from_slice(&sample.to_ne_bytes());
        }
        let level = microphone_level(&bytes).unwrap();
        assert!((level.rms - 0.5).abs() < 0.000_001);
        assert!((level.dbfs - -6.020_599_9).abs() < 0.000_1);
        assert!((0.0..=1.0).contains(&level.meter_fraction));
        assert!(microphone_level(&[]).is_none());
    }

    #[test]
    fn speaker_test_is_stereo_bounded_and_has_separate_channels() {
        let samples = speaker_test_samples();
        assert_eq!(samples.len(), (TEST_RATE + TEST_RATE / 5) as usize * 4);
        assert!(samples.len() < 256 * 1024);
        let first_half_has_left = samples[..TEST_RATE as usize * 2]
            .chunks_exact(4)
            .any(|frame| i16::from_ne_bytes([frame[0], frame[1]]) != 0);
        let first_half_has_right = samples[..TEST_RATE as usize * 2]
            .chunks_exact(4)
            .any(|frame| i16::from_ne_bytes([frame[2], frame[3]]) != 0);
        let second_start = TEST_RATE as usize * 7 / 10 * 4;
        let second_has_right = samples[second_start..]
            .chunks_exact(4)
            .any(|frame| i16::from_ne_bytes([frame[2], frame[3]]) != 0);
        assert!(first_half_has_left);
        assert!(!first_half_has_right);
        assert!(second_has_right);
    }

    #[test]
    fn server_names_are_exact_or_rejected_never_truncated() {
        let mut diagnostics = Vec::new();
        assert_eq!(
            validated_server_name(Some("guest_sink"), "sink", &mut diagnostics),
            Some("guest_sink".into())
        );
        let oversized = "a".repeat(MAX_SERVER_NAME_BYTES + 1);
        assert_eq!(
            validated_server_name(Some(&oversized), "sink", &mut diagnostics),
            None
        );
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn display_text_is_sanitized_and_utf8_bounded() {
        let text = format!("hello\n{}", "界".repeat(300));
        let bounded = bounded_display_text(&text, "fallback");
        assert!(!bounded.contains('\n'));
        assert!(bounded.len() <= MAX_DISPLAY_TEXT_BYTES);
        assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
    }

    #[test]
    fn default_device_resolution_uses_exact_server_name() {
        let id = DeviceId {
            index: 4,
            name: "sink.a".into(),
        };
        let device = SoundDevice {
            id: id.clone(),
            description: "Output".into(),
            volume_raw: Volume::NORMAL.0,
            volume_percent: 100.0,
            channels: 2,
            muted: false,
            activity: DeviceActivity::Idle,
            active_port: None,
            monitor: false,
        };
        let mut state = SoundState {
            default_output_name: Some(id.name().into()),
            ..SoundState::default()
        };
        state.outputs.push(device);
        assert_eq!(state.default_output().unwrap().id, id);
    }

    #[test]
    fn connecting_or_authorizing_has_a_bounded_ready_deadline() {
        assert!(!connection_wait_expired(
            CONNECT_TIMEOUT - Duration::from_millis(1)
        ));
        assert!(connection_wait_expired(CONNECT_TIMEOUT));
        assert!(connection_wait_expired(
            CONNECT_TIMEOUT + Duration::from_secs(1)
        ));
    }

    #[test]
    fn snapshot_timeout_finishes_every_waiting_refresh_as_failed() {
        let state = Arc::new(Mutex::new(SoundState::default()));
        let request = SoundRequestId(10);
        begin_operation(&state, request, SoundOperationKind::Refresh, "pending");
        let mut pending = VecDeque::from([(request, SoundOperationKind::Refresh)]);
        assert!(snapshot_wait_expired(SNAPSHOT_TIMEOUT));
        finish_refresh_feedback(&state, &mut pending, false, "timed out");
        assert!(pending.is_empty());
        let feedback = lock_state(&state).last_operation.clone().unwrap();
        assert_eq!(feedback.request_id, request);
        assert_eq!(feedback.status, SoundOperationStatus::Failed);
    }

    #[test]
    fn callback_operations_complete_once_and_have_a_bounded_deadline() {
        assert!(!callback_operation_wait_expired(
            CALLBACK_OPERATION_TIMEOUT - Duration::from_millis(1)
        ));
        assert!(callback_operation_wait_expired(CALLBACK_OPERATION_TIMEOUT));
        let pending = Rc::new(RefCell::new(VecDeque::from([PendingCallbackOperation {
            request_id: SoundRequestId(41),
            operation: SoundOperationKind::SetOutputMute,
            started: Instant::now(),
        }])));
        assert!(complete_callback_operation(&pending, SoundRequestId(41)));
        assert!(!complete_callback_operation(&pending, SoundRequestId(41)));
        assert!(pending.borrow().is_empty());
    }

    #[test]
    fn repeated_refresh_requests_are_coalesced_without_orphaning_pending_state() {
        let state = Arc::new(Mutex::new(SoundState::default()));
        let first = SoundRequestId(20);
        let second = SoundRequestId(21);
        begin_operation(&state, first, SoundOperationKind::Refresh, "pending");
        begin_operation(&state, second, SoundOperationKind::Refresh, "pending");
        let mut pending = VecDeque::from([
            (first, SoundOperationKind::Refresh),
            (second, SoundOperationKind::Refresh),
        ]);
        finish_refresh_feedback(&state, &mut pending, true, "refreshed");
        assert!(pending.is_empty());
        let feedback = lock_state(&state).last_operation.clone().unwrap();
        assert_eq!(feedback.request_id, second);
        assert_eq!(feedback.status, SoundOperationStatus::Succeeded);
    }

    #[test]
    fn stopping_a_starting_test_resolves_its_start_request() {
        let state = Arc::new(Mutex::new(SoundState::default()));
        let request = SoundRequestId(30);
        begin_operation(
            &state,
            request,
            SoundOperationKind::StartMicrophoneTest,
            "starting",
        );
        cancel_pending_test_start(
            &state,
            request,
            SoundOperationKind::StartMicrophoneTest,
            false,
            "cancelled before capture",
        );
        let feedback = lock_state(&state).last_operation.clone().unwrap();
        assert_eq!(feedback.request_id, request);
        assert_eq!(feedback.status, SoundOperationStatus::Failed);
        assert!(feedback.message.contains("cancelled"));
    }
}
