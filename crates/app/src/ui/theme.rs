//! The tonal theme (`docs/brand/brand.md`): one hue per mode, no accent
//! color, and surfaces that darken with age.
//!
//! The user chooses a hue and a tint amount for night and for day
//! ([`Settings`]). Lightness is fixed here and is never theirs to change:
//! that is what keeps every text pair readable whatever they pick, and the
//! tests check each pair for every choice on offer.

pub(crate) mod settings;

use std::path::PathBuf;

use gpui::{App, AppContext, BorrowAppContext, Global, Rgba, Task, Window, WindowAppearance};

pub(crate) use self::settings::{Choice, Hue, Settings, Tint};

/// Messages, times and states.
pub(crate) const BODY_FONT: &str = "Avenir Next";
/// People: chat titles, sender names, the wordmark.
pub(crate) const NAME_FONT: &str = "Charter";
/// Code and preformatted text.
pub(crate) const MONO_FONT: &str = "Menlo";

/// The one corner radius, for messages, fields and buttons.
pub(crate) const RADIUS: f32 = 12.;
/// Opacity of quiet text (times, previews, hints) over its surface.
pub(crate) const QUIET: f32 = 0.74;
/// Opacity of a hairline drawn in the text color.
const LINE: f32 = 0.24;

const STEPS: usize = 7;
const DEEPEST: u8 = 6;
/// Tones of the mark, outermost first.
pub(crate) const MARK_STEPS: usize = 6;

/// How far a surface has sunk: 0 is today, 6 is a year or more ago.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Depth(u8);

impl Depth {
    pub(crate) const NOW: Self = Self(0);

    /// The depth of something last touched `days` local calendar days ago.
    pub(crate) fn from_days(days: i64) -> Self {
        Self(match days {
            ..=0 => 0,
            1 => 1,
            2..=6 => 2,
            7..=29 => 3,
            30..=89 => 4,
            90..=364 => 5,
            _ => DEEPEST,
        })
    }

    fn step(self) -> usize {
        usize::from(self.0)
    }
}

/// Night or day: which of the user's two choices is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Night,
    Day,
}

impl Mode {
    pub(crate) const ALL: [Self; 2] = [Self::Night, Self::Day];

    /// The mode the system appearance asks for.
    pub(crate) fn of(window: &Window) -> Self {
        #[cfg(all(test, feature = "snapshots"))]
        if let Some(mode) = forced::get() {
            return mode;
        }
        match window.appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Night,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Day,
        }
    }

    fn spec(self) -> &'static Spec {
        match self {
            Self::Night => &NIGHT,
            Self::Day => &DAY,
        }
    }
}

/// The snapshot tests' override of [`Mode::of`]: GPUI's headless windows are
/// always light, so the tests pin the mode for their thread. Compiled only
/// into `--features snapshots` test builds.
#[cfg(all(test, feature = "snapshots"))]
pub(crate) mod forced {
    use std::cell::Cell;

    use super::Mode;

    thread_local! {
        static MODE: Cell<Option<Mode>> = const { Cell::new(None) };
    }

    /// Pins the mode on this thread; `None` follows the window again.
    pub(crate) fn set(mode: Option<Mode>) {
        MODE.set(mode);
    }

    pub(super) fn get() -> Option<Mode> {
        MODE.get()
    }
}

/// An OKLCH lightness and chroma; the hue comes from the user's choice.
#[derive(Clone, Copy, Debug)]
struct Tone {
    lightness: f32,
    chroma: f32,
}

const fn tone(lightness: f32, chroma: f32) -> Tone {
    Tone { lightness, chroma }
}

/// The fixed part of a mode: everything except hue and tint.
#[derive(Debug)]
struct Spec {
    surfaces: [Tone; STEPS],
    text: Tone,
    on_text: Tone,
    /// The first step whose surface takes `on_text` instead of `text`.
    flip: usize,
    /// Opacity of the white wash behind an incoming message.
    incoming: f32,
}

const NIGHT: Spec = Spec {
    surfaces: [
        tone(0.300, 0.045),
        tone(0.275, 0.044),
        tone(0.250, 0.042),
        tone(0.225, 0.040),
        tone(0.200, 0.037),
        tone(0.175, 0.034),
        tone(0.150, 0.030),
    ],
    text: tone(0.93, 0.015),
    on_text: tone(0.18, 0.03),
    flip: STEPS,
    incoming: 0.08,
};

const DAY: Spec = Spec {
    surfaces: [
        tone(0.970, 0.006),
        tone(0.925, 0.012),
        tone(0.860, 0.020),
        tone(0.790, 0.028),
        tone(0.430, 0.045),
        tone(0.355, 0.045),
        tone(0.280, 0.040),
    ],
    text: tone(0.24, 0.03),
    on_text: tone(0.97, 0.006),
    flip: 4,
    incoming: 0.80,
};

/// The mark and everything public use these six tones, surface to floor.
const MARK: [Tone; MARK_STEPS] = [
    tone(0.93, 0.015),
    tone(0.74, 0.035),
    tone(0.56, 0.050),
    tone(0.40, 0.050),
    tone(0.27, 0.045),
    tone(0.17, 0.032),
];

/// The tone a choice is previewed in: mid water.
const SWATCH: Tone = tone(0.56, 0.050);

/// A hidden spoiler: an opaque block midway between the two text colors, so
/// it shows on an incoming message and on the user's own alike.
const HIDDEN: Tone = tone(0.60, 0.030);

/// The colors of one mode, for one choice of hue and tint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Theme {
    surfaces: [Rgba; STEPS],
    flip: usize,
    /// Text, and the fill of the user's own messages and of buttons: the
    /// strongest contrast on screen.
    pub(crate) text: Rgba,
    /// Text on a `text` fill.
    pub(crate) on_text: Rgba,
    /// The fill of an incoming message and of input fields; `text` reads on
    /// it over every surface.
    pub(crate) incoming: Rgba,
    /// The mark's tones, outermost first.
    pub(crate) mark: [Rgba; MARK_STEPS],
    /// Text and background of a hidden spoiler (the same, opaque color).
    pub(crate) hidden: Rgba,
}

impl Theme {
    pub(crate) fn new(mode: Mode, choice: Choice) -> Self {
        let spec = mode.spec();
        let color = |t: Tone| oklch(t, choice.hue.degrees(), choice.tint.factor());
        Self {
            surfaces: spec.surfaces.map(color),
            flip: spec.flip,
            text: color(spec.text),
            on_text: color(spec.on_text),
            incoming: Rgba {
                r: 1.,
                g: 1.,
                b: 1.,
                a: spec.incoming,
            },
            mark: MARK.map(color),
            hidden: color(HIDDEN),
        }
    }

    /// The surface at `depth`.
    pub(crate) fn surface(&self, depth: Depth) -> Rgba {
        self.surfaces.get(depth.step()).copied().unwrap_or_default()
    }

    /// The text color that reads on the surface at `depth`.
    pub(crate) fn text_on(&self, depth: Depth) -> Rgba {
        if depth.step() >= self.flip {
            self.on_text
        } else {
            self.text
        }
    }
}

/// How a choice looks, for the controls that offer it. Never below the
/// default tint, so hues stay tellable apart when the user has chosen none.
pub(crate) fn swatch(choice: Choice) -> Rgba {
    let tint = choice.tint.factor().max(Tint::DEFAULT.factor());
    oklch(SWATCH, choice.hue.degrees(), tint)
}

/// How a tint amount looks at the chosen hue, for the controls that offer it.
pub(crate) fn tint_swatch(choice: Choice) -> Rgba {
    oklch(SWATCH, choice.hue.degrees(), choice.tint.factor())
}

/// The user's choices and the two themes built from them; an app global.
pub(crate) struct Themes {
    settings: Settings,
    night: Theme,
    day: Theme,
    /// Where the choice is kept; `None` keeps it for this run only.
    store: Option<PathBuf>,
    /// The save in flight. Each save waits for the one before, so the last
    /// choice made is the last one written.
    saving: Option<Task<()>>,
}

impl Global for Themes {}

impl Themes {
    /// Themes for `settings`, saved to `store` when they change.
    pub(crate) fn new(settings: Settings, store: Option<PathBuf>) -> Self {
        Self {
            settings,
            night: Theme::new(Mode::Night, settings.night),
            day: Theme::new(Mode::Day, settings.day),
            store,
            saving: None,
        }
    }

    /// Installs the saved choice, or the defaults. Call once at startup.
    pub(crate) fn init(cx: &mut App) {
        let store = settings::default_path();
        let settings = store
            .as_deref()
            .map_or_else(Settings::default, settings::load);
        cx.set_global(Self::new(settings, store));
    }

    /// The theme for the window's current system appearance. Views that can
    /// render before [`Themes::init`] (tests) get the defaults.
    pub(crate) fn of(window: &Window, cx: &App) -> Theme {
        let mode = Mode::of(window);
        cx.try_global::<Self>().map_or_else(
            || Theme::new(mode, Settings::default().get(mode)),
            |t| t.theme(mode),
        )
    }

    /// The user's choices, or the defaults before [`Themes::init`].
    pub(crate) fn settings(cx: &App) -> Settings {
        cx.try_global::<Self>()
            .map_or_else(Settings::default, |t| t.settings)
    }

    fn theme(&self, mode: Mode) -> Theme {
        match mode {
            Mode::Night => self.night,
            Mode::Day => self.day,
        }
    }

    /// Replaces the choices, repaints every window and saves.
    pub(crate) fn set(settings: Settings, cx: &mut App) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::new(Settings::default(), None));
        }
        cx.update_global::<Self, _>(|themes, cx| {
            if themes.settings == settings {
                return;
            }
            let saving = themes.saving.take();
            *themes = Self::new(settings, themes.store.take());
            themes.saving = saving;
            themes.save(cx);
        });
        cx.refresh_windows();
    }

    fn save(&mut self, cx: &App) {
        let Some(path) = self.store.clone() else {
            return;
        };
        let (settings, before) = (self.settings, self.saving.take());
        self.saving = Some(cx.background_spawn(async move {
            if let Some(before) = before {
                before.await;
            }
            if let Err(error) = settings::save(&path, settings) {
                // The path holds the user's name; the kind is enough.
                tracing::warn!(kind = ?error.kind(), "couldn't save the theme choice");
            }
        }));
    }
}

/// `color` at quiet-text strength.
pub(crate) fn quiet(color: Rgba) -> Rgba {
    faded(color, QUIET)
}

/// A hairline in `color`.
pub(crate) fn line(color: Rgba) -> Rgba {
    faded(color, LINE)
}

/// `color` with its opacity scaled by `factor`.
pub(crate) fn faded(color: Rgba, factor: f32) -> Rgba {
    Rgba {
        a: color.a * factor,
        ..color
    }
}

/// OKLCH to sRGB (Björn Ottosson's Oklab matrices), clamped to the gamut.
fn oklch(tone: Tone, hue_degrees: f32, tint: f32) -> Rgba {
    let (sin, cos) = hue_degrees.to_radians().sin_cos();
    let chroma = tone.chroma * tint;
    let (green_red, blue_yellow) = (chroma * cos, chroma * sin);
    let light = tone.lightness;
    let long = (light + 0.396_337_78 * green_red + 0.215_803_76 * blue_yellow).powi(3);
    let medium = (light - 0.105_561_346 * green_red - 0.063_854_17 * blue_yellow).powi(3);
    let short = (light - 0.089_484_18 * green_red - 1.291_485_5 * blue_yellow).powi(3);
    Rgba {
        r: encode(4.076_741_7 * long - 3.307_711_6 * medium + 0.230_969_94 * short),
        g: encode(-1.268_438 * long + 2.609_757_4 * medium - 0.341_319_38 * short),
        b: encode(-0.004_196_086_4 * long - 0.703_418_6 * medium + 1.707_614_7 * short),
        a: 1.,
    }
}

/// Linear light to an sRGB component.
fn encode(linear: f32) -> f32 {
    let linear = linear.clamp(0., 1.);
    if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1. / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use gpui::rgb;

    use super::*;

    /// WCAG AA for body text.
    const AA: f32 = 4.5;

    fn default_theme(mode: Mode) -> Theme {
        Theme::new(mode, Settings::default().get(mode))
    }

    fn luminance(color: Rgba) -> f32 {
        let linear = |c: f32| {
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    fn contrast(a: Rgba, b: Rgba) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// `top` painted over an opaque `under`.
    fn over(top: Rgba, under: Rgba) -> Rgba {
        let mix = |t: f32, u: f32| t * top.a + u * (1. - top.a);
        Rgba {
            r: mix(top.r, under.r),
            g: mix(top.g, under.g),
            b: mix(top.b, under.b),
            a: 1.,
        }
    }

    fn close(a: Rgba, b: Rgba) -> bool {
        let near = |x: f32, y: f32| (x - y).abs() <= 1.5 / 255.;
        near(a.r, b.r) && near(a.g, b.g) && near(a.b, b.b)
    }

    fn depths() -> impl Iterator<Item = Depth> {
        (0..=DEEPEST).map(Depth)
    }

    /// Every way text can sit on this theme: on a surface, in an incoming
    /// message over that surface, and in the user's own message; each at full
    /// and at quiet strength.
    fn weakest_pair(theme: &Theme) -> f32 {
        let pair = |ink: Rgba, ground: Rgba| {
            contrast(ink, ground).min(contrast(over(quiet(ink), ground), ground))
        };
        depths()
            .flat_map(|d| {
                let surface = theme.surface(d);
                let wash = over(theme.incoming, surface);
                [pair(theme.text_on(d), surface), pair(theme.text, wash)]
            })
            .chain([pair(theme.on_text, theme.text)])
            .fold(f32::INFINITY, f32::min)
    }

    #[test]
    fn defaults_match_the_brand_sheet() {
        let night = default_theme(Mode::Night);
        assert!(close(night.surface(Depth::NOW), rgb(0x0c_333c)));
        assert!(close(night.surface(Depth(DEEPEST)), rgb(0x00_0e13)));
        assert!(close(night.text, rgb(0xdd_ebee)));
        let day = default_theme(Mode::Day);
        assert!(close(day.surface(Depth::NOW), rgb(0xf1_f6f7)));
        assert!(close(day.surface(Depth(DEEPEST)), rgb(0x0b_2f31)));
        assert!(close(day.text, rgb(0x0b_2425)));
        // The mark at the night default is the public ramp, surface to floor.
        assert!(close(night.mark[0], rgb(0xdd_ebee)));
        assert!(close(night.mark[5], rgb(0x00_1318)));
    }

    /// The property the picker relies on: lightness alone decides contrast,
    /// so nothing on offer can make text unreadable.
    #[test]
    fn every_text_pair_reads_for_every_choice_on_offer() {
        for mode in Mode::ALL {
            for hue in Hue::all() {
                for tint in Tint::all() {
                    let weakest = weakest_pair(&Theme::new(mode, Choice { hue, tint }));
                    assert!(weakest >= AA, "{mode:?} {hue:?} {tint:?}: {weakest}");
                }
            }
        }
    }

    #[test]
    fn old_things_sink() {
        for mode in Mode::ALL {
            let theme = default_theme(mode);
            let tones: Vec<f32> = depths().map(|d| luminance(theme.surface(d))).collect();
            assert!(tones.is_sorted_by(|a, b| a > b), "{mode:?}: {tones:?}");
        }
    }

    #[test]
    fn depth_follows_age_in_days() {
        let steps =
            |days: &[i64]| -> Vec<u8> { days.iter().map(|d| Depth::from_days(*d).0).collect() };
        assert_eq!(steps(&[-3, 0, 1, 2, 6, 7, 29]), [0, 0, 1, 2, 2, 3, 3]);
        assert_eq!(steps(&[30, 89, 90, 364, 365, i64::MAX]), [4, 4, 5, 5, 6, 6]);
    }

    #[test]
    fn swatches_stay_tellable_apart_without_tint() {
        let none = |hue| Choice {
            hue,
            tint: Tint::all().next().unwrap(),
        };
        let mut hues = Hue::all();
        let (a, b) = (hues.next().unwrap(), hues.nth(11).unwrap());
        assert_ne!(swatch(none(a)), swatch(none(b)));
        assert_eq!(
            tint_swatch(none(a)),
            tint_swatch(none(b)),
            "no tint is grey"
        );
    }
}
