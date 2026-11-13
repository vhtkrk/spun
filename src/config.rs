use std::fs::{File, TryLockError};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, ensure};
use sendspin::audio::synced_player::MAX_STATIC_DELAY_MS;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::icon::{self, Color, Desktop, Rgb, Style};

/// What a tray mouse event does. Names match the actions in the README.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayAction {
    #[default]
    PlayPause,
    Next,
    Previous,
    Stop,
    VolumeUp,
    VolumeDown,
    ToggleMute,
    /// Opens the Music Assistant web UI in the desktop browser.
    OpenWebUi,
    None,
}

/// Mouse bindings for the tray icon. Right click is not here: the SNI spec
/// has panels open the menu on right click, so there is nothing to rebind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrayConfig {
    pub left_click: TrayAction,
    pub double_click: TrayAction,
    pub middle_click: TrayAction,
    pub scroll_up: TrayAction,
    pub scroll_down: TrayAction,
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            left_click: TrayAction::PlayPause,
            double_click: TrayAction::OpenWebUi,
            middle_click: TrayAction::None,
            scroll_up: TrayAction::VolumeUp,
            scroll_down: TrayAction::VolumeDown,
        }
    }
}

/// A setting that is on, off, or follows the desktop. In the config, it is
/// `true`, `false`, or `"auto"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Toggle {
    On,
    Off,
    #[default]
    Auto,
}

/// What spun does while another media player on the desktop plays. Names
/// match the actions in the README.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OtherMediaAction {
    /// Plays as usual.
    Ignore,
    /// Fades to silence and keeps playing, then fades back up.
    #[default]
    Mute,
    /// Fades to silence, then pauses. Plays again when the other media
    /// stops.
    Pause,
}

impl Toggle {
    /// The setting, with `"auto"` replaced by `auto`.
    fn or(self, auto: bool) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Auto => auto,
        }
    }
}

impl<'de> Deserialize<'de> for Toggle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Bool(true) => Ok(Self::On),
            Raw::Bool(false) => Ok(Self::Off),
            Raw::Text(text) if text.eq_ignore_ascii_case("auto") => Ok(Self::Auto),
            Raw::Text(text) => Err(serde::de::Error::custom(format!(
                "`{text}` must be true, false, or \"auto\""
            ))),
        }
    }
}

impl Serialize for Toggle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::On => serializer.serialize_bool(true),
            Self::Off => serializer.serialize_bool(false),
            Self::Auto => serializer.serialize_str("auto"),
        }
    }
}

/// How the tray badge looks. Every setting defaults to `"auto"`, which
/// follows the server and the desktop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LooksConfig {
    /// `"auto"` is dark unless the desktop prefers light.
    pub dark_mode: Toggle,
    /// The main colour of the badge. `"auto"` takes the server colour for
    /// the track, then the desktop accent colour, then teal.
    pub primary: Color,
    /// The second colour of the badge. `"auto"` takes the server's second
    /// colour for the track, then a shade of the primary colour: darker in
    /// dark mode, lighter in light mode.
    pub secondary: Color,
    /// The grey parts of the badge. `"auto"` is a mid gray in dark mode and
    /// a darker gray in light mode.
    pub inactive: Color,
    /// Turns the badge while something plays, steps it while spun connects,
    /// and blinks it on an error. `"auto"` is on unless the desktop asks for
    /// reduced motion.
    pub animations: Toggle,
}

impl LooksConfig {
    /// The badge style for these settings on `desktop`.
    pub(crate) fn style(&self, desktop: Desktop) -> Style {
        let dark = self.dark_mode.or(!desktop.light);
        Style {
            primary: self.primary.explicit(),
            secondary: self.secondary.explicit(),
            inactive: self.inactive.explicit().unwrap_or(if dark {
                Rgb::GREY
            } else {
                icon::shade(Rgb::GREY, true)
            }),
            accent: desktop.accent.unwrap_or(Rgb::TEAL),
            dark,
            animations: self.animations.or(!desktop.reduced_motion),
        }
    }
}

/// Checks that `url` can name a Sendspin server: `ws://` or `wss://`, with
/// a host, and without credentials or a fragment.
pub fn check_server_url(url: &str) -> anyhow::Result<()> {
    // `http::Uri` drops a fragment without an error, so the text is checked.
    ensure!(!url.contains('#'), "server_url must not contain a fragment");
    let url: http::Uri = url.parse().context("invalid server_url")?;
    ensure!(
        matches!(url.scheme_str(), Some("ws" | "wss")),
        "server_url must use ws:// or wss://"
    );
    let authority = url.authority().context("server_url must have a host")?;
    ensure!(!authority.host().is_empty(), "server_url must have a host");
    ensure!(
        !authority.as_str().contains('@'),
        "server_url credentials are not supported"
    );
    Ok(())
}

/// The config file. Every key is optional, so the file holds only what
/// differs from the defaults. spun reads the file again after each edit and
/// never writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// `"auto"`, or a `ws://` or `wss://` URL that pins the server. With
    /// `"auto"`, spun connects to the server that it used last, or to the
    /// only server that discovery finds.
    pub server_url: String,
    /// Pins the client ID. Without it, spun makes one up on the first run.
    pub client_id: Option<String>,
    pub name: String,
    /// Pins the static delay. Without it, spun keeps the delay that the
    /// server sets.
    pub static_delay_ms: Option<u16>,
    /// Pins the output device, by its name on the audio host. On Linux, that
    /// is an ALSA name as `aplay -L` lists it. Without it, spun keeps the
    /// last menu pick, or uses the system default.
    pub audio_device: Option<String>,
    /// Pins TV Mode, which keeps the display awake while something plays.
    /// Without it, spun keeps the last menu pick, or leaves TV Mode off: a
    /// desktop that never sleeps the screen drains a battery faster.
    pub tv_mode: Option<bool>,
    /// Pins what spun does while another media player on the desktop plays.
    /// Without it, spun keeps the last menu pick, or mutes.
    pub other_media_action: Option<OtherMediaAction>,
    /// Milliseconds that another player must play before spun gets out of
    /// its way. A player that stops sooner leaves spun alone.
    pub other_media_delay_ms: u64,
    /// Minutes that spun keeps playing while another player holds it
    /// silent, before it pauses. 0 never pauses.
    pub mute_snooze: u64,
    pub tray: TrayConfig,
    pub looks: LooksConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: "auto".into(),
            client_id: None,
            name: env!("CARGO_PKG_NAME").into(),
            static_delay_ms: None,
            audio_device: None,
            tv_mode: None,
            other_media_action: None,
            other_media_delay_ms: 500,
            mute_snooze: 30,
            tray: TrayConfig::default(),
            looks: LooksConfig::default(),
        }
    }
}

/// Dotted paths of the keys in `found` that `known` does not have.
fn unknown_keys(found: &toml::Table, known: &toml::Table, prefix: &str) -> Vec<String> {
    found
        .iter()
        .flat_map(|(key, value)| {
            let path = format!("{prefix}{key}");
            match (value, known.get(key)) {
                (_, None) => vec![path],
                (toml::Value::Table(found), Some(toml::Value::Table(known))) => {
                    unknown_keys(found, known, &format!("{path}."))
                }
                _ => Vec::new(),
            }
        })
        .collect()
}

/// `$XDG_CONFIG_HOME/spun/config.toml`, or `~/.config/spun/config.toml`.
pub fn path() -> anyhow::Result<PathBuf> {
    Ok(app_dir("XDG_CONFIG_HOME", ".config")?.join("config.toml"))
}

/// `$XDG_STATE_HOME/spun/state.toml`, or `~/.local/state/spun/state.toml`.
pub fn state_path() -> anyhow::Result<PathBuf> {
    Ok(app_dir("XDG_STATE_HOME", ".local/state")?.join("state.toml"))
}

/// Takes the lock for the state file at `state_path`, and returns `None`
/// when another spun holds it. Two spuns with one state file have one client
/// ID. The server keeps one connection for each client ID, so each connect
/// closes the connection of the other spun, without end. The lock lasts
/// until the returned file drops.
pub fn lock(state_path: &Path) -> anyhow::Result<Option<File>> {
    let path = state_path.with_extension("lock");
    let parent = path
        .parent()
        .context("state path has no parent directory")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("locking {}", path.display()))
        }
    }
}

/// The spun directory in the XDG base directory that `var` names. Without
/// `var`, the base directory is `fallback` in the home directory.
fn app_dir(var: &str, fallback: &str) -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os(var).filter(|dir| !dir.is_empty()) {
        Some(dir) => {
            let path = PathBuf::from(dir);
            ensure!(path.is_absolute(), "{var} must be absolute");
            path
        }
        None => {
            let home = std::env::var_os("HOME")
                .filter(|dir| !dir.is_empty())
                .with_context(|| format!("neither {var} nor HOME is set"))?;
            PathBuf::from(home).join(fallback)
        }
    };
    Ok(base.join(env!("CARGO_PKG_NAME")))
}

impl Config {
    /// The pinned server, or `None` for `"auto"`.
    pub fn server(&self) -> Option<&str> {
        (!self.server_url.eq_ignore_ascii_case("auto")).then_some(self.server_url.as_str())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.server().is_some() {
            check_server_url(&self.server_url)?;
        }
        ensure!(
            self.client_id
                .as_deref()
                .is_none_or(|id| !id.trim().is_empty()),
            "client_id must not be empty; remove it to keep the current one"
        );
        ensure!(!self.name.trim().is_empty(), "name must not be empty");
        // The server sends audio ahead by the reported delay, so a value that
        // the player clamps would put spun out of sync.
        ensure!(
            self.static_delay_ms
                .is_none_or(|delay| delay <= MAX_STATIC_DELAY_MS),
            "static_delay_ms must be between 0 and {MAX_STATIC_DELAY_MS}"
        );
        ensure!(
            self.audio_device
                .as_deref()
                .is_none_or(|name| !name.trim().is_empty()),
            "audio_device must not be empty; remove it to keep the current device"
        );
        Ok(())
    }

    /// How long another player must play before spun gets out of its way.
    pub fn other_media_delay(&self) -> Duration {
        Duration::from_millis(self.other_media_delay_ms)
    }

    /// How long spun keeps playing while it is muted by other media, or
    /// `None` when [`Config::mute_snooze`] turns the pause off.
    pub fn mute_snooze_time(&self) -> Option<Duration> {
        (self.mute_snooze > 0).then(|| Duration::from_secs(self.mute_snooze.saturating_mul(60)))
    }

    /// Takes the menu picks `tv_mode` and `other_media_action` for each of
    /// the two keys that this config does not set. So a pick survives a
    /// restart and an edit of the file. A key that the config sets wins, at
    /// startup and at each edit of the file.
    pub fn keep_picks(
        &mut self,
        tv_mode: Option<bool>,
        other_media_action: Option<OtherMediaAction>,
    ) {
        self.tv_mode = self.tv_mode.or(tv_mode);
        self.other_media_action = self.other_media_action.or(other_media_action);
    }

    /// Parses config text. Unknown keys only log a warning, so a typo or a
    /// removed setting does not stop spun.
    fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(text)?;
        let found: toml::Table = toml::from_str(text)?;
        let known = toml::Table::try_from(&config)?;
        for key in unknown_keys(&found, &known, "") {
            log::warn!("ignoring unknown config key `{key}`");
        }
        Ok(config)
    }

    /// Reads the config at `path`. Without the file, each key has its
    /// default.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let config = Self::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("validating {}", path.display()))?;
        Ok(config)
    }
}

/// How long the config file must stay quiet before spun reads it.
#[cfg(target_os = "linux")]
const SETTLE: std::time::Duration = std::time::Duration::from_millis(200);

/// The inotify events of a directory.
#[cfg(target_os = "linux")]
type Events = inotify::EventStream<[u8; 4096]>;

/// Watches the config file at `path` with inotify. Each valid edit goes to
/// `configs`. An edit with an error goes to `errors`, and the current config
/// stays. The next valid edit clears the error. The watch starts before
/// this function returns, so no edit after the call goes unseen.
// ponytail: watches the directory of `path`. When config.toml is a link to a
// file in another directory, an edit of that file goes unseen. Watch the
// directory of the link target too if that matters.
#[cfg(target_os = "linux")]
pub fn watch(
    path: PathBuf,
    configs: watch::Sender<Config>,
    errors: watch::Sender<Option<String>>,
) -> impl std::future::Future<Output = ()> {
    let events = watch_dir(&path);
    async move {
        let (mut events, name) = match events {
            Ok(events) => events,
            Err(error) => {
                log::warn!(
                    "cannot watch {}: {error:#}; config edits apply after a restart",
                    path.display()
                );
                return;
            }
        };
        while next_event(&mut events, &name).await {
            settle(&mut events, &name).await;
            reload(&path, &configs, &errors);
        }
    }
}

/// Waits until no event about the file `name` comes for `SETTLE`. An editor
/// can save in several steps, for example a rename and then a write, and
/// spun reads the file once after the last step.
#[cfg(target_os = "linux")]
async fn settle(events: &mut Events, name: &std::ffi::OsStr) {
    while let Ok(true) = tokio::time::timeout(SETTLE, next_event(events, name)).await {}
}

/// Starts an inotify watch on the directory of the config file at `path`.
/// Returns the events and the file name.
#[cfg(target_os = "linux")]
fn watch_dir(path: &Path) -> anyhow::Result<(Events, std::ffi::OsString)> {
    use inotify::{Inotify, WatchMask};

    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        anyhow::bail!("the config path names no file in a directory");
    };
    // spun writes no config, but inotify can watch only a directory that
    // exists.
    std::fs::create_dir_all(dir)?;
    let inotify = Inotify::init()?;
    // A save writes the file in place, or moves a new file over it. A delete
    // counts too, so that spun goes back to the defaults.
    let mask = WatchMask::CLOSE_WRITE
        | WatchMask::MOVED_TO
        | WatchMask::MOVED_FROM
        | WatchMask::CREATE
        | WatchMask::DELETE;
    inotify.watches().add(dir, mask)?;
    Ok((inotify.into_event_stream([0_u8; 4096])?, name.to_owned()))
}

/// Waits for an event about the file `name`. A full event queue counts too,
/// because it can hide an edit. Returns false when no more events can come.
#[cfg(target_os = "linux")]
async fn next_event(events: &mut Events, name: &std::ffi::OsStr) -> bool {
    use futures_util::StreamExt;

    while let Some(event) = events.next().await {
        match event {
            Ok(event)
                if event.name.as_deref() == Some(name)
                    || event.mask.contains(inotify::EventMask::Q_OVERFLOW) =>
            {
                return true;
            }
            Ok(_) => {}
            Err(error) => {
                log::warn!("reading the config events failed: {error}");
                return false;
            }
        }
    }
    false
}

/// Reads the config at `path` after an edit. A valid config goes to
/// `configs` and clears the error. An error goes to `errors`.
#[cfg(target_os = "linux")]
fn reload(path: &Path, configs: &watch::Sender<Config>, errors: &watch::Sender<Option<String>>) {
    match Config::load(path) {
        Ok(mut config) => {
            log::info!("applying the edit of {}", path.display());
            configs.send_if_modified(|current| {
                config.keep_picks(current.tv_mode, current.other_media_action);
                let changed = *current != config;
                *current = config;
                changed
            });
            errors.send_if_modified(|error| error.take().is_some());
        }
        Err(error) => {
            log::warn!("{error:#}; keeping the current config");
            errors.send_replace(Some(format!("{error:#}")));
        }
    }
}

/// Without inotify, spun reads the config only at startup.
#[cfg(not(target_os = "linux"))]
pub fn watch(
    path: PathBuf,
    _configs: watch::Sender<Config>,
    _errors: watch::Sender<Option<String>>,
) -> impl std::future::Future<Output = ()> {
    log::info!(
        "edits of {} apply after a restart: the config watch needs inotify, which only Linux has",
        path.display()
    );
    std::future::ready(())
}

/// What spun remembers between runs. It has a file of its own, so spun never
/// writes the config. A value that the config sets wins; see [`State::pin`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub client_id: String,
    /// The server that spun used last.
    pub server_url: Option<String>,
    /// The player volume that the server set last. The next connection
    /// starts with it.
    pub volume: u8,
    /// The static delay that the server set last. The Sendspin spec says
    /// that a client must keep it across reconnects and restarts.
    pub static_delay_ms: u16,
    /// The output device. `None` is the system default.
    pub audio_device: Option<String>,
    /// The last TV Mode pick in the menu.
    pub tv_mode: bool,
    /// The last pick in the Other media menu.
    pub other_media_action: OtherMediaAction,
}

impl Default for State {
    /// The state of the first run, with a new client ID.
    fn default() -> Self {
        Self {
            client_id: uuid::Uuid::new_v4().to_string(),
            server_url: None,
            volume: 100,
            static_delay_ms: 0,
            audio_device: None,
            tv_mode: false,
            other_media_action: OtherMediaAction::default(),
        }
    }
}

impl State {
    /// Reads the state at `path`. Without the file, this is the state of the
    /// first run.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Replaces the file at `path` with this state atomically. Only the user
    /// can read the file.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let parent = path
            .parent()
            .context("state path has no parent directory")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("creating a temporary file in {}", parent.display()))?;
        file.write_all(toml::to_string(self)?.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        file.as_file().sync_all().context("syncing the state")?;
        file.persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Takes each value that `config` sets and `old` does not. So an edit
    /// applies only the keys that it changes, and a menu pick survives an
    /// edit elsewhere in the file. At startup, `old` is the default config,
    /// which sets none of these keys. A menu pick of `tv_mode` or
    /// `other_media_action` changes the running config, so it comes here
    /// as an edit too.
    pub fn pin(&mut self, config: &Config, old: &Config) {
        if config.server_url != old.server_url
            && let Some(url) = config.server()
        {
            self.server_url = Some(url.into());
        }
        if config.client_id != old.client_id
            && let Some(id) = &config.client_id
        {
            self.client_id = id.clone();
        }
        if config.static_delay_ms != old.static_delay_ms
            && let Some(delay) = config.static_delay_ms
        {
            self.static_delay_ms = delay;
        }
        if config.audio_device != old.audio_device
            && let Some(device) = &config.audio_device
        {
            self.audio_device = Some(device.clone());
        }
        if config.tv_mode != old.tv_mode
            && let Some(on) = config.tv_mode
        {
            self.tv_mode = on;
        }
        if config.other_media_action != old.other_media_action
            && let Some(action) = config.other_media_action
        {
            self.other_media_action = action;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn tray_bindings_default_to_the_documented_actions() {
        let tray = TrayConfig::default();
        assert_eq!(tray.left_click, TrayAction::PlayPause);
        assert_eq!(tray.double_click, TrayAction::OpenWebUi);
        assert_eq!(tray.middle_click, TrayAction::None);
        assert_eq!(tray.scroll_up, TrayAction::VolumeUp);
        assert_eq!(tray.scroll_down, TrayAction::VolumeDown);
    }

    #[test]
    fn a_config_needs_only_what_differs_from_the_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let config = Config::parse("name = 'kitchen'\n[tray]\nleft_click = 'next'\n").unwrap();
        assert_eq!(config.name, "kitchen");
        assert_eq!(config.tray.left_click, TrayAction::Next);
        assert_eq!(config.tray.double_click, TrayAction::OpenWebUi);
        assert_eq!(config.server(), None, "server_url defaults to auto");
        assert_eq!(config.looks, LooksConfig::default());
    }

    #[test]
    fn tray_bindings_round_trip_and_rebind() {
        let config = Config::parse("[tray]\nleft_click = 'next'\nscroll_up = 'none'\n").unwrap();
        assert_eq!(config.tray.left_click, TrayAction::Next);
        assert_eq!(config.tray.scroll_up, TrayAction::None);
        assert_eq!(config.tray.scroll_down, TrayAction::VolumeDown);
        let parsed: Config = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(parsed.tray, config.tray);
    }

    #[test]
    fn tray_bindings_reject_unknown_actions() {
        assert!(Config::parse("[tray]\nleft_click = 'shout'\n").is_err());
    }

    #[test]
    fn every_look_defaults_to_auto() {
        let looks = Config::default().looks;
        assert_eq!(looks.dark_mode, Toggle::Auto);
        assert_eq!(looks.primary, Color::Auto);
        assert_eq!(looks.secondary, Color::Auto);
        assert_eq!(looks.inactive, Color::Auto);
        assert_eq!(looks.animations, Toggle::Auto);
    }

    #[test]
    fn looks_parse_from_the_config_and_round_trip() {
        let config = Config::parse(
            "[looks]\ndark_mode = false\nprimary = '#2fb0ac'\nsecondary = 'auto'\ninactive = '#abc'\nanimations = true\n",
        )
        .unwrap();
        let looks = &config.looks;
        assert_eq!(looks.dark_mode, Toggle::Off);
        assert_eq!(looks.primary, Color::Explicit(Rgb::TEAL));
        assert_eq!(looks.secondary, Color::Auto);
        assert_eq!(looks.inactive, Color::Explicit(Rgb::new(0xaa, 0xbb, 0xcc)));
        assert_eq!(looks.animations, Toggle::On);
        // The unknown key check writes the config out, so each key must
        // come back.
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("[looks]"), "{text}");
        assert_eq!(Config::parse(&text).unwrap().looks, config.looks);
    }

    #[test]
    fn a_bad_look_is_an_error() {
        for bad in [
            "primary = 'chartreuse'",
            "dark_mode = 'yes'",
            "animations = 1",
        ] {
            assert!(
                Config::parse(&format!("[looks]\n{bad}\n")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn auto_looks_follow_the_desktop() {
        let auto = LooksConfig::default();
        // A desktop that says nothing: dark, teal, mid gray, and animated.
        let style = auto.style(Desktop::default());
        assert!(style.dark);
        assert_eq!(style.accent, Rgb::TEAL);
        assert_eq!(style.inactive, Rgb::GREY);
        assert!(style.animations);
        assert_eq!((style.primary, style.secondary), (None, None));

        let light = Desktop {
            light: true,
            accent: Some(Rgb::new(1, 2, 3)),
            reduced_motion: true,
        };
        let style = auto.style(light);
        assert!(!style.dark);
        assert_eq!(style.accent, Rgb::new(1, 2, 3));
        assert_eq!(style.inactive, Rgb::new(0x5d, 0x5d, 0x5d));
        assert!(!style.animations);

        // Explicit settings win over the desktop.
        let fixed = LooksConfig {
            dark_mode: Toggle::On,
            primary: Color::Explicit(Rgb::TEAL),
            inactive: Color::Explicit(Rgb::new(9, 9, 9)),
            animations: Toggle::On,
            ..LooksConfig::default()
        }
        .style(light);
        assert!(fixed.dark);
        assert_eq!(fixed.primary, Some(Rgb::TEAL));
        assert_eq!(fixed.inactive, Rgb::new(9, 9, 9));
        assert!(fixed.animations);
    }

    #[test]
    fn unknown_keys_are_reported_and_ignored() {
        let text = "insecure_tls = true\nname = 'spun'\n[tray]\nleft = 'next'\nscroll_up = 'none'\n[extra]\nkey = 1\n";
        let config = Config::parse(text).unwrap();
        assert_eq!(config.tray.scroll_up, TrayAction::None);
        let found: toml::Table = toml::from_str(text).unwrap();
        let known = toml::Table::try_from(&config).unwrap();
        let mut unknown = unknown_keys(&found, &known, "");
        unknown.sort();
        assert_eq!(unknown, ["extra", "insecure_tls", "tray.left"]);
    }

    #[test]
    fn audio_device_is_optional_and_round_trips() {
        assert_eq!(Config::parse("").unwrap().audio_device, None);
        let config = Config::parse("audio_device = 'sysdefault:CARD=DAC'\n").unwrap();
        assert_eq!(config.audio_device.as_deref(), Some("sysdefault:CARD=DAC"));
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(
            Config::parse(&text).unwrap().audio_device,
            config.audio_device
        );
        let empty = Config {
            audio_device: Some(" ".into()),
            ..Config::default()
        };
        assert!(empty.validate().is_err());
    }

    #[test]
    fn the_display_stays_awake_only_when_asked() {
        assert_eq!(Config::default().tv_mode, None);
        assert_eq!(Config::parse("").unwrap().tv_mode, None);
        let config = Config::parse("tv_mode = true\n").unwrap();
        assert_eq!(config.tv_mode, Some(true));
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(Config::parse(&text).unwrap().tv_mode, Some(true));
        assert!(Config::parse("tv_mode = 'yes'\n").is_err());
    }

    #[test]
    fn other_media_mutes_by_default_and_round_trips() {
        let default = Config::default();
        assert_eq!(default.other_media_action, None);
        assert_eq!(OtherMediaAction::default(), OtherMediaAction::Mute);
        assert_eq!(default.mute_snooze, 30);
        assert_eq!(Config::parse("").unwrap(), default);
        for (text, action) in [
            ("ignore", OtherMediaAction::Ignore),
            ("mute", OtherMediaAction::Mute),
            ("pause", OtherMediaAction::Pause),
        ] {
            let config = Config::parse(&format!("other_media_action = '{text}'\n")).unwrap();
            assert_eq!(config.other_media_action, Some(action));
            let text = toml::to_string_pretty(&config).unwrap();
            assert_eq!(
                Config::parse(&text).unwrap().other_media_action,
                Some(action),
                "each action must come back"
            );
        }
        assert!(Config::parse("other_media_action = 'shout'\n").is_err());
        assert_eq!(default.other_media_delay(), Duration::from_millis(500));
        let config = Config::parse("other_media_delay_ms = 0\n").unwrap();
        assert_eq!(config.other_media_delay(), Duration::ZERO);
        assert!(Config::parse("other_media_delay_ms = -1\n").is_err());
    }

    #[test]
    fn the_snooze_is_in_minutes_and_zero_turns_it_off() {
        let snooze = |minutes| Config {
            mute_snooze: minutes,
            ..Config::default()
        };
        assert_eq!(snooze(0).mute_snooze_time(), None);
        assert_eq!(snooze(1).mute_snooze_time(), Some(Duration::from_secs(60)));
        assert_eq!(
            Config::default().mute_snooze_time(),
            Some(Duration::from_secs(30 * 60))
        );
        let config = Config::parse("mute_snooze = 5\n").unwrap();
        assert_eq!(config.mute_snooze, 5);
        assert_eq!(
            Config::parse(&toml::to_string_pretty(&config).unwrap())
                .unwrap()
                .mute_snooze,
            5
        );
        assert!(Config::parse("mute_snooze = -1\n").is_err());
        assert!(Config::parse("mute_snooze = '30'\n").is_err());
    }

    #[test]
    fn rejects_out_of_range_static_delay() {
        let config = Config {
            static_delay_ms: Some(MAX_STATIC_DELAY_MS + 1),
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn server_url_is_auto_or_a_websocket_url() {
        for (url, valid) in [
            ("auto", true),
            ("AUTO", true),
            ("ws://localhost:8927/sendspin", true),
            ("wss://localhost:8927/sendspin", true),
            ("ws://192.168.1.20:8927/sendspin?room=1", true),
            ("ws://[2001:db8::20]:8927/sendspin", true),
            ("https://localhost/sendspin", false),
            ("ws:///", false),
            ("ws://:8927/sendspin", false),
            ("ws://user:secret@localhost/sendspin", false),
            ("ws://user@localhost/sendspin", false),
            ("ws://localhost/sendspin#now", false),
            ("localhost:8927", false),
            ("/sendspin", false),
            ("ws://local host/sendspin", false),
            ("", false),
        ] {
            let config = Config {
                server_url: url.into(),
                ..Config::default()
            };
            assert_eq!(config.validate().is_ok(), valid, "{url:?}");
        }
    }

    #[test]
    fn a_missing_config_is_the_default_and_spun_does_not_create_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        assert!(!path.exists());
        // A directory where the file belongs is an error.
        assert!(Config::load(dir.path()).is_err());
    }

    #[test]
    fn the_state_keeps_its_client_id_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spun/state.toml");
        let first = State::load(&path).unwrap();
        first.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn one_spun_holds_the_lock_of_a_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spun/state.toml");
        let first = lock(&path).unwrap();
        assert!(first.is_some());
        assert!(lock(&path).unwrap().is_none());
        // The exit of the first spun frees the lock.
        drop(first);
        assert!(lock(&path).unwrap().is_some());
    }

    #[test]
    fn the_config_pins_the_state_and_an_edit_applies_only_what_it_changes() {
        let mut state = State {
            server_url: Some("ws://a/sendspin".into()),
            static_delay_ms: 120,
            ..State::default()
        };
        let remembered = state.clone();
        // A config that sets nothing keeps the state.
        state.pin(&Config::default(), &Config::default());
        assert_eq!(state, remembered);
        // At startup, each value that the config sets wins.
        let pinned = Config {
            audio_device: Some("hw:0".into()),
            static_delay_ms: Some(0),
            ..Config::default()
        };
        state.pin(&pinned, &Config::default());
        assert_eq!(state.audio_device.as_deref(), Some("hw:0"));
        assert_eq!(state.static_delay_ms, 0);
        // A menu pick survives an edit elsewhere in the file.
        state.audio_device = Some("hw:1".into());
        let renamed = Config {
            name: "kitchen".into(),
            ..pinned.clone()
        };
        state.pin(&renamed, &pinned);
        assert_eq!(state.audio_device.as_deref(), Some("hw:1"));
        // An edit of the key wins. Back to auto keeps the current server.
        let moved = Config {
            server_url: "ws://b/sendspin".into(),
            ..renamed.clone()
        };
        state.pin(&moved, &renamed);
        assert_eq!(state.server_url.as_deref(), Some("ws://b/sendspin"));
        state.pin(&renamed, &moved);
        assert_eq!(state.server_url.as_deref(), Some("ws://b/sendspin"));
    }

    #[test]
    fn the_state_keeps_the_menu_picks_unless_the_config_sets_the_key() {
        // A menu pick arrives as a change of the running config.
        let mut state = State::default();
        let picked = Config {
            tv_mode: Some(true),
            other_media_action: Some(OtherMediaAction::Pause),
            ..Config::default()
        };
        state.pin(&picked, &Config::default());
        let state: State = toml::from_str(&toml::to_string(&state).unwrap()).unwrap();
        // After a restart, a config that sets neither key takes the picks.
        let mut config = Config::default();
        config.keep_picks(Some(state.tv_mode), Some(state.other_media_action));
        assert_eq!(config, picked);
        // A key that the config sets wins, also when it sets the default.
        let mut config = Config::parse("tv_mode = false\n").unwrap();
        config.keep_picks(Some(state.tv_mode), Some(state.other_media_action));
        assert_eq!(config.tv_mode, Some(false));
        assert_eq!(config.other_media_action, Some(OtherMediaAction::Pause));
        // A state file from before these keys has the defaults.
        let old: State = toml::from_str("client_id = 'a'\n").unwrap();
        assert!(!old.tv_mode);
        assert_eq!(old.other_media_action, OtherMediaAction::Mute);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_watcher_applies_valid_edits_and_reports_bad_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (configs, mut config_rx) = watch::channel(Config::default());
        let (errors, mut error_rx) = watch::channel(None);
        // The watch starts at the call, so the first write is not too early.
        tokio::spawn(watch(path.clone(), configs.clone(), errors));
        let wait = std::time::Duration::from_secs(5);

        // An editor that writes the file in place.
        std::fs::write(&path, "name = 'kitchen'\n").unwrap();
        tokio::time::timeout(wait, config_rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(config_rx.borrow_and_update().name, "kitchen");

        std::fs::write(&path, "name = \n").unwrap();
        tokio::time::timeout(wait, error_rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(error_rx.borrow_and_update().is_some());
        assert_eq!(
            config_rx.borrow().name,
            "kitchen",
            "a bad edit keeps the config"
        );

        // A menu pick changes the running config, not the file.
        configs.send_modify(|config| config.tv_mode = Some(true));
        config_rx.borrow_and_update();

        // An editor that moves a new file over the old one.
        let new = dir.path().join("config.toml.new");
        std::fs::write(&new, "name = 'attic'\n").unwrap();
        std::fs::rename(&new, &path).unwrap();
        tokio::time::timeout(wait, config_rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(config_rx.borrow().name, "attic");
        assert_eq!(
            config_rx.borrow().tv_mode,
            Some(true),
            "a menu pick survives an edit"
        );
        assert!(error_rx.borrow().is_none(), "a valid edit clears the error");
    }
}
