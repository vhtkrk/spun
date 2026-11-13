use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait};
use sendspin::audio::decode::{Decoder, StreamDecoder};
use sendspin::audio::{
    AudioBuffer, CommitRejected, OutputError, PlayerEvent, PlayerSession, SyncedPlayer,
    SyncedPlayerConfig,
};
use sendspin::protocol::messages::{
    ArtworkChannel, ArtworkSource, ArtworkV1Support, AudioFormatSpec, GoodbyeReason, ImageFormat,
    Message, PlaybackState, PlayerState, PlayerStateCommand, PlayerV1Support, RepeatMode,
};
use sendspin::roles::{ArtworkEvent, ArtworkOutput};
use sendspin::sync::ClockSync;
use sendspin::{Connection, Controller, ProtocolClientBuilder};
use tokio::sync::{mpsc, watch};

use crate::config::{Config, State};
use crate::discovery;
use crate::icon::{Rgb, ServerColors};
use crate::other_media::Fade;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const MIN_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const VOLUME_STEP: u8 = 5;
/// How long the discovered list must stay the same before spun picks a lone
/// server. mDNS reports the servers one by one.
const DISCOVERY_SETTLE: Duration = Duration::from_secs(3);
/// Largest album art, in pixels, that spun asks the server for.
const ART_SIZE: u32 = 512;
/// The buffer that spun asks a sound server for, in frames: 100 ms at
/// 48 kHz. Without a size, the server buffers about 2 s, and each volume
/// change, mute, or fade takes as long to be heard. ALSA keeps its own
/// default, because it rejects a size outside the range of the device.
// ponytail: 100-150 ms in all on PipeWire and PulseAudio; tune on real
// hardware.
const SERVER_BUFFER_FRAMES: u32 = 4800;
/// How often spun asks the sound server for its default output, while spun
/// plays the system default.
const DEFAULT_OUTPUT_CHECK: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Link {
    /// spun knows no server yet. It waits for discovery or a menu pick.
    NoServer,
    #[default]
    Connecting,
    Connected,
    Disconnected,
}

/// What the tray shows. Volume and mute are the group values from the
/// controller role, not the local output gain.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub link: Link,
    /// The `ws://` or `wss://` URL of the current server. The tray opens the
    /// Music Assistant web UI on its host, also after a menu-driven switch.
    pub server_url: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub volume: u8,
    pub muted: bool,
    pub playing: bool,
    pub controllable: bool,
    pub supported_commands: Vec<String>,
    /// The repeat mode from the controller state; `None` when the server
    /// sends no mode.
    pub repeat: Option<RepeatMode>,
    /// Whether shuffle is on; `None` when the server sends no shuffle state.
    pub shuffle: Option<bool>,
    /// Why spun cannot play: the reason for a dropped connection, or a
    /// stream or device failure while connected. It clears when the cause
    /// goes away.
    pub error: Option<String>,
    pub art: Option<Art>,
    /// The configured output device; `None` means the system default.
    pub audio_device: Option<String>,
    /// The track position from the server; `None` when the server sends none.
    pub progress: Option<Progress>,
    /// The badge colours that the server picks for the track.
    pub colors: ServerColors,
}

/// The track position at one moment. spun moves it forward with the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// The position in milliseconds at `at`.
    pub position_ms: i64,
    /// The track length in milliseconds; 0 for a live stream.
    pub duration_ms: i64,
    /// The playback speed × 1000; 0 while paused.
    pub speed: i32,
    pub at: Instant,
}

impl Progress {
    /// The position at `now`, inside the track.
    pub fn position_ms(&self, now: Instant) -> i64 {
        let elapsed = now.saturating_duration_since(self.at).as_millis() as i64;
        (self.position_ms + elapsed * i64::from(self.speed) / 1000).clamp(0, self.duration_ms)
    }
}

/// Album art as PNG bytes from the server.
#[derive(Clone)]
pub struct Art(pub Arc<[u8]>);

impl std::fmt::Debug for Art {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Art({} bytes)", self.0.len())
    }
}

impl Art {
    pub fn ptr_eq(&self, other: &Art) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Snapshot {
    /// The snapshot before the server says anything: only the values that
    /// spun runs with.
    pub fn from_state(state: &State) -> Self {
        Self {
            link: match state.server_url {
                Some(_) => Link::Connecting,
                None => Link::NoServer,
            },
            volume: state.volume,
            server_url: state.server_url.clone(),
            audio_device: state.audio_device.clone(),
            ..Self::default()
        }
    }

    /// True when the server lets spun control `command` right now. MPRIS
    /// asks about `play` and `pause` separately, so this takes the
    /// controller command name rather than a `Command`.
    pub fn allows(&self, name: &str) -> bool {
        self.link == Link::Connected
            && self.controllable
            && self.supported_commands.iter().any(|s| s == name)
    }

    /// True when the server lets spun send `command` right now. A server
    /// switch is not a protocol command, so this is always false for it.
    pub fn supports(&self, command: &Command) -> bool {
        let Some(name) = command.protocol_name(self.playing) else {
            return false;
        };
        self.allows(name)
    }
}

/// Puts `command` on the queue, or logs a warning when the queue is full or
/// closed. ksni callbacks run synchronously on the D-Bus task, so they only
/// enqueue; the player task does the awaiting.
pub fn enqueue(tx: &CommandTx, command: Command) {
    if let Err(error) = tx.try_send(command) {
        log::warn!(
            "command queue unavailable; dropping {:?}",
            error.into_inner()
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    TogglePlayback,
    /// Plays or pauses, without toggling. Other media uses it to pause spun
    /// while it is silent and to play it again afterwards.
    SetPlaying(bool),
    Next,
    Previous,
    Stop,
    VolumeUp,
    VolumeDown,
    ToggleMute,
    /// Shuffle the queue, or restore its original order.
    SetShuffle(bool),
    /// Set the repeat mode.
    SetRepeat(RepeatMode),
    /// The user picked a discovered server in the tray menu; the payload is
    /// that server's `ws://` URL.
    SelectServer(String),
    /// The user picked an output device in the tray menu; `None` is the
    /// system default.
    SelectDevice(Option<String>),
}

impl Command {
    /// The controller command name, or `None` for menu-only commands.
    fn protocol_name(&self, playing: bool) -> Option<&'static str> {
        Some(match self {
            Self::SelectServer(_) | Self::SelectDevice(_) => return None,
            Self::TogglePlayback if playing => "pause",
            Self::TogglePlayback => "play",
            Self::SetPlaying(false) => "pause",
            Self::SetPlaying(true) => "play",
            Self::Next => "next",
            Self::Previous => "previous",
            Self::Stop => "stop",
            Self::VolumeUp | Self::VolumeDown => "volume",
            Self::ToggleMute => "mute",
            Self::SetShuffle(true) => "shuffle",
            Self::SetShuffle(false) => "unshuffle",
            Self::SetRepeat(RepeatMode::Off) => "repeat_off",
            Self::SetRepeat(RepeatMode::One) => "repeat_one",
            Self::SetRepeat(RepeatMode::All) => "repeat_all",
        })
    }
}

pub const COMMAND_CAPACITY: usize = 32;
pub type SnapshotTx = watch::Sender<Snapshot>;
pub type SnapshotRx = watch::Receiver<Snapshot>;
pub type DiscoveredRx = watch::Receiver<Vec<discovery::Server>>;
pub type CommandTx = mpsc::Sender<Command>;
pub type CommandRx = mpsc::Receiver<Command>;

pub async fn shutdown_requested(mut rx: watch::Receiver<bool>) {
    if rx.wait_for(|shutdown| *shutdown).await.is_err() {
        log::debug!("shutdown sender closed");
    }
}

fn volume_after(volume: u8, command: &Command) -> u8 {
    match command {
        Command::VolumeUp => volume.saturating_add(VOLUME_STEP).min(100),
        Command::VolumeDown => volume.saturating_sub(VOLUME_STEP),
        _ => volume,
    }
}

/// How a connected session or an `idle` wait ended.
enum SessionEnd {
    /// Shutdown was requested or the command queue closed.
    Shutdown,
    /// The state names another server: a menu pick or a config edit.
    Switch,
    /// A config edit changed what spun tells the server when it connects:
    /// the client ID, the name, or the static delay.
    Restart,
    /// The connection dropped; the payload says why.
    Lost(String),
}

/// What `run` needs from the start of the process.
pub struct Startup {
    pub edits: watch::Receiver<Config>,
    pub state: State,
    pub state_path: PathBuf,
    pub fade: Fade,
}

/// The settings that the player runs with: the config as last applied, and
/// the state, which spun saves to `state_path` after each change.
struct Settings {
    config: Config,
    /// Each config edit arrives here.
    edits: watch::Receiver<Config>,
    state: State,
    state_path: PathBuf,
    /// The gain that other media turns down while it plays.
    fade: Fade,
}

impl Settings {
    /// Saves the state. A failure only logs: the values still apply for this
    /// run.
    fn save(&self) {
        if let Err(error) = self.state.save(&self.state_path) {
            log::warn!("saving the state failed: {error:#}");
        }
    }

    /// Applies `change` to the state and saves the state. Returns true when
    /// the state changed.
    fn remember(&mut self, change: impl FnOnce(&mut State)) -> bool {
        let before = self.state.clone();
        change(&mut self.state);
        let changed = self.state != before;
        if changed {
            self.save();
        }
        changed
    }

    /// Points the state at `url`. Returns true when that is another server.
    fn select_server(&mut self, url: String) -> bool {
        if let Err(error) = crate::config::check_server_url(&url) {
            log::warn!("cannot switch to {url}: {error}");
            return false;
        }
        let changed = self.remember(|state| state.server_url = Some(url.clone()));
        if changed {
            log::info!("switching to {url}");
        }
        changed
    }

    /// Remembers the output device that the user picked. Returns false when
    /// `name` is the current device already.
    fn select_device(&mut self, name: Option<String>) -> bool {
        let label = name.clone().unwrap_or_else(|| "system default".into());
        let changed = self.remember(|state| state.audio_device = name);
        if changed {
            log::info!("audio device: {label}");
        }
        changed
    }

    /// Applies the latest config edit. Returns how the current session must
    /// end, or `None` when it can go on.
    fn edit(&mut self) -> Option<SessionEnd> {
        let config = self.edits.borrow_and_update().clone();
        let before = self.state.clone();
        self.state.pin(&config, &self.config);
        let renamed = config.name != self.config.name;
        self.config = config;
        if self.state != before {
            self.save();
        }
        if self.state.server_url != before.server_url {
            Some(SessionEnd::Switch)
        } else if renamed
            || self.state.client_id != before.client_id
            || self.state.static_delay_ms != before.static_delay_ms
        {
            Some(SessionEnd::Restart)
        } else {
            None
        }
    }
}

/// Connects to the server of the state and keeps the connection up until
/// shutdown. Without a server, spun waits for `discovered` to list exactly
/// one server, or for a pick in the menu. Each config edit arrives on
/// `edits`, and each change to the state goes to `state_path`.
pub async fn run(
    startup: Startup,
    tx: SnapshotTx,
    discovered: DiscoveredRx,
    mut commands: CommandRx,
    shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let Startup {
        mut edits,
        mut state,
        state_path,
        fade,
    } = startup;
    // At startup, each value that the config sets wins over the state.
    let config = edits.borrow_and_update().clone();
    state.pin(&config, &Config::default());
    // The save keeps a new client ID, so the server sees the same client
    // after a restart.
    state.save(&state_path)?;
    let mut settings = Settings {
        config,
        edits,
        state,
        state_path,
        fade,
    };
    let mut delay = MIN_RETRY_DELAY;
    let shutdown = shutdown_requested(shutdown_rx);
    tokio::pin!(shutdown);
    loop {
        tx.send_replace(Snapshot::from_state(&settings.state));
        let Some(server_url) = settings.state.server_url.clone() else {
            log::info!("no server yet; waiting for a lone discovered server or a menu pick");
            let found = only_server(discovered.clone());
            match idle(found, &mut settings, &tx, &mut commands, &mut shutdown).await {
                Ok(url) => {
                    settings.select_server(url);
                }
                Err(SessionEnd::Shutdown) => return Ok(()),
                Err(_) => {}
            }
            continue;
        };
        let builder = builder(&settings.state, &settings.config.name);
        let client = tokio::time::timeout(CONNECT_TIMEOUT, builder.connect(server_url));
        let error = match idle(client, &mut settings, &tx, &mut commands, &mut shutdown).await {
            Ok(Ok(Ok(client))) => {
                let connected_at = tokio::time::Instant::now();
                let connection = client.split();
                let end =
                    session(connection, &mut settings, &tx, &mut commands, &mut shutdown).await;
                if connected_at.elapsed() >= MAX_RETRY_DELAY {
                    delay = MIN_RETRY_DELAY;
                }
                match end {
                    SessionEnd::Shutdown => return Ok(()),
                    SessionEnd::Lost(error) => error,
                    SessionEnd::Switch | SessionEnd::Restart => {
                        delay = MIN_RETRY_DELAY;
                        continue;
                    }
                }
            }
            Ok(Ok(Err(error))) => error.to_string(),
            Ok(Err(_)) => "connection timed out".into(),
            Err(SessionEnd::Shutdown) => return Ok(()),
            Err(_) => {
                delay = MIN_RETRY_DELAY;
                continue;
            }
        };
        log::warn!("Sendspin disconnected: {error}");
        tx.send_replace(Snapshot {
            link: Link::Disconnected,
            error: Some(error),
            ..Snapshot::from_state(&settings.state)
        });
        let retry = tokio::time::sleep(delay);
        match idle(retry, &mut settings, &tx, &mut commands, &mut shutdown).await {
            Ok(()) => {}
            Err(SessionEnd::Shutdown) => return Ok(()),
            Err(_) => {
                delay = MIN_RETRY_DELAY;
                continue;
            }
        }
        delay = (delay * 2).min(MAX_RETRY_DELAY);
    }
}

/// Runs `until` while spun has no session, and returns its output. A
/// server pick, or a config edit that changes the server or the client,
/// ends the wait early. A pick of the same server ends it too, so spun
/// tries again at once. A shutdown ends the wait with
/// `SessionEnd::Shutdown`. A device pick is saved, and spun drops the other
/// commands.
async fn idle<T>(
    until: impl std::future::Future<Output = T>,
    settings: &mut Settings,
    tx: &SnapshotTx,
    commands: &mut CommandRx,
    shutdown: &mut std::pin::Pin<&mut impl std::future::Future<Output = ()>>,
) -> Result<T, SessionEnd> {
    tokio::pin!(until);
    loop {
        tokio::select! {
            _ = shutdown.as_mut() => return Err(SessionEnd::Shutdown),
            output = &mut until => return Ok(output),
            Ok(()) = settings.edits.changed() => {
                if let Some(end) = settings.edit() {
                    return Err(end);
                }
            }
            command = commands.recv() => match command {
                None => return Err(SessionEnd::Shutdown),
                Some(Command::SelectServer(url)) => {
                    settings.select_server(url);
                    return Err(SessionEnd::Switch);
                }
                Some(Command::SelectDevice(name)) => {
                    settings.select_device(name);
                }
                Some(command) => log::warn!("ignoring {command:?}: not connected"),
            }
        }
        // A device pick or an edit can change the device.
        let device = &settings.state.audio_device;
        tx.send_if_modified(|snapshot| {
            let changed = snapshot.audio_device != *device;
            snapshot.audio_device.clone_from(device);
            changed
        });
    }
}

/// Returns the URL of the only discovered server, after the list has kept
/// that one server for `DISCOVERY_SETTLE`. After two or more servers turn
/// up, this never returns, so that the user picks one in the menu.
// ponytail: a server that turns up more than DISCOVERY_SETTLE after the
// first one comes too late. The user can still switch in the menu.
async fn only_server(mut discovered: DiscoveredRx) -> String {
    loop {
        let only = match discovered.borrow_and_update().as_slice() {
            [] => None,
            [server] => Some(server.url.clone()),
            _ => break,
        };
        let changed = match only {
            Some(url) => match tokio::time::timeout(DISCOVERY_SETTLE, discovered.changed()).await {
                Ok(changed) => changed,
                Err(_) => return url,
            },
            None => discovered.changed().await,
        };
        // An error means that discovery has stopped.
        if changed.is_err() {
            break;
        }
    }
    std::future::pending().await
}

/// An output device for the tray menu. `name` is the value for
/// `audio_device` in the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDevice {
    pub name: String,
    pub label: String,
}

/// The audio host for the whole run. On a sound server, each
/// `cpal::default_host()` call opens a connection to the server, and a
/// dropped host keeps its connection open. So spun keeps one host, and makes
/// a new one only when the server goes away, for example in a restart.
fn audio_host() -> Arc<cpal::Host> {
    static HOST: std::sync::Mutex<Option<Arc<cpal::Host>>> = std::sync::Mutex::new(None);
    let mut kept = HOST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(host) = kept.as_ref()
        && !server_gone(host)
    {
        return Arc::clone(host);
    }
    Arc::clone(kept.insert(Arc::new(cpal::default_host())))
}

/// True when the sound server behind `host` went away. The connection of the
/// host is closed then, so a new host leaks nothing. Other errors, such as a
/// reply that cpal cannot read, keep the host.
fn server_gone(host: &cpal::Host) -> bool {
    on_sound_server(host)
        && host
            .devices()
            .is_err_and(|error| error.kind() == cpal::ErrorKind::StreamInvalidated)
}

/// True when `host` plays through a sound server, PulseAudio or PipeWire,
/// rather than straight to ALSA.
fn on_sound_server(host: &cpal::Host) -> bool {
    // The name, because cpal has the PulseAudio host only on Linux.
    host.id().name() == "PulseAudio"
}

/// The output devices of the audio host. On ALSA, the names are the ones
/// that `aplay -L` lists. On a sound server, they are its sink names.
pub fn output_devices() -> Vec<OutputDevice> {
    let devices = match audio_host().output_devices() {
        Ok(devices) => devices,
        Err(error) => {
            log::warn!("listing audio devices failed: {error}");
            return Vec::new();
        }
    };
    devices
        .filter_map(|device| {
            let name = device.id().ok()?.id().to_string();
            // ALSA gives one description to many names, so show both.
            let label = match device.description() {
                Ok(description) if description.name() != name => {
                    format!("{} ({name})", description.name())
                }
                _ => name.clone(),
            };
            Some(OutputDevice { name, label })
        })
        .collect()
}

/// The device named `name` on `host`, or the system default for `None`.
fn output_device(host: &cpal::Host, name: Option<&str>) -> anyhow::Result<cpal::Device> {
    match name {
        Some(name) => host
            .device_by_id(&cpal::DeviceId::new(host.id(), name))
            .with_context(|| format!("audio device {name} not found")),
        None => host
            .default_output_device()
            .context("no default audio output"),
    }
}

fn builder(state: &State, name: &str) -> ProtocolClientBuilder {
    let supported_formats = [
        ("flac", 24),
        ("flac", 16),
        ("opus", 16),
        ("pcm", 24),
        ("pcm", 16),
    ]
    .into_iter()
    .map(|(codec, bit_depth)| AudioFormatSpec {
        codec: codec.into(),
        channels: 2,
        sample_rate: 48_000,
        bit_depth,
    })
    .collect();
    ProtocolClientBuilder::builder()
        .client_id(state.client_id.clone())
        .name(name.to_owned())
        // Without a manufacturer, sendspin reports "Sendspin" as the maker.
        .product_name(Some(env!("CARGO_PKG_NAME").into()))
        .manufacturer(Some(env!("CARGO_PKG_NAME").into()))
        .software_version(Some(env!("CARGO_PKG_VERSION").into()))
        .player_v1_support(PlayerV1Support {
            supported_formats,
            buffer_capacity: 50 * 1024 * 1024,
            supported_commands: vec!["volume".into(), "mute".into()],
        })
        .initial_player_state(PlayerState {
            volume: Some(state.volume),
            muted: Some(false),
            static_delay_ms: Some(state.static_delay_ms),
            supported_commands: Some(vec![PlayerStateCommand::SetStaticDelay]),
            ..PlayerState::default()
        })
        // ksni `icon_data` accepts only PNG, so the album art is PNG too.
        .artwork_v1_support(ArtworkV1Support {
            channels: vec![ArtworkChannel {
                source: ArtworkSource::Album,
                format: ImageFormat::Png,
                media_width: ART_SIZE,
                media_height: ART_SIZE,
            }],
        })
        .metadata()
        .controller()
        .color()
        .build()
}

/// The clock that the audio output follows.
type SharedClock = Arc<parking_lot::Mutex<ClockSync>>;

/// The player role of one connection. The sendspin session applies the
/// server's volume, mute, and delay commands to the output and reports them
/// back. spun decodes the audio and owns the device.
struct Player {
    session: PlayerSession,
    clock: SharedClock,
    /// The decoder for the current stream; `None` between streams.
    decoder: Option<StreamDecoder>,
    /// The open device. The session holds its control handle; a drop closes
    /// the device.
    output: Option<SyncedPlayer>,
    /// The gain that other media turns down while it plays.
    fade: Fade,
    /// The output that was the default of the sound server when the device
    /// opened, while spun plays the system default there. A stream stays on
    /// that output, so spun opens the device again when the default changes.
    default_output: Option<cpal::DeviceId>,
}

impl Player {
    /// Handles one player event. An error is for the menu and the badge:
    /// spun then plays nothing until the next stream start or device pick.
    fn handle(&mut self, event: PlayerEvent, device: Option<&str>) -> anyhow::Result<()> {
        match event {
            PlayerEvent::Start(config) => {
                log::info!(
                    "stream start: {} {} Hz {} ch {} bit",
                    config.codec,
                    config.sample_rate,
                    config.channels,
                    config.bit_depth
                );
                let decoder = match StreamDecoder::new(&config) {
                    Ok(decoder) => decoder,
                    Err(error) => {
                        self.close();
                        return Err(anyhow::Error::new(error).context("cannot play stream"));
                    }
                };
                // The device ignores the codec and the bit depth. So when the
                // rate and the channels match, the device and its queued
                // audio stay.
                let fits = self.output.as_ref().is_some_and(|output| {
                    output.format().sample_rate == decoder.format().sample_rate
                        && output.format().channels == decoder.format().channels
                });
                self.decoder = Some(decoder);
                if !fits {
                    self.open(device).context("audio output failed")?;
                }
            }
            PlayerEvent::Audio(work) => {
                let (Some(decoder), Some(_)) = (&self.decoder, &self.output) else {
                    return Ok(());
                };
                let samples = match decoder.decode(work.data()) {
                    Ok(samples) => samples,
                    Err(error) => {
                        log::warn!("decode failed: {error}");
                        return Ok(());
                    }
                };
                let buffer = AudioBuffer {
                    timestamp: work.timestamp(),
                    samples,
                    format: decoder.format().clone(),
                };
                // A failed device comes back as the next `OutputLost` event.
                if let Err(CommitRejected::Output(error)) = work.commit(buffer) {
                    log::warn!("audio output rejected a buffer: {error}");
                }
            }
            // The session clears the output itself.
            PlayerEvent::Clear => {}
            PlayerEvent::End => self.close(),
            // ponytail: opens the device again after each loss. A device that
            // opens and then fails at once makes this repeat; count the
            // attempts if that happens.
            PlayerEvent::OutputLost(error) => {
                log::warn!("audio output lost: {error}");
                self.output = None;
                self.open(device).context("audio output failed")?;
            }
        }
        Ok(())
    }

    /// Opens `device` for the current stream and installs it in the
    /// session, which replays its controls to the device. Without a stream,
    /// this does nothing.
    fn open(&mut self, device: Option<&str>) -> anyhow::Result<()> {
        let Some(decoder) = &self.decoder else {
            return Ok(());
        };
        let format = decoder.format().clone();
        // Close the old device first: ALSA opens a hardware device only once.
        self.release();
        let host = audio_host();
        let output_device = output_device(&host, device)?;
        let server = on_sound_server(&host);
        let default_output = if server && device.is_none() {
            output_device.id().ok()
        } else {
            None
        };
        let controls = self.session.controls().borrow().clone();
        let config = SyncedPlayerConfig {
            // With a device, sendspin opens no host, and so no connection to
            // the sound server, of its own.
            device: Some(output_device),
            volume: controls.volume,
            muted: controls.muted,
            buffer_size: server.then_some(SERVER_BUFFER_FRAMES),
        };
        // The fade sits after the server's volume and mute, so that the
        // server keeps reporting the volume that it set.
        let output = SyncedPlayer::with_process_callback(
            format,
            Arc::clone(&self.clock),
            config,
            self.fade.callback(),
        )?;
        self.session
            .begin_output_install()?
            .install(output.control())?;
        self.output = Some(output);
        self.default_output = default_output;
        Ok(())
    }

    /// True when the open device plays the system default of the sound
    /// server, and the server has another default output now.
    fn default_output_changed(&self) -> bool {
        let (Some(_), Some(opened)) = (&self.output, &self.default_output) else {
            return false;
        };
        audio_host()
            .default_output_device()
            .and_then(|device| device.id().ok())
            .is_some_and(|current| current != *opened)
    }

    /// Opens `device` for the current stream after a menu pick or a config
    /// edit, and shows the outcome in `snapshot`.
    fn reopen(&mut self, device: Option<&str>, snapshot: &mut Snapshot) {
        snapshot.audio_device = device.map(Into::into);
        // ponytail: the audio queued in the old device is lost, so playback
        // stops for about the server's lead time.
        let opened = self.open(device).context("audio output failed");
        // Without a stream, nothing opens, and a stream that spun cannot
        // decode keeps its error.
        if self.decoder.is_some() {
            snapshot.error = failure(opened);
        }
    }

    /// Detaches the output and closes the device. The session keeps the
    /// audio that it has not handed out yet.
    fn release(&mut self) {
        if let Err(error) = self.session.release_output() {
            log::debug!("releasing the audio output failed: {error}");
        }
        self.output = None;
    }

    /// Ends the stream. The device stays closed until the next stream start.
    fn close(&mut self) {
        self.release();
        self.decoder = None;
    }
}

/// The artwork session needs an output. spun takes the art from the session
/// events, so this output keeps nothing.
struct NoArtworkOutput;

impl ArtworkOutput for NoArtworkOutput {
    type Prepared = ();

    fn commit(&self, _channel: u8, _prepared: ()) -> Result<(), OutputError> {
        Ok(())
    }

    fn end(&self) -> Result<(), OutputError> {
        Ok(())
    }
}

async fn session(
    connection: Connection,
    settings: &mut Settings,
    tx: &SnapshotTx,
    commands: &mut CommandRx,
    shutdown: &mut std::pin::Pin<&mut impl std::future::Future<Output = ()>>,
) -> SessionEnd {
    let Connection {
        mut messages,
        player,
        artwork,
        clock_sync,
        controller,
        guard,
        ..
    } = connection;
    let mut snapshot = Snapshot {
        link: Link::Connected,
        controllable: controller.is_some(),
        ..Snapshot::from_state(&settings.state)
    };
    tx.send_replace(snapshot.clone());
    let mut player = Player {
        session: player.attach_empty(),
        clock: clock_sync,
        decoder: None,
        output: None,
        fade: settings.fade.clone(),
        default_output: None,
    };
    let mut controls = player.session.controls();
    let mut artwork = artwork.attach(NoArtworkOutput);
    let mut default_check = tokio::time::interval(DEFAULT_OUTPUT_CHECK);
    default_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let end = loop {
        tokio::select! {
            _ = shutdown.as_mut() => break SessionEnd::Shutdown,
            // The role sessions take the stream messages and the player
            // commands, so this channel carries the rest.
            message = messages.recv() => {
                let Some(message) = message else {
                    return SessionEnd::Lost("server closed the connection".into());
                };
                apply_message(&mut snapshot, message, |time| {
                    player.clock.lock().server_to_local_instant(time)
                });
                tx.send_replace(snapshot.clone());
            }
            // A `Some(..)` pattern turns its branch off when the session
            // closes. A stream start, a stream end, and a lost device decide
            // whether spun can play. Audio and clear events change nothing.
            Some(event) = player.session.recv() => {
                let settles = !matches!(event, PlayerEvent::Audio(_) | PlayerEvent::Clear);
                let result = player.handle(event, settings.state.audio_device.as_deref());
                if settles {
                    snapshot.error = failure(result);
                    tx.send_replace(snapshot.clone());
                }
            }
            // The session applies the server's volume and delay itself.
            // spun remembers both: the next start begins at that volume, and
            // the spec says that a client must keep the delay across
            // reconnects and restarts.
            Ok(()) = controls.changed() => {
                let (volume, delay_ms) = {
                    let controls = controls.borrow_and_update();
                    (controls.volume, controls.static_delay_ms)
                };
                settings.remember(|state| {
                    state.volume = volume;
                    state.static_delay_ms = delay_ms;
                });
            }
            Some(event) = artwork.recv() => {
                // ponytail: shows the art on arrival and ignores the frame
                // timestamp, so the art can change a moment before the audio
                // does.
                snapshot.art = match event {
                    ArtworkEvent::Data(work) if !work.is_clear() => {
                        Some(Art(Arc::from(work.data())))
                    }
                    ArtworkEvent::Data(_) | ArtworkEvent::End => None,
                    ArtworkEvent::Start(_) => continue,
                };
                tx.send_replace(snapshot.clone());
            }
            // ponytail: asks the sound server every 2 s, because cpal's
            // PulseAudio host keeps a stream on the output that was the
            // default when it opened. Remove this when cpal lets the stream
            // follow the default.
            _ = default_check.tick(), if player.default_output.is_some() => {
                if player.default_output_changed() {
                    log::info!("the default audio output changed; switching to it");
                    player.reopen(None, &mut snapshot);
                    tx.send_replace(snapshot.clone());
                }
            }
            Ok(()) = settings.edits.changed() => {
                let device = settings.state.audio_device.clone();
                if let Some(end) = settings.edit() {
                    break end;
                }
                if settings.state.audio_device != device {
                    player.reopen(settings.state.audio_device.as_deref(), &mut snapshot);
                    tx.send_replace(snapshot.clone());
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break SessionEnd::Shutdown };
                match command {
                    // A switch ends the session; `run` connects to the new
                    // server.
                    Command::SelectServer(url) => {
                        if settings.select_server(url) {
                            break SessionEnd::Switch;
                        }
                        continue;
                    }
                    Command::SelectDevice(name) => {
                        if settings.select_device(name) {
                            player.reopen(settings.state.audio_device.as_deref(), &mut snapshot);
                            tx.send_replace(snapshot.clone());
                        }
                        continue;
                    }
                    _ => {}
                }
                if !snapshot.supports(&command) { continue; }
                let Some(controller) = &controller else { continue; };
                let result = tokio::select! {
                    _ = shutdown.as_mut() => break SessionEnd::Shutdown,
                    result = tokio::time::timeout(
                        COMMAND_TIMEOUT, send_command(controller, &snapshot, &command)
                    ) => result,
                };
                // A failed command is a single event, and nothing clears it
                // later, so it only goes to the log. A sent command changes
                // nothing either: the menu waits for the server state.
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => log::warn!("command failed: {error}"),
                    Err(_) => log::warn!("command timed out"),
                }
            }
        }
    };
    // Close the audio device before saying goodbye.
    drop(player);
    drop(artwork);
    let reason = match end {
        SessionEnd::Switch => GoodbyeReason::AnotherServer,
        SessionEnd::Restart => GoodbyeReason::Restart,
        _ => GoodbyeReason::Shutdown,
    };
    match tokio::time::timeout(COMMAND_TIMEOUT, guard.disconnect(reason)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::warn!("graceful disconnect failed: {error}"),
        Err(_) => log::warn!("graceful disconnect timed out"),
    }
    end
}

/// The text of a failure for the menu and the badge, or `None` for a
/// success. This also logs the failure.
fn failure(result: anyhow::Result<()>) -> Option<String> {
    let error = format!("{:#}", result.err()?);
    log::warn!("{error}");
    Some(error)
}

/// Applies a server message to `snapshot`. `to_local` turns a server time
/// into a local instant; it gives `None` before the clock is in sync.
fn apply_message(
    snapshot: &mut Snapshot,
    message: Message,
    to_local: impl Fn(i64) -> Option<Instant>,
) {
    match message {
        Message::ServerState(state) => {
            if let Some(metadata) = state.metadata {
                snapshot.title = metadata.title;
                snapshot.artist = metadata.artist;
                snapshot.album = metadata.album;
                // Before the clock is in sync, the arrival time stands in for
                // the server time.
                let at = to_local(metadata.timestamp).unwrap_or_else(Instant::now);
                snapshot.progress = metadata.progress.map(|progress| Progress {
                    position_ms: progress.track_progress,
                    duration_ms: progress.track_duration,
                    speed: progress.playback_speed,
                    at,
                });
            }
            // ponytail: applies the colours on arrival and ignores their
            // timestamp, so the badge can change a moment before the audio.
            if let Some(color) = state.color {
                let rgb = |color: Option<[u8; 3]>| color.map(|[r, g, b]| Rgb::new(r, g, b));
                // The server sends the whole palette each time, so a missing
                // colour clears the old one.
                snapshot.colors = ServerColors {
                    primary: rgb(color.primary),
                    secondary: rgb(color.accent),
                };
            }
            if let Some(controller) = state.controller {
                if controller.volume > 100 {
                    log::warn!(
                        "ignoring the server volume {}: outside 0–100",
                        controller.volume
                    );
                    return;
                }
                snapshot.volume = controller.volume;
                snapshot.muted = controller.muted;
                snapshot.supported_commands = controller.supported_commands;
                // The server sends the whole controller state each time, so a
                // missing mode clears the old one.
                snapshot.repeat = controller.repeat;
                snapshot.shuffle = controller.shuffle;
            }
        }
        Message::GroupUpdate(update) => {
            if let Some(state) = update.playback_state {
                snapshot.playing = match state {
                    PlaybackState::Playing => true,
                    PlaybackState::Stopped => false,
                };
            }
        }
        _ => {}
    }
}

async fn send_command(
    controller: &Controller,
    snapshot: &Snapshot,
    command: &Command,
) -> sendspin::Result<()> {
    match command {
        // `session` handles the menu-only commands before it calls this function.
        Command::SelectServer(_) | Command::SelectDevice(_) => {
            unreachable!("a menu-only command is not a controller command")
        }
        Command::TogglePlayback if snapshot.playing => controller.pause().await,
        Command::TogglePlayback => controller.play().await,
        Command::SetPlaying(false) => controller.pause().await,
        Command::SetPlaying(true) => controller.play().await,
        Command::Next => controller.next().await,
        Command::Previous => controller.previous().await,
        Command::Stop => controller.stop().await,
        Command::VolumeUp | Command::VolumeDown => {
            controller
                .set_volume(volume_after(snapshot.volume, command))
                .await
        }
        Command::ToggleMute => controller.set_mute(!snapshot.muted).await,
        Command::SetShuffle(enabled) => controller.shuffle(*enabled).await,
        Command::SetRepeat(mode) => controller.repeat(mode.clone()).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as Frame;

    #[tokio::test]
    async fn shutdown_already_requested_is_not_missed() {
        let (_tx, rx) = watch::channel(true);
        shutdown_requested(rx).await;
    }

    #[test]
    fn the_position_moves_with_the_clock_and_stops_at_the_end() {
        let at = Instant::now();
        let progress = Progress {
            position_ms: 0,
            duration_ms: 190_000,
            speed: 1000,
            at,
        };
        let position = |seconds| progress.position_ms(at + Duration::from_secs(seconds));
        assert_eq!(position(100), 100_000);
        assert_eq!(position(500), 190_000);
        let paused = Progress {
            speed: 0,
            ..progress
        };
        assert_eq!(paused.position_ms(at + Duration::from_secs(60)), 0);
    }

    #[test]
    fn volume_is_bounded() {
        assert_eq!(volume_after(98, &Command::VolumeUp), 100);
        assert_eq!(volume_after(2, &Command::VolumeDown), 0);
    }

    #[test]
    fn spun_keeps_one_audio_host() {
        assert!(Arc::ptr_eq(&audio_host(), &audio_host()));
    }

    #[test]
    fn playing_on_and_off_needs_the_same_commands_as_the_toggle() {
        assert_eq!(Command::SetPlaying(true).protocol_name(false), Some("play"));
        assert_eq!(
            Command::SetPlaying(false).protocol_name(true),
            Some("pause")
        );
        // Kinds of playback do not depend on the current state.
        assert_eq!(
            Command::SetPlaying(false).protocol_name(false),
            Some("pause")
        );
        assert_eq!(Command::SetPlaying(true).protocol_name(true), Some("play"));
    }

    #[test]
    fn controls_need_a_connection_the_role_and_the_current_command() {
        let snapshot = Snapshot {
            link: Link::Connected,
            controllable: true,
            supported_commands: vec!["play".into(), "volume".into()],
            ..Snapshot::default()
        };
        assert!(snapshot.supports(&Command::TogglePlayback));
        assert!(snapshot.supports(&Command::VolumeUp));
        assert!(!snapshot.supports(&Command::Next));
        let playing = Snapshot {
            playing: true,
            ..snapshot.clone()
        };
        assert!(!playing.supports(&Command::TogglePlayback));
        let no_role = Snapshot {
            controllable: false,
            ..snapshot.clone()
        };
        assert!(!no_role.supports(&Command::TogglePlayback));
        let disconnected = Snapshot {
            link: Link::Disconnected,
            ..snapshot
        };
        assert!(!disconnected.supports(&Command::TogglePlayback));
    }

    #[test]
    fn advertises_player_metadata_controller_and_color() {
        let builder = builder(&State::default(), "spun");
        assert_eq!(
            builder.supported_roles(),
            [
                "player@v1",
                "artwork@v1",
                "metadata@v1",
                "controller@v1",
                "color@v1"
            ]
        );
    }

    #[test]
    fn the_server_colours_replace_the_whole_palette() {
        use sendspin::protocol::messages::{ColorState, ServerState};

        let colors = |primary, accent| {
            Message::ServerState(ServerState {
                metadata: None,
                controller: None,
                color: Some(ColorState {
                    timestamp: 0,
                    background_dark: None,
                    background_light: None,
                    primary,
                    accent,
                    on_dark: None,
                    on_light: None,
                }),
            })
        };
        let mut snapshot = Snapshot::default();
        apply_message(
            &mut snapshot,
            colors(Some([1, 2, 3]), Some([4, 5, 6])),
            |_| None,
        );
        assert_eq!(
            snapshot.colors,
            ServerColors {
                primary: Some(Rgb::new(1, 2, 3)),
                secondary: Some(Rgb::new(4, 5, 6)),
            }
        );
        apply_message(&mut snapshot, colors(Some([7, 8, 9]), None), |_| None);
        assert_eq!(snapshot.colors.secondary, None);
        // A state without colours keeps the palette.
        apply_message(
            &mut snapshot,
            Message::ServerState(ServerState {
                metadata: None,
                controller: None,
                color: None,
            }),
            |_| None,
        );
        assert_eq!(snapshot.colors.primary, Some(Rgb::new(7, 8, 9)));
    }

    /// A `run` and the server side of its connection.
    struct TestServer {
        socket: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        run: Run,
    }

    /// The parts of a `run` that a test drives.
    struct Run {
        state_dir: tempfile::TempDir,
        snapshots: SnapshotRx,
        commands: CommandTx,
        shutdown: watch::Sender<bool>,
        config: watch::Sender<Config>,
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl Run {
        fn saved_state(&self) -> State {
            State::load(&self.state_dir.path().join("state.toml")).unwrap()
        }

        async fn finish(self) {
            self.shutdown.send_replace(true);
            tokio::time::timeout(Duration::from_secs(2), self.task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }

    /// Starts `run` with `server_url` in a throwaway state file and the
    /// default config. Discovery finds no servers.
    fn spawn_run(server_url: Option<String>) -> Run {
        let state_dir = tempfile::tempdir().unwrap();
        let state = State {
            server_url,
            ..State::default()
        };
        let (config, edits) = watch::channel(Config::default());
        let (tx, snapshots) = watch::channel(Snapshot::default());
        let (_, discovered) = watch::channel(Vec::new());
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run(
            Startup {
                edits,
                state,
                state_path: state_dir.path().join("state.toml"),
                fade: Fade::new(),
            },
            tx,
            discovered,
            command_rx,
            shutdown_rx,
        ));
        Run {
            state_dir,
            snapshots,
            commands,
            shutdown,
            config,
            task,
        }
    }

    async fn listen() -> (tokio::net::TcpListener, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/sendspin", listener.local_addr().unwrap());
        (listener, url)
    }

    impl TestServer {
        async fn connect(roles: &[&str]) -> Self {
            let (listener, url) = listen().await;
            Self::serve(&listener, url, roles).await
        }

        /// Starts `run` against `url` and completes the handshake on the
        /// first connection to `listener`.
        async fn serve(listener: &tokio::net::TcpListener, url: String, roles: &[&str]) -> Self {
            let run = spawn_run(Some(url));
            let mut server = Self {
                socket: accept(listener).await,
                run,
            };
            server.handshake(roles).await;
            server
        }

        /// Answers `client/hello` and returns it with the first
        /// `client/state`.
        async fn handshake(&mut self, roles: &[&str]) -> (serde_json::Value, serde_json::Value) {
            let hello = self.receive().await;
            assert_eq!(hello["type"], "client/hello");
            self.send(serde_json::json!({
                "type": "server/hello",
                "payload": {"server_id": "test-server", "name": "Test", "version": 1,
                    "active_roles": roles, "connection_reason": "playback"}
            }))
            .await;
            let state = self.receive().await;
            assert_eq!(state["type"], "client/state");
            self.wait_snapshot(|snapshot| snapshot.link == Link::Connected)
                .await;
            (hello, state)
        }

        /// Waits until the state file satisfies `predicate`. spun saves in
        /// its own loop, so a save can come after the report to the server.
        async fn wait_saved(&self, predicate: impl Fn(&State) -> bool) {
            tokio::time::timeout(Duration::from_secs(2), async {
                while !predicate(&self.run.saved_state()) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }

        async fn send(&mut self, message: serde_json::Value) {
            self.socket
                .send(Frame::Text(message.to_string().into()))
                .await
                .unwrap();
        }

        /// Sends a binary artwork frame for channel 0: the type byte, an
        /// 8-byte timestamp, then the image bytes. Empty bytes clear the art.
        async fn send_artwork(&mut self, data: &[u8]) {
            let mut frame = vec![8u8];
            frame.extend_from_slice(&0i64.to_be_bytes());
            frame.extend_from_slice(data);
            self.socket.send(Frame::Binary(frame.into())).await.unwrap();
        }

        async fn receive(&mut self) -> serde_json::Value {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let frame = self.socket.next().await.unwrap().unwrap();
                    let Frame::Text(text) = frame else {
                        continue;
                    };
                    let message: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if message["type"] != "client/time" {
                        return message;
                    }
                }
            })
            .await
            .unwrap()
        }

        async fn wait_snapshot(&mut self, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let snapshot = self.run.snapshots.borrow_and_update().clone();
                    if predicate(&snapshot) {
                        return snapshot;
                    }
                    self.run.snapshots.changed().await.unwrap();
                }
            })
            .await
            .unwrap()
        }

        async fn finish(mut self) {
            self.run.shutdown.send_replace(true);
            let goodbye = self.receive().await;
            assert_eq!(goodbye["type"], "client/goodbye");
            assert_eq!(goodbye["payload"]["reason"], "shutdown");
            self.run.finish().await;
        }
    }

    #[tokio::test]
    async fn live_commands_wait_for_server_confirmation_and_shutdown_sends_goodbye() {
        let mut server = TestServer::connect(&["controller@v1", "metadata@v1"]).await;
        server.send(serde_json::json!({"type": "server/state", "payload": {
            "controller": {"supported_commands": ["play", "pause", "volume", "mute"], "volume": 40, "muted": false}
        }})).await;
        let snapshot = server.wait_snapshot(|snapshot| snapshot.volume == 40).await;
        assert!(snapshot.supports(&Command::TogglePlayback));
        server
            .run
            .commands
            .send(Command::TogglePlayback)
            .await
            .unwrap();
        let command = server.receive().await;
        assert_eq!(command["type"], "client/command");
        assert_eq!(command["payload"]["controller"]["command"], "play");
        assert!(!server.run.snapshots.borrow().playing);
        server
            .send(serde_json::json!({"type": "group/update", "payload": {"playback_state": "playing"}}))
            .await;
        let snapshot = server.wait_snapshot(|snapshot| snapshot.playing).await;
        assert!(snapshot.playing);
        server.run.commands.send(Command::VolumeUp).await.unwrap();
        let command = server.receive().await;
        assert_eq!(command["payload"]["controller"]["command"], "volume");
        assert_eq!(command["payload"]["controller"]["volume"], 45);
        assert_eq!(server.run.snapshots.borrow().volume, 40);
        server.finish().await;
    }

    #[tokio::test]
    async fn server_player_commands_are_reported_back_and_the_volume_is_remembered() {
        let mut server = TestServer::connect(&["player@v1"]).await;
        server
            .send(serde_json::json!({"type": "server/command", "payload": {
                "player": {"command": "volume", "volume": 30}
            }}))
            .await;
        let state = server.receive().await;
        assert_eq!(state["type"], "client/state");
        assert_eq!(state["payload"]["player"]["volume"], 30);
        server.wait_saved(|state| state.volume == 30).await;
        server.finish().await;
    }

    #[tokio::test]
    async fn config_edits_switch_the_server_or_reconnect_with_the_new_name() {
        let mut server = TestServer::connect(&[]).await;
        let (next, next_url) = listen().await;
        server
            .run
            .config
            .send_modify(|config| config.server_url = next_url.clone());
        let goodbye = server.receive().await;
        assert_eq!(goodbye["payload"]["reason"], "another_server");
        server.socket = accept(&next).await;
        server.handshake(&[]).await;
        assert_eq!(server.run.saved_state().server_url, Some(next_url));

        server
            .run
            .config
            .send_modify(|config| config.name = "kitchen".into());
        let goodbye = server.receive().await;
        assert_eq!(goodbye["payload"]["reason"], "restart");
        server.socket = accept(&next).await;
        let (hello, _) = server.handshake(&[]).await;
        assert_eq!(hello["payload"]["name"], "kitchen");
        server.finish().await;
    }

    #[tokio::test]
    async fn a_server_static_delay_is_saved_and_survives_a_reconnect() {
        let (listener, url) = listen().await;
        let mut server = TestServer::serve(&listener, url, &["player@v1"]).await;
        // The session clamps to the spec range of 0–5000. A larger saved
        // value would fail the config check at the next start.
        for (sent, applied) in [(9000, 5000), (120, 120)] {
            server
                .send(serde_json::json!({"type": "server/command", "payload": {
                    "player": {"command": "set_static_delay", "static_delay_ms": sent}
                }}))
                .await;
            let state = server.receive().await;
            assert_eq!(state["payload"]["player"]["static_delay_ms"], applied);
            server
                .wait_saved(|config| config.static_delay_ms == applied)
                .await;
        }
        server.socket.close(None).await.unwrap();
        server.socket = accept(&listener).await;
        let (_, state) = server.handshake(&["player@v1"]).await;
        assert_eq!(state["payload"]["player"]["static_delay_ms"], 120);
        server.finish().await;
    }

    #[tokio::test]
    async fn an_unplayable_stream_shows_an_error_until_it_ends() {
        let mut server = TestServer::connect(&["player@v1"]).await;
        server
            .send(serde_json::json!({"type": "stream/start", "payload": {
                "player": {"codec": "mp3", "sample_rate": 48000, "channels": 2, "bit_depth": 16}
            }}))
            .await;
        let snapshot = server
            .wait_snapshot(|snapshot| snapshot.error.is_some())
            .await;
        assert!(
            snapshot
                .error
                .is_some_and(|error| error.starts_with("cannot play stream"))
        );
        server
            .send(serde_json::json!({"type": "stream/end", "payload": {}}))
            .await;
        server
            .wait_snapshot(|snapshot| snapshot.error.is_none())
            .await;
        server.finish().await;
    }

    #[tokio::test]
    async fn a_device_pick_is_saved_and_shown() {
        let mut server = TestServer::connect(&[]).await;
        let dac = Some("sysdefault:CARD=DAC".to_string());
        server
            .run
            .commands
            .send(Command::SelectDevice(dac.clone()))
            .await
            .unwrap();
        let snapshot = server
            .wait_snapshot(|snapshot| snapshot.audio_device.is_some())
            .await;
        assert_eq!(snapshot.audio_device, dac);
        assert_eq!(server.run.saved_state().audio_device, dac);
        server
            .run
            .commands
            .send(Command::SelectDevice(None))
            .await
            .unwrap();
        server
            .wait_snapshot(|snapshot| snapshot.audio_device.is_none())
            .await;
        assert_eq!(server.run.saved_state().audio_device, None);
        server.finish().await;
    }

    #[tokio::test]
    async fn dials_an_ipv6_literal() {
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        // A V6 socket address displays with brackets: `[::1]:port`.
        let url = format!("ws://{}/sendspin", listener.local_addr().unwrap());
        let server = TestServer::serve(&listener, url, &[]).await;
        server.finish().await;
    }

    #[tokio::test]
    async fn album_art_arrives_and_clears() {
        let mut server = TestServer::connect(&["artwork@v1"]).await;
        server.send_artwork(b"fake png").await;
        let snapshot = server
            .wait_snapshot(|snapshot| snapshot.art.is_some())
            .await;
        assert_eq!(
            snapshot.art.as_ref().map(|art| art.0.as_ref()),
            Some(&b"fake png"[..])
        );
        server.send_artwork(&[]).await;
        let snapshot = server
            .wait_snapshot(|snapshot| snapshot.art.is_none())
            .await;
        assert!(snapshot.art.is_none());
        server.finish().await;
    }

    #[tokio::test]
    async fn server_state_cannot_grant_a_role_missing_from_handshake() {
        let mut server = TestServer::connect(&["metadata@v1"]).await;
        server
            .send(serde_json::json!({"type": "server/state", "payload": {
                "controller": {"supported_commands": ["play"], "volume": 40, "muted": false}
            }}))
            .await;
        let snapshot = server.wait_snapshot(|snapshot| snapshot.volume == 40).await;
        assert!(!snapshot.supports(&Command::TogglePlayback));
        server.finish().await;
    }

    #[tokio::test]
    async fn repeat_and_shuffle_follow_the_controller_state_and_send_commands() {
        let mut server = TestServer::connect(&["controller@v1"]).await;
        server
            .send(serde_json::json!({"type": "server/state", "payload": {
                "controller": {
                    "supported_commands": ["repeat_off", "repeat_one", "repeat_all", "shuffle", "unshuffle"],
                    "volume": 40, "muted": false, "repeat": "all", "shuffle": true
                }
            }}))
            .await;
        let snapshot = server.wait_snapshot(|snapshot| snapshot.volume == 40).await;
        assert_eq!(snapshot.repeat, Some(RepeatMode::All));
        assert_eq!(snapshot.shuffle, Some(true));

        for (command, name) in [
            (Command::SetRepeat(RepeatMode::Off), "repeat_off"),
            (Command::SetRepeat(RepeatMode::One), "repeat_one"),
            (Command::SetRepeat(RepeatMode::All), "repeat_all"),
            (Command::SetShuffle(true), "shuffle"),
            (Command::SetShuffle(false), "unshuffle"),
        ] {
            assert!(snapshot.supports(&command), "{command:?}");
            server.run.commands.send(command).await.unwrap();
            let message = server.receive().await;
            assert_eq!(message["payload"]["controller"]["command"], name);
        }
        // The menu waits for the server: a sent command changes no state.
        assert_eq!(server.run.snapshots.borrow().repeat, Some(RepeatMode::All));
        server.finish().await;
    }

    #[tokio::test]
    async fn repeat_and_shuffle_are_unavailable_without_their_commands() {
        let mut server = TestServer::connect(&["controller@v1"]).await;
        server
            .send(serde_json::json!({"type": "server/state", "payload": {
                "controller": {"supported_commands": ["play"], "volume": 40, "muted": false}
            }}))
            .await;
        let snapshot = server.wait_snapshot(|snapshot| snapshot.volume == 40).await;
        assert!(!snapshot.supports(&Command::SetRepeat(RepeatMode::All)));
        assert!(!snapshot.supports(&Command::SetShuffle(true)));
        assert_eq!(snapshot.repeat, None);
        assert_eq!(snapshot.shuffle, None);
        server.finish().await;
    }

    #[tokio::test]
    async fn invalid_server_volume_preserves_last_valid_state() {
        let mut server = TestServer::connect(&["controller@v1", "metadata@v1"]).await;
        server
            .send(serde_json::json!({"type": "server/state", "payload": {
                "controller": {"supported_commands": ["play"], "volume": 101, "muted": true}
            }}))
            .await;
        // spun reads the messages in order, so this title shows that spun
        // has read the bad state.
        server
            .send(serde_json::json!({"type": "server/state", "payload": {
                "metadata": {"timestamp": 0, "title": "After"}
            }}))
            .await;
        let snapshot = server
            .wait_snapshot(|snapshot| snapshot.title.is_some())
            .await;
        assert_eq!(snapshot.volume, 100);
        assert!(!snapshot.muted);
        assert!(snapshot.supported_commands.is_empty());
        // A bad value does not stop playback, so it is no error state.
        assert!(snapshot.error.is_none());
        server.finish().await;
    }

    #[tokio::test]
    async fn disconnect_clears_metadata_and_capabilities_before_retry() {
        let mut server = TestServer::connect(&["controller@v1", "metadata@v1"]).await;
        server
            .send(serde_json::json!({"type": "server/state", "payload": {
                "metadata": {"timestamp": 0, "title": "Test track", "artist": "Test artist",
                    "progress": {"track_progress": 1000, "track_duration": 200000, "playback_speed": 1000}},
                "controller": {"supported_commands": ["play"], "volume": 40, "muted": false}
            }}))
            .await;
        let snapshot = server
            .wait_snapshot(|snapshot| snapshot.title.is_some())
            .await;
        assert_eq!(snapshot.title.as_deref(), Some("Test track"));
        assert_eq!(snapshot.progress.map(|p| p.duration_ms), Some(200_000));
        assert!(snapshot.supports(&Command::TogglePlayback));
        server.socket.close(None).await.unwrap();
        let snapshot = server
            .wait_snapshot(|snapshot| snapshot.link == Link::Disconnected)
            .await;
        assert!(snapshot.title.is_none());
        assert!(snapshot.artist.is_none());
        assert!(snapshot.progress.is_none());
        assert!(!snapshot.controllable);
        assert!(snapshot.supported_commands.is_empty());
        assert!(snapshot.error.is_some());
        server.run.finish().await;
    }

    #[test]
    fn server_playback_is_authoritative() {
        let mut snapshot = Snapshot::default();
        apply_message(
            &mut snapshot,
            Message::GroupUpdate(sendspin::protocol::messages::GroupUpdate {
                playback_state: Some(PlaybackState::Playing),
                group_id: None,
                group_name: None,
            }),
            |_| None,
        );
        assert!(snapshot.playing);
    }

    async fn accept(
        listener: &tokio::net::TcpListener,
    ) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        tokio_tungstenite::accept_async(stream).await.unwrap()
    }

    #[tokio::test]
    async fn closed_commands_cancel_a_stalled_connection() {
        let (listener, url) = listen().await;
        let run = spawn_run(Some(url));
        let (_stream, _) = listener.accept().await.unwrap();
        drop(run.commands);
        tokio::time::timeout(Duration::from_secs(1), run.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn select_server_while_connecting_dials_and_saves_the_new_server() {
        let (stalled, stalled_url) = listen().await;
        let (next, next_url) = listen().await;
        let run = spawn_run(Some(stalled_url));
        // Accept TCP but never answer the WebSocket handshake.
        let (_stream, _) = stalled.accept().await.unwrap();
        run.commands
            .send(Command::SelectServer(next_url.clone()))
            .await
            .unwrap();
        let _socket = accept(&next).await;
        assert_eq!(run.saved_state().server_url, Some(next_url));
        run.finish().await;
    }

    #[tokio::test]
    async fn connected_select_server_says_goodbye_and_reconnects() {
        let mut server = TestServer::connect(&[]).await;
        let (next, next_url) = listen().await;
        server
            .run
            .commands
            .send(Command::SelectServer(next_url))
            .await
            .unwrap();
        let goodbye = server.receive().await;
        assert_eq!(goodbye["type"], "client/goodbye");
        assert_eq!(goodbye["payload"]["reason"], "another_server");
        server.socket = accept(&next).await;
        server.handshake(&[]).await;
        server.finish().await;
    }

    #[tokio::test]
    async fn without_a_server_a_menu_pick_connects_and_is_saved() {
        let (listener, url) = listen().await;
        let mut run = spawn_run(None);
        run.snapshots
            .wait_for(|snapshot| snapshot.link == Link::NoServer)
            .await
            .unwrap();
        run.commands
            .send(Command::SelectServer(url.clone()))
            .await
            .unwrap();
        let _socket = accept(&listener).await;
        assert_eq!(run.saved_state().server_url, Some(url));
        run.finish().await;
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_picks_a_lone_server_but_not_one_of_several() {
        let server = |label: &str| discovery::Server {
            label: label.into(),
            url: format!("ws://{label}.local:8927/sendspin"),
        };
        let (tx, rx) = watch::channel(Vec::new());
        let started = tokio::time::Instant::now();
        let pick = tokio::spawn(only_server(rx));
        tx.send_replace(vec![server("kitchen")]);
        assert_eq!(pick.await.unwrap(), server("kitchen").url);
        assert!(started.elapsed() >= DISCOVERY_SETTLE);

        // A second server turns up before the list settles. spun then waits
        // for a menu pick, even after the second server goes away.
        let (tx, rx) = watch::channel(vec![server("kitchen")]);
        let pick = tokio::spawn(only_server(rx));
        tokio::time::sleep(DISCOVERY_SETTLE / 2).await;
        tx.send_replace(vec![server("kitchen"), server("attic")]);
        tokio::time::sleep(DISCOVERY_SETTLE / 2).await;
        tx.send_replace(vec![server("kitchen")]);
        tokio::time::sleep(DISCOVERY_SETTLE * 10).await;
        assert!(!pick.is_finished());
    }
}
