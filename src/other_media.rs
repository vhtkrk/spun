//! Fades spun to silence while another media player on the desktop plays,
//! so that the two do not talk over each other.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use sendspin::audio::ProcessCallback;
use tokio::sync::watch;

use crate::config::{Config, OtherMediaAction};
use crate::player::{self, Command, CommandTx, Link, SnapshotRx};

/// How long a fade between full volume and silence takes.
const FADE: Duration = Duration::from_millis(600);

/// The step between two gains of a fade. The fade is short enough to hear
/// as one move, and each step is too small to hear as a click.
const STEP: Duration = Duration::from_millis(25);

/// How long another player may take to answer. A hung player must not hold
/// up the fade.
const TIMEOUT: Duration = Duration::from_millis(200);

/// The bus name of an MPRIS player, minus the suffix.
const NAME_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// Where each MPRIS player keeps its interfaces.
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";

/// The interface of an MPRIS player.
const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

/// The gain that spun applies to its own output, on top of the volume that
/// the server sets. Fading it takes the speakers over without a click, and
/// it leaves the volume that spun reports to the server alone.
#[derive(Debug, Clone)]
pub struct Fade {
    /// The gain in use, as the bits of an `f32` from 0 to 1.
    level: Arc<AtomicU32>,
    /// The gain that the ramp moves to, in the same form.
    target: Arc<AtomicU32>,
    /// Wakes the ramp when the target moves, so that it does not tick
    /// while the gain is at rest.
    changed: Arc<tokio::sync::Notify>,
}

impl Fade {
    /// A fade that passes the audio through unchanged.
    pub fn new() -> Self {
        let full = 1.0_f32.to_bits();
        Self {
            level: Arc::new(AtomicU32::new(full)),
            target: Arc::new(AtomicU32::new(full)),
            changed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// The callback that applies the fade to the decoded audio. The callback
    /// runs on the audio thread, so it only reads the gain.
    pub fn callback(&self) -> ProcessCallback {
        let level = Arc::clone(&self.level);
        Box::new(move |data: &mut [f32]| {
            let gain = f32::from_bits(level.load(Ordering::Relaxed));
            if gain != 1.0 {
                for sample in data.iter_mut() {
                    *sample *= gain;
                }
            }
        })
    }

    /// The gain in use.
    fn gain(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// The gain that the ramp moves to.
    fn wanted(&self) -> f32 {
        f32::from_bits(self.target.load(Ordering::Relaxed))
    }

    /// Sends the ramp to `gain`, which is 0 or 1.
    fn set_target(&self, gain: f32) {
        self.target.store(gain.to_bits(), Ordering::Relaxed);
        self.changed.notify_one();
    }

    fn set_gain(&self, gain: f32) {
        self.level.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Moves the gain one step toward the target. Returns true while the
    /// gain is still on its way.
    fn step(&self) -> bool {
        let target = self.wanted();
        let gain = self.gain();
        if gain == target {
            return false;
        }
        let step = STEP.as_secs_f32() / FADE.as_secs_f32();
        let next = if gain < target {
            (gain + step).min(target)
        } else {
            (gain - step).max(target)
        };
        self.set_gain(next);
        next != target
    }
}

/// Moves the fade to its target, one step per [`STEP`], until shutdown.
async fn ramp(fade: Fade, mut shutdown: watch::Receiver<bool>) {
    loop {
        // At rest the gain stays put, so the ramp waits for a new target
        // rather than ticking.
        if fade.step() {
            tokio::select! {
                _ = tokio::time::sleep(STEP) => {}
                Ok(()) = shutdown.changed() => {}
            }
        } else {
            tokio::select! {
                _ = fade.changed.notified() => {}
                Ok(()) = shutdown.changed() => {}
            }
        }
        if *shutdown.borrow() {
            return;
        }
    }
}

/// What spun may do about the other players right now.
#[derive(Debug, Clone, Copy)]
struct Input {
    action: OtherMediaAction,
    /// How long another player must play before spun gets out of its way.
    delay: Duration,
    /// How long spun keeps playing while it is silent before it pauses.
    snooze: Option<Duration>,
    /// True while another player on the desktop plays.
    others: bool,
    /// True while spun plays.
    playing: bool,
    /// True when the server lets spun pause and play.
    can_pause: bool,
}

/// What the fade and the player must do to follow [`Input`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    FadeToSilence,
    FadeToVolume,
    Pause,
    Play,
}

/// The fade and the pause that spun applies while another player plays.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ducking {
    /// When another player has played for [`Input::delay`], or `None`
    /// while no other player plays. From then on, spun is out of its way.
    duck_at: Option<Instant>,
    /// The gain that spun asked for: 0 while it holds itself silent.
    target: f32,
    /// True when spun sent a pause and has not played again.
    paused: bool,
    /// When to pause with [`Effect::Pause`]. In pause mode, that is the end
    /// of the fade; in mute mode, the end of the snooze.
    pause_at: Option<Instant>,
}

impl Ducking {
    fn new() -> Self {
        Self {
            duck_at: None,
            target: 1.0,
            paused: false,
            pause_at: None,
        }
    }

    /// The effects that take the fade and the player from this state to
    /// what `input` asks for at `now`.
    fn settle(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        if !input.others {
            self.duck_at = None;
        }
        // A player that stops within the delay never takes the speakers.
        let heard = input.others && now >= *self.duck_at.get_or_insert(now + input.delay);
        if !heard || input.action == OtherMediaAction::Ignore {
            return self.resume(input.playing);
        }
        let mut effects = Vec::new();
        if self.target > 0.0 {
            self.target = 0.0;
            effects.push(Effect::FadeToSilence);
        }
        // Pause mode pauses as soon as the fade is done. Mute mode keeps
        // playing until the snooze runs out. Without a pause command from
        // the server, spun only holds the silence.
        let hold = match input.action {
            OtherMediaAction::Pause => Some(FADE),
            OtherMediaAction::Mute | OtherMediaAction::Ignore => input.snooze,
        }
        .filter(|_| input.can_pause);
        if !input.playing || self.paused {
            self.pause_at = None;
            return effects;
        }
        let Some(hold) = hold else {
            self.pause_at = None;
            return effects;
        };
        let at = *self.pause_at.get_or_insert(now + hold);
        if now >= at {
            self.pause_at = None;
            self.paused = true;
            effects.push(Effect::Pause);
        }
        effects
    }

    /// When `settle` must run again without a new input: at the end of the
    /// delay, and at the pause.
    fn wake_at(&self, now: Instant) -> Option<Instant> {
        let delay_end = self.duck_at.filter(|at| *at > now);
        delay_end.into_iter().chain(self.pause_at).min()
    }

    /// Fades back up, and plays again if spun paused itself.
    fn resume(&mut self, playing: bool) -> Vec<Effect> {
        self.pause_at = None;
        let mut effects = Vec::new();
        if self.target < 1.0 {
            self.target = 1.0;
            effects.push(Effect::FadeToVolume);
        }
        if self.paused {
            self.paused = false;
            // A user who pressed play again needs no command.
            if !playing {
                effects.push(Effect::Play);
            }
        }
        effects
    }
}

/// True while the MPRIS player at `destination` plays. A player that does
/// not answer, or does not say, counts as silent.
async fn playing(connection: &zbus::Connection, destination: &str) -> bool {
    // A plain `Get` rather than a proxy: spun makes no other call to the
    // player, and a proxy would cache every property of it.
    let call = connection.call_method(
        Some(destination),
        MPRIS_PATH,
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &(MPRIS_PLAYER_INTERFACE, "PlaybackStatus"),
    );
    let playing = async {
        let reply = tokio::time::timeout(TIMEOUT, call).await??;
        let status: zbus::zvariant::OwnedValue = reply.body().deserialize()?;
        anyhow::Ok(String::try_from(status)? == "Playing")
    };
    playing.await.unwrap_or_else(|error| {
        log::debug!("{destination}: {error}");
        false
    })
}

/// An MPRIS player on the bus other than spun's own.
struct OtherPlayer {
    /// The unique name of the connection that owns the bus name. A signal
    /// names its sender this way.
    owner: String,
    playing: bool,
}

/// The sender and the new state from a `PropertiesChanged` signal of an
/// MPRIS player, or `None` when the signal carries no playback status.
// ponytail: reads only a status that the signal carries, as MPRIS requires.
// A player that only invalidates the property goes unseen until its next
// signal; ask it with `playing` if a player does that.
fn status_change(message: &zbus::Message) -> Option<(String, bool)> {
    use zbus::zvariant::Value;
    let sender = message.header().sender()?.to_string();
    let body = message.body();
    let (_, changed, _): (&str, HashMap<&str, Value<'_>>, Vec<&str>) = body.deserialize().ok()?;
    let playing =
        matches!(changed.get("PlaybackStatus")?, Value::Str(status) if status == "Playing");
    Some((sender, playing))
}

/// Publishes whether an MPRIS player other than spun's own plays, until the
/// bus connection closes. spun follows the bus signals: a player sends
/// `PropertiesChanged` when its status changes, and the bus sends
/// `NameOwnerChanged` when a player comes or goes. Nothing is sent while
/// nothing changes.
async fn follow_players(
    connection: &zbus::Connection,
    playing_tx: &watch::Sender<bool>,
) -> zbus::Result<()> {
    use futures_util::StreamExt;
    use zbus::message::Type::Signal;

    // Both subscriptions start before the first read, so that no change
    // falls between the two.
    let rule = zbus::MatchRule::builder()
        .msg_type(Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path(MPRIS_PATH)?
        .arg(0, MPRIS_PLAYER_INTERFACE)?
        .build();
    let mut statuses = zbus::MessageStream::for_match_rule(rule, connection, None).await?;
    let rule = zbus::MatchRule::builder()
        .msg_type(Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg0ns(NAME_PREFIX.trim_end_matches('.'))?
        .build();
    let mut owners = zbus::MessageStream::for_match_rule(rule, connection, None).await?;

    // spun registers an MPRIS player too, and ducking for itself would
    // silence the music for good.
    let own = format!("{NAME_PREFIX}{}", env!("CARGO_PKG_NAME"));
    let other = |name: &str| name.starts_with(NAME_PREFIX) && name != own;
    let dbus = zbus::fdo::DBusProxy::new(connection).await?;
    let mut players = HashMap::new();
    for name in dbus.list_names().await? {
        if !other(name.as_str()) {
            continue;
        }
        // A player that left since the list has no owner.
        if let Ok(owner) = dbus.get_name_owner(name.as_ref()).await {
            let playing = playing(connection, name.as_str()).await;
            let owner = owner.to_string();
            players.insert(name.to_string(), OtherPlayer { owner, playing });
        }
    }
    loop {
        let plays = players.values().any(|player| player.playing);
        playing_tx.send_if_modified(|others| {
            let changed = *others != plays;
            *others = plays;
            changed
        });
        tokio::select! {
            Some(message) = statuses.next() => {
                // The two streams keep no order between them, so a signal
                // can come from a player that already left. Only a known
                // owner counts; a new player is read when it takes its name.
                if let Some((owner, playing)) = status_change(&message?) {
                    for player in players.values_mut().filter(|player| player.owner == owner) {
                        player.playing = playing;
                    }
                }
            }
            Some(message) = owners.next() => {
                // An empty owner means that the name is free again.
                let (name, _, owner): (String, String, String) = message?.body().deserialize()?;
                players.remove(&name);
                if other(&name) && !owner.is_empty() {
                    let playing = playing(connection, &name).await;
                    players.insert(name, OtherPlayer { owner, playing });
                }
            }
            else => return Ok(()),
        }
    }
}

/// Publishes whether another MPRIS player on the desktop plays, until
/// shutdown. Without a session bus, spun logs one warning and never fades.
async fn follow(shutdown: watch::Receiver<bool>, playing_tx: watch::Sender<bool>) {
    let following = async {
        let connection = zbus::Connection::session().await?;
        follow_players(&connection, &playing_tx).await
    };
    tokio::select! {
        result = following => {
            if let Err(error) = result {
                log::warn!("watching the other media players failed: {error}");
            }
            // Without the bus, no signal can say that a player stopped.
            playing_tx.send_replace(false);
        }
        _ = player::shutdown_requested(shutdown) => {}
    }
}

/// Fades spun to silence while another media player on the desktop plays,
/// and back up when it stops. With `other_media_action = "pause"`, spun
/// also pauses, and plays again afterwards. Other media is optional:
/// without a session bus, spun logs one warning and plays as usual.
pub fn spawn(
    fade: Fade,
    snapshots: SnapshotRx,
    commands: CommandTx,
    config: watch::Receiver<Config>,
    shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(ramp(fade.clone(), shutdown.clone()));
    let (playing_tx, playing_rx) = watch::channel(false);
    tokio::spawn(follow(shutdown.clone(), playing_tx));
    tokio::spawn(control(
        fade, snapshots, commands, config, playing_rx, shutdown,
    ));
}

/// Applies the fade and the pause to the player, following the config, the
/// player state, and the other players.
async fn control(
    fade: Fade,
    mut snapshots: SnapshotRx,
    commands: CommandTx,
    mut config: watch::Receiver<Config>,
    mut others: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ducking = Ducking::new();
    loop {
        let input = {
            let config = config.borrow_and_update();
            let snapshot = snapshots.borrow_and_update();
            let playing = snapshot.link == Link::Connected && snapshot.playing;
            Input {
                action: config.other_media_action.unwrap_or_default(),
                delay: config.other_media_delay(),
                snooze: config.mute_snooze_time(),
                others: *others.borrow_and_update(),
                playing,
                can_pause: playing && snapshot.allows("pause") && snapshot.allows("play"),
            }
        };
        let now = Instant::now();
        for effect in ducking.settle(input, now) {
            match effect {
                Effect::FadeToSilence => fade.set_target(0.0),
                Effect::FadeToVolume => fade.set_target(1.0),
                Effect::Pause => player::enqueue(&commands, Command::SetPlaying(false)),
                Effect::Play => player::enqueue(&commands, Command::SetPlaying(true)),
            }
        }
        // A closed channel disables its branch.
        tokio::select! {
            Ok(()) = others.changed() => {}
            Ok(()) = snapshots.changed() => {}
            Ok(()) = config.changed() => {}
            Ok(()) = shutdown.changed() => {}
            _ = wait_until(ducking.wake_at(now)) => {}
            else => break,
        }
        if *shutdown.borrow() {
            return;
        }
    }
}

/// Waits for `at`, or forever without one.
async fn wait_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(action: OtherMediaAction, others: bool, playing: bool) -> Input {
        Input {
            action,
            delay: Duration::ZERO,
            snooze: Some(Duration::from_secs(30 * 60)),
            others,
            playing,
            can_pause: true,
        }
    }

    #[test]
    fn a_fade_passes_the_audio_through_until_it_moves() {
        let fade = Fade::new();
        assert_eq!(fade.gain(), 1.0);
        let mut audio = [1.0_f32, -1.0, 0.5];
        fade.callback()(&mut audio);
        assert_eq!(audio, [1.0, -1.0, 0.5]);
        fade.set_target(0.0);
        let steps = (FADE.as_secs_f32() / STEP.as_secs_f32()).ceil() as usize;
        for _ in 0..steps {
            fade.step();
        }
        assert_eq!(fade.gain(), 0.0);
        let mut audio = [1.0_f32, -1.0];
        fade.callback()(&mut audio);
        assert_eq!(audio, [0.0, 0.0]);
    }

    #[test]
    fn a_step_moves_the_gain_toward_the_target() {
        let fade = Fade::new();
        fade.set_gain(0.4);
        fade.set_target(0.0);
        assert!(fade.step());
        assert!(fade.gain() < 0.4);
        assert_eq!(fade.gain(), 0.4 - STEP.as_secs_f32() / FADE.as_secs_f32());
    }

    #[test]
    fn mute_fades_to_silence_and_keeps_playing_until_the_snooze_runs_out() {
        let now = Instant::now();
        let mut ducking = Ducking::new();
        let play = input(OtherMediaAction::Mute, true, true);
        assert_eq!(ducking.settle(play, now), [Effect::FadeToSilence]);
        // Long before the snooze ends, spun only stays silent.
        assert_eq!(ducking.settle(play, now + Duration::from_secs(60)), []);
        assert!(!ducking.paused);
        // At the end of the snooze, spun pauses once.
        let snooze = play.snooze.unwrap();
        assert_eq!(ducking.settle(play, now + snooze), [Effect::Pause]);
        assert!(ducking.paused);
        assert_eq!(ducking.settle(play, now + snooze), []);
        // The other player stops: spun fades up and plays again.
        let stop = input(OtherMediaAction::Mute, false, false);
        assert_eq!(
            ducking.settle(stop, now + snooze),
            [Effect::FadeToVolume, Effect::Play]
        );
        assert!(!ducking.paused);
        assert_eq!(ducking.target, 1.0);
    }

    #[test]
    fn another_player_must_play_for_the_delay_before_spun_fades() {
        let now = Instant::now();
        let delay = Duration::from_millis(500);
        let mut ducking = Ducking::new();
        let play = Input {
            delay,
            ..input(OtherMediaAction::Mute, true, true)
        };
        let stop = Input {
            others: false,
            ..play
        };
        // Within the delay, spun plays on and waits for the end of it.
        assert_eq!(ducking.settle(play, now), []);
        assert_eq!(ducking.wake_at(now), Some(now + delay));
        assert_eq!(ducking.settle(play, now + delay / 2), []);
        // A player that stops within the delay leaves spun alone, and the
        // next one starts a new delay.
        assert_eq!(ducking.settle(stop, now + delay / 2), []);
        assert_eq!(ducking.wake_at(now + delay / 2), None);
        assert_eq!(ducking.settle(play, now + delay), []);
        assert_eq!(
            ducking.settle(play, now + delay * 2),
            [Effect::FadeToSilence]
        );
        // After the delay, only the snooze wakes spun.
        assert_eq!(
            ducking.wake_at(now + delay * 2),
            Some(now + delay * 2 + play.snooze.unwrap())
        );
        assert_eq!(
            ducking.settle(stop, now + delay * 3),
            [Effect::FadeToVolume]
        );
    }

    #[test]
    fn pause_mode_fades_and_then_pauses() {
        let now = Instant::now();
        let mut ducking = Ducking::new();
        let play = input(OtherMediaAction::Pause, true, true);
        assert_eq!(ducking.settle(play, now), [Effect::FadeToSilence]);
        // The fade is still running.
        assert_eq!(ducking.settle(play, now + FADE / 2), []);
        assert_eq!(ducking.settle(play, now + FADE), [Effect::Pause]);
        let stop = input(OtherMediaAction::Pause, false, false);
        assert_eq!(
            ducking.settle(stop, now + FADE),
            [Effect::FadeToVolume, Effect::Play]
        );
    }

    #[test]
    fn ignore_does_nothing() {
        let now = Instant::now();
        let mut ducking = Ducking::new();
        let ignore = input(OtherMediaAction::Ignore, true, true);
        assert_eq!(ducking.settle(ignore, now), []);
        assert_eq!(
            ducking.settle(ignore, now + Duration::from_secs(60 * 60)),
            []
        );
        assert!(ducking.pause_at.is_none());
    }

    #[test]
    fn a_zero_snooze_never_pauses() {
        let now = Instant::now();
        let mut ducking = Ducking::new();
        let quiet = Input {
            snooze: None,
            ..input(OtherMediaAction::Mute, true, true)
        };
        assert_eq!(ducking.settle(quiet, now), [Effect::FadeToSilence]);
        assert_eq!(
            ducking.settle(quiet, now + Duration::from_secs(24 * 60 * 60)),
            []
        );
        assert!(!ducking.paused);
    }

    #[test]
    fn without_a_pause_command_pause_mode_only_ducks() {
        let now = Instant::now();
        let mut ducking = Ducking::new();
        let play = Input {
            can_pause: false,
            ..input(OtherMediaAction::Pause, true, true)
        };
        assert_eq!(ducking.settle(play, now), [Effect::FadeToSilence]);
        // spun never pauses without the server's leave, so it only holds
        // the silence.
        assert_eq!(ducking.settle(play, now + FADE), []);
        assert_eq!(
            ducking.settle(play, now + play.snooze.unwrap()),
            [],
            "the snooze of pause mode is no pause without a command"
        );
        assert!(!ducking.paused);

        // Mute mode does not pause either.
        let mute = Input {
            can_pause: false,
            ..input(OtherMediaAction::Mute, true, true)
        };
        let mut ducking = Ducking::new();
        ducking.settle(mute, now);
        assert_eq!(ducking.settle(mute, now + mute.snooze.unwrap()), []);
    }

    #[test]
    fn the_snooze_clock_runs_only_while_spun_plays() {
        let now = Instant::now();
        let mut ducking = Ducking::new();
        let idle = input(OtherMediaAction::Mute, true, false);
        assert_eq!(ducking.settle(idle, now), [Effect::FadeToSilence]);
        assert!(ducking.pause_at.is_none());
        // Spun plays, so the snooze starts now.
        let play = input(OtherMediaAction::Mute, true, true);
        assert_eq!(ducking.settle(play, now + Duration::from_secs(60)), []);
        let started = ducking.pause_at.expect("the snooze runs");
        assert_eq!(ducking.settle(play, started - Duration::from_secs(1)), []);
        assert_eq!(ducking.settle(play, started), [Effect::Pause]);
    }

    #[test]
    fn spun_that_finds_itself_playing_again_does_not_pause_twice() {
        let now = Instant::now();
        let mut ducking = Ducking::new();
        let play = input(OtherMediaAction::Pause, true, true);
        ducking.settle(play, now);
        assert_eq!(ducking.settle(play, now + FADE), [Effect::Pause]);
        // A server that reports playing again is no reason to pause twice.
        assert_eq!(ducking.settle(play, now + FADE * 2), []);
    }

    /// A connected player that the server lets control.
    fn playing_player(playing: bool) -> crate::player::Snapshot {
        crate::player::Snapshot {
            link: Link::Connected,
            controllable: true,
            supported_commands: vec!["play".into(), "pause".into()],
            playing,
            ..crate::player::Snapshot::default()
        }
    }

    #[tokio::test]
    async fn another_player_fades_spun_and_the_end_of_the_snooze_pauses_it() {
        let fade = Fade::new();
        let (snapshots, snapshot_rx) = watch::channel(playing_player(true));
        let (commands, mut command_rx) = tokio::sync::mpsc::channel(4);
        let (config, config_rx) = watch::channel(Config {
            other_media_action: Some(OtherMediaAction::Mute),
            other_media_delay_ms: 0,
            mute_snooze: 0,
            ..Config::default()
        });
        let (others, others_rx) = watch::channel(false);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(control(
            fade.clone(),
            snapshot_rx,
            commands,
            config_rx,
            others_rx,
            shutdown_rx,
        ));
        assert_eq!(fade.wanted(), 1.0);

        // Another player starts: spun fades down but keeps playing.
        others.send_replace(true);
        tokio::time::timeout(Duration::from_secs(2), async {
            while fade.wanted() != 0.0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            command_rx.try_recv().is_err(),
            "mute mode keeps playing while the snooze is off"
        );

        // It stops: spun fades back up.
        others.send_replace(false);
        tokio::time::timeout(Duration::from_secs(2), async {
            while fade.wanted() != 1.0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        // A config edit of the action applies at once.
        config.send_modify(|config| config.other_media_action = Some(OtherMediaAction::Ignore));
        others.send_replace(true);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fade.wanted(), 1.0, "ignore holds the volume");
        assert!(command_rx.try_recv().is_err());
        assert_eq!(snapshots.borrow().link, Link::Connected);

        shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn pause_mode_pauses_after_the_fade_and_plays_again_when_the_other_player_stops() {
        let fade = Fade::new();
        let (snapshots, snapshot_rx) = watch::channel(playing_player(true));
        let (commands, mut command_rx) = tokio::sync::mpsc::channel(4);
        let (_config, config_rx) = watch::channel(Config {
            other_media_action: Some(OtherMediaAction::Pause),
            other_media_delay_ms: 0,
            ..Config::default()
        });
        let (others, others_rx) = watch::channel(false);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(control(
            fade.clone(),
            snapshot_rx,
            commands,
            config_rx,
            others_rx,
            shutdown_rx,
        ));

        others.send_replace(true);
        let pause = tokio::time::timeout(Duration::from_secs(2), command_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pause, Command::SetPlaying(false));
        assert_eq!(fade.wanted(), 0.0);

        // The server reports the pause, as it does for a command.
        snapshots.send_replace(playing_player(false));
        others.send_replace(false);
        let play = tokio::time::timeout(Duration::from_secs(2), command_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(play, Command::SetPlaying(true));
        assert_eq!(fade.wanted(), 1.0);

        shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }
    /// A fake MPRIS player on the session bus that owns `name`. It reports
    /// `status` after each call of the returned handle's `set`.
    async fn bus_player(name: &str, status: &str) -> BusPlayer {
        let status = Arc::new(std::sync::Mutex::new(status.to_string()));
        let connection = zbus::connection::Builder::session()
            .unwrap()
            .name(name.to_string())
            .unwrap()
            .serve_at(MPRIS_PATH, FakePlayer(Arc::clone(&status)))
            .unwrap()
            .build()
            .await
            .unwrap();
        BusPlayer { connection, status }
    }

    struct BusPlayer {
        connection: zbus::Connection,
        status: Arc<std::sync::Mutex<String>>,
    }

    impl BusPlayer {
        /// Changes the status and sends `PropertiesChanged`, as a player does.
        async fn set(&self, status: &str) {
            *self.status.lock().unwrap() = status.into();
            let player = self
                .connection
                .object_server()
                .interface::<_, FakePlayer>(MPRIS_PATH)
                .await
                .unwrap();
            player
                .get()
                .await
                .playback_status_changed(player.signal_emitter())
                .await
                .unwrap();
        }
    }

    /// Waits until `others` says `plays`.
    async fn others_say(others: &mut watch::Receiver<bool>, plays: bool) {
        tokio::time::timeout(
            Duration::from_secs(2),
            others.wait_for(|others| *others == plays),
        )
        .await
        .unwrap_or_else(|_| panic!("the other players must say {plays}"))
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires an isolated session bus; run under dbus-run-session"]
    async fn the_bus_signals_say_when_another_player_plays() {
        // A player from before spun started is read once.
        let early = bus_player("org.mpris.MediaPlayer2.early", "Playing").await;
        // spun's own player never counts.
        let own = format!("{NAME_PREFIX}{}", env!("CARGO_PKG_NAME"));
        let _own = bus_player(&own, "Playing").await;
        let (playing_tx, mut others) = watch::channel(false);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(follow(shutdown_rx, playing_tx));
        others_say(&mut others, true).await;
        early.set("Paused").await;
        others_say(&mut others, false).await;

        // A player that comes later is read when it takes its name, and
        // then followed by its signals.
        let late = bus_player("org.mpris.MediaPlayer2.late.instance1", "Stopped").await;
        late.set("Playing").await;
        others_say(&mut others, true).await;
        late.set("Paused").await;
        others_say(&mut others, false).await;
        late.set("Playing").await;
        others_say(&mut others, true).await;
        // A player that leaves the bus while it plays is silent.
        drop(late);
        others_say(&mut others, false).await;

        let late = bus_player("org.mpris.MediaPlayer2.late.instance2", "Playing").await;
        others_say(&mut others, true).await;
        drop(late);
        others_say(&mut others, false).await;

        shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    /// A fake MPRIS player that reports the status that it is given.
    struct FakePlayer(Arc<std::sync::Mutex<String>>);

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl FakePlayer {
        #[zbus(property)]
        fn playback_status(&self) -> String {
            self.0.lock().unwrap().clone()
        }
    }

    /// A connection to a fake player, and the handle that sets its status.
    /// The connection has no bus, so the player answers to each name.
    async fn fake_player(status: &str) -> (zbus::Connection, Arc<std::sync::Mutex<String>>) {
        let status = Arc::new(std::sync::Mutex::new(status.to_string()));
        let player = FakePlayer(Arc::clone(&status));
        let (_, spun) = crate::tests::dbus_pair(|bus| bus.serve_at(MPRIS_PATH, player)).await;
        (spun, status)
    }

    const FAKE: &str = "org.mpris.MediaPlayer2.fake";

    #[tokio::test]
    async fn a_player_counts_as_playing_only_while_it_says_so() {
        let (spun, status) = fake_player("Playing").await;
        assert!(playing(&spun, FAKE).await);
        *status.lock().unwrap() = "Paused".into();
        assert!(!playing(&spun, FAKE).await);
        *status.lock().unwrap() = "Stopped".into();
        assert!(!playing(&spun, FAKE).await);
    }

    #[tokio::test]
    async fn a_player_that_does_not_answer_counts_as_silent() {
        // A player that serves no object where its status belongs.
        let player = FakePlayer(Arc::new(std::sync::Mutex::new("Playing".into())));
        let (_, spun) =
            crate::tests::dbus_pair(|bus| bus.serve_at("/somewhere/else", player)).await;
        assert!(!playing(&spun, FAKE).await);
    }
}
