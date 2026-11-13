use ksni::Tray;
use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use sendspin::protocol::messages::RepeatMode;
use tokio::sync::watch;

use crate::config::{Config, OtherMediaAction, TrayAction};
use crate::discovery;
use crate::icon::{self, Clock, State, Style};
use crate::player::{self, Command, CommandTx, Link, OutputDevice, Snapshot};

/// The state that the badge shows.
fn icon_state(snapshot: &Snapshot) -> State {
    let muted = snapshot.muted;
    match snapshot.link {
        Link::NoServer | Link::Disconnected => State::Offline,
        Link::Connecting => State::Connecting,
        Link::Connected if snapshot.error.is_some() => State::Error,
        Link::Connected if snapshot.playing => State::Playing { muted },
        // Sendspin has no pause state, so a pause arrives as a stop. As in
        // MPRIS, a kept track title tells the two apart.
        Link::Connected if snapshot.title.is_some() => State::Paused { muted },
        Link::Connected => State::Stopped {
            muted,
            controllable: snapshot.controllable,
        },
    }
}

/// Two `activate` calls within this window count as a double click. The SNI
/// spec has no separate double-click event, so the tray derives one.
const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);

/// How the desktop's tray host maps mouse events to the SNI D-Bus calls.
/// GNOME's appindicator extension differs from the SNI docs in two ways: it
/// opens the menu on a single left click and forwards `Activate` only for a
/// double click, and its `Scroll` deltas follow Clutter's sign, positive for
/// scroll down, where the SNI docs and KDE use positive for scroll up.
/// The extension also sends `Math.floor` of Clutter's smooth scroll delta,
/// so a down scroll smaller than one unit arrives as 0 while an up scroll
/// arrives as a negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Panel {
    /// Everything that follows the SNI docs, such as KDE Plasma, Waybar, and
    /// Sway.
    Standard,
    Gnome,
}

impl Panel {
    /// Whether a `Scroll` delta from this panel means "scroll up". A zero
    /// delta only reaches GNOME when the extension floored a partial down
    /// scroll to nothing, so it scrolls down there; the SNI docs never send
    /// a zero.
    fn scrolls_up(self, delta: i32) -> bool {
        match self {
            Panel::Standard => delta >= 0,
            Panel::Gnome => delta < 0,
        }
    }
}

fn detect_panel() -> Panel {
    // `XDG_CURRENT_DESKTOP` lists desktops colon-separated, and GNOME forks
    // report themselves as `ubuntu:GNOME` or `GNOME-Flashback:GNOME` too.
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|desktop| panel_for_desktop(&desktop))
        .unwrap_or(Panel::Standard)
}

fn panel_for_desktop(desktop: &str) -> Panel {
    if desktop.to_ascii_lowercase().contains("gnome") {
        Panel::Gnome
    } else {
        Panel::Standard
    }
}

pub struct SpunTray {
    /// The player state. `set_snapshot` changes it, so that `clock` sees
    /// each change of badge state.
    snapshot: Snapshot,
    pub servers: Vec<discovery::Server>,
    /// The Music Assistant web UIs on the LAN, for `open_web_ui`.
    pub web_uis: Vec<String>,
    /// The config, for the mouse bindings and the settings in the menu. A
    /// menu pick changes only this copy, so the next edit of the config
    /// file or the next start brings the configured value back.
    config: watch::Sender<Config>,
    /// The badge looks, already resolved from the config and the desktop.
    pub icon: Style,
    /// How long the badge has shown its state. The frame follows this clock
    /// instead of a counter.
    clock: Clock,
    /// The output devices for the menu; the list refreshes when the menu opens.
    pub devices: Vec<OutputDevice>,
    command_tx: CommandTx,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// When the last left click came, for double clicks.
    last_activate: Option<std::time::Instant>,
    panel: Panel,
}

impl SpunTray {
    pub fn new(
        snapshot: Snapshot,
        config: watch::Sender<Config>,
        icon: Style,
        devices: Vec<OutputDevice>,
        command_tx: CommandTx,
        shutdown_tx: tokio::sync::watch::Sender<bool>,
    ) -> Self {
        Self {
            clock: Clock::new(icon_state(&snapshot), std::time::Instant::now()),
            snapshot,
            servers: Vec::new(),
            web_uis: Vec::new(),
            config,
            icon,
            devices,
            command_tx,
            shutdown_tx,
            last_activate: None,
            panel: detect_panel(),
        }
    }

    /// Takes a new player state. A new badge state starts its animation
    /// again.
    pub fn set_snapshot(&mut self, snapshot: Snapshot) {
        self.clock
            .set(icon_state(&snapshot), std::time::Instant::now());
        self.snapshot = snapshot;
    }

    /// One line for the tooltip: the track, or the connection state.
    fn status_line(&self) -> String {
        let s = &self.snapshot;
        match (s.link, &s.title, &s.artist) {
            (Link::Connected, Some(title), Some(artist)) => format!("{title} – {artist}"),
            (Link::Connected, Some(title), None) => title.clone(),
            (Link::Connected, None, _) => "Nothing playing".into(),
            (link, ..) => link_text(link).into(),
        }
    }

    /// Runs a configured action. Playback commands the server does not
    /// currently allow are dropped, as in the menu.
    fn run(&self, action: TrayAction) {
        let command = match action {
            TrayAction::PlayPause => Command::TogglePlayback,
            TrayAction::Next => Command::Next,
            TrayAction::Previous => Command::Previous,
            TrayAction::Stop => Command::Stop,
            TrayAction::VolumeUp => Command::VolumeUp,
            TrayAction::VolumeDown => Command::VolumeDown,
            TrayAction::ToggleMute => Command::ToggleMute,
            TrayAction::OpenWebUi => return self.open_web_ui(),
            TrayAction::None => return,
        };
        if self.snapshot.supports(&command) {
            player::enqueue(&self.command_tx, command);
        }
    }

    /// The web UI of the Music Assistant server behind the current Sendspin
    /// server.
    fn web_ui_url(&self) -> Option<&String> {
        discovery::web_ui_url(self.snapshot.server_url.as_deref()?, &self.web_uis)
    }

    /// Opens the Music Assistant web UI in the desktop browser. Failures
    /// only log; a tray icon is no place for dialogs.
    fn open_web_ui(&self) {
        let Some(url) = self.web_ui_url() else {
            log::warn!("found no Music Assistant web UI over mDNS");
            return;
        };
        match std::process::Command::new("xdg-open").arg(url).spawn() {
            // Waiting in a detached thread reaps the browser process.
            Ok(child) => {
                std::thread::spawn(move || {
                    let mut child = child;
                    let _ = child.wait();
                });
            }
            Err(error) => log::warn!("opening {url} in the browser failed: {error}"),
        }
    }
}

/// The connection state in words.
fn link_text(link: Link) -> &'static str {
    match link {
        Link::NoServer => "No server selected",
        Link::Connecting => "Connecting...",
        Link::Connected => "Connected",
        Link::Disconnected => "Disconnected",
    }
}

/// dbusmenu hides a single `_` and makes the next character the access key.
/// Track titles and device names can contain `_`, so each one is doubled.
fn menu_label(text: impl Into<String>) -> String {
    text.into().replace('_', "__")
}

/// A clickable row. ksni callbacks run synchronously on the D-Bus task, so they
/// only enqueue a command; the player task does the awaiting.
fn action<F>(
    label: impl Into<String>,
    icon_name: &str,
    enabled: bool,
    on_activate: F,
) -> MenuItem<SpunTray>
where
    F: Fn(&mut SpunTray) + Send + 'static,
{
    MenuItem::Standard(StandardItem {
        label: menu_label(label),
        icon_name: icon_name.into(),
        enabled,
        activate: Box::new(on_activate),
        ..StandardItem::default()
    })
}

/// A row that sends `command`. The row is enabled while the server allows
/// the command.
fn command_item(
    label: &str,
    icon_name: &str,
    snapshot: &Snapshot,
    command: Command,
) -> MenuItem<SpunTray> {
    let enabled = snapshot.supports(&command);
    action(label, icon_name, enabled, move |t| {
        player::enqueue(&t.command_tx, command.clone())
    })
}

/// A row that displays state and cannot be clicked.
fn row(label: impl Into<String>) -> MenuItem<SpunTray> {
    MenuItem::Standard(StandardItem {
        label: menu_label(label),
        enabled: false,
        ..StandardItem::default()
    })
}

fn submenu(
    label: impl Into<String>,
    icon_name: &str,
    items: Vec<MenuItem<SpunTray>>,
) -> MenuItem<SpunTray> {
    MenuItem::SubMenu(SubMenu {
        label: menu_label(label),
        icon_name: icon_name.into(),
        submenu: items,
        ..SubMenu::default()
    })
}

/// A checkable row. A click runs `on_activate`; the row shows `checked`.
fn checkmark<F>(
    label: impl Into<String>,
    checked: bool,
    enabled: bool,
    on_activate: F,
) -> MenuItem<SpunTray>
where
    F: Fn(&mut SpunTray) + Send + 'static,
{
    MenuItem::Checkmark(ksni::menu::CheckmarkItem {
        label: menu_label(label),
        checked,
        enabled,
        activate: Box::new(on_activate),
        ..ksni::menu::CheckmarkItem::default()
    })
}

/// Radio items. Each option is a label, the command that a click sends, and
/// whether the server allows that command. A `selected` index out of range
/// marks no item.
fn radio(options: Vec<(String, Command, bool)>, selected: usize) -> MenuItem<SpunTray> {
    let (labels, commands): (Vec<_>, Vec<_>) = options
        .into_iter()
        .map(|(label, command, enabled)| ((label, enabled), command))
        .unzip();
    MenuItem::RadioGroup(RadioGroup {
        selected,
        select: Box::new(move |t: &mut SpunTray, index| {
            if let Some(command) = commands.get(index) {
                player::enqueue(&t.command_tx, command.clone());
            }
        }),
        options: labels
            .into_iter()
            .map(|(label, enabled)| RadioItem {
                label: menu_label(label),
                enabled,
                ..RadioItem::default()
            })
            .collect(),
    })
}

/// The repeat row for the Controls submenu: one radio item per mode that the
/// server allows. The current mode is marked, and the row names it, so the
/// mode shows before the submenu opens. A mode the server does not allow
/// stays visible but cannot be clicked.
fn repeat_menu(snapshot: &Snapshot) -> MenuItem<SpunTray> {
    let options = [
        ("Off", RepeatMode::Off),
        ("Track", RepeatMode::One),
        ("All", RepeatMode::All),
    ]
    .into_iter()
    .map(|(label, mode)| {
        let command = Command::SetRepeat(mode);
        let enabled = snapshot.supports(&command);
        (label.to_string(), command, enabled)
    })
    .collect();
    let (selected, label) = match snapshot.repeat {
        Some(RepeatMode::Off) => (0, "Repeat (off)"),
        Some(RepeatMode::One) => (1, "Repeat (track)"),
        Some(RepeatMode::All) => (2, "Repeat (all)"),
        // A server that sends no mode marks no row.
        None => (usize::MAX, "Repeat"),
    };
    submenu(
        label,
        "media-playlist-repeat",
        vec![radio(options, selected)],
    )
}

/// The other media row for the More submenu: one radio item per
/// `other_media_action`. The row names the current action, as the Repeat
/// row does. A pick changes the running config, not the config file. The
/// player saves the pick in the state.
fn other_media_menu(current: OtherMediaAction) -> MenuItem<SpunTray> {
    const ACTIONS: [(&str, OtherMediaAction); 3] = [
        ("Ignore", OtherMediaAction::Ignore),
        ("Mute", OtherMediaAction::Mute),
        ("Pause", OtherMediaAction::Pause),
    ];
    let (selected, label) = match current {
        OtherMediaAction::Ignore => (0, "Other media (ignore)"),
        OtherMediaAction::Mute => (1, "Other media (mute)"),
        OtherMediaAction::Pause => (2, "Other media (pause)"),
    };
    let group = RadioGroup {
        selected,
        select: Box::new(|t: &mut SpunTray, index| {
            if let Some(&(_, action)) = ACTIONS.get(index) {
                t.config
                    .send_modify(|config| config.other_media_action = Some(action));
            }
        }),
        options: ACTIONS
            .iter()
            .map(|(label, _)| RadioItem {
                label: (*label).into(),
                ..RadioItem::default()
            })
            .collect(),
    };
    submenu(
        label,
        "multimedia-volume-control",
        vec![MenuItem::RadioGroup(group)],
    )
}

/// The TV Mode row for the More submenu. A click flips the running
/// config, not the config file. The player saves the pick in the state.
fn tv_mode_item(on: bool) -> MenuItem<SpunTray> {
    checkmark("TV Mode", on, true, |t| {
        t.config
            .send_modify(|config| config.tv_mode = Some(!config.tv_mode.unwrap_or_default()));
    })
}

/// The shuffle row for the Controls submenu. A click flips shuffle.
fn shuffle_item(snapshot: &Snapshot) -> MenuItem<SpunTray> {
    let on = snapshot.shuffle.unwrap_or(false);
    checkmark(
        "Shuffle",
        on,
        snapshot.supports(&Command::SetShuffle(!on)),
        |t| {
            player::enqueue(
                &t.command_tx,
                Command::SetShuffle(!t.snapshot.shuffle.unwrap_or(false)),
            )
        },
    )
}

/// The output device submenu: the system default, then each device that the
/// audio host lists. A configured device that is gone marks no row.
fn device_menu(devices: &[OutputDevice], current: Option<&str>) -> MenuItem<SpunTray> {
    let options: Vec<_> = std::iter::once(("System default".to_string(), None))
        .chain(
            devices
                .iter()
                .map(|device| (device.label.clone(), Some(device.name.clone()))),
        )
        .map(|(label, name)| (label, Command::SelectDevice(name), true))
        .collect();
    let current = Command::SelectDevice(current.map(Into::into));
    let selected = options
        .iter()
        .position(|(_, command, _)| *command == current)
        .unwrap_or(usize::MAX);
    submenu(
        "Output device",
        "audio-card",
        vec![radio(options, selected)],
    )
}

/// The server submenu: the connection state and its error, then each
/// discovered server. The current server is marked.
fn server_menu(snapshot: &Snapshot, servers: &[discovery::Server]) -> MenuItem<SpunTray> {
    let mut items = vec![row(link_text(snapshot.link))];
    items.extend(snapshot.error.iter().map(|error| row(error.as_str())));
    items.push(MenuItem::Separator);
    if servers.is_empty() {
        items.push(row("No servers discovered"));
    } else {
        let selected = servers
            .iter()
            .position(|server| snapshot.server_url.as_ref() == Some(&server.url))
            .unwrap_or(usize::MAX);
        let options = servers
            .iter()
            .map(|server| {
                let url = server.url.clone();
                (server.label.clone(), Command::SelectServer(url), true)
            })
            .collect();
        items.push(radio(options, selected));
    }
    submenu("Server", "network-server", items)
}

impl Tray for SpunTray {
    // A left click runs the configured action; a right click opens the menu.
    const MENU_ON_ACTIVATE: bool = false;

    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").into()
    }

    fn title(&self) -> String {
        format!("{} – Sendspin player", env!("CARGO_PKG_NAME"))
    }

    // A media player is an application status item, per the SNI spec.
    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: env!("CARGO_PKG_NAME").into(),
            description: self.status_line(),
            ..ksni::ToolTip::default()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let state = icon_state(&self.snapshot);
        let run = self.clock.run(std::time::Instant::now());
        vec![ksni::Icon {
            width: icon::SIZE as i32,
            height: icon::SIZE as i32,
            data: self.icon.pixels(state, self.snapshot.colors, run),
        }]
    }

    // A left click runs the configured action; a second click within
    // `DOUBLE_CLICK_WINDOW` also runs the double-click action. On GNOME,
    // a single left click opens the menu and the panel forwards `activate`
    // only for a double click, so this only ever runs the double-click
    // action there.
    fn activate(&mut self, _x: i32, _y: i32) {
        let bindings = self.config.borrow().tray.clone();
        if self.panel == Panel::Gnome {
            self.run(bindings.double_click);
            return;
        }
        let double = self
            .last_activate
            .replace(std::time::Instant::now())
            .is_some_and(|last| last.elapsed() <= DOUBLE_CLICK_WINDOW);
        self.run(bindings.left_click);
        if double {
            self.run(bindings.double_click);
        }
    }

    // A device that the user plugs in shows up the next time the menu opens.
    // ponytail: the list refreshes on the D-Bus task. ALSA lists devices
    // without opening them, so this is quick; move it to a blocking task if
    // a slow audio host stalls the menu.
    fn menu_about_to_show(&mut self) {
        self.devices = player::output_devices();
    }

    // A middle click runs the configured action.
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        let action = self.config.borrow().tray.middle_click;
        self.run(action);
    }

    // The mouse wheel over the icon: the SNI docs leave the sign to the
    // panel. KDE sends positive for scroll up; GNOME sends positive for
    // scroll down and its extension floors the delta, so a partial down
    // scroll arrives as 0 and only an up scroll goes negative.
    fn scroll(&mut self, delta: i32, _orientation: ksni::Orientation) {
        let bindings = self.config.borrow().tray.clone();
        let action = if self.panel.scrolls_up(delta) {
            bindings.scroll_up
        } else {
            bindings.scroll_down
        };
        self.run(action);
    }

    // The top level shows only the track. The rest is in submenus.
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = &self.snapshot;
        let headline = match (&s.title, s.link) {
            (Some(title), _) => title.as_str(),
            (None, Link::Connected) => "Nothing playing",
            (None, link) => link_text(link),
        };
        let mut items = vec![row(headline)];
        items.extend(
            s.artist
                .iter()
                .chain(&s.album)
                .map(|text| row(text.as_str())),
        );
        items.push(MenuItem::Separator);

        let (label, icon_name) = if s.playing {
            ("Pause", "media-playback-pause")
        } else {
            ("Play", "media-playback-start")
        };
        items.push(command_item(label, icon_name, s, Command::TogglePlayback));
        let playback = vec![
            command_item("Next", "media-skip-forward", s, Command::Next),
            command_item("Previous", "media-skip-backward", s, Command::Previous),
            command_item("Stop", "media-playback-stop", s, Command::Stop),
            MenuItem::Separator,
            repeat_menu(s),
            shuffle_item(s),
        ];
        items.push(submenu("Controls", "media-playback-start", playback));

        let (mute, mute_icon) = if s.muted {
            ("Unmute", "audio-volume-high")
        } else {
            ("Mute", "audio-volume-muted")
        };
        let volume = vec![
            command_item("Volume up", "audio-volume-high", s, Command::VolumeUp),
            command_item("Volume down", "audio-volume-low", s, Command::VolumeDown),
            command_item(mute, mute_icon, s, Command::ToggleMute),
        ];
        let (label, icon_name) = if s.muted {
            (
                format!("Volume: {}% (muted)", s.volume),
                "audio-volume-muted",
            )
        } else {
            (format!("Volume: {}%", s.volume), "audio-volume-high")
        };
        items.push(submenu(label, icon_name, volume));

        let mut more = vec![
            device_menu(&self.devices, s.audio_device.as_deref()),
            server_menu(s, &self.servers),
            action(
                "Open Music Assistant",
                "web-browser",
                self.web_ui_url().is_some(),
                |t| t.open_web_ui(),
            ),
            MenuItem::Separator,
        ];
        {
            let config = self.config.borrow();
            more.push(other_media_menu(
                config.other_media_action.unwrap_or_default(),
            ));
            more.push(tv_mode_item(config.tv_mode.unwrap_or_default()));
        }
        more.push(MenuItem::Separator);
        more.push(action("Quit", "application-exit", true, |t| {
            t.shutdown_tx.send_replace(true);
        }));
        items.push(submenu("More", "view-more-symbolic", more));
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TrayConfig;

    fn tray(
        config: TrayConfig,
        snapshot: Snapshot,
        command_tx: CommandTx,
        shutdown_tx: tokio::sync::watch::Sender<bool>,
    ) -> SpunTray {
        let (config, _) = watch::channel(Config {
            tray: config,
            ..Config::default()
        });
        // The panel does not follow the environment of the test run.
        SpunTray {
            panel: Panel::Standard,
            ..SpunTray::new(
                snapshot,
                config,
                brand_style(),
                Vec::new(),
                command_tx,
                shutdown_tx,
            )
        }
    }

    /// The looks for a desktop that says nothing, without asking it.
    fn brand_style() -> Style {
        crate::config::LooksConfig {
            animations: crate::config::Toggle::Off,
            ..Default::default()
        }
        .style(crate::icon::Desktop::default())
    }

    #[test]
    fn the_panel_follows_the_desktop_names() {
        // GNOME forks list themselves alongside their real name.
        for desktop in ["GNOME", "ubuntu:GNOME", "GNOME-Flashback:GNOME"] {
            assert_eq!(panel_for_desktop(desktop), Panel::Gnome, "{desktop}");
        }
        for desktop in ["KDE", "", "Hyprland", "KDE:wlroots"] {
            assert_eq!(panel_for_desktop(desktop), Panel::Standard, "{desktop}");
        }
    }

    fn connected(commands: &[&str]) -> Snapshot {
        Snapshot {
            link: Link::Connected,
            controllable: true,
            supported_commands: commands.iter().map(|c| c.to_string()).collect(),
            ..Snapshot::default()
        }
    }

    #[test]
    fn the_badge_is_a_square_argb_pixmap() {
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let tray = tray(
            TrayConfig::default(),
            Snapshot::default(),
            command_tx,
            shutdown_tx,
        );
        let icons = tray.icon_pixmap();
        assert_eq!(icons.len(), 1);
        let icon = &icons[0];
        // A square pixmap fills the panel's square slot; a wide one would be
        // scaled down by width and leave the slot half empty.
        assert_eq!(icon.width, icon::SIZE as i32);
        assert_eq!(icon.height, icon::SIZE as i32);
        assert_eq!(icon.data.len(), (icon::SIZE * icon::SIZE * 4) as usize);
        assert!(tray.icon_name().is_empty());
    }

    #[test]
    fn the_badge_state_follows_the_player_state() {
        let link = |link| Snapshot {
            link,
            ..Snapshot::default()
        };
        let connected = |playing, title: Option<&str>| Snapshot {
            playing,
            title: title.map(Into::into),
            ..link(Link::Connected)
        };
        let cases = [
            (link(Link::NoServer), State::Offline),
            (link(Link::Connecting), State::Connecting),
            (
                Snapshot {
                    error: Some("connection refused".into()),
                    ..link(Link::Disconnected)
                },
                State::Offline,
            ),
            (
                connected(true, Some("Song")),
                State::Playing { muted: false },
            ),
            (
                Snapshot {
                    muted: true,
                    ..connected(true, None)
                },
                State::Playing { muted: true },
            ),
            // A pause arrives as a stop, and the kept title marks it.
            (
                connected(false, Some("Song")),
                State::Paused { muted: false },
            ),
            (
                Snapshot {
                    controllable: true,
                    ..connected(false, None)
                },
                State::Stopped {
                    muted: false,
                    controllable: true,
                },
            ),
            // An error wins over playback.
            (
                Snapshot {
                    error: Some("audio output failed".into()),
                    ..connected(true, Some("Song"))
                },
                State::Error,
            ),
        ];
        for (snapshot, state) in cases {
            assert_eq!(icon_state(&snapshot), state, "{snapshot:?}");
        }
    }

    #[test]
    fn a_turning_badge_sends_new_frames_until_a_pause_holds_it() {
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let playing = Snapshot {
            link: Link::Connected,
            playing: true,
            title: Some("Song".into()),
            ..Snapshot::default()
        };
        let mut tray = tray(
            TrayConfig::default(),
            playing.clone(),
            command_tx,
            shutdown_tx,
        );
        tray.icon.animations = true;
        let upright = tray.icon_pixmap()[0].data.clone();
        // Pretend the badge has been turning for a moment, which is what the
        // redraw timer in main does by advancing the clock.
        let earlier = std::time::Instant::now() - icon::SPIN_FRAME * 3;
        tray.clock = Clock::new(icon_state(&playing), earlier);
        assert_ne!(tray.icon_pixmap()[0].data, upright);

        // A pause keeps the angle, so the panel gets no new frames.
        tray.set_snapshot(Snapshot {
            playing: false,
            ..playing
        });
        let paused = tray.icon_pixmap()[0].data.clone();
        assert_ne!(paused, upright, "the pause keeps the angle");
        assert_eq!(tray.icon_pixmap()[0].data, paused);
    }

    #[tokio::test]
    #[ignore = "requires an isolated session bus; run under dbus-run-session"]
    async fn dbus_tray_registers_updates_and_shuts_down() {
        use ksni::TrayMethods;

        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tray(
            TrayConfig::default(),
            Snapshot::default(),
            command_tx,
            shutdown_tx,
        )
        .assume_sni_available(true)
        .spawn()
        .await
        .unwrap();
        assert!(!handle.is_closed());
        let title = handle
            .update(|tray| {
                tray.snapshot = Snapshot {
                    title: Some("D-Bus test".into()),
                    ..Snapshot::default()
                };
                tray.snapshot.title.clone()
            })
            .await;
        assert_eq!(title, Some(Some("D-Bus test".into())));
        handle.shutdown().await;
        assert!(handle.is_closed());
        assert!(handle.update(|_| ()).await.is_none());
    }

    #[test]
    fn tool_tip_shows_the_track_only_when_connected() {
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let mut tray = tray(
            TrayConfig::default(),
            Snapshot::default(),
            command_tx,
            shutdown_tx,
        );
        assert_eq!(tray.tool_tip().description, "Connecting...");
        tray.snapshot = Snapshot {
            link: Link::Connected,
            title: Some("Song".into()),
            artist: Some("Band".into()),
            ..Snapshot::default()
        };
        assert_eq!(tray.tool_tip().description, "Song – Band");
        assert_eq!(tray.tool_tip().title, "spun");
    }

    #[test]
    fn quit_bypasses_a_full_command_queue() {
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        command_tx.try_send(Command::Stop).unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut tray = tray(
            TrayConfig::default(),
            Snapshot::default(),
            command_tx,
            shutdown_tx,
        );
        let Some(MenuItem::SubMenu(mut more)) = tray.menu().pop() else {
            panic!("More must be the last submenu");
        };
        let Some(MenuItem::Standard(quit)) = more.submenu.pop() else {
            panic!("Quit must be the last item in More");
        };
        assert_eq!(quit.label, "Quit");
        (quit.activate)(&mut tray);
        assert!(*shutdown_rx.borrow());
    }

    #[tokio::test]
    async fn left_click_toggles_playback_when_allowed() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let mut tray = tray(
            TrayConfig::default(),
            connected(&["play"]),
            command_tx,
            shutdown_tx,
        );
        tray.activate(0, 0);
        assert_eq!(command_rx.recv().await, Some(Command::TogglePlayback));
        // A left click does nothing while playback control is unavailable.
        tray.snapshot.playing = true;
        tray.activate(0, 0);
        assert!(command_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_second_click_within_the_window_also_runs_the_double_click_action() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(3);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let config = TrayConfig {
            left_click: TrayAction::PlayPause,
            double_click: TrayAction::Next,
            ..TrayConfig::default()
        };
        let mut tray = tray(
            config,
            connected(&["play", "next"]),
            command_tx,
            shutdown_tx,
        );
        tray.activate(0, 0);
        tray.activate(0, 0);
        // The second click runs the left-click action again, then the double-click action.
        assert_eq!(command_rx.recv().await, Some(Command::TogglePlayback));
        assert_eq!(command_rx.recv().await, Some(Command::TogglePlayback));
        assert_eq!(command_rx.recv().await, Some(Command::Next));
    }

    #[tokio::test]
    async fn a_click_after_the_window_only_runs_the_single_click_action() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(2);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let config = TrayConfig {
            double_click: TrayAction::Next,
            ..TrayConfig::default()
        };
        let mut tray = tray(
            config,
            connected(&["play", "next"]),
            command_tx,
            shutdown_tx,
        );
        // Pretend the last click happened long ago without sleeping.
        tray.last_activate = Some(std::time::Instant::now() - 2 * DOUBLE_CLICK_WINDOW);
        tray.activate(0, 0);
        assert_eq!(command_rx.recv().await, Some(Command::TogglePlayback));
        assert!(command_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn scrolling_adjusts_volume_and_the_middle_click_is_rebindable() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(4);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let config = TrayConfig {
            middle_click: TrayAction::ToggleMute,
            ..TrayConfig::default()
        };
        let mut tray = tray(
            config,
            connected(&["volume", "mute"]),
            command_tx,
            shutdown_tx,
        );
        tray.scroll(120, ksni::Orientation::Vertical);
        assert_eq!(command_rx.recv().await, Some(Command::VolumeUp));
        tray.scroll(-120, ksni::Orientation::Vertical);
        assert_eq!(command_rx.recv().await, Some(Command::VolumeDown));
        tray.secondary_activate(0, 0);
        assert_eq!(command_rx.recv().await, Some(Command::ToggleMute));
    }

    #[tokio::test]
    async fn unbound_mouse_events_do_nothing() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let mut tray = tray(
            TrayConfig::default(),
            connected(&["volume", "mute"]),
            command_tx,
            shutdown_tx,
        );
        tray.secondary_activate(0, 0);
        assert!(command_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn on_gnome_activate_is_a_double_click_and_the_wheel_flips() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(4);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let config = TrayConfig {
            double_click: TrayAction::Next,
            ..TrayConfig::default()
        };
        let mut tray = tray(
            config,
            connected(&["next", "volume"]),
            command_tx,
            shutdown_tx,
        );
        tray.panel = Panel::Gnome;
        // GNOME opens the menu on a single left click and forwards `activate`
        // only for a double click, so this runs the double-click action only.
        tray.activate(0, 0);
        tray.activate(0, 0);
        assert_eq!(command_rx.recv().await, Some(Command::Next));
        assert_eq!(command_rx.recv().await, Some(Command::Next));
        assert!(command_rx.try_recv().is_err());
        // Clutter deltas are positive for scroll down and the extension
        // floors them, so a partial down scroll arrives as 0 and only an up
        // scroll arrives negative. Both must not run the up binding.
        tray.scroll(0, ksni::Orientation::Vertical);
        assert_eq!(command_rx.recv().await, Some(Command::VolumeDown));
        tray.scroll(-1, ksni::Orientation::Vertical);
        assert_eq!(command_rx.recv().await, Some(Command::VolumeUp));
        // A full notch still scrolls down.
        tray.scroll(1, ksni::Orientation::Vertical);
        assert_eq!(command_rx.recv().await, Some(Command::VolumeDown));
    }

    #[tokio::test]
    async fn mouse_events_open_the_web_ui_only_when_bound() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(2);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let config = TrayConfig {
            middle_click: TrayAction::OpenWebUi,
            ..TrayConfig::default()
        };
        let mut tray = tray(
            config,
            Snapshot {
                server_url: Some("ws://localhost:8927/sendspin".into()),
                ..connected(&["play"])
            },
            command_tx,
            shutdown_tx,
        );
        // The default double-click binding opens the web UI. Without a Music
        // Assistant server on the LAN, it opens nothing, and the command
        // queue stays empty.
        tray.activate(0, 0);
        tray.activate(0, 0);
        assert_eq!(command_rx.recv().await, Some(Command::TogglePlayback));
        assert_eq!(command_rx.recv().await, Some(Command::TogglePlayback));
        assert!(command_rx.try_recv().is_err());
    }

    #[test]
    fn the_top_level_shows_the_track_and_the_rest_is_in_submenus() {
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let kitchen = "ws://192.168.1.20:8927/sendspin".to_string();
        let snapshot = Snapshot {
            title: Some("Song".into()),
            artist: Some("Band".into()),
            album: Some("Album".into()),
            volume: 40,
            server_url: Some(kitchen.clone()),
            ..connected(&[])
        };
        let mut tray = tray(TrayConfig::default(), snapshot, command_tx, shutdown_tx);
        tray.servers = vec![
            discovery::Server {
                label: "Attic".into(),
                url: "ws://192.168.1.21:8927/sendspin".into(),
            },
            discovery::Server {
                label: "Kitchen".into(),
                url: kitchen,
            },
        ];
        let menu = tray.menu();
        assert_eq!(
            labels(&menu),
            [
                "Song",
                "Band",
                "Album",
                "-",
                "Play",
                "Controls",
                "Volume: 40%",
                "More"
            ]
        );
        let more = find_submenu(&menu, "More").expect("a More submenu");
        assert_eq!(
            labels(&more.submenu),
            [
                "Output device",
                "Server",
                "Open Music Assistant",
                "-",
                "Other media (mute)",
                "TV Mode",
                "-",
                "Quit"
            ]
        );
        // The server submenu marks the current server.
        let server = find_submenu(&more.submenu, "Server").expect("a Server submenu");
        let Some(MenuItem::RadioGroup(group)) = server.submenu.last() else {
            panic!("the servers must be radio items");
        };
        assert_eq!(group.selected, 1);
    }

    /// The label of each item in `menu`, with `-` for a separator.
    fn labels(menu: &[MenuItem<SpunTray>]) -> Vec<&str> {
        menu.iter()
            .map(|item| match item {
                MenuItem::Standard(item) => item.label.as_str(),
                MenuItem::Checkmark(item) => item.label.as_str(),
                MenuItem::SubMenu(submenu) => submenu.label.as_str(),
                _ => "-",
            })
            .collect()
    }

    /// The submenu whose label starts with `label`, at any depth of `menu`.
    /// The Repeat row adds its mode to its label.
    fn find_submenu<'a>(
        menu: &'a [MenuItem<SpunTray>],
        label: &str,
    ) -> Option<&'a SubMenu<SpunTray>> {
        menu.iter().find_map(|item| match item {
            MenuItem::SubMenu(submenu) if submenu.label.starts_with(label) => Some(submenu),
            MenuItem::SubMenu(submenu) => find_submenu(&submenu.submenu, label),
            _ => None,
        })
    }

    /// The radio group inside the "Output device" submenu.
    fn device_group(menu: &[MenuItem<SpunTray>]) -> &RadioGroup<SpunTray> {
        let device = find_submenu(menu, "Output device").expect("an Output device submenu");
        match device.submenu.first() {
            Some(MenuItem::RadioGroup(group)) => group,
            _ => panic!("the Output device submenu must hold radio items"),
        }
    }

    /// The radio group inside the "Repeat" submenu.
    fn repeat_group(menu: &[MenuItem<SpunTray>]) -> &RadioGroup<SpunTray> {
        let repeat = find_submenu(menu, "Repeat").expect("a Repeat submenu");
        match repeat.submenu.first() {
            Some(MenuItem::RadioGroup(group)) => group,
            _ => panic!("the Repeat submenu must hold radio items"),
        }
    }

    /// The checkmark with `label` in the submenu `parent`.
    fn check_row<'a>(
        menu: &'a [MenuItem<SpunTray>],
        parent: &str,
        label: &str,
    ) -> &'a ksni::menu::CheckmarkItem<SpunTray> {
        find_submenu(menu, parent)
            .into_iter()
            .flat_map(|parent| &parent.submenu)
            .find_map(|item| match item {
                MenuItem::Checkmark(item) if item.label == label => Some(item),
                _ => None,
            })
            .unwrap_or_else(|| panic!("a {label} row in {parent}"))
    }

    #[test]
    fn the_more_submenu_changes_the_settings_and_opens_the_web_ui() {
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let snapshot = Snapshot {
            server_url: Some("ws://192.168.1.20:8927/sendspin".into()),
            ..connected(&[])
        };
        let mut tray = tray(TrayConfig::default(), snapshot, command_tx, shutdown_tx);
        let menu = tray.menu();

        let awake = check_row(&menu, "More", "TV Mode");
        assert!(!awake.checked);
        (awake.activate)(&mut tray);
        assert_eq!(tray.config.borrow().tv_mode, Some(true));
        assert!(check_row(&tray.menu(), "More", "TV Mode").checked);

        // The other media submenu marks the action, and its row names it.
        let other_media = |menu: &[MenuItem<SpunTray>]| {
            let row = find_submenu(menu, "Other media").expect("an Other media submenu");
            let Some(MenuItem::RadioGroup(group)) = row.submenu.first() else {
                panic!("the Other media submenu must hold radio items");
            };
            (row.label.clone(), group.selected)
        };
        assert_eq!(other_media(&menu), ("Other media (mute)".into(), 1));
        let Some(MenuItem::RadioGroup(group)) =
            find_submenu(&menu, "Other media").and_then(|row| row.submenu.first())
        else {
            panic!("the Other media submenu must hold radio items");
        };
        let options: Vec<&str> = group.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(options, ["Ignore", "Mute", "Pause"]);
        for (index, action, label) in [
            (2, OtherMediaAction::Pause, "Other media (pause)"),
            (0, OtherMediaAction::Ignore, "Other media (ignore)"),
        ] {
            (group.select)(&mut tray, index);
            assert_eq!(tray.config.borrow().other_media_action, Some(action));
            assert_eq!(other_media(&tray.menu()), (label.into(), index));
        }

        // The web UI row is enabled once discovery finds the web UI.
        let web_ui_enabled = |tray: &SpunTray| {
            let menu = tray.menu();
            let more = find_submenu(&menu, "More").expect("a More submenu");
            more.submenu.iter().any(|item| {
                matches!(item, MenuItem::Standard(item)
                    if item.label == "Open Music Assistant" && item.enabled)
            })
        };
        assert!(!web_ui_enabled(&tray));
        tray.web_uis = vec!["http://192.168.1.20:8095".into()];
        assert!(web_ui_enabled(&tray));
    }

    #[tokio::test]
    async fn the_device_menu_marks_the_current_device_and_sends_a_pick() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(2);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let mut tray = tray(
            TrayConfig::default(),
            Snapshot {
                audio_device: Some("hw:1".into()),
                ..Snapshot::default()
            },
            command_tx,
            shutdown_tx,
        );
        tray.devices = vec![
            OutputDevice {
                name: "hw:0".into(),
                label: "Speakers (hw:0)".into(),
            },
            OutputDevice {
                name: "hw:1".into(),
                label: "USB_DAC (hw:1)".into(),
            },
        ];
        let menu = tray.menu();
        let group = device_group(&menu);
        let labels: Vec<&str> = group.options.iter().map(|o| o.label.as_str()).collect();
        // A doubled underscore shows as one `_` instead of an access key.
        assert_eq!(
            labels,
            ["System default", "Speakers (hw:0)", "USB__DAC (hw:1)"]
        );
        assert_eq!(group.selected, 2);
        (group.select)(&mut tray, 0);
        (group.select)(&mut tray, 1);
        assert_eq!(command_rx.recv().await, Some(Command::SelectDevice(None)));
        assert_eq!(
            command_rx.recv().await,
            Some(Command::SelectDevice(Some("hw:0".into())))
        );
        // A configured device that is gone marks no row.
        tray.snapshot.audio_device = Some("hw:9".into());
        let menu = tray.menu();
        assert!(device_group(&menu).selected >= device_group(&menu).options.len());
        tray.snapshot.audio_device = None;
        assert_eq!(device_group(&tray.menu()).selected, 0);
    }

    #[tokio::test]
    async fn the_controls_submenu_repeats_and_shuffles() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(4);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let mut tray = tray(
            TrayConfig::default(),
            Snapshot {
                repeat: Some(RepeatMode::One),
                shuffle: Some(true),
                ..connected(&[
                    "repeat_off",
                    "repeat_one",
                    "repeat_all",
                    "shuffle",
                    "unshuffle",
                ])
            },
            command_tx,
            shutdown_tx,
        );
        let menu = tray.menu();
        let controls = find_submenu(&menu, "Controls").expect("a Controls submenu");
        assert_eq!(
            labels(&controls.submenu),
            ["Next", "Previous", "Stop", "-", "Repeat (track)", "Shuffle"]
        );
        // The repeat submenu marks the current mode.
        let group = repeat_group(&menu);
        let options: Vec<&str> = group.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(options, ["Off", "Track", "All"]);
        assert_eq!(group.selected, 1);
        (group.select)(&mut tray, 2);
        assert_eq!(
            command_rx.recv().await,
            Some(Command::SetRepeat(RepeatMode::All))
        );
        // The shuffle row shows the server state and sends its opposite.
        let shuffle = check_row(&menu, "Controls", "Shuffle");
        assert!(shuffle.checked);
        assert!(shuffle.enabled);
        (shuffle.activate)(&mut tray);
        assert_eq!(command_rx.recv().await, Some(Command::SetShuffle(false)));
        assert!(command_rx.try_recv().is_err());
    }

    #[test]
    fn repeat_and_shuffle_are_disabled_without_the_commands() {
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let tray = tray(
            TrayConfig::default(),
            connected(&["play"]),
            command_tx,
            shutdown_tx,
        );
        let menu = tray.menu();
        // A server that sends no mode marks no repeat row.
        assert_eq!(repeat_group(&menu).selected, usize::MAX);
        assert!(repeat_group(&menu).options.iter().all(|o| !o.enabled));
        assert!(!check_row(&menu, "Controls", "Shuffle").enabled);
    }
}
