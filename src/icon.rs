//! The tray badge: the U and N glyph masks from `build.rs`, drawn in the
//! configured colours and rotated. Rendering at runtime instead of shipping
//! prerendered pictures means every colour and angle combination is
//! available, at the cost of one pass over 16K pixels when the state changes.

use std::fmt;
use std::time::{Duration, Instant};

use tokio::sync::watch;

mod masks {
    include!(concat!(env!("OUT_DIR"), "/tray_icon.rs"));
}

/// The icon is square so panels fill their square slot with it. A
/// wider-than-tall bitmap is scaled by width, so it ends up about half the
/// height of the slot.
pub const SIZE: u32 = masks::SIZE;

/// How long a full turn of the spinning icon takes.
/// `60 / x` gives the same speed as an `x` rpm vinyl.
const SPIN_SECONDS: f64 = 60.0 / 33.0;

/// How often an animated icon redraws, and the step of the connecting and
/// error animations. Each frame sends the whole bitmap over D-Bus, so this
/// is deliberately slow.
pub const SPIN_FRAME: std::time::Duration = std::time::Duration::from_millis(337);

/// An sRGB colour of the badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// The wordmark colour.
    pub const TEAL: Self = Self::new(0x2f, 0xb0, 0xac);
    /// The inactive colour on a dark panel. A light panel gets a darker shade.
    pub const GREY: Self = Self::new(0x8f, 0x8f, 0x8f);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parses `#rrggbb` or `#rgb`. The leading `#` is optional, as in CSS.
    pub fn parse(text: &str) -> Result<Self, String> {
        let digits = text.strip_prefix('#').unwrap_or(text);
        let expand = |c: u8| c * 17;
        match digits.len() {
            6 => {
                let value = u32::from_str_radix(digits, 16)
                    .map_err(|_| format!("`{text}` is not a hex colour"))?;
                Ok(Self::new(
                    (value >> 16) as u8,
                    (value >> 8) as u8,
                    value as u8,
                ))
            }
            3 => {
                let value = u16::from_str_radix(digits, 16)
                    .map_err(|_| format!("`{text}` is not a hex colour"))?;
                Ok(Self::new(
                    expand((value >> 8) as u8),
                    expand((value >> 4) as u8 & 0xf),
                    expand(value as u8 & 0xf),
                ))
            }
            _ => Err(format!(
                "`{text}` must be a hex colour such as #2fb0ac or #abc"
            )),
        }
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// A state that the badge shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Connected and playing. The badge turns.
    Playing { muted: bool },
    /// Playback stopped, and the server kept the track. The badge keeps the
    /// angle and the colours of `Playing`.
    Paused { muted: bool },
    /// Connected, with no track. `controllable` is true when the server
    /// gave spun the controller role.
    Stopped { muted: bool, controllable: bool },
    /// No server, or the connection dropped.
    Offline,
    /// spun dials the server.
    Connecting,
    /// spun cannot play the stream.
    Error,
}

impl State {
    /// True for the states that turn the badge or hold its turn.
    fn turns(self) -> bool {
        matches!(self, Self::Playing { .. } | Self::Paused { .. })
    }
}

/// How long the badge has shown its state, for the animations. A new state
/// starts again at zero. Between playing and paused, the turn goes on
/// instead, and a pause holds it.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    state: State,
    /// When the state began.
    since: Instant,
    /// The run time before `since`.
    held: Duration,
}

impl Clock {
    pub fn new(state: State, now: Instant) -> Self {
        Self {
            state,
            since: now,
            held: Duration::ZERO,
        }
    }

    /// Moves to `state` at `now`. A change of flags only, such as a mute,
    /// keeps the animation going.
    pub fn set(&mut self, state: State, now: Instant) {
        if std::mem::discriminant(&state) != std::mem::discriminant(&self.state) {
            self.held = if self.state.turns() && state.turns() {
                self.run(now)
            } else {
                Duration::ZERO
            };
            self.since = now;
        }
        self.state = state;
    }

    /// How far the animation has run at `now`.
    pub fn run(&self, now: Instant) -> Duration {
        match self.state {
            State::Paused { .. } => self.held,
            _ => self.held + now.saturating_duration_since(self.since),
        }
    }
}

/// A colour slot in the config. A hex string is that colour; `"auto"` lets
/// spun pick one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Color {
    Explicit(Rgb),
    #[default]
    Auto,
}

impl Color {
    /// The colour from the config, or `None` for `"auto"`.
    pub fn explicit(self) -> Option<Rgb> {
        match self {
            Self::Explicit(rgb) => Some(rgb),
            Self::Auto => None,
        }
    }
}

impl<'de> serde::Deserialize<'de> for Color {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        Rgb::parse(&text)
            .map(Self::Explicit)
            .map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for Color {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Explicit(rgb) => serializer.serialize_str(&rgb.to_string()),
            Self::Auto => serializer.serialize_str("auto"),
        }
    }
}

/// The colours that the server picks for the current track, from the
/// `color@v1` role. Each is `None` when the server sends none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerColors {
    pub primary: Option<Rgb>,
    pub secondary: Option<Rgb>,
}

/// How far [`shade`] moves a colour toward black or white.
const SHADE: f64 = 0.35;

/// `rgb` moved toward black when `darker`, else toward white.
pub fn shade(rgb: Rgb, darker: bool) -> Rgb {
    let target = if darker { 0.0 } else { 255.0 };
    let mix = |c: u8| (f64::from(c) + (target - f64::from(c)) * SHADE).round() as u8;
    Rgb::new(mix(rgb.r), mix(rgb.g), mix(rgb.b))
}

/// The badge in every state it can be drawn for. The desktop settings are
/// already in here, so drawing costs no D-Bus call. The server colours
/// change with the track, so each draw takes them as an argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Style {
    /// The colour from the config. `None` takes the server colour, then
    /// `accent`.
    pub primary: Option<Rgb>,
    /// The colour from the config. `None` takes the server colour, then a
    /// shade of the primary colour.
    pub secondary: Option<Rgb>,
    pub inactive: Rgb,
    /// The primary colour when the config and the server set none: the
    /// desktop accent colour, or teal.
    pub accent: Rgb,
    /// The panel is dark: the secondary shade is darker than the primary
    /// colour. On a light panel, it is lighter.
    pub dark: bool,
    /// Animates the badge: it turns while it plays, steps while spun
    /// connects, and blinks on an error.
    pub animations: bool,
}

impl Style {
    /// The colours of the U and the N in `state`, `run` into its animation.
    /// The primary and secondary colours come from the config first, then
    /// the server, then the desktop.
    fn glyph_colors(&self, state: State, server: ServerColors, run: Duration) -> (Rgb, Rgb) {
        let primary = self.primary.or(server.primary).unwrap_or(self.accent);
        let secondary = self
            .secondary
            .or(server.secondary)
            .unwrap_or_else(|| shade(primary, self.dark));
        let grey = self.inactive;
        // A still badge stays on frame 0.
        let frame = if self.animations {
            run.as_millis() / SPIN_FRAME.as_millis()
        } else {
            0
        };
        match state {
            State::Playing { muted: true }
            | State::Paused { muted: true }
            | State::Stopped { muted: true, .. }
            | State::Offline => (grey, grey),
            State::Playing { .. } | State::Paused { .. } => (primary, secondary),
            State::Stopped {
                controllable: true, ..
            } => (grey, primary),
            State::Stopped { .. } => (grey, secondary),
            // Grey, then the U lit, then the N lit, for 2 frames each.
            State::Connecting => match frame / 2 % 3 {
                1 => (primary, grey),
                2 => (grey, primary),
                _ => (grey, grey),
            },
            // A 3-second loop of 15 frames: grey, two blinks of 2 frames,
            // then a rest. A still badge stays lit, so the error shows.
            State::Error if !self.animations || matches!(frame % 15, 2 | 3 | 6 | 7) => {
                (primary, primary)
            }
            State::Error => (grey, grey),
        }
    }

    /// The icon for `state`, `run` into its animation, as ARGB32 pixels in
    /// network byte order. With animations on, the badge turns while it
    /// plays and keeps its angle while paused. Otherwise it stands upright.
    pub fn pixels(&self, state: State, server: ServerColors, run: Duration) -> Vec<u8> {
        let degrees = if self.animations && state.turns() {
            run.as_secs_f64() / SPIN_SECONDS * 360.0
        } else {
            0.0
        };
        let (u, n) = self.glyph_colors(state, server, run);
        render(u, n, 360.0 - degrees)
    }

    /// How often the icon changes by itself; `None` when it holds still.
    pub fn frame_interval(&self) -> Option<std::time::Duration> {
        self.animations.then_some(SPIN_FRAME)
    }
}

/// Draws the badge: each glyph in its own colour, both rotated about the
/// centre by `degrees`. Sampling the masks per destination pixel keeps the
/// rotation smooth, and since the masks are disjoint the two colours simply
/// paint over each other.
fn render(u_rgb: Rgb, n_rgb: Rgb, degrees: f64) -> Vec<u8> {
    let angle = degrees.to_radians();
    let (sin, cos) = angle.sin_cos();
    let center = (f64::from(SIZE) - 1.0) / 2.0;
    let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            // Rotate the destination pixel back to the mask it came from.
            let dx = f64::from(x) - center;
            let dy = f64::from(y) - center;
            let sx = cos * dx + sin * dy + center;
            let sy = -sin * dx + cos * dy + center;
            let u = coverage(&masks::MASK_U, sx, sy);
            let n = coverage(&masks::MASK_N, sx, sy);
            let (alpha, rgb) = over(u, u_rgb, n, n_rgb);
            // ARGB32 in network byte order: a, r, g, b.
            let alpha = (alpha * 255.0).round() as u8;
            pixels.extend_from_slice(&[alpha, rgb.r, rgb.g, rgb.b]);
        }
    }
    pixels
}

/// `top` over `bottom`, returning the combined alpha and the colour to draw
/// the result in. Straight alpha, so the colour is the opacity-weighted mix.
fn over(top_a: f64, top: Rgb, bottom_a: f64, bottom: Rgb) -> (f64, Rgb) {
    let alpha = top_a + bottom_a * (1.0 - top_a);
    if alpha <= 0.0 {
        return (0.0, Rgb::new(0, 0, 0));
    }
    let mix = |t: u8, b: u8| {
        let value = (f64::from(t) * top_a + f64::from(b) * bottom_a * (1.0 - top_a)) / alpha;
        value.round() as u8
    };
    (
        alpha,
        Rgb::new(
            mix(top.r, bottom.r),
            mix(top.g, bottom.g),
            mix(top.b, bottom.b),
        ),
    )
}

/// The mask's coverage at `(x, y)`, interpolated between the four pixels
/// around it so a rotation does not stair-step the edges. Outside the mask
/// is empty.
fn coverage(mask: &[u8], x: f64, y: f64) -> f64 {
    let size = SIZE as i64;
    let sample = |ix: i64, iy: i64| -> f64 {
        if ix < 0 || iy < 0 || ix >= size || iy >= size {
            0.0
        } else {
            f64::from(mask[(iy * size + ix) as usize]) / 255.0
        }
    };
    // The masks are drawn so that the art never touches the edge, so a
    // fractional coordinate below zero still has a neighbour to blend with.
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let (x0, y0) = (x0 as i64, y0 as i64);
    let top = sample(x0, y0) * (1.0 - fx) + sample(x0 + 1, y0) * fx;
    let bottom = sample(x0, y0 + 1) * (1.0 - fx) + sample(x0 + 1, y0 + 1) * fx;
    top * (1.0 - fy) + bottom * fy
}

/// The portal namespace of the looks settings.
const APPEARANCE: &str = "org.freedesktop.appearance";

/// The portal interface of the settings.
const SETTINGS: &str = "org.freedesktop.portal.Settings";

/// What the desktop says about its looks. The default is what spun assumes
/// when the desktop says nothing: a dark panel, no accent colour, and
/// animations allowed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Desktop {
    /// The desktop prefers light. Panels may differ, so only a clear
    /// preference for light counts.
    pub light: bool,
    pub accent: Option<Rgb>,
    pub reduced_motion: bool,
}

impl Desktop {
    /// Reads the desktop settings, then follows each change on a background
    /// task. Without a session bus, the settings stay at the default.
    pub async fn watch() -> watch::Receiver<Self> {
        let connection = match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(error) => {
                log::debug!("cannot read the desktop looks: {error}");
                return watch::channel(Self::default()).1;
            }
        };
        let (tx, rx) = watch::channel(Self::read(&connection).await);
        tokio::spawn(async move {
            if let Err(error) = Self::follow(&connection, &tx).await {
                log::warn!("cannot follow the desktop looks: {error}");
            }
        });
        rx
    }

    /// Sends the settings to `tx` after each `SettingChanged` signal of the
    /// portal. The signal says only that a setting changed, so spun reads
    /// them all again. This ends when the connection closes.
    async fn follow(connection: &zbus::Connection, tx: &watch::Sender<Self>) -> zbus::Result<()> {
        use futures_util::StreamExt;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(SETTINGS)?
            .member("SettingChanged")?
            .arg(0, APPEARANCE)?
            .build();
        let mut changes = zbus::MessageStream::for_match_rule(rule, connection, None).await?;
        // The first read catches a change from before the subscription.
        loop {
            let desktop = Self::read(connection).await;
            tx.send_if_modified(|current| {
                let changed = *current != desktop;
                *current = desktop;
                changed
            });
            if changes.next().await.is_none() {
                return Ok(());
            }
        }
    }

    /// Reads the settings from the XDG settings portal, the only standard
    /// API for them. Without an answer, this is the default.
    async fn read(connection: &zbus::Connection) -> Self {
        use std::collections::HashMap;
        use zbus::zvariant::OwnedValue;
        /// A hung portal must not hold up startup.
        const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
        let read = async {
            let reply = connection
                .call_method(
                    Some("org.freedesktop.portal.Desktop"),
                    "/org/freedesktop/portal/desktop",
                    Some(SETTINGS),
                    "ReadAll",
                    &(vec![APPEARANCE],),
                )
                .await
                .ok()?;
            let mut namespaces: HashMap<String, HashMap<String, OwnedValue>> =
                reply.body().deserialize().ok()?;
            Some(Self::from_settings(&namespaces.remove(APPEARANCE)?))
        };
        let desktop = match tokio::time::timeout(TIMEOUT, read).await {
            Ok(desktop) => desktop.unwrap_or_default(),
            Err(_) => {
                log::debug!("the desktop portal did not answer in time");
                Self::default()
            }
        };
        log::debug!("desktop looks: {desktop:?}");
        desktop
    }

    /// Reads the `org.freedesktop.appearance` settings. A missing or
    /// malformed setting counts as no preference.
    fn from_settings(
        settings: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> Self {
        let number = |key: &str| {
            settings
                .get(key)
                .and_then(|value| value.downcast_ref::<u32>().ok())
        };
        let accent = settings
            .get("accent-color")
            .and_then(|value| value.downcast_ref::<(f64, f64, f64)>().ok())
            .and_then(|(r, g, b)| Some(Rgb::new(to_u8(r)?, to_u8(g)?, to_u8(b)?)));
        Self {
            // 0 is no preference, 1 dark, and 2 light.
            light: number("color-scheme") == Some(2),
            accent,
            // 0 is no preference and 1 reduced motion.
            reduced_motion: number("reduced-motion") == Some(1),
        }
    }
}

/// The portal reports each channel as a fraction of full intensity. The spec
/// treats a channel outside 0–1 as an unset accent colour, so it gives `None`.
fn to_u8(channel: f64) -> Option<u8> {
    (0.0..=1.0)
        .contains(&channel)
        .then_some((channel * 255.0).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const ORANGE: Rgb = Rgb::new(0xe8, 0x7b, 0x2c);
    const NONE: ServerColors = ServerColors {
        primary: None,
        secondary: None,
    };
    const PLAYING: State = State::Playing { muted: false };

    fn style() -> Style {
        Style {
            primary: Some(Rgb::TEAL),
            secondary: Some(ORANGE),
            inactive: Rgb::GREY,
            accent: Rgb::TEAL,
            dark: true,
            animations: false,
        }
    }

    fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
        let at = ((y * SIZE + x) * 4) as usize;
        pixels[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn colours_parse_from_hex_and_round_trip() {
        assert_eq!(Rgb::parse("#2fb0ac"), Ok(Rgb::TEAL));
        assert_eq!(Rgb::parse("2fb0ac"), Ok(Rgb::TEAL));
        assert_eq!(Rgb::parse("#fff"), Ok(Rgb::new(255, 255, 255)));
        assert_eq!(Rgb::parse("#08f"), Ok(Rgb::new(0x00, 0x88, 0xff)));
        assert_eq!(Rgb::TEAL.to_string(), "#2fb0ac");
        assert_eq!(Rgb::parse(&ORANGE.to_string()), Ok(ORANGE));
        assert!(Rgb::parse("#2fb0ag").is_err());
        assert!(Rgb::parse("teal").is_err());
        assert!(Rgb::parse("#2fb0acc").is_err());
    }

    #[test]
    fn the_icon_is_square_and_fully_opaque_somewhere() {
        let pixels = style().pixels(PLAYING, NONE, Duration::ZERO);
        assert_eq!(pixels.len(), (SIZE * SIZE * 4) as usize);
        assert!(pixels.as_chunks::<4>().0.iter().any(|px| px[0] > 0));
    }

    #[test]
    fn the_pixels_use_network_byte_order_argb() {
        let u_rgb = Rgb::new(0x11, 0x22, 0x33);
        let pixels = render(u_rgb, Rgb::new(0, 0, 0), 0.0);
        assert!(
            pixels.as_chunks::<4>().0.contains(&[255, 0x11, 0x22, 0x33]),
            "alpha comes first, then red, green, and blue"
        );
    }

    #[test]
    fn a_playing_badge_has_a_primary_u_and_a_secondary_n() {
        // The U is left of centre, the N right, so both colours appear.
        let pixels = style().pixels(PLAYING, NONE, Duration::ZERO);
        let has = |rgb: Rgb| {
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|px| px[0] == 255 && px[1..] == [rgb.r, rgb.g, rgb.b])
        };
        assert!(has(Rgb::TEAL), "the U is the primary colour");
        assert!(has(ORANGE), "the N is the secondary colour");
    }

    #[test]
    fn an_offline_badge_is_drawn_only_in_the_inactive_colour() {
        let pixels = style().pixels(State::Offline, NONE, Duration::ZERO);
        let mut colours: Vec<[u8; 3]> = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[0] == 255)
            .map(|px| [px[1], px[2], px[3]])
            .collect();
        colours.sort_unstable();
        colours.dedup();
        assert_eq!(colours, vec![[Rgb::GREY.r, Rgb::GREY.g, Rgb::GREY.b]]);
    }

    #[test]
    fn the_art_never_touches_the_edge_so_a_rotation_cannot_clip_it() {
        for rotation in [0, 45, 90, 135, 180, 260, 315] {
            let pixels = render(Rgb::TEAL, ORANGE, f64::from(rotation));
            for x in 0..SIZE {
                for y in [0, SIZE - 1] {
                    assert_eq!(pixel(&pixels, x, y)[0], 0, "{rotation}° clips at {x},{y}");
                }
            }
            for y in 0..SIZE {
                for x in [0, SIZE - 1] {
                    assert_eq!(pixel(&pixels, x, y)[0], 0, "{rotation}° clips at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn a_rotation_moves_the_glyphs() {
        let upright = render(Rgb::TEAL, ORANGE, 0.0);
        let turned = render(Rgb::TEAL, ORANGE, 180.0);
        assert_ne!(upright, turned);
        // Half a turn maps the top row onto the bottom row.
        for x in 0..SIZE {
            assert_eq!(
                pixel(&upright, x, 0),
                pixel(&turned, SIZE - 1 - x, SIZE - 1)
            );
        }
    }

    #[test]
    fn a_badge_turns_only_while_playing() {
        let style = Style {
            animations: true,
            ..style()
        };
        let at = |state, seconds| style.pixels(state, NONE, Duration::from_secs(seconds));
        assert_ne!(at(PLAYING, 1), at(PLAYING, 2));
        // The tray clock holds the run time of a pause, so a paused badge
        // keeps the angle and the colours of the playing badge.
        assert_eq!(at(State::Paused { muted: false }, 1), at(PLAYING, 1));
        let stopped = State::Stopped {
            muted: false,
            controllable: true,
        };
        assert_eq!(at(stopped, 1), at(stopped, 2));
        assert_eq!(style.frame_interval(), Some(SPIN_FRAME));
        assert_eq!(self::style().frame_interval(), None);
    }

    #[test]
    fn each_state_has_its_own_colours() {
        let (p, s, g) = (Rgb::TEAL, ORANGE, Rgb::GREY);
        let style = Style {
            animations: true,
            ..style()
        };
        let colors = |state, frame| style.glyph_colors(state, NONE, SPIN_FRAME * frame);
        let stopped = |muted, controllable| State::Stopped {
            muted,
            controllable,
        };
        assert_eq!(colors(PLAYING, 0), (p, s));
        assert_eq!(colors(State::Playing { muted: true }, 0), (g, g));
        assert_eq!(colors(State::Paused { muted: false }, 0), (p, s));
        assert_eq!(colors(State::Paused { muted: true }, 0), (g, g));
        assert_eq!(colors(stopped(false, true), 0), (g, p));
        assert_eq!(colors(stopped(false, false), 0), (g, s));
        assert_eq!(colors(stopped(true, true), 0), (g, g));
        assert_eq!(colors(State::Offline, 0), (g, g));

        // Connecting lights the U, then the N, for 2 frames each, and loops.
        let connecting: Vec<_> = (0..7).map(|f| colors(State::Connecting, f)).collect();
        let lit_u = (p, g);
        let lit_n = (g, p);
        assert_eq!(
            connecting,
            [(g, g), (g, g), lit_u, lit_u, lit_n, lit_n, (g, g)]
        );
        // An error blinks twice in each 15-frame loop and rests in grey.
        let blinks: Vec<u32> = (0..30)
            .filter(|&f| colors(State::Error, f) == (p, p))
            .collect();
        assert_eq!(blinks, [2, 3, 6, 7, 17, 18, 21, 22]);
        assert_eq!(colors(State::Error, 0), (g, g));

        // Without animations, connecting stays grey and an error stays lit.
        let still = |state| self::style().glyph_colors(state, NONE, SPIN_FRAME * 2);
        assert_eq!(still(State::Connecting), (g, g));
        assert_eq!(still(State::Error), (p, p));
    }

    #[test]
    fn a_pause_holds_the_turn_and_a_new_state_starts_again() {
        let ms = Duration::from_millis;
        let start = Instant::now();
        let at = |millis| start + ms(millis);
        let mut clock = Clock::new(PLAYING, start);
        assert_eq!(clock.run(at(1000)), ms(1000));
        // A mute changes only the colours, so the turn goes on.
        clock.set(State::Playing { muted: true }, at(1000));
        assert_eq!(clock.run(at(1500)), ms(1500));
        // A pause holds the turn, and playback goes on from it.
        clock.set(State::Paused { muted: true }, at(1500));
        assert_eq!(clock.run(at(9000)), ms(1500));
        clock.set(PLAYING, at(9000));
        assert_eq!(clock.run(at(9500)), ms(2000));
        // Any other state starts again at zero, and so does playback after it.
        clock.set(State::Error, at(9500));
        assert_eq!(clock.run(at(9700)), ms(200));
        clock.set(PLAYING, at(9700));
        assert_eq!(clock.run(at(9700)), Duration::ZERO);
    }

    #[test]
    fn the_config_wins_then_the_server_then_the_desktop() {
        let server = ServerColors {
            primary: Some(Rgb::new(1, 2, 3)),
            secondary: Some(Rgb::new(4, 5, 6)),
        };
        let playing = |style: &Style, server| style.glyph_colors(PLAYING, server, Duration::ZERO);
        assert_eq!(playing(&style(), server), (Rgb::TEAL, ORANGE));
        assert_eq!(
            style().glyph_colors(State::Offline, server, Duration::ZERO),
            (Rgb::GREY, Rgb::GREY),
            "an offline badge ignores the server"
        );

        let auto = Style {
            primary: None,
            secondary: None,
            accent: Rgb::new(200, 100, 0),
            ..style()
        };
        assert_eq!(
            playing(&auto, server),
            (server.primary.unwrap(), server.secondary.unwrap())
        );

        // Without server colours, the primary is the desktop accent, and the
        // secondary is a shade of it: darker on a dark panel.
        assert_eq!(
            playing(&auto, NONE),
            (Rgb::new(200, 100, 0), Rgb::new(130, 65, 0))
        );
        let light = Style {
            dark: false,
            ..auto.clone()
        };
        assert_eq!(playing(&light, NONE).1, Rgb::new(219, 154, 89));
        // The secondary shade follows the server primary, too.
        let only_primary = ServerColors {
            secondary: None,
            ..server
        };
        assert_eq!(
            playing(&auto, only_primary).1,
            shade(Rgb::new(1, 2, 3), true)
        );
    }

    #[test]
    fn the_portal_settings_are_read_from_their_variants() {
        use std::collections::HashMap;
        use zbus::zvariant::{OwnedValue, Value};

        let settings = |pairs: Vec<(&str, Value)>| -> HashMap<String, OwnedValue> {
            pairs
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.try_to_owned().unwrap()))
                .collect()
        };
        let light = Desktop::from_settings(&settings(vec![
            ("color-scheme", Value::from(2_u32)),
            ("accent-color", Value::from((0.5_f64, 0.25_f64, 1.0_f64))),
            ("reduced-motion", Value::from(1_u32)),
        ]));
        assert_eq!(
            light,
            Desktop {
                light: true,
                // Each channel is a fraction of full intensity.
                accent: Some(Rgb::new(128, 64, 255)),
                reduced_motion: true,
            }
        );
        // Only 2 is light: no preference and dark both mean a dark panel.
        for scheme in [0_u32, 1, 7] {
            let desktop =
                Desktop::from_settings(&settings(vec![("color-scheme", Value::from(scheme))]));
            assert!(!desktop.light, "{scheme}");
        }
        // A missing or malformed setting is no preference.
        assert_eq!(
            Desktop::from_settings(&settings(vec![])),
            Desktop::default()
        );
        assert_eq!(
            Desktop::from_settings(&settings(vec![
                ("color-scheme", Value::from("light")),
                ("accent-color", Value::from((-1.0_f64, -1.0_f64, -1.0_f64))),
            ])),
            Desktop::default()
        );
    }

    #[tokio::test]
    async fn the_desktop_looks_follow_the_portal_signal() {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use zbus::zvariant::{OwnedValue, Value};

        /// A portal that reports the colour scheme in the cell.
        struct Portal(Arc<Mutex<u32>>);

        #[zbus::interface(name = "org.freedesktop.portal.Settings")]
        impl Portal {
            fn read_all(
                &self,
                _namespaces: Vec<String>,
            ) -> HashMap<String, HashMap<String, OwnedValue>> {
                let scheme = OwnedValue::from(*self.0.lock().unwrap());
                let appearance = HashMap::from([("color-scheme".to_string(), scheme)]);
                HashMap::from([(APPEARANCE.to_string(), appearance)])
            }
        }

        let scheme = Arc::new(Mutex::new(2));
        let path = "/org/freedesktop/portal/desktop";
        let service = Portal(Arc::clone(&scheme));
        let (portal, spun) = crate::tests::dbus_pair(|bus| bus.serve_at(path, service)).await;
        let (tx, mut rx) = watch::channel(Desktop::default());
        tokio::spawn(async move { Desktop::follow(&spun, &tx).await });
        let wait = Duration::from_secs(2);
        // The first read comes after the subscription.
        tokio::time::timeout(wait, rx.wait_for(|desktop| desktop.light))
            .await
            .unwrap()
            .unwrap();

        // From here, only the signal makes spun read again.
        *scheme.lock().unwrap() = 1;
        let change = (APPEARANCE, "color-scheme", Value::from(1_u32));
        portal
            .emit_signal(None::<&str>, path, SETTINGS, "SettingChanged", &change)
            .await
            .unwrap();
        tokio::time::timeout(wait, rx.wait_for(|desktop| !desktop.light))
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn an_accent_channel_outside_zero_to_one_means_no_accent_colour() {
        for channel in [-1.0, 1.5, f64::NAN] {
            assert_eq!(to_u8(channel), None, "{channel}");
        }
        assert_eq!(to_u8(0.0), Some(0));
        assert_eq!(to_u8(1.0), Some(255));
    }
}
