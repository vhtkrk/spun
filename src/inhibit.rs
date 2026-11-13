//! Keeps the display awake while something plays, so that a display which
//! turns off its audio when it sleeps does not cut the sound.

use std::time::Duration;

use tokio::sync::watch;

use crate::config::Config;
use crate::player::{Link, Snapshot, SnapshotRx};

/// How long spun waits for the desktop to answer. A hung desktop must not
/// hold up the player.
const TIMEOUT: Duration = Duration::from_secs(2);

/// What spun tells the desktop. It shows in the list of what keeps the
/// session awake.
const REASON: &str = "Playing audio";

/// GNOME's flag for inhibiting idleness. Its neighbours are 1 for logout,
/// 2 for a user switch, and 4 for suspend.
const IDLE: u32 = 8;

/// A desktop service that inhibits idleness. The idle inhibition service is
/// the standard one; the other two cover desktops that do not offer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Service {
    /// `org.freedesktop.ScreenSaver` on the standard object path.
    Idle,
    /// The same interface on the `/ScreenSaver` path, which KDE's releases
    /// before kde-workspace 4.11.8 use. Some desktops still serve only this
    /// path.
    LegacyIdle,
    /// GNOME's session manager, which has a method and flags of its own.
    Session,
}

impl Service {
    /// Each service that spun asks, in order.
    const ALL: [Self; 3] = [Self::Idle, Self::LegacyIdle, Self::Session];

    /// The bus name. Each service names its interface after it.
    fn name(self) -> &'static str {
        match self {
            Self::Session => "org.gnome.SessionManager",
            Self::Idle | Self::LegacyIdle => "org.freedesktop.ScreenSaver",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Idle => "/org/freedesktop/ScreenSaver",
            Self::LegacyIdle => "/ScreenSaver",
            Self::Session => "/org/gnome/SessionManager",
        }
    }

    /// The method that drops `cookie`.
    fn uninhibit(self) -> &'static str {
        match self {
            Self::Session => "Uninhibit",
            Self::Idle | Self::LegacyIdle => "UnInhibit",
        }
    }

    /// Calls `method` of the service with `body`.
    async fn call<B>(
        self,
        connection: &zbus::Connection,
        method: &str,
        body: &B,
    ) -> zbus::Result<zbus::Message>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        connection
            .call_method(
                Some(self.name()),
                self.path(),
                Some(self.name()),
                method,
                body,
            )
            .await
    }

    /// Asks the service to inhibit idleness, and returns its cookie.
    async fn inhibit(self, connection: &zbus::Connection) -> zbus::Result<u32> {
        let app = env!("CARGO_PKG_NAME");
        let reply = match self {
            // GNOME takes an application ID, a window, the reason, and the
            // flags. spun has no window, and the session manager accepts 0.
            Self::Session => {
                let body = (app, 0_u32, REASON, IDLE);
                self.call(connection, "Inhibit", &body).await?
            }
            Self::Idle | Self::LegacyIdle => {
                self.call(connection, "Inhibit", &(app, REASON)).await?
            }
        };
        reply.body().deserialize()
    }
}

/// An inhibition that spun holds until it drops it.
#[derive(Debug)]
struct Inhibitor {
    service: Service,
    cookie: u32,
}

impl Inhibitor {
    /// Takes an inhibition from the first service that answers. A desktop
    /// without idle inhibition is no error, so each failure only logs.
    async fn take(connection: &zbus::Connection) -> Option<Self> {
        for service in Service::ALL {
            match tokio::time::timeout(TIMEOUT, service.inhibit(connection)).await {
                Ok(Ok(cookie)) => {
                    log::info!("keeping the display awake while spun plays");
                    return Some(Self { service, cookie });
                }
                Ok(Err(error)) => log::debug!("{}: {error}", service.name()),
                Err(_) => log::debug!("{} did not answer in time", service.name()),
            }
        }
        log::warn!(
            "no desktop service inhibits idleness, so the display may sleep while spun plays"
        );
        None
    }

    /// Drops the inhibition, so the display may sleep again.
    async fn release(self, connection: &zbus::Connection) {
        let cookie = (self.cookie,);
        let call = self
            .service
            .call(connection, self.service.uninhibit(), &cookie);
        match tokio::time::timeout(TIMEOUT, call).await {
            Ok(Ok(_)) => log::info!("letting the display sleep again"),
            Ok(Err(error)) => log::debug!("dropping the inhibition failed: {error}"),
            Err(_) => log::debug!("dropping the inhibition timed out"),
        }
    }
}

/// True while the display should stay awake: the setting is on and something
/// plays.
fn wanted(config: &Config, snapshot: &Snapshot) -> bool {
    config.tv_mode.unwrap_or_default() && snapshot.link == Link::Connected && snapshot.playing
}

/// Inhibits idleness while [`Config::tv_mode`] is on and something plays.
/// The inhibition follows the player and each config edit. Idle inhibition is
/// optional: without a session bus, spun logs one warning and plays as usual.
pub fn spawn(mut snapshots: SnapshotRx, mut config: watch::Receiver<Config>) {
    tokio::spawn(async move {
        let connection = match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(error) => {
                log::warn!("desktop idle inhibition unavailable: {error}");
                return;
            }
        };
        let mut held: Option<Inhibitor> = None;
        loop {
            let awake = wanted(&config.borrow_and_update(), &snapshots.borrow_and_update());
            if !awake {
                if let Some(inhibitor) = held.take() {
                    inhibitor.release(&connection).await;
                }
            } else if held.is_none() {
                held = Inhibitor::take(&connection).await;
            }
            // A closed channel disables its branch.
            tokio::select! {
                Ok(()) = snapshots.changed() => {}
                Ok(()) = config.changed() => {}
                else => break,
            }
        }
        if let Some(inhibitor) = held {
            inhibitor.release(&connection).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    /// A screensaver service that hands out `COOKIE` and remembers the
    /// cookie that it is asked to drop.
    const COOKIE: u32 = 7;

    struct ScreenSaver(Arc<Mutex<Option<u32>>>);

    #[zbus::interface(name = "org.freedesktop.ScreenSaver")]
    impl ScreenSaver {
        fn inhibit(&self, _application: &str, _reason: &str) -> u32 {
            COOKIE
        }

        fn un_inhibit(&self, cookie: u32) {
            *self.0.lock().unwrap() = Some(cookie);
        }
    }

    /// A connection to a fake idle inhibition service at `path`, and the
    /// cookie that it is asked to drop.
    async fn screen_saver(path: &'static str) -> (zbus::Connection, Arc<Mutex<Option<u32>>>) {
        let dropped = Arc::new(Mutex::new(None));
        let service = ScreenSaver(Arc::clone(&dropped));
        let (_, spun) = crate::tests::dbus_pair(|bus| bus.serve_at(path, service)).await;
        (spun, dropped)
    }

    #[tokio::test]
    async fn an_inhibition_is_taken_and_dropped() {
        let (spun, dropped) = screen_saver(Service::Idle.path()).await;
        let inhibitor = Inhibitor::take(&spun)
            .await
            .expect("the standard service answers");
        assert_eq!(inhibitor.service, Service::Idle);
        assert_eq!(inhibitor.cookie, COOKIE);
        inhibitor.release(&spun).await;
        assert_eq!(*dropped.lock().unwrap(), Some(COOKIE));
    }

    #[tokio::test]
    async fn kde_older_path_is_asked_second() {
        let (spun, dropped) = screen_saver(Service::LegacyIdle.path()).await;
        let inhibitor = Inhibitor::take(&spun)
            .await
            .expect("the legacy path answers");
        assert_eq!(inhibitor.service, Service::LegacyIdle);
        inhibitor.release(&spun).await;
        assert_eq!(*dropped.lock().unwrap(), Some(COOKIE));
    }

    #[tokio::test]
    async fn a_desktop_without_the_service_holds_nothing() {
        let (_, spun) = crate::tests::dbus_pair(Ok).await;
        assert!(Inhibitor::take(&spun).await.is_none());
    }

    #[test]
    fn the_display_stays_awake_only_while_the_setting_is_on_and_something_plays() {
        let playing = Snapshot {
            link: Link::Connected,
            playing: true,
            ..Snapshot::default()
        };
        let on = Config {
            tv_mode: Some(true),
            ..Config::default()
        };
        assert!(wanted(&on, &playing));
        assert!(!wanted(&Config::default(), &playing));
        for idle in [
            Snapshot {
                playing: false,
                ..playing.clone()
            },
            Snapshot {
                link: Link::Disconnected,
                ..playing.clone()
            },
            Snapshot {
                link: Link::Connecting,
                ..playing
            },
        ] {
            assert!(!wanted(&on, &idle), "{idle:?}");
        }
    }
}
