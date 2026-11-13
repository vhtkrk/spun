mod config;
mod discovery;
mod icon;
mod inhibit;
mod mpris;
mod notify;
mod other_media;
mod player;
mod tray;

use anyhow::{Context, Result};
use ksni::{Handle, TrayMethods};
use tokio::sync::watch;

use crate::config::Config;
use crate::icon::{Desktop, Style};
use crate::other_media::Fade;
use crate::player::Snapshot;
use crate::tray::SpunTray;

/// The app ID, which is also the Flatpak ID. The .desktop file, the icon, and
/// the metainfo in `packaging/` use it as their file name. The desktop uses it
/// to show the name and icon of spun with the media controls and the
/// notifications.
const APP_ID: &str = "foo.xn__nda.spun";

/// The tray, its badge style, and the desktop looks that the style follows.
type TrayView = (Handle<SpunTray>, Style, watch::Receiver<Desktop>);

/// Forwards the player state, the discovered servers and web UIs, and the
/// badge style to the tray. The style follows each config edit and each
/// change of the desktop looks. The tray reads the rest of the config
/// itself, and each config edit makes it draw the menu again. While the
/// badge animates, a timer redraws it. Without a tray, this only logs the
/// player state and the servers.
async fn show(
    mut snapshots: player::SnapshotRx,
    mut servers: player::DiscoveredRx,
    mut web_uis: watch::Receiver<Vec<String>>,
    mut config: watch::Receiver<Config>,
    tray: Option<TrayView>,
) -> Result<()> {
    let Some((tray, mut style, mut desktop)) = tray else {
        return log_changes(snapshots, servers).await;
    };
    let mut redraw = animation(&style);
    loop {
        // A closed channel disables its branch; discovery may be unavailable.
        tokio::select! {
            Ok(()) = snapshots.changed() => {}
            Ok(()) = servers.changed() => {}
            Ok(()) = web_uis.changed() => {}
            Ok(()) = config.changed() => {}
            Ok(()) = desktop.changed() => {}
            Some(_) = next_tick(&mut redraw) => {}
            else => return Ok(()),
        }
        let restyled = config
            .borrow_and_update()
            .looks
            .style(*desktop.borrow_and_update());
        // The badge follows each edit of [looks] and each change of the
        // desktop looks. The desktop watch keeps D-Bus out of each draw.
        if restyled != style {
            style = restyled;
            redraw = animation(&style);
        }
        let snapshot = snapshots.borrow_and_update().clone();
        let found = servers.borrow_and_update().clone();
        let found_web_uis = web_uis.borrow_and_update().clone();
        let icon = style.clone();
        // ksni sends only what changed, so a redraw of a still badge sends
        // nothing.
        tray.update(|tray| {
            tray.set_snapshot(snapshot);
            tray.servers = found;
            tray.web_uis = found_web_uis;
            tray.icon = icon;
        })
        .await
        .context("tray service closed")?;
    }
}

/// Logs each new player state and server list, for `--headless`.
async fn log_changes(
    mut snapshots: player::SnapshotRx,
    mut servers: player::DiscoveredRx,
) -> Result<()> {
    loop {
        tokio::select! {
            Ok(()) = snapshots.changed() => {}
            Ok(()) = servers.changed() => {}
            else => return Ok(()),
        }
        log::info!(
            "{:?}; discovered: {:?}",
            *snapshots.borrow_and_update(),
            *servers.borrow_and_update()
        );
    }
}

/// A timer for the badge animations, or `None` when the badge holds still.
fn animation(style: &Style) -> Option<tokio::time::Interval> {
    style.frame_interval().map(|frame| {
        let mut timer = tokio::time::interval(frame);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        timer
    })
}

/// Waits for the next tick, or gives `None` at once without a timer.
async fn next_tick(timer: &mut Option<tokio::time::Interval>) -> Option<()> {
    timer.as_mut()?.tick().await;
    Some(())
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    headless: bool,
    help: bool,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut options = Self {
            headless: false,
            help: false,
        };
        for arg in args {
            match arg.as_str() {
                "--headless" => options.headless = true,
                "--help" | "-h" => options.help = true,
                _ => anyhow::bail!("unknown argument: {arg}; use --help"),
            }
        }
        Ok(options)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let options = Options::parse(std::env::args().skip(1))?;
    if options.help {
        println!(
            "Usage: spun [--headless]\n\n\
             --headless   Log snapshots instead of registering a tray"
        );
        return Ok(());
    }
    let config_path = config::path()?;
    // A bad config does not stop spun. spun runs with the defaults, shows
    // the error, and takes the next valid edit.
    let (mut config, config_error) = match Config::load(&config_path) {
        Ok(config) => (config, None),
        Err(error) => {
            log::warn!("{error:#}; using the defaults");
            (Config::default(), Some(format!("{error:#}")))
        }
    };
    let state_path = config::state_path()?;
    // The desktop starts spun again for a click on its launcher. spun has no
    // window to show, so the second spun stops here.
    let Some(_lock) = config::lock(&state_path)? else {
        log::info!("spun runs already with this state file; this one stops");
        return Ok(());
    };
    let state = config::State::load(&state_path)?;
    config.keep_picks(Some(state.tv_mode), Some(state.other_media_action));
    let (config_tx, config_rx) = watch::channel(config.clone());
    let (config_error_tx, config_error_rx) = watch::channel(config_error);
    tokio::spawn(config::watch(
        config_path,
        config_tx.clone(),
        config_error_tx,
    ));
    let (snapshot_tx, snapshot_rx) = watch::channel(Snapshot::default());
    let (discovered_tx, discovered_rx) = watch::channel(Vec::new());
    let (web_ui_tx, web_ui_rx) = watch::channel(Vec::new());
    discovery::spawn(discovered_tx, web_ui_tx);
    let (command_tx, command_rx) = tokio::sync::mpsc::channel(player::COMMAND_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // Also in --headless mode: MPRIS and the notifications are optional,
    // and a missing session bus only logs one warning.
    mpris::spawn(snapshot_rx.clone(), command_tx.clone(), shutdown_tx.clone());
    notify::spawn(snapshot_rx.clone(), discovered_rx.clone(), config_error_rx);
    inhibit::spawn(snapshot_rx.clone(), config_rx.clone());
    let fade = Fade::new();
    other_media::spawn(
        fade.clone(),
        snapshot_rx.clone(),
        command_tx.clone(),
        config_rx.clone(),
        shutdown_rx.clone(),
    );

    let tray = if options.headless {
        None
    } else {
        let desktop = Desktop::watch().await;
        let style = config.looks.style(*desktop.borrow());
        let handle = SpunTray::new(
            Snapshot::default(),
            config_tx,
            style.clone(),
            player::output_devices(),
            command_tx.clone(),
            shutdown_tx.clone(),
        )
        // A Flatpak may not own the org.kde.StatusNotifierItem-PID-ID name,
        // so there the tray registers with its unique bus name.
        .disable_dbus_name(std::env::var_os("FLATPAK_ID").is_some())
        .spawn()
        .await
        .context("starting tray; a session D-Bus and StatusNotifierWatcher are required")?;
        Some((handle, style, desktop))
    };
    let handle = tray.as_ref().map(|(handle, ..)| handle.clone());
    let view = show(
        snapshot_rx,
        discovered_rx.clone(),
        web_ui_rx,
        config_rx.clone(),
        tray,
    );
    let backend = player::run(
        player::Startup {
            edits: config_rx,
            state,
            state_path,
            fade,
        },
        snapshot_tx,
        discovered_rx,
        command_rx,
        shutdown_rx.clone(),
    );
    let shutdown = async {
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("listening for SIGINT"),
            result = terminate_signal() => result,
            _ = player::shutdown_requested(shutdown_rx) => Ok(()),
        }
    };
    tokio::pin!(backend);
    let (result, backend_finished) = tokio::select! {
        result = &mut backend => (result, true),
        result = view => (result, false),
        result = shutdown => (result, false),
    };
    shutdown_tx.send_replace(true);
    let backend_result = if backend_finished {
        Ok(())
    } else {
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut backend)
            .await
            .context("backend shutdown timed out")
            .and_then(|result| result)
    };
    if let Some(handle) = handle {
        handle.shutdown().await;
    }
    result.and(backend_result)
}

async fn terminate_signal() -> Result<()> {
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("listening for SIGTERM")?;
    signal.recv().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer-to-peer D-Bus connection: the service that `serve` sets up,
    /// and its client. It needs no bus daemon, so it runs everywhere. The
    /// service stays alive until the test run ends.
    pub(crate) async fn dbus_pair(
        serve: impl FnOnce(
            zbus::connection::Builder<'static>,
        ) -> zbus::Result<zbus::connection::Builder<'static>>,
    ) -> (zbus::Connection, zbus::Connection) {
        let (service, client) = tokio::net::UnixStream::pair().unwrap();
        let service = zbus::connection::Builder::unix_stream(service)
            .server(zbus::Guid::generate())
            .unwrap()
            .p2p();
        let (service, client) = tokio::try_join!(
            serve(service).unwrap().build(),
            zbus::connection::Builder::unix_stream(client).p2p().build(),
        )
        .unwrap();
        std::mem::forget(service.clone());
        (service, client)
    }

    #[test]
    fn parses_headless() {
        assert_eq!(
            Options::parse(["--headless".into()]).unwrap(),
            Options {
                headless: true,
                help: false,
            }
        );
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(Options::parse(["--unknown".into()]).is_err());
    }
}
