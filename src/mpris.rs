use std::hash::{DefaultHasher, Hasher};
use std::sync::Mutex;

use mpris_server::{
    LoopStatus, Metadata, PlaybackStatus, PlayerInterface, Property, RootInterface, Server, Signal,
    Time, TrackId, zbus, zbus::fdo,
};
use sendspin::protocol::messages::RepeatMode;
use tokio::sync::watch;

use crate::player::{self, Art, Command, CommandTx, Progress, Snapshot};

/// A server position this far from the one that spun expects counts as a
/// jump, for example a seek. MPRIS listeners then hear about it.
const JUMP_TOLERANCE_MS: i64 = 1000;

pub struct Mpris {
    snapshot: watch::Receiver<Snapshot>,
    command_tx: CommandTx,
    shutdown_tx: watch::Sender<bool>,
    /// `file://` URL of the current album art file.
    art_url: Mutex<Option<String>>,
}

impl Mpris {
    fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }

    fn allowed(&self, name: &str) -> bool {
        self.snapshot.borrow().allows(name)
    }

    fn not_supported(what: &str) -> zbus::fdo::Error {
        zbus::fdo::Error::NotSupported(format!("spun cannot {what}"))
    }
}

impl RootInterface for Mpris {
    async fn raise(&self) -> fdo::Result<()> {
        // spun has no window to raise.
        Ok(())
    }

    async fn quit(&self) -> fdo::Result<()> {
        self.shutdown_tx.send_replace(true);
        Ok(())
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> zbus::Result<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok(env!("CARGO_PKG_NAME").into())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        // The desktop takes the name and icon of the media controls from
        // this .desktop file.
        Ok(crate::APP_ID.into())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl PlayerInterface for Mpris {
    async fn next(&self) -> fdo::Result<()> {
        if !self.allowed("next") {
            return Err(Self::not_supported("go to the next track"));
        }
        player::enqueue(&self.command_tx, Command::Next);
        Ok(())
    }

    async fn previous(&self) -> fdo::Result<()> {
        if !self.allowed("previous") {
            return Err(Self::not_supported("go to the previous track"));
        }
        player::enqueue(&self.command_tx, Command::Previous);
        Ok(())
    }

    async fn pause(&self) -> fdo::Result<()> {
        let snapshot = self.snapshot();
        if !snapshot.allows("pause") {
            return Err(Self::not_supported("pause"));
        }
        if snapshot.playing {
            player::enqueue(&self.command_tx, Command::TogglePlayback);
        }
        Ok(())
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        let snapshot = self.snapshot();
        let name = if snapshot.playing { "pause" } else { "play" };
        if !snapshot.allows(name) {
            return Err(Self::not_supported("toggle playback"));
        }
        player::enqueue(&self.command_tx, Command::TogglePlayback);
        Ok(())
    }

    async fn stop(&self) -> fdo::Result<()> {
        if !self.allowed("stop") {
            return Err(Self::not_supported("stop playback"));
        }
        player::enqueue(&self.command_tx, Command::Stop);
        Ok(())
    }

    async fn play(&self) -> fdo::Result<()> {
        let snapshot = self.snapshot();
        if !snapshot.allows("play") {
            return Err(Self::not_supported("play"));
        }
        if !snapshot.playing {
            player::enqueue(&self.command_tx, Command::TogglePlayback);
        }
        Ok(())
    }

    async fn seek(&self, _offset: Time) -> fdo::Result<()> {
        Err(Self::not_supported("seek"))
    }

    async fn set_position(&self, _track_id: TrackId, _position: Time) -> fdo::Result<()> {
        Err(Self::not_supported("seek"))
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(Self::not_supported("open a URI"))
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(playback_status(&self.snapshot()))
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(loop_status(&self.snapshot()))
    }

    async fn set_loop_status(&self, loop_status: LoopStatus) -> zbus::Result<()> {
        let command = Command::SetRepeat(repeat_mode(loop_status));
        if !self.snapshot.borrow().supports(&command) {
            return Err(Self::not_supported("set the loop status").into());
        }
        player::enqueue(&self.command_tx, command);
        Ok(())
    }

    async fn rate(&self) -> fdo::Result<f64> {
        Ok(1.0)
    }

    async fn set_rate(&self, _rate: f64) -> zbus::Result<()> {
        Err(Self::not_supported("set the playback rate").into())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(shuffle(&self.snapshot()))
    }

    async fn set_shuffle(&self, shuffle: bool) -> zbus::Result<()> {
        let command = Command::SetShuffle(shuffle);
        if !self.snapshot.borrow().supports(&command) {
            return Err(Self::not_supported("set shuffle").into());
        }
        player::enqueue(&self.command_tx, command);
        Ok(())
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(metadata(
            &self.snapshot(),
            self.art_url.lock().unwrap().as_deref(),
        ))
    }

    async fn volume(&self) -> fdo::Result<f64> {
        Ok(self.snapshot.borrow().volume as f64 / 100.0)
    }

    async fn set_volume(&self, _volume: f64) -> zbus::Result<()> {
        Err(Self::not_supported("set the volume").into())
    }

    async fn position(&self) -> fdo::Result<Time> {
        Ok(position(&self.snapshot()))
    }

    async fn minimum_rate(&self) -> fdo::Result<f64> {
        Ok(1.0)
    }

    async fn maximum_rate(&self) -> fdo::Result<f64> {
        Ok(1.0)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(self.allowed("next"))
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(self.allowed("previous"))
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(self.allowed("play"))
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(self.allowed("pause"))
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        // The spec says this must not change.
        Ok(true)
    }
}

/// The Sendspin playback state has only `Playing` and `Stopped`; a pause shows
/// up as `Stopped`. `Paused` keeps the track in the desktop media controls.
pub fn playback_status(snapshot: &Snapshot) -> PlaybackStatus {
    match (snapshot.link, &snapshot.title, snapshot.playing) {
        (_, _, true) => PlaybackStatus::Playing,
        (_, Some(_), false) => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    }
}

/// The MPRIS loop status for the server's repeat mode. Without a mode from
/// the server, there is nothing to repeat.
pub fn loop_status(snapshot: &Snapshot) -> LoopStatus {
    match snapshot.repeat {
        Some(RepeatMode::One) => LoopStatus::Track,
        Some(RepeatMode::All) => LoopStatus::Playlist,
        Some(RepeatMode::Off) | None => LoopStatus::None,
    }
}

/// The repeat mode for an MPRIS loop status. MPRIS has one loop-the-track
/// state and one loop-the-list state, which map onto `One` and `All`.
pub fn repeat_mode(loop_status: LoopStatus) -> RepeatMode {
    match loop_status {
        LoopStatus::Track => RepeatMode::One,
        LoopStatus::Playlist => RepeatMode::All,
        LoopStatus::None => RepeatMode::Off,
    }
}

/// Whether shuffle is on. Without a shuffle state from the server, it is off.
pub fn shuffle(snapshot: &Snapshot) -> bool {
    snapshot.shuffle.unwrap_or(false)
}

/// Builds the MPRIS metadata from the snapshot. Without a title there is no
/// track, so the ID is `NO_TRACK`.
pub fn metadata(snapshot: &Snapshot, art_url: Option<&str>) -> Metadata {
    let Some(title) = &snapshot.title else {
        return Metadata::builder().trackid(TrackId::NO_TRACK).build();
    };
    let mut hasher = DefaultHasher::new();
    hasher.write(title.as_bytes());
    if let Some(artist) = &snapshot.artist {
        hasher.write(artist.as_bytes());
    }
    if let Some(album) = &snapshot.album {
        hasher.write(album.as_bytes());
    }
    let track_id = TrackId::try_from(format!("/org/spun/track/{:x}", hasher.finish())).unwrap();
    let mut builder = Metadata::builder().trackid(track_id).title(title.clone());
    if let Some(artist) = &snapshot.artist {
        builder = builder.artist([artist.clone()]);
    }
    builder = builder.album(snapshot.album.clone().unwrap_or_default());
    if let Some(progress) = snapshot.progress.filter(|p| p.duration_ms > 0) {
        builder = builder.length(Time::from_millis(progress.duration_ms));
    }
    if let Some(art_url) = art_url {
        builder = builder.art_url(art_url);
    }
    builder.build()
}

/// The track position now. MPRIS listeners ask for it; spun sends no change
/// signal for it, as the MPRIS spec says.
fn position(snapshot: &Snapshot) -> Time {
    snapshot.progress.map_or(Time::ZERO, |progress| {
        Time::from_millis(progress.position_ms(std::time::Instant::now()))
    })
}

/// True when `new` puts the track where `old` does not predict, for
/// example after a seek or a track change. A first position counts too.
fn jumped(old: Option<Progress>, new: Option<Progress>) -> bool {
    match (old, new) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(old), Some(new)) => {
            (old.position_ms(new.at) - new.position_ms(new.at)).abs() > JUMP_TOLERANCE_MS
        }
    }
}

/// Writes `art` to a new PNG file in `$XDG_RUNTIME_DIR` (or the temp dir) and
/// returns its `file://` URL. In a Flatpak, the rest of the runtime dir is
/// private to the sandbox, so the file goes to
/// `$XDG_RUNTIME_DIR/app/$FLATPAK_ID`, which the host sees at the same path.
/// Each image gets a new random name, so desktops that cache art by URL still
/// update. The caller keeps the file alive; a drop deletes it.
fn write_art(art: &Art) -> anyhow::Result<(tempfile::NamedTempFile, String)> {
    let mut dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    if let Some(app) = std::env::var_os("FLATPAK_ID") {
        dir = dir.join("app").join(app);
    }
    let file = tempfile::Builder::new()
        .prefix("spun-art-")
        .suffix(".png")
        .tempfile_in(dir)?;
    let path = file.path().to_path_buf();
    std::fs::write(&path, art.0.as_ref())?;
    // ponytail: the path goes into the URL as it is. The runtime dir and the
    // file name hold no character that a URL must escape; percent-encode the
    // path if a desktop puts one there.
    Ok((file, format!("file://{}", path.display())))
}

/// Starts the MPRIS service on a background task, like `discovery::spawn`.
/// MPRIS is optional: without a session bus, spun logs one warning and
/// playback continues.
pub fn spawn(
    mut snapshot: watch::Receiver<Snapshot>,
    command_tx: CommandTx,
    shutdown_tx: watch::Sender<bool>,
) {
    tokio::spawn(async move {
        let server = match Server::new(
            env!("CARGO_PKG_NAME"),
            Mpris {
                snapshot: snapshot.clone(),
                command_tx,
                shutdown_tx,
                art_url: Mutex::new(None),
            },
        )
        .await
        {
            Ok(server) => server,
            Err(error) => {
                log::warn!("MPRIS unavailable: {error}");
                return;
            }
        };
        let mut last_art: Option<Art> = None;
        // The current art file; dropping it deletes it.
        let mut art_file = None;
        // The last snapshot that the loop handled. The art URL belongs to it.
        let mut s = Snapshot::default();
        while snapshot.changed().await.is_ok() {
            let old_progress = s.progress;
            s = snapshot.borrow_and_update().clone();
            let art = s.art.clone();
            if let Some(art) = art {
                let changed = last_art.as_ref().is_none_or(|old| !old.ptr_eq(&art));
                if changed {
                    match write_art(&art) {
                        Ok((file, url)) => {
                            // Keeps the art file alive; a drop deletes it.
                            drop(art_file.replace(file));
                            *server.imp().art_url.lock().unwrap() = Some(url);
                            last_art = Some(art);
                        }
                        Err(error) => log::warn!("writing album art failed: {error:#}"),
                    }
                }
            } else {
                last_art = None;
                art_file = None;
                *server.imp().art_url.lock().unwrap() = None;
            }
            let art_url = server.imp().art_url.lock().unwrap().clone();
            // ponytail: sends all nine on each change; compare with the
            // last values if the traffic matters.
            if let Err(error) = server
                .properties_changed([
                    Property::PlaybackStatus(playback_status(&s)),
                    Property::Metadata(metadata(&s, art_url.as_deref())),
                    Property::Volume(s.volume as f64 / 100.0),
                    Property::CanPlay(s.allows("play")),
                    Property::CanPause(s.allows("pause")),
                    Property::CanGoNext(s.allows("next")),
                    Property::CanGoPrevious(s.allows("previous")),
                    Property::LoopStatus(loop_status(&s)),
                    Property::Shuffle(shuffle(&s)),
                ])
                .await
            {
                log::warn!("notifying MPRIS listeners failed: {error}");
            }
            // Listeners move the position on by themselves, so they need a
            // signal only when it jumps.
            if jumped(old_progress, s.progress) {
                let seeked = server.emit(Signal::Seeked {
                    position: position(&s),
                });
                if let Err(error) = seeked.await {
                    log::warn!("notifying MPRIS listeners of a seek failed: {error}");
                }
            }
        }
        log::debug!("MPRIS task finished");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(link: crate::player::Link, title: Option<&str>, playing: bool) -> Snapshot {
        Snapshot {
            link,
            title: title.map(String::from),
            playing,
            ..Snapshot::default()
        }
    }

    #[test]
    fn playback_status_maps_the_three_cases() {
        use crate::player::Link;
        assert_eq!(
            playback_status(&snapshot(Link::Connected, Some("Song"), true)),
            PlaybackStatus::Playing
        );
        assert_eq!(
            playback_status(&snapshot(Link::Connected, Some("Song"), false)),
            PlaybackStatus::Paused
        );
        assert_eq!(
            playback_status(&snapshot(Link::Connected, None, false)),
            PlaybackStatus::Stopped
        );
        assert_eq!(
            playback_status(&snapshot(Link::Disconnected, Some("Song"), false)),
            PlaybackStatus::Paused
        );
    }

    #[test]
    fn metadata_without_a_title_has_no_track_and_no_art() {
        let m = metadata(&snapshot(crate::player::Link::Connected, None, true), None);
        assert_eq!(m.trackid(), Some(TrackId::NO_TRACK));
        assert!(m.title().is_none());
        assert!(m.art_url().is_none());
    }

    #[test]
    fn metadata_with_a_title_and_art_url_has_both() {
        let s = Snapshot {
            title: Some("Song".into()),
            artist: Some("Band".into()),
            album: Some("Album".into()),
            ..Snapshot::default()
        };
        let m = metadata(&s, Some("file:///tmp/spun-art-abc.png"));
        assert_ne!(m.trackid(), Some(TrackId::NO_TRACK));
        assert!(
            m.trackid()
                .is_some_and(|id| id.as_str().starts_with("/org/spun/track/"))
        );
        assert_eq!(m.title(), Some("Song"));
        assert_eq!(m.artist().as_deref(), Some(&["Band".to_string()][..]));
        assert_eq!(m.album(), Some("Album"));
        assert_eq!(m.art_url().as_deref(), Some("file:///tmp/spun-art-abc.png"));
    }

    #[test]
    fn the_length_comes_from_the_progress_and_jumps_are_seeks() {
        let at = std::time::Instant::now();
        let progress = Progress {
            position_ms: 10_000,
            duration_ms: 200_000,
            speed: 1000,
            at,
        };
        let s = Snapshot {
            title: Some("Song".into()),
            progress: Some(progress),
            ..Snapshot::default()
        };
        assert_eq!(
            metadata(&s, None).length(),
            Some(Time::from_millis(200_000))
        );
        let live = Snapshot {
            progress: Some(Progress {
                duration_ms: 0,
                ..progress
            }),
            ..s.clone()
        };
        assert_eq!(metadata(&live, None).length(), None);

        // Five seconds later, the server says 15 s: playback went on as expected.
        let later = at + std::time::Duration::from_secs(5);
        let on_time = Progress {
            position_ms: 15_000,
            at: later,
            ..progress
        };
        assert!(!jumped(Some(progress), Some(on_time)));
        // A pause at the same moment is no jump either.
        let paused = Progress {
            speed: 0,
            ..on_time
        };
        assert!(!jumped(Some(progress), Some(paused)));
        let seeked = Progress {
            position_ms: 90_000,
            ..on_time
        };
        assert!(jumped(Some(progress), Some(seeked)));
        assert!(jumped(None, Some(progress)));
        assert!(!jumped(Some(progress), None));
    }

    #[test]
    fn track_ids_differ_between_tracks() {
        let a = metadata(
            &Snapshot {
                title: Some("Song A".into()),
                ..Snapshot::default()
            },
            None,
        );
        let b = metadata(
            &Snapshot {
                title: Some("Song B".into()),
                ..Snapshot::default()
            },
            None,
        );
        assert_ne!(a.trackid(), b.trackid());
    }

    #[test]
    fn loop_status_and_shuffle_follow_the_server_state() {
        let s = |repeat, shuffle| Snapshot {
            repeat,
            shuffle,
            ..Snapshot::default()
        };
        assert_eq!(loop_status(&s(None, None)), LoopStatus::None);
        assert!(!shuffle(&s(None, None)));
        assert_eq!(
            loop_status(&s(Some(RepeatMode::Off), Some(false))),
            LoopStatus::None
        );
        assert_eq!(
            loop_status(&s(Some(RepeatMode::One), Some(true))),
            LoopStatus::Track
        );
        assert_eq!(
            loop_status(&s(Some(RepeatMode::All), Some(true))),
            LoopStatus::Playlist
        );
        assert!(shuffle(&s(Some(RepeatMode::All), Some(true))));
        // The reverse mapping covers every MPRIS loop status.
        assert_eq!(repeat_mode(LoopStatus::None), RepeatMode::Off);
        assert_eq!(repeat_mode(LoopStatus::Track), RepeatMode::One);
        assert_eq!(repeat_mode(LoopStatus::Playlist), RepeatMode::All);
    }

    #[tokio::test]
    #[ignore = "requires an isolated session bus; run under dbus-run-session"]
    async fn dbus_mpris_answers_play_pause() {
        let (snapshot_tx, snapshot_rx) = watch::channel(Snapshot {
            link: crate::player::Link::Connected,
            controllable: true,
            supported_commands: vec!["play".into(), "pause".into()],
            ..Snapshot::default()
        });
        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let server = Server::new(
            env!("CARGO_PKG_NAME"),
            Mpris {
                snapshot: snapshot_rx,
                command_tx,
                shutdown_tx,
                art_url: Mutex::new(None),
            },
        )
        .await
        .unwrap();
        let connection = zbus::Connection::session().await.unwrap();
        connection
            .call_method(
                Some("org.mpris.MediaPlayer2.spun"),
                "/org/mpris/MediaPlayer2",
                Some("org.mpris.MediaPlayer2.Player"),
                "PlayPause",
                &(),
            )
            .await
            .unwrap();
        let command = tokio::time::timeout(std::time::Duration::from_secs(2), command_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(command, Command::TogglePlayback);
        drop(server);
        drop(snapshot_tx);
    }
}
