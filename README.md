# ![Spu∩](assets/spun-wordmark.png)

Spun is a lightweight [Sendspin](https://sendspin-audio.com/) client that wires [Music Assistant](https://www.music-assistant.io/) to your Linux desktop's [native multimedia controls](https://specifications.freedesktop.org/mpris/latest/) and keyboard's media keys. It lives in your system tray, and is designed to require minimal setup and never to bother you unless something is wrong. Add it to your login items if you wish. When you (or your automations) want to play music or announce something, it dutifully does just that and then gets out of your way again.

What ended up becoming Spun started as an Home Assistant automation that fired up a terminal running the [sendspin python cli](https://github.com/Sendspin/sendspin-python-cli) when I wanted to play something. As it evolved into a real application, the design philosophy stayed. The best automations are those that you barely even remember are there (until they stop working of course; and often the problem is elsewhere): Spun aims for the same.

## Features

- Works with controls you already know:
  - Music Assistant
  - Your desktop's multimedia controls and widgets
  - Your keyboard's multimedia keys
  - Your automations
  - Scriptable with [playerctl](https://github.com/altdesktop/playerctl)
- Sane defaults, zero setup:
  - Automatically discovers your HA & MA on startup
  - If multiple instances are found, asks you to choose before connecting
- Delightful tray icon:
  - Adapts to the colors of the current track, or your DE's theme if unavailable
  - Spins at 33rpm when playing, like an LP
  - Quick volume control by scrolling on the icon
  - Configurable actions for wheel and left/middle/double click (GNOME: only double & middle click; left and right both open the menu)
- Knows when to shut up:
  - Automatically mute or pause when other media starts playing, and resume when it stops
  - With mute option, configurable delay (default 30 min) after which music is paused
  - Configurable: mute/pause/off, optional delay before acting and before stopping muted stream
- Doesn't get tired:
  - TV Mode keeps your screen awake while media is playing
- Lightweight:
  - Currently sits at around 40 MB RSS
- Boring
- Wayland native
- Written in Rust

## Requirements

- Wayland desktop on Linux
- A session D-Bus and a panel that implements StatusNotifierWatcher:
  - GNOME requires AppIndicator extension, pretty much everything else works out of the box
- Flatpak

## Get Spun

Install with Flatpak:
```sh
flatpak install --user https://ö.foo/spun.flatpakref
```
Or download from releases.

## Build

`packaging/` holds the Flatpak build assets. The app ID is `foo.xn__nda.spun`.

To build and install the Flatpak from the repository root, you need also `uv` and
`flatpak-builder`.

On fresh clone (or after changes to Cargo.lock):
```sh
uv run -s \
  https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/refs/heads/master/cargo/flatpak-cargo-generator.py \
  Cargo.lock -o packaging/cargo-sources.json
```

To (re)build and launch:
```sh
flatpak-builder --user --install --force-clean build-dir packaging/foo.xn__nda.spun.yml
flatpak run foo.xn__nda.spun
```

To start Spun at login, add it in the autostart settings of the desktop, for
example in KDE System Settings > Autostart or in GNOME Tweaks > Startup
Applications.

One instance runs for each state file. A second start, for example a click on
the launcher, finds the lock `state.lock` next to the state file and stops
at once. Two Spuns with one `client_id` would close the connection of each
other without end.

The Flatpak manifest asks for these permissions:

| Permission | Use |
| --- | --- |
| `--share=network` | The WebSocket connection and mDNS |
| `--socket=pulseaudio` | Audio, also on PipeWire |
| `--env=PULSE_COOKIE=/app/share/spun/pulse-cookie` | A cookie for the PulseAudio client of cpal |
| `--talk-name=org.kde.StatusNotifierWatcher` | The tray |
| `--talk-name=org.freedesktop.Notifications` | The desktop notifications |
| `--own-name=org.mpris.MediaPlayer2.spun` | The desktop media controls |
| `--talk-name=org.mpris.MediaPlayer2.*` | `other_media_action`, which follows the state of each player |
| `--talk-name=org.freedesktop.ScreenSaver` and `org.gnome.SessionManager` | `tv_mode` |

## Config

Spun uses a `config.toml` for persistent config overrides applied at launch. In the 
Flatpak, the config file is in `~/.var/app/foo.xn__nda.spun/config/spun/config.toml`.

The file is optional, and so is each key in it. Put in only what you want to change. 
These are the defaults:

```toml
server_url = "auto"   # or a ws:// or wss:// URL; see Server discovery
name = "spun"         # the player name on the server
# client_id = "…"     # without it, spun makes one up on the first run
# static_delay_ms = 0 # 0–5000; without it, spun keeps the delay that the server sets
# audio_device = "alsa_output.usb-Generic_USB_Audio-00.analog-stereo"  # without it, spun keeps the last menu pick
# tv_mode = false     # true keeps the display awake while something plays; without it, spun keeps the last menu pick
# other_media_action = "mute"  # or ignore, pause; see Other media; without it, spun keeps the last menu pick
other_media_delay_ms = 500   # how long other media must play before spun reacts
mute_snooze = 30      # minutes; after this long muted, spun pauses. 0 never pauses

[tray]
left_click = "play_pause"    # or next, previous, stop, volume_up, volume_down, toggle_mute, open_web_ui, none
double_click = "open_web_ui" # opens the Music Assistant web UI in the browser
middle_click = "none"
scroll_up = "volume_up"
scroll_down = "volume_down"

[looks]
dark_mode = "auto"  # or true, false
primary = "auto"    # or a hex colour such as "#2fb0ac"
secondary = "auto"
inactive = "auto"
animations = "auto" # or true, false
```

Spun watches the file with inotify and reads it again when you save it.
Changes to most settings apply at once. A new `server_url` makes Spun switch
servers. A new `name`, `client_id`, or `static_delay_ms` causes a reconnect.

Spun logs a warning for each unknown key and ignores it. If the file is not
valid TOML, or a key has a bad value, Spun keeps its current settings and
shows a desktop notification with the error.

### State

The following settings are saved between runs in Spun's `state.toml`,
in the same folder as the `config.toml`:

- `client_id`, which is made up on the first run.
- `server_url`: the server that was used last.
- `volume`: the player volume that was used last.
- `static_delay_ms`: the delay that the server set last.
- `audio_device`: the last pick in the Output device menu.
- `tv_mode`: the last TV Mode pick in the menu.
- `other_media_action`: the last pick in the Other media menu.

A key in the config overrides the saved state at startup, and when you edit
that key. Until then, a menu pick or a server command changes the value, and
the new value is saved in the state. For example, with `audio_device` in
the config file, a menu pick plays through the new device until the next
start or the next edit of `audio_device` the config file.

## Server discovery

Spun browses the LAN for `_sendspin-server._tcp.local.` services over mDNS.
The More > Server submenu lists each active server that it finds.
Click a server to switch to it. The last used server is saved in the state file,
so the next start uses the same server.

With `server_url = "auto"`, the default, Spun connects to the server that it
used last. On first run, it instead waits for discovery. When discovery finds
exactly one server, and no more turn up after a short delay, it is 
automatically chosen. If two or more servers turn up, Spun shows a desktop
notification asking you to manually choose the correct server through the
tray menu.

A URL in `server_url` pins the server, and spun connects to it at each
start. A click in the menu still switches servers until the next start.

With `--headless`, there is no menu. If the LAN has more than one server, set
`server_url` in the config file. Set it also if the LAN has no mDNS.

Spun prefers an IPv4 address. IPv6 is only used when the server has no IPv4
address. Loopback and link-local IPv6 addresses are skipped. Discovered servers
use plain `ws://` URLs, as the Sendspin spec mandates.

## Audio device

Spun prefers PulseAudio when available. Without PulseAudio, it tries ALSA. 
ALSA is best effort and considered an unsupported configuration.

The More > Output device submenu lists the system default and each output
that PulseAudio reports, such as a Bluetooth headset or an HDMI output.
The list refreshes each time the menu opens. Click an output to switch to it:
Spun opens the new output at once. The audio that the old output had queued
is lost, so the sound stops for about the server's lead time. The last used
output is remembered across restarts in the state file.

`audio_device` in the config file pins an output by its name, which the menu
shows in brackets and `pactl list short sinks` lists, for example
`alsa_output.usb-Generic_USB_Audio-00.analog-stereo`. Without the key, the
last menu pick, or the system default is used. If the output cannot be opened,
Spun plays nothing and shows the error in the Server submenu and in a desktop
notification. Pick another output, or wait for the next stream start, to play
again. When an output fails during playback, for example in a restart of the
sound server, Spun tries to open it again.

## TV Mode

Some displays stop passing the sound on, or sleep a device that does, when
the screen turns off. With `tv_mode = true`, Spun tells the desktop to
stay idle while something plays, and lets it sleep again when playback stops.
The default is `false`, but you can switch it from the menu easily and
your choice is remembered.

These are the ways Spun tries to accomplish it:

1. `org.freedesktop.ScreenSaver` on `/org/freedesktop/ScreenSaver`. KDE,
   GNOME, Xfce, Cinnamon, MATE, and a bare X screen saver all serve it.
2. The same interface on `/ScreenSaver`, the path that older KDE releases
   use.
3. `org.gnome.SessionManager.Inhibit`, the fallback that GNOME desktops
   offer when the screensaver interface is missing.

The first service that answers holds the inhibition until playback stops.
Without any of these services, a warning is logged.

## Other media

When `other_media_action` is anything except `ignore`, Spun follows the [MPRIS](https://specifications.freedesktop.org/mpris-spec/latest/) 
players on the session bus and gets out of the way while one of them plays.
Here are the valid choices:

- `"mute"`, the default, fades to silence in about half a second and keeps
  playing. When the other player stops or pauses, Spun fades back up.
- `"pause"` fades to silence and then pauses. When the other player stops,
  Spun plays again and fades back up. Without a server that offers `play` and
  `pause`, this is a mute.
- `"ignore"` plays as usual.

`other_media_delay_ms` is how long the other player must play before Spun
gets out of the way, in milliseconds. The default is 500. A player that
stops or pauses within that time leaves spun alone, so a short sound, such
as a notification in a browser tab, does not interrupt the music. Set it to
`0` to react at once.

`mute_snooze` is how long spun keeps playing while it is silent, in minutes.
After that, it pauses, so that a call or a film does not leave a stream
running for hours on the server. The default is 30. Set it to `0` to keep
playing until the other player stops. The server needs to offer `pause`
and `play`, for this to work.

## Under the hood

At its core Spun is basically just a bit of plumbing between the excellent
[sedspin-rs](https://github.com/Sendspin/sendspin-rs) and [mpris-server](https://github.com/SeaDve/mpris-server)
crates, with a few convenience features added in.

These are the Sendspin roles Spun uses:

- `player@v1` plays FLAC, Opus, or PCM at 48 kHz stereo through the configured
  audio device. The `SyncedPlayer` from the sendspin crate keeps the output in
  sync with the server clock. The player session from the sendspin crate
  applies server `volume`, `mute`, and `set_static_delay` commands to the
  output and reports the new state back. Spun decodes the audio chunks and
  feeds them to the output device.
- `metadata@v1` supplies the title, artist, and album for the menu and the
  tooltip. It also supplies the track position for the desktop media
  controls.
- `artwork@v1` supplies the album art for the desktop media controls.
- `controller@v1` supplies the play, pause, stop, next, previous, group
  volume, mute, repeat, and shuffle controls. These are enabled only
  when the server grants the role and lists the command.
- `color@v1` supplies colours for the current track, which the tray badge
  can use.

| File | Responsibility |
| --- | --- |
| `src/main.rs` | Options, runtime, signals, and shutdown |
| `src/config.rs` | Config and state files, validation, and the watch for config edits |
| `src/discovery.rs` | mDNS browse for Sendspin servers and Music Assistant web UIs |
| `src/player.rs` | Connection, reconnect, server switch, audio output and devices, and the state the tray shows |
| `src/mpris.rs` | MPRIS service for the desktop media controls |
| `src/notify.rs` | Desktop notifications for errors |
| `src/inhibit.rs` | Keeps the display awake while something plays |
| `src/other_media.rs` | Fades out while another media player on the desktop plays |
| `src/tray.rs` | StatusNotifierItem tray menu and tooltip |


## Tray mouse bindings

The `[tray]` config section rebinds every mouse event on the tray icon:
`left_click`, `double_click`, `middle_click`, `scroll_up`, and `scroll_down`.
Each takes one of `play_pause`, `next`, `previous`, `stop`, `volume_up`,
`volume_down`, `toggle_mute`, `open_web_ui`, or `none`. The defaults are
shown above in the config section.

`open_web_ui` opens the discovered Music Assistant web UI in the desktop
browser.

Right click always opens the menu: panels that implement StatusNotifierItem
decide that themselves, so it is not rebindable. Playback actions follow the
same rules as in the menu: the server must grant the controller role and list
the command, and a bound action that the server does not allow does nothing.
The double click is a second left click within 400 ms; panels do not report
double clicks separately, so the first click still runs `left_click`.

On GNOME with an AppIndicator extension, the panel itself behaves
differently, and we must adapt: both left and right clicks open the
menu, so `left_click` on the settings does nothing, and the scroll direction 
is opposite from what every other panel reports. Spun adjusts for this 
automatically by reading `$XDG_CURRENT_DESKTOP` at startup.

## Tray icon

The tray badge's colours and motion show the player state in its `u` and `∩` shapes:

| State | Badge |
| --- | --- |
| Playing | The `u` is primary and the `∩` is secondary color. The badge turns. |
| Paused | The badge stops at its angle and keeps the colours of Playing. |
| Stopped | Upright. The `u` is grey and the `∩` is primary color when Spun has the controller role and can resume. The `∩` is secondary color when the server does not grant the controller role. |
| Offline | All grey and upright. Spun has no server, or the connection dropped. |
| Connecting | Upright. The badge is grey, occasionally the `u` blinks, then the `∩`, in a loop. |
| Error | Upright and grey. The whole badge blinks twice in primary color every 3 seconds. |

While the group is muted, Playing, Paused, and Stopped are all grey. Paused
needs a track title. Sendspin has no pause state, so a pause arrives as a
stop, and only a kept title tells the two apart. Error means that spun cannot
play the stream, for example because the audio device does not open. The
Server submenu and/or a desktop notification show the reason. The error clears
when the stream ends, or when a later stream start or device pick opens the
device.

The `[looks]` config section picks the colours and the motion. Each setting
takes `"auto"`, which is also the default:

- `dark_mode` is `true`, `false`, or `"auto"`. `"auto"` is light only when the
  desktop prefers light, and dark otherwise. It is dark also when the desktop
  does not respond to the query.
- `primary` is the main colour of the badge. `"auto"` prefers the primary colour
  that the server picks for the track; or if unavailable the desktop accent colour.
  The teal `#2fb0ac` from Spun logo is used as a fallback if neither of the above
  can be determined.
- `secondary` is the second colour of the badge. `"auto"` takes the server's
  secondary colour for the track. Without one, it takes a shade of the primary
  colour: darker in dark mode, lighter in light mode.
- `inactive` is the grey in the table above. `"auto"` is `#8f8f8f` in dark
  mode and the darker `#5d5d5d` in light mode.
- `animations` is `true`, `false`, or `"auto"`. When it is on, the badge turns
  while something plays, steps while spun connects, and blinks on an error.
  A panel that ignores icon updates shows the badge still. When animations are
  off, Playing and Paused look the same, Connecting is all grey, and Error is all
  primary. `"auto"` means the animations are enabled unless the desktop asks for
  reduced motion.

The server colours come from the Sendspin `color@v1` role. The server can
take them from the album art, so they can change with the track. If artwork is
missing, the server can send no colours, or only some of them.

The desktop settings come from the [XDG settings
portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Settings.html):
`color-scheme`, `accent-color`, and `reduced-motion` in the
`org.freedesktop.appearance` namespace. The colour scheme is the preference
for app windows, and the panel can differ. For example, GNOME's normal light
mode reports no preference and keeps a dark top bar, so spun stays dark
there. Set `dark_mode` to match your panel when `"auto"` gets it wrong.

Spun reads the desktop settings at startup. It listens for the
`SettingChanged` signal of the portal, so a new colour scheme, accent
colour, or motion setting should apply at once.

## MPRIS

Spun publishes its state over [MPRIS](https://specifications.freedesktop.org/mpris-spec/latest/),
so the desktop media controls work. KDE shows them in the Media Player entry
of the system tray; GNOME shows them in the calendar drop-down. They show the
title, artist, album art, track length, and position, and the play/pause,
next, and previous buttons.

Media keys, KDE Connect, and `playerctl` control Spun:

```sh
playerctl -p spun metadata
playerctl -p spun position
playerctl -p spun play-pause
```

## Notifications

Spun shows a desktop notification when it needs your attention:

- Spun cannot reach the server, or the connection drops. The notification
  shows after the error has lasted 5 seconds, so a short disconnect shows
  none. Spun tries again and stays quiet while the error stays the same.
- Spun cannot play the stream, for example because the audio device does
  not open.
- Discovery found several servers. In that case you must manually select the
  correct server from Spun's menu (and optionally persist it to your
  `config.toml`)
- The config file has an error. The old settings stay in effect until
  the problem is fixed.

Spun closes a notification when its problem ends, for example when the
connection is back. A click on a notification closes it too.

## Test

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

A pre-commit hook runs the first two steps and stops a commit that fails
them. Enable it once per clone:

```sh
git config core.hooksPath .githooks
```

The tests include a loopback WebSocket server for the protocol paths. A
separate test registers the tray on an isolated D-Bus:

```sh
dbus-run-session -- cargo test tray::tests::dbus_tray_registers_updates_and_shuts_down -- --ignored --exact
```
