//! Desktop notifications for the problems that need the user: a server that
//! spun cannot reach, a stream that spun cannot play, a choice between
//! servers, and a config edit with an error.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use zbus::zvariant::Value;

use crate::player::{DiscoveredRx, Link, Snapshot, SnapshotRx};

/// How long spun waits for the notification daemon to answer.
const TIMEOUT: Duration = Duration::from_secs(3);

/// How long a connection error must last before its notification shows. A
/// short disconnect, for example after the computer wakes, shows none.
const CONNECTION_GRACE: Duration = Duration::from_secs(5);

/// A kind of problem. A new notification replaces the last one of its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Player,
    Servers,
    Config,
}

#[derive(Debug)]
struct Notice {
    kind: Kind,
    summary: String,
    body: String,
}

/// What the notifications show now, so that each problem shows once.
#[derive(Debug, Default)]
struct Shown {
    /// The player error. A retry keeps it, so the same error does not show
    /// again. A working connection clears it.
    error: Option<String>,
    /// The connection error that waits for `CONNECTION_GRACE` to pass, and
    /// the time when it shows.
    waiting: Option<(Instant, Notice)>,
    /// True after the notification about several servers, until spun has a
    /// server.
    servers: bool,
    /// The config error.
    config: Option<String>,
}

impl Shown {
    /// The notifications that the new state calls for at `now`. `servers` is
    /// the number of discovered servers. A connection error shows only in
    /// the update at [`Shown::due`], and only when it still lasts then.
    fn update(
        &mut self,
        snapshot: &Snapshot,
        servers: usize,
        config_error: Option<&String>,
        now: Instant,
    ) -> Vec<Notice> {
        let mut notices = Vec::new();
        match &snapshot.error {
            Some(error) if self.error.as_ref() != Some(error) => {
                self.error = Some(error.clone());
                let notice = |summary: &str| Notice {
                    kind: Kind::Player,
                    summary: summary.into(),
                    body: error.clone(),
                };
                // A new connection error keeps the time of the one that
                // waits, so an error that changes with each retry shows too.
                let due = self.waiting.take().map(|(due, _)| due);
                if snapshot.link == Link::Connected {
                    notices.push(notice("spun cannot play"));
                } else {
                    let due = due.unwrap_or(now + CONNECTION_GRACE);
                    self.waiting = Some((due, notice("spun cannot reach the server")));
                }
            }
            None if snapshot.link != Link::Connecting => {
                self.error = None;
                self.waiting = None;
            }
            _ => {}
        }
        if let Some((_, notice)) = self.waiting.take_if(|(due, _)| *due <= now) {
            notices.push(notice);
        }
        if snapshot.link != Link::NoServer {
            self.servers = false;
        } else if servers > 1 && !self.servers {
            self.servers = true;
            notices.push(Notice {
                kind: Kind::Servers,
                summary: format!("spun found {servers} servers"),
                body: "Pick one in the tray menu, or set server_url in config.toml.".into(),
            });
        }
        if config_error != self.config.as_ref() {
            self.config = config_error.cloned();
            if let Some(error) = config_error {
                notices.push(Notice {
                    kind: Kind::Config,
                    summary: "config.toml has an error".into(),
                    body: format!("{error}\nspun keeps the current settings."),
                });
            }
        }
        notices
    }

    /// When the connection error that waits shows; the next update is due
    /// then.
    fn due(&self) -> Option<Instant> {
        self.waiting.as_ref().map(|(due, _)| *due)
    }

    /// True while the problem of `kind` lasts. A retry does not end the
    /// player error.
    fn active(&self, kind: Kind) -> bool {
        match kind {
            Kind::Player => self.error.is_some(),
            Kind::Servers => self.servers,
            Kind::Config => self.config.is_some(),
        }
    }
}

/// Shows a desktop notification and returns its ID. It replaces the
/// notification with `replaces_id`; 0 replaces none. Daemons can parse markup
/// in the body, so the body escapes `&`, `<`, and `>`.
async fn notify(
    connection: &zbus::Connection,
    replaces_id: u32,
    summary: &str,
    body: &str,
) -> anyhow::Result<u32> {
    let body = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    // The arguments: app_name, replaces_id, app_icon, summary, body,
    // actions, hints, and expire_timeout. -1 lets the daemon pick the time.
    // The icon and the desktop-entry hint let the desktop show the name and
    // icon of spun with the notification.
    let hints = HashMap::from([("desktop-entry", Value::from(crate::APP_ID))]);
    // The actions are pairs of a key and a label. Without a `default`
    // action, a click on the notification makes GNOME start the app of the
    // desktop-entry hint, which is a second spun. With it, the click only
    // closes the notification; spun does not listen for the action.
    let actions = ["default", "Dismiss"];
    let args = (
        env!("CARGO_PKG_NAME"),
        replaces_id,
        crate::APP_ID,
        summary,
        body,
        actions.as_slice(),
        hints,
        -1_i32,
    );
    let call = connection.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &args,
    );
    // zbus has no default timeout, and a hung daemon must not stop the
    // notifications that come after.
    let reply = tokio::time::timeout(TIMEOUT, call).await??;
    Ok(reply.body().deserialize()?)
}

/// Closes the notification `id`. The daemon answers with an error when the
/// user closed the notification already, so an error only logs at debug.
async fn close(connection: &zbus::Connection, id: u32) {
    let call = connection.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "CloseNotification",
        &id,
    );
    match tokio::time::timeout(TIMEOUT, call).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => log::debug!("closing notification {id} failed: {error}"),
        Err(_) => log::debug!("closing notification {id} timed out"),
    }
}

/// Waits until `due`, or gives `None` at once without one.
async fn wait_until(due: Option<Instant>) -> Option<()> {
    tokio::time::sleep_until(due?).await;
    Some(())
}

/// Shows a notification for each new problem, and closes it when the problem
/// ends, on a background task.
/// Notifications are optional: without a session bus, spun logs one warning
/// and shows none.
pub fn spawn(
    mut snapshots: SnapshotRx,
    mut servers: DiscoveredRx,
    mut config_errors: watch::Receiver<Option<String>>,
) {
    tokio::spawn(async move {
        let connection = match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(error) => {
                log::warn!("desktop notifications unavailable: {error}");
                return;
            }
        };
        let mut shown = Shown::default();
        // The last notification of each kind, which the next one replaces.
        let mut ids = HashMap::new();
        loop {
            let notices = shown.update(
                &snapshots.borrow_and_update(),
                servers.borrow_and_update().len(),
                config_errors.borrow_and_update().as_ref(),
                Instant::now(),
            );
            for notice in notices {
                let replaces = ids.get(&notice.kind).copied().unwrap_or(0);
                match notify(&connection, replaces, &notice.summary, &notice.body).await {
                    Ok(id) => {
                        ids.insert(notice.kind, id);
                    }
                    Err(error) => log::warn!("desktop notification failed: {error:#}"),
                }
            }
            // A notification about a problem that ended only invites a click.
            let ended: Vec<(Kind, u32)> = ids
                .iter()
                .filter(|(kind, _)| !shown.active(**kind))
                .map(|(kind, id)| (*kind, *id))
                .collect();
            for (kind, id) in ended {
                ids.remove(&kind);
                close(&connection, id).await;
            }
            // A closed channel disables its branch; discovery may be
            // unavailable.
            tokio::select! {
                Ok(()) = snapshots.changed() => {}
                Ok(()) = servers.changed() => {}
                Ok(()) = config_errors.changed() => {}
                Some(()) = wait_until(shown.due()) => {}
                else => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(notices: Vec<Notice>) -> Vec<Kind> {
        notices.into_iter().map(|notice| notice.kind).collect()
    }

    fn snapshot(link: Link, error: Option<&str>) -> Snapshot {
        Snapshot {
            link,
            error: error.map(Into::into),
            ..Snapshot::default()
        }
    }

    #[test]
    fn each_problem_shows_once() {
        let mut shown = Shown::default();
        let now = Instant::now();
        let refused = snapshot(Link::Disconnected, Some("connection refused"));
        let retry = snapshot(Link::Connecting, None);
        let connected = snapshot(Link::Connected, None);
        // A connection error that ends within the grace time never shows.
        assert!(shown.update(&refused, 0, None, now).is_empty());
        assert_eq!(shown.due(), Some(now + CONNECTION_GRACE));
        assert!(shown.update(&connected, 0, None, now).is_empty());
        assert_eq!(shown.due(), None);
        // One that lasts shows when the grace time ends, also in a retry.
        assert!(shown.update(&refused, 0, None, now).is_empty());
        let now = now + CONNECTION_GRACE;
        assert_eq!(kinds(shown.update(&retry, 0, None, now)), [Kind::Player]);
        assert_eq!(shown.due(), None);
        // A retry with the same error stays quiet.
        assert!(shown.update(&retry, 0, None, now).is_empty());
        assert!(shown.active(Kind::Player), "a retry keeps the notification");
        assert!(shown.update(&refused, 0, None, now).is_empty());
        // A working connection clears the error, so it can show again.
        assert!(shown.update(&connected, 0, None, now).is_empty());
        assert!(!shown.active(Kind::Player), "the notification closes");
        assert!(shown.update(&refused, 0, None, now).is_empty());
        // A stream that spun cannot play shows at once, and the connection
        // error that waited shows no more.
        let broken = snapshot(Link::Connected, Some("audio output failed"));
        let notices = shown.update(&broken, 0, None, now);
        assert_eq!(notices[0].summary, "spun cannot play");
        assert_eq!(shown.due(), None);

        // Several servers show once while spun waits for a pick.
        let waiting = snapshot(Link::NoServer, None);
        assert!(shown.update(&waiting, 1, None, now).is_empty());
        assert_eq!(kinds(shown.update(&waiting, 2, None, now)), [Kind::Servers]);
        assert!(shown.update(&waiting, 3, None, now).is_empty());

        // A config error shows once, and a valid edit clears it.
        let error = "parsing config.toml".to_string();
        assert_eq!(
            kinds(shown.update(&waiting, 3, Some(&error), now)),
            [Kind::Config]
        );
        assert!(shown.update(&waiting, 3, Some(&error), now).is_empty());
        assert!(shown.active(Kind::Servers) && shown.active(Kind::Config));
        assert!(shown.update(&waiting, 3, None, now).is_empty());
        assert!(!shown.active(Kind::Config));
        // A server ends the wait for a pick.
        assert!(shown.update(&connected, 3, None, now).is_empty());
        assert!(!shown.active(Kind::Servers));
        assert_eq!(kinds(shown.update(&waiting, 3, None, now)), [Kind::Servers]);
        assert_eq!(
            kinds(shown.update(&waiting, 3, Some(&error), now)),
            [Kind::Config]
        );
    }
}
