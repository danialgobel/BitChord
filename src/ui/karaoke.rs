//! Synced lyrics, ported from Better Lyrics' renderer (`@braccato/core` 1.16: engine.js,
//! inject.js, instrumental.js, lyrics.css, variables.css) to egui.
//!
//! What is the same as the extension, by construction:
//! - Timing: the clock is shown 150 ms ahead for word-synced lyrics and 115 ms for
//!   line-synced ones, and the view scrolls 0.5 s ahead of that.
//! - Words: every provider part is split on whitespace with its time pro-rated by
//!   characters; words between spaces form groups that never wrap unless longer than 20
//!   characters.
//! - Karaoke: each word has a lit copy over the dim one, revealed by a linear gradient
//!   (soft edge one tenth of the word wide) that runs from -0.2..-0.1 to 1.4..1.5 over 1.6×
//!   the word's duration, 10 % ahead of it. With letters split (the default "letter
//!   wave"), the gradient advances letter by letter in index space, each letter floats up
//!   as it is lit, and the word group gives a small sideways pop.
//! - Glow: `drop-shadow(0 0 0.8rem glow-color)` on the lit copy, decaying to nothing over
//!   max(1.2×, 1.2 s) with `ease`: a Gaussian blur of the lit glyph pixels only (never a box).
//! - Lines: scale 0.95 → 1 over 166 ms when they start, back when they end; lit words
//!   fade back to dim over 0.5 s once the next line starts.
//! - Scrolling: the view jumps to the new position and every line eases from where it was,
//!   lines further down taking a little longer (cubic-bezier(0.86, 0, 0.2, 1), 750 ms +).

use std::collections::HashMap;
use std::sync::Arc;

use egui::epaint::Vertex;
use egui::epaint::text::FontsView;
use egui::layers::ShapeIdx;
use egui::{Color32, ColorImage, FontId, Galley, Mesh, Pos2, Rect, Shape, TextureHandle, TextureId, TextureOptions, Vec2, pos2, vec2};

use crate::lyrics::{Line, Lyrics, Part, Sync};
use crate::preset::{BlurScope, Fill, LyricStyle, ThemeCss, Weight};
use crate::theme;
use crate::ui::lyric_prefs::LyricAnimMode;

// ------------------------------------------------------------------ easing

/// A CSS `cubic-bezier()` timing function.
#[derive(Clone, Copy, Debug)]
pub struct Bezier(pub f32, pub f32, pub f32, pub f32);

impl Bezier {
    pub fn at(self, x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        let Bezier(x1, y1, x2, y2) = self;
        let curve = |t: f32, a: f32, b: f32| {
            let u = 1.0 - t;
            3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
        };
        let slope = |t: f32, a: f32, b: f32| {
            let u = 1.0 - t;
            3.0 * u * u * a + 6.0 * u * t * (b - a) + 3.0 * t * t * (1.0 - b)
        };
        let mut t = x;
        for _ in 0..8 {
            let e = curve(t, x1, x2) - x;
            if e.abs() < 1e-6 {
                return curve(t, y1, y2);
            }
            let d = slope(t, x1, x2);
            if d.abs() < 1e-6 {
                break;
            }
            t = (t - e / d).clamp(0.0, 1.0);
        }
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        t = x;
        for _ in 0..40 {
            let v = curve(t, x1, x2);
            if (v - x).abs() < 1e-6 {
                break;
            }
            if v < x {
                lo = t;
            } else {
                hi = t;
            }
            t = (lo + hi) * 0.5;
        }
        curve(t, y1, y2)
    }
}

pub const EASE: Bezier = Bezier(0.25, 0.1, 0.25, 1.0);
pub const EASE_IN: Bezier = Bezier(0.42, 0.0, 1.0, 1.0);
pub const EASE_OUT: Bezier = Bezier(0.0, 0.0, 0.58, 1.0);
pub const EASE_IN_OUT: Bezier = Bezier(0.42, 0.0, 0.58, 1.0);
/// `--blyrics-lyric-scroll-timing-function`.
pub const SCROLL_EASE: Bezier = Bezier(0.86, 0.0, 0.2, 1.0);
/// `blyrics-decoration-in`.
const DECO_EASE: Bezier = Bezier(0.25, 1.0, 0.5, 1.0);

// ------------------------------------------------------------------ theme values

/// `--blyrics-richsync-timing-offset` / `--blyrics-timing-offset`.
pub const RICHSYNC_OFFSET: f64 = 0.150;
pub const LINE_OFFSET: f64 = 0.115;
/// `--blyrics-scroll-timing-offset`.
const SCROLL_OFFSET: f64 = 0.5;
/// `blyrics-early-scroll-consider-s`.
const EARLY_SCROLL: f64 = 0.54;
/// `blyrics-lyric-ending-threshold-s`.
const ENDING_THRESHOLD: f64 = 0.5;
const TIME_JUMP: f64 = 0.5;
/// `blyrics-swipe-lead-ratio`, `blyrics-swipe-duration-ratio` and the swipe ramp.
const SWIPE_LEAD: f64 = 0.1;
const SWIPE_DURATION: f64 = 1.6;
const SWIPE_START: (f32, f32) = (-0.2, 1.4);
const SWIPE_END: (f32, f32) = (-0.1, 1.5);
const GLOW_RATIO: f64 = 1.2;
const GLOW_MIN: f64 = 1.2;
const FADE_IN: f64 = 0.33;
const FADE_OUT: f64 = 0.5;
const SCALE_DURATION: f64 = 0.166;
const WOBBLE_DURATION: f64 = 1.0;
const LETTER_WAVE_CREST: f32 = 0.4;
/// `blyrics-long-word-threshold` (ms → s): held words swell their letters.
const LONG_WORD: f64 = 1.5;
/// `blyrics-line-synced-animation-delay`.
const LINE_SYNC_DELAY: f64 = 0.05;
/// Word groups longer than twice `blyrics-long-word-wrap-threshold` may break.
const LONG_GROUP_CHARS: usize = 20;
const USER_SCROLL_RESUME: f64 = 25.0;
const DECO_ENTRY: f64 = 0.35;

/// Resolved look for one frame: the extension's defaults, adjusted by the preset.
#[derive(Clone)]
struct Cfg {
    font: String,
    weight: Weight,
    size: f32,
    active: Color32,
    /// Alpha of dim text on the line being sung.
    inactive_alpha: f32,
    glow: Color32,
    glow_rem: f32,
    scale: f32,
    active_scale: f32,
    padding: f32,
    target_ratio: f32,
    centered: bool,
    fill: Fill,
    wave_crest: f32,
    wave_settle: f32,
    emphasis: f32,
    upcoming_alpha: f32,
    falloff: f32,
    min_alpha: f32,
    past_alpha: Option<f32>,
    blur: f32,
    blur_scope: BlurScope,
    /// One CSS rem in points (YouTube Music's 10 px when the theme sizes in rem).
    rem: f32,
    /// `.blyrics-background-lyric` font size (em).
    bg_em: f32,
    /// `.blyrics-container` inline padding (start, end).
    pad: (f32, f32),
    /// Word wobble on (themes can set its transforms to identity).
    wobble: bool,
    wave_duration: f64,
    /// `--blyrics-glow-color` alpha on the container and on held words.
    glow_alpha: f32,
    long_glow_alpha: f32,
    css: Option<ThemeCss>,
}

/// `GLOW_INVISIBLE_ALPHA`: a glow colour this faint renders nothing and is skipped.
const GLOW_INVISIBLE: f32 = 0.5 / 255.0;

impl Cfg {
    /// `width`: the lyrics panel; `viewport`: the window (the theme's media queries).
    /// The most a line is scaled up: while sung, or under a theme's hover scale.
    fn max_scale(&self) -> f32 {
        self.active_scale.max(self.css.as_ref().and_then(|c| c.hover_scale).unwrap_or(1.0)).max(1.0)
    }

    fn new(s: &LyricStyle, accent: Color32, width: f32, viewport: f32, centered: bool, scale: f32) -> Self {
        let css = s.css.clone();
        // Font size and rem: a theme's own `--blyrics-font-size` steps in YouTube Music's
        // 10 px rem; otherwise 48 pt scaled by the preset with the extension's proportions.
        let (mut size, mut rem) = match css.as_ref().filter(|c| !c.font_rem.is_empty()) {
            Some(c) => {
                let r = c.font_rem.iter().filter(|st| viewport >= st[0]).map(|st| st[1]).next_back().unwrap_or(c.font_rem[0][1]);
                (r * 10.0, 10.0)
            }
            None => {
                let size = 48.0 * s.size.clamp(0.6, 1.6);
                (size, size / 3.0)
            }
        };
        // Kept to about a dozen CJK characters per row on narrow windows.
        let cap = (width / 10.5).max(22.0);
        if size > cap {
            rem *= cap / size;
            size = cap;
        }
        // The listener's text size goes on top, everything in rem along with it
        // (romanization, translation, spacing), so the theme keeps its proportions.
        // Whole points: every new size fills the font atlas again (it never shrinks),
        // and resizing the window would otherwise make one per pixel of width.
        let rounded = (size * scale).round().max(1.0);
        rem *= rounded / size;
        size = rounded;
        let glow = s.glow.clamp(0.0, 1.0);
        let weight = match css.as_ref().and_then(|c| c.font_weight) {
            Some(w) if w >= 700 => Weight::Bold,
            Some(w) if w >= 550 => Weight::Semibold,
            Some(_) => Weight::Regular,
            None => s.weight,
        };
        let wave = css.as_ref().and_then(|c| c.letter_wave).unwrap_or([0.05, 0.02, 0.9, 1.11]);
        // The app's own look: a letter rises by the preset's lift and stays there, so
        // the words already sung sit above the line being sung.
        let (crest, settle) = if css.is_some() { (wave[0], wave[1]) } else { (s.lift.clamp(0.0, 8.0) / size, s.lift.clamp(0.0, 8.0) / size) };
        let (glow_alpha, long_glow_alpha) = match css.as_ref().and_then(|c| c.glow_alpha) {
            Some(a) => (a, css.as_ref().and_then(|c| c.long_word_glow_alpha).unwrap_or(a)),
            // A look without a glow setting of its own glows only as much as its preset asks:
            // the default look has none.
            None => (glow, glow),
        };
        let pad = css.as_ref().and_then(|c| c.padding_inline).map_or((0.0, 0.0), |p| (p[0] * rem / 10.0, p[1] * rem / 10.0));
        Self {
            font: theme::effective_font(&s.font),
            weight,
            size,
            active: crate::preset::paint(&s.sung_color, accent),
            inactive_alpha: s.active_alpha,
            glow: crate::preset::paint(&s.glow_color, accent),
            glow_rem: if css.is_some() { 0.8 } else { 0.8 * (1.0 + glow) },
            scale: 0.95,
            active_scale: s.active_scale.max(1.0),
            padding: rem * 2.0 * (s.line_gap / 0.6).clamp(0.2, 3.0),
            target_ratio: s.anchor.clamp(0.1, 0.7),
            centered,
            fill: s.fill,
            wave_crest: crest,
            wave_settle: settle,
            emphasis: wave[3],
            upcoming_alpha: s.inactive_alpha,
            falloff: s.falloff,
            min_alpha: s.min_alpha,
            past_alpha: s.past_alpha,
            blur: s.blur,
            blur_scope: s.blur_scope,
            rem,
            bg_em: css.as_ref().and_then(|c| c.background_em).unwrap_or(0.75),
            pad,
            // A theme opts out of the sideways pop; the app's own look doesn't have it.
            wobble: css.as_ref().is_some_and(|c| !c.no_wobble),
            wave_duration: wave[2] as f64,
            glow_alpha,
            long_glow_alpha,
            css,
        }
    }
    fn rem(&self) -> f32 {
        self.rem
    }
    // Each takes the text it sets, so a line in a script the font lacks is all Inter.
    fn main_font(&self, text: &str) -> FontId {
        theme::lyric_font_for(&self.font, self.weight, self.size, text)
    }
    fn bg_font(&self, text: &str) -> FontId {
        theme::lyric_font_for(&self.font, Weight::Semibold, self.size * self.bg_em, text)
    }
    fn roman_font(&self, text: &str) -> FontId {
        theme::lyric_font_for(&self.font, Weight::Semibold, self.rem() * 2.0 / 1.25, text)
    }
    fn trans_font(&self, text: &str) -> FontId {
        theme::lyric_font_for(&self.font, Weight::Semibold, self.rem() * 2.0, text)
    }
}

// ------------------------------------------------------------------ layout

/// One animated word (`.blyrics--word`), relative to its line's content box.
#[derive(Clone)]
struct Word {
    text: String,
    font: FontId,
    start: f64,
    dur: f64,
    /// Zero-duration (line-synced) word: fades in instead of sweeping.
    line_synced: bool,
    bg: bool,
    /// Held longer than `LONG_WORD`: letters swell as they crest.
    long: bool,
    pos: Pos2,
    w: f32,
    h: f32,
    group: usize,
    letters: bool,
    /// Right-to-left letters: lit from the right.
    rtl: bool,
}

/// A word group (`.blyrics-word-group`): what wobbles, driven by its first word.
#[derive(Clone)]
struct Group {
    first: usize,
    words: std::ops::Range<usize>,
    /// Inline (long) groups can't be transformed.
    wobbles: bool,
}

#[derive(Clone)]
enum DecoBody {
    Timed { words: Vec<Word>, groups: Vec<Group> },
    Text(String, FontId),
}

#[derive(Clone)]
struct Deco {
    body: DecoBody,
    /// Box relative to the line's content origin (the pill, or the translation's text box).
    rect: Rect,
    pill: bool,
}

#[derive(Clone)]
struct Block {
    /// Index into the lyrics' lines (`None` for the credits).
    line: Option<usize>,
    start: f64,
    dur: f64,
    instrumental: bool,
    /// `v2`/`v3` sing on the right, `v1000` (everyone) in the middle.
    align: Align,
    top: f32,
    height: f32,
    words: Vec<Word>,
    groups: Vec<Group>,
    decos: Vec<Deco>,
    /// Songwriter credits text.
    credits: Option<(String, FontId)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Right,
    Center,
}

struct Layout {
    key: u64,
    width: f32,
    blocks: Vec<Block>,
    pad_top: f32,
    content_h: f32,
    richsync: bool,
}

/// A whitespace run or a piece of a part (`splitPartIntoTokens`).
struct Tok {
    text: String,
    start: f64,
    dur: f64,
    bg: bool,
    space: bool,
}

fn split_part(p: &Part) -> Vec<Tok> {
    let mut chunks: Vec<(String, bool)> = Vec::new();
    for c in p.text.chars() {
        let ws = c.is_whitespace();
        match chunks.last_mut() {
            Some((s, w)) if *w == ws => s.push(c),
            _ => chunks.push((c.to_string(), ws)),
        }
    }
    let total: usize = chunks.iter().filter(|(_, w)| !*w).map(|(s, _)| s.chars().count()).sum();
    let mut cursor = 0usize;
    chunks
        .into_iter()
        .map(|(text, space)| {
            if space {
                return Tok { text, start: 0.0, dur: 0.0, bg: p.bg, space: true };
            }
            let n = text.chars().count();
            let (start_ms, end_ms) = if total == 0 {
                (p.start as f64, p.start as f64)
            } else {
                (
                    p.start as f64 + (p.dur as f64 * cursor as f64 / total as f64).round(),
                    p.start as f64 + (p.dur as f64 * (cursor + n) as f64 / total as f64).round(),
                )
            };
            cursor += n;
            Tok { text, start: start_ms / 1000.0, dur: (end_ms - start_ms) / 1000.0, bg: p.bg, space: false }
        })
        .collect()
}

/// `buildLineSyncedParts`: a line without timed parts becomes zero-duration words, each
/// 50 ms after the one before.
fn line_synced_parts(l: &Line) -> Vec<Part> {
    let mut out = Vec::new();
    let mut word = 0;
    let mut chunks: Vec<(String, bool)> = Vec::new();
    for c in l.text.chars() {
        let ws = c.is_whitespace();
        match chunks.last_mut() {
            Some((s, w)) if *w == ws => s.push(c),
            _ => chunks.push((c.to_string(), ws)),
        }
    }
    for (text, ws) in chunks {
        let start = l.start + (word as f64 * LINE_SYNC_DELAY * 1000.0) as i32;
        out.push(Part { start, dur: 0, text, bg: false });
        if !ws {
            word += 1;
        }
    }
    out
}

/// Joining scripts (Arabic and friends) keep whole-word sweeps, as in the extension.
fn joining_script(s: &str) -> bool {
    s.chars().any(|c| matches!(c as u32, 0x0600..=0x06FF | 0x0700..=0x074F | 0x0750..=0x077F | 0x0780..=0x07BF | 0x0840..=0x085F | 0x08A0..=0x08FF | 0x1800..=0x18AF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF))
}

struct Flow {
    words: Vec<Word>,
    groups: Vec<Group>,
    height: f32,
    width: f32,
}

/// Lay out tokens the way inline formatting does it in the extension: groups between
/// spaces don't break (unless long), spaces do.
fn text_size(fonts: &mut FontsView<'_>, s: &str, font: &FontId) -> Vec2 {
    fonts.layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE).size()
}

/// `rtl`: the line reads right to left, so each row is laid out from the right.
fn flow(fonts: &mut FontsView<'_>, toks: Vec<Tok>, font: &FontId, max_w: f32, line_h: f32, align: Align, rtl: bool) -> Flow {
    // `groupTokensByWord`
    enum Item {
        Space,
        Group(Vec<Tok>),
    }
    let mut items: Vec<Item> = Vec::new();
    // A single piece wider than the line (a line-synced CJK phrase arrives as one, with no
    // spaces to break at) is split into its characters, sharing its time, so it can wrap.
    let toks = toks.into_iter().flat_map(|t| {
        // Not right-to-left words either: their letters, laid out one by one, would read backwards.
        if t.space || joining_script(&t.text) || crate::bidi::has_rtl(&t.text) || t.text.chars().count() < 2 || text_size(fonts, &t.text, font).x <= max_w {
            return vec![t];
        }
        let n = t.text.chars().count();
        t.text
            .chars()
            .enumerate()
            .map(|(i, c)| Tok { text: c.to_string(), start: t.start + t.dur * i as f64 / n as f64, dur: t.dur / n as f64, bg: t.bg, space: false })
            .collect()
    });
    for t in toks {
        if t.space {
            items.push(Item::Space);
            continue;
        }
        match items.last_mut() {
            Some(Item::Group(g)) if g[0].bg == t.bg => g.push(t),
            _ => items.push(Item::Group(vec![t])),
        }
    }
    let space_w = text_size(fonts, " ", font).x.max(font.size * 0.25);
    let mut words: Vec<Word> = Vec::new();
    let mut groups: Vec<Group> = Vec::new();
    let mut rows: Vec<(usize, f32)> = vec![(0, 0.0)]; // (first word index, width)
    let (mut x, mut row) = (0.0f32, 0usize);
    let mut pending_space = false;
    for item in items {
        match item {
            Item::Space => pending_space = !words.is_empty(),
            Item::Group(toks) => {
                let text: String = toks.iter().map(|t| t.text.as_str()).collect();
                let long = text.chars().count() > LONG_GROUP_CHARS;
                let rtl_group = crate::bidi::has_rtl(&text);
                let letters = !joining_script(&text) && !rtl_group;
                let sizes: Vec<Vec2> = toks.iter().map(|t| text_size(fonts, &t.text, font)).collect();
                let w: f32 = sizes.iter().map(|s| s.x).sum();
                let sp = if pending_space { space_w } else { 0.0 };
                pending_space = false;
                // A group that can't fit even on a row of its own breaks between its parts
                // rather than running off the side (the extension lets it overflow).
                let breakable = long || w > max_w;
                if x > 0.0 && x + sp + w > max_w && !breakable {
                    row += 1;
                    x = 0.0;
                    rows.push((words.len(), 0.0));
                } else {
                    x += sp;
                }
                let gi = groups.len();
                let first = words.len();
                for (t, size) in toks.iter().zip(sizes) {
                    if breakable && x > 0.0 && x + size.x > max_w {
                        row += 1;
                        x = 0.0;
                        rows.push((words.len(), 0.0));
                    }
                    words.push(Word {
                        text: t.text.clone(),
                        font: font.clone(),
                        start: t.start,
                        dur: t.dur,
                        line_synced: t.dur <= 0.0,
                        bg: t.bg,
                        long: t.dur > LONG_WORD,
                        pos: pos2(x, row as f32 * line_h + (line_h - size.y) * 0.5),
                        w: size.x,
                        h: size.y,
                        group: gi,
                        letters,
                        rtl: crate::bidi::has_rtl(&t.text),
                    });
                    x += size.x;
                    rows[row].1 = x;
                }
                groups.push(Group { first, words: first..words.len(), wobbles: !long });
            }
        }
    }
    // Right to left: words go from the right of each row, except runs of left-to-right
    // ones (Latin, numbers), which keep their order; in a left-to-right line, the other
    // way round.
    for (r, &(first, rw)) in rows.iter().enumerate() {
        let end = rows.get(r + 1).map_or(words.len(), |n| n.0);
        let row = &mut words[first..end];
        if rtl {
            for w in row.iter_mut() {
                w.pos.x = rw - w.pos.x - w.w;
            }
        }
        let mut i = 0;
        while i < row.len() {
            let mut j = i;
            while j < row.len() && row[j].rtl != rtl {
                j += 1;
            }
            if j > i + 1 {
                let lo = row[i..j].iter().map(|w| w.pos.x).fold(f32::INFINITY, f32::min);
                let hi = row[i..j].iter().map(|w| w.pos.x + w.w).fold(f32::NEG_INFINITY, f32::max);
                for w in &mut row[i..j] {
                    w.pos.x = lo + hi - w.pos.x - w.w;
                }
            }
            i = j.max(i + 1);
        }
    }
    // Align rows to the right or centre.
    if align != Align::Left {
        for (r, &(first, rw)) in rows.iter().enumerate() {
            let end = rows.get(r + 1).map_or(words.len(), |n| n.0);
            let shift = if align == Align::Right { max_w - rw } else { (max_w - rw) * 0.5 };
            for w in &mut words[first..end] {
                w.pos.x += shift.max(0.0);
            }
        }
    }
    let width = rows.iter().map(|r| r.1).fold(0.0, f32::max);
    let height = if words.is_empty() { 0.0 } else { (row + 1) as f32 * line_h };
    Flow { words, groups, height, width }
}

/// Romanization for a line: timed (from the lyrics) or plain text.
pub enum Roman<'a> {
    Timed(&'a [Part], &'a str),
    Text(&'a str),
}

fn hash<T: std::hash::Hash>(v: T) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

#[allow(clippy::too_many_arguments)]
fn build_block(fonts: &mut FontsView<'_>, cfg: &Cfg, l: &Line, index: usize, width: f32, roman: Option<Roman>, trans: Option<&str>, agent: Option<&str>) -> Block {
    let em = cfg.size;
    let rtl = crate::bidi::rtl_base(&l.text);
    let align = match agent {
        Some("v2" | "v3") => Align::Right,
        Some("v1000") => Align::Center,
        _ if cfg.centered => Align::Center,
        _ => Align::Left,
    };
    // Right-to-left lines start on the right (and the second singer's on the left).
    let align = match align {
        Align::Left if rtl => Align::Right,
        Align::Right if rtl => Align::Left,
        a => a,
    };
    // Wrapped to fit at the line's largest scale (sung, or a theme's hover), which grows
    // it about its aligned edge: wrapped at full width, a sung line ran off the side.
    let full_w = (width - em * 0.5).max(40.0);
    let content_w = (full_w / cfg.max_scale()).max(40.0);
    let line_h = em * 1.333;
    let mut y = cfg.padding;
    let mut block = Block {
        line: Some(index),
        start: l.start as f64 / 1000.0,
        dur: l.dur.max(0) as f64 / 1000.0,
        instrumental: l.instrumental,
        align,
        top: 0.0,
        height: 0.0,
        words: vec![],
        groups: vec![],
        decos: vec![],
        credits: None,
    };
    if l.instrumental {
        block.height = cfg.padding * 2.0 + line_h;
        return block;
    }
    let parts = if l.parts.is_empty() { line_synced_parts(l) } else { l.parts.clone() };
    let toks: Vec<Tok> = parts.iter().flat_map(split_part).collect();
    let (main, bg): (Vec<Tok>, Vec<Tok>) = toks.into_iter().partition(|t| !t.bg);
    let joined = |toks: &[Tok]| toks.iter().map(|t| t.text.as_str()).collect::<String>();
    let font = cfg.main_font(&joined(&main));
    let f = flow(fonts, main, &font, content_w, line_h, align, rtl);
    for w in f.words {
        block.words.push(Word { pos: w.pos + vec2(0.0, y), ..w });
    }
    block.groups = f.groups;
    y += f.height;
    if bg.iter().any(|t| !t.space) {
        y += em * 0.1;
        let base = block.words.len();
        let g0 = block.groups.len();
        let font = cfg.bg_font(&joined(&bg));
        let f = flow(fonts, bg, &font, content_w, line_h, align, rtl);
        for w in f.words {
            block.words.push(Word { pos: w.pos + vec2(0.0, y), group: w.group + g0, ..w });
        }
        block.groups.extend(f.groups.into_iter().map(|g| Group { first: g.first + base, words: g.words.start + base..g.words.end + base, ..g }));
        y += f.height;
    }
    let gap = em / 6.0; // margin-top: 8px at 48 px
    if let Some(r) = roman {
        y += gap;
        let pad = vec2(cfg.rem() * 0.75, cfg.rem() * 0.25);
        let inner_w = content_w - pad.x * 2.0 - 2.0;
        let (body, inner) = match r {
            Roman::Timed(parts, text) => {
                let toks: Vec<Tok> = parts.iter().flat_map(split_part).collect();
                let font = cfg.roman_font(text);
                let lh = fonts.row_height(&font);
                let f = flow(fonts, toks, &font, inner_w, lh, Align::Left, false);
                (DecoBody::Timed { words: f.words, groups: f.groups }, vec2(f.width, f.height))
            }
            Roman::Text(t) => {
                let font = cfg.roman_font(t);
                let g = fonts.layout(t.to_owned(), font.clone(), Color32::WHITE, inner_w);
                (DecoBody::Text(t.to_owned(), font), g.size())
            }
        };
        let size = inner + pad * 2.0 + vec2(2.0, 2.0);
        let x = match align {
            Align::Left => 0.0,
            Align::Right => content_w - size.x,
            Align::Center => (content_w - size.x) * 0.5,
        };
        let rect = Rect::from_min_size(pos2(x, y), size);
        // Timed words sit inside the pill's padding.
        let body = match body {
            DecoBody::Timed { words, groups } => DecoBody::Timed { words: words.into_iter().map(|w| Word { pos: w.pos + rect.min.to_vec2() + pad + vec2(1.0, 1.0), ..w }).collect(), groups },
            b => b,
        };
        block.decos.push(Deco { body, rect, pill: true });
        y += size.y;
    }
    if let Some(t) = trans {
        y += gap;
        let font = cfg.trans_font(t);
        let g = fonts.layout(t.to_owned(), font.clone(), Color32::WHITE, content_w);
        let x = match align {
            Align::Left => 0.0,
            Align::Right => content_w - g.size().x,
            Align::Center => (content_w - g.size().x) * 0.5,
        };
        block.decos.push(Deco { body: DecoBody::Text(t.to_owned(), font), rect: Rect::from_min_size(pos2(x, y), g.size()), pill: false });
        y += g.size().y;
    }
    block.height = y + cfg.padding;
    // Back to the column's edge or centre, which the narrower wrap moved.
    let inset = match align {
        Align::Left => 0.0,
        Align::Right => full_w - content_w,
        Align::Center => (full_w - content_w) * 0.5,
    };
    if inset > 0.0 {
        let dx = vec2(inset, 0.0);
        for w in &mut block.words {
            w.pos += dx;
        }
        for d in &mut block.decos {
            d.rect = d.rect.translate(dx);
            if let DecoBody::Timed { words, .. } = &mut d.body {
                for w in words {
                    w.pos += dx;
                }
            }
        }
    }
    block
}

// ------------------------------------------------------------------ state

/// One scroll: every line eases from `delta` above/below its new place.
#[derive(Clone, Copy)]
struct ScrollAnim {
    start: f64,
    delta: f32,
    active: usize,
}

/// A line shifted by a relayout (a translation or romanization arriving).
#[derive(Clone, Copy)]
struct Shift {
    start: f64,
    delta: f32,
}

/// A CSS transition on one value: eases from where it was to the new target, after a delay.
#[derive(Clone, Copy, Default)]
struct Trans {
    from: f32,
    to: f32,
    start: f64,
    dur: f64,
    delay: f64,
    init: bool,
}

impl Trans {
    fn value(&self, now: f64) -> f32 {
        if !self.init {
            return self.to;
        }
        let p = ((now - self.start - self.delay) / self.dur.max(1e-3)).clamp(0.0, 1.0) as f32;
        lerp(self.from, self.to, EASE.at(p))
    }

    fn set(&mut self, to: f32, now: f64, dur: f64, delay: f64) {
        if !self.init {
            *self = Trans { from: to, to, start: now, dur, delay, init: true };
        } else if (to - self.to).abs() > 1e-4 {
            let from = self.value(now);
            *self = Trans { from, to, start: now, dur, delay, init: true };
        }
    }

    fn moving(&self, now: f64) -> bool {
        self.init && (self.from - self.to).abs() > 1e-4 && now < self.start + self.delay + self.dur
    }
}

#[derive(Clone, Copy, Default)]
struct LineState {
    /// Whole-line opacity and blur (the theme's line-state rules).
    opacity: Trans,
    blur: Trans,
    selected: bool,
    /// Wall time the exit animations started (line scale back, lit words fading).
    exit: Option<f64>,
    /// Song time at exit: which words had started.
    exit_t: f64,
}

/// The lyrics view's state between frames.
#[derive(Default)]
pub struct View {
    layout: Option<Layout>,
    states: Vec<LineState>,
    scroll_top: f32,
    scroll_set: bool,
    scrolls: Vec<ScrollAnim>,
    shifts: Vec<Vec<Shift>>,
    last_scroll: Vec<usize>,
    /// The line taller than the view being sung, and its row: the view pages down with it.
    page: Option<(usize, i32)>,
    last_t: f64,
    last_wall: f64,
    user_until: f64,
    /// Reader scrolling was on last frame: when it ends, scroll back to the current line.
    was_user: bool,
    glow: Glow,
    credits_focused: bool,
    /// The frame (`cumulative_pass_nr`) the view was last drawn in; 0 once its textures
    /// are freed.
    shown_pass: u64,
}

pub struct Frame<'a> {
    pub lyrics: &'a Lyrics,
    /// Identifies the song, source, decorations and settings the layout depends on.
    pub key: u64,
    pub roman: Vec<Option<Roman<'a>>>,
    pub trans: Vec<Option<&'a str>>,
    /// Audible song position (s), already corrected for output latency and the
    /// listener's offsets.
    pub t: f64,
    pub playing: bool,
    pub now: f64,
    pub rect: Rect,
    pub style: &'a LyricStyle,
    pub accent: Color32,
    pub centered: bool,
    /// The listener's lyrics text size, a multiple of the theme's.
    pub scale: f32,
    pub anim_mode: LyricAnimMode,
    pub title: Option<&'a str>,
    pub artist: Option<&'a str>,
    pub thumbs: &'a [crate::model::Thumb],
}

#[derive(Default)]
pub struct Out {
    pub seek: Option<f64>,
    /// Something is moving: keep drawing frames.
    pub animating: bool,
    /// The reader scrolled away; a "resume" control can bring the view back.
    pub user_scrolling: bool,
    pub return_to_prev_mode: bool,
    pub toggle_play: bool,
    pub next_track: bool,
    pub prev_track: bool,
}

impl View {
    pub fn reset(&mut self) {
        *self = Self { glow: std::mem::take(&mut self.glow), ..Default::default() };
    }

    /// Called every frame: once the view isn't drawn (another tab or page), its glows,
    /// blurred lines and atlas copy go; they're rebuilt as it comes back.
    pub fn release_hidden(&mut self, pass: u64) {
        if self.shown_pass != 0 && self.shown_pass + 1 < pass {
            self.glow = Glow::default();
            self.shown_pass = 0;
        }
    }

    pub fn resume(&mut self) {
        self.user_until = 0.0;
    }

    /// CSS `text-shadow: 0 0 <blur>` for a laid-out text at `at`: the same Gaussian the
    /// lyric glow uses (σ = blur / 2), as a white mask to tint when painting.
    pub fn text_glow(&mut self, ctx: &egui::Context, g: Arc<Galley>, at: Pos2, blur: f32) -> Option<(TextureId, Rect, Rect)> {
        let atlas = ctx.fonts(|fv| fv.font_image_size());
        let ppp = ctx.pixels_per_point();
        // Cached per position, so a moved or resized view re-renders it.
        let key = (hash(("text-glow", at.x.round() as i32, at.y.round() as i32, g.job.text.as_str())), usize::MAX);
        self.glow.blurred_line(ctx, key, blur / 2.0, &[], &[(g, at, 1.0)], &[], 1.0, atlas, ppp)
    }

    pub fn show(&mut self, ui: &mut egui::Ui, f: Frame) -> Out {
        let rect = f.rect;
        let ctx = ui.ctx().clone();
        self.shown_pass = ctx.cumulative_pass_nr();
        let cfg = Cfg::new(f.style, f.accent, rect.width(), ctx.content_rect().width(), f.centered, f.scale);
        let layout_key = hash((f.key, rect.width().round() as i32, cfg.size.to_bits(), cfg.padding.to_bits(), cfg.max_scale().to_bits(), cfg.centered, &cfg.font, cfg.weight as u8));
        if self.layout.as_ref().is_none_or(|l| l.key != layout_key) {
            self.relayout(ui, &cfg, &f, layout_key, rect);
        }
        let layout = self.layout.as_ref().unwrap();
        let richsync = layout.richsync;
        // Lines that aren't credits.
        let lines_n = layout.blocks.iter().filter(|b| b.line.is_some()).count();

        // Clock: the extension shows lyrics this far ahead of the media time.
        let t = f.t + if richsync { RICHSYNC_OFFSET } else { LINE_OFFSET };
        let jumped = (t - self.last_t - (f.now - self.last_wall)).abs() > TIME_JUMP || self.last_wall == 0.0;
        self.last_t = t;
        self.last_wall = f.now;

        // ---- selection and exits (tickView's per-line loop)
        let early = if f.playing { 2.0 } else { 0.0 };
        let mut animating = false;
        for i in 0..lines_n {
            let b = &layout.blocks[i];
            let next = layout.blocks.get(i + 1).filter(|n| n.line.is_some()).map_or(f64::INFINITY, |n| n.start);
            let end = next.max(b.start + b.dur + 0.05);
            let st = &mut self.states[i];
            let selected = t + early >= b.start && t < end;
            if selected && !st.selected {
                st.selected = true;
                st.exit = None;
            } else if !selected && st.selected {
                st.selected = false;
                let stale = end + FADE_OUT + 0.05;
                st.exit = if (f.playing || jumped) && t <= stale { Some(f.now) } else { None };
                st.exit_t = t;
            } else if !selected && jumped {
                st.exit = None;
            }
            if st.exit.is_some_and(|e| f.now - e > FADE_OUT.max(SCALE_DURATION)) {
                st.exit = None;
            }
            animating |= st.exit.is_some();
        }

        // ---- scrolling
        let view_h = rect.height();
        let scroll_time = t + SCROLL_OFFSET;
        let user = f.now < self.user_until;
        let mut active: Vec<usize> = Vec::new();
        // Leaving reader scrolling (the button, or the timeout) returns to the current line now.
        let mut new_lyric = jumped || (self.was_user && !user);
        self.was_user = user;
        for i in 0..lines_n {
            let b = &layout.blocks[i];
            let next = layout.blocks.get(i + 1).filter(|n| n.line.is_some()).map_or(f64::INFINITY, |n| n.start);
            if scroll_time >= b.start - EARLY_SCROLL && (scroll_time < next || scroll_time < b.start + b.dur) {
                active.push(i);
                if !self.last_scroll.contains(&i) && scroll_time >= b.start {
                    new_lyric = true;
                }
            }
        }
        let active_real = !active.is_empty();
        let last_sung = (0..lines_n).rev().find(|&i| !layout.blocks[i].instrumental);
        let credits = layout.blocks.iter().position(|b| b.credits.is_some());
        let credits_focused = credits.is_some() && last_sung.is_some_and(|i| scroll_time >= layout.blocks[i].start + layout.blocks[i].dur);
        if credits_focused != self.credits_focused {
            self.credits_focused = credits_focused;
            new_lyric = true;
        }
        // A line taller than the view would run off the bottom while it's sung: how far
        // down it the singing is (the latest word's row, or for a line-synced line, as far
        // as its time has gone), so the view pages down with it.
        let line_h = cfg.size * 1.333;
        let sung_y = |i: usize| -> Option<f32> {
            let b = &layout.blocks[i];
            if b.height <= view_h || scroll_time < b.start {
                return None;
            }
            let mut timed = b.words.iter().filter(|w| !w.line_synced && !w.bg).peekable();
            Some(if timed.peek().is_none() {
                b.height * ((scroll_time - b.start) / b.dur.max(0.001)).clamp(0.0, 1.0) as f32
            } else {
                timed.filter(|w| w.start <= scroll_time).map(|w| w.pos.y).fold(0.0, f32::max)
            })
        };
        let page = active.last().and_then(|&i| sung_y(i).map(|y| (i, (y / line_h) as i32)));
        if page != self.page {
            self.page = page;
            new_lyric |= page.is_some();
        }
        let pos = |i: usize| layout.pad_top + layout.blocks[i].top;
        let max_scroll = (layout.content_h - view_h).max(0.0);
        if !user && (new_lyric || !self.scroll_set) && lines_n > 0 {
            if active.is_empty() {
                active.push(0);
            }
            let offset = view_h * cfg.target_ratio;
            let last = *active.last().unwrap();
            let sung: Vec<usize> = active.iter().copied().enumerate().filter(|&(k, i)| scroll_time < layout.blocks[i].start + layout.blocks[i].dur - ENDING_THRESHOLD || k == active.len() - 1).map(|(_, i)| i).collect();
            let anchor = sung
                .iter()
                .rev()
                .find(|&&i| layout.blocks[i].start <= scroll_time && scroll_time < layout.blocks[i].start + layout.blocks[i].dur)
                .or_else(|| sung.iter().rev().find(|&&i| layout.blocks[i].start <= scroll_time))
                .copied()
                .unwrap_or(sung[0]);
            // `computeActiveLinesScrollTop`
            let (first, lastl) = (sung[0], *sung.last().unwrap());
            let centre = sung.iter().map(|&i| pos(i) + layout.blocks[i].height / 2.0).sum::<f32>() / sung.len() as f32;
            let h = |i: usize| layout.blocks[i].height;
            let mut top = centre - offset;
            top = top.max(pos(lastl) + h(lastl) - view_h);
            top = top.min(pos(lastl)).min(pos(first));
            top = top.max(pos(anchor) + h(anchor) - view_h);
            top = top.min(pos(anchor));
            if let Some(y) = sung_y(anchor) {
                // Keep the row being sung, and the one after it, in view.
                top = top.max(pos(anchor) + y + line_h * 2.0 - view_h);
            }
            if credits_focused && let (Some(c), Some(ls)) = (credits, last_sung) {
                top = (pos(c) + h(c) / 2.0 - offset).min(pos(ls));
            }
            let top = top.clamp(0.0, max_scroll);
            if !self.scroll_set {
                self.scroll_top = top;
                self.scroll_set = true;
            } else if (top - self.scroll_top).abs() > 2.0 {
                self.scrolls.push(ScrollAnim { start: f.now, delta: top - self.scroll_top, active: last });
                self.scroll_top = top;
            }
            self.last_scroll = active.clone();
        }

        // Reader scrolling.
        let hovered = ui.rect_contains_pointer(rect);
        let wheel = if hovered { ui.input(|i| i.smooth_scroll_delta.y) } else { 0.0 };
        if wheel != 0.0 {
            self.scroll_top = (self.scroll_top - wheel).clamp(0.0, max_scroll);
            self.user_until = f.now + USER_SCROLL_RESUME;
            self.scrolls.clear();
        }
        let user = f.now < self.user_until;

        // Scroll animations: per line, eased from where it was.
        self.scrolls.retain(|a| f.now - a.start < 2.0);
        animating |= !self.scrolls.is_empty();
        for s in &mut self.shifts {
            s.retain(|a| f.now - a.start < DECO_ENTRY);
            animating |= !s.is_empty();
        }

        // ---- paint
        let painter = ui.painter_at(rect);
        let first_shape = ctx.graphics(|g| g.get(painter.layer_id()).map_or(ShapeIdx(0), |l| l.next_idx()));
        let origin_y = rect.top() - self.scroll_top + layout.pad_top;
        let x0 = rect.left() + cfg.pad.0;
        let line_w = rect.width() - cfg.pad.0 - cfg.pad.1;
        let atlas = ctx.fonts(|fv| fv.font_image_size());
        let ppp = ctx.pixels_per_point();
        let mut seek = None;
        let active_line = active.last().copied();
        let first_active = active.first().copied().filter(|_| active_real);
        let mut glow_work: Vec<GlowJob> = Vec::new();
        let pointer = ui.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p));

        if f.anim_mode == LyricAnimMode::Constellation {
            return paint_constellation_view(&painter, &ctx, &cfg, &f, layout, t, rect);
        }
        if f.anim_mode == LyricAnimMode::ConvexLens {
            let out = paint_convex_lens_view(&mut self.glow, &painter, &ctx, &cfg, &f, layout, t, rect, atlas, ppp);
            self.glow.finish(&ctx, &painter, Vec::new(), atlas, ppp);
            return out;
        }
        if f.anim_mode == LyricAnimMode::PhysicsThrow {
            let out = paint_physics_throw_view(&mut self.glow, &painter, &ctx, &cfg, &f, layout, t, rect, atlas, ppp);
            self.glow.finish(&ctx, &painter, Vec::new(), atlas, ppp);
            return out;
        }
        if f.anim_mode == LyricAnimMode::CodeEditor {
            let out = paint_code_editor_view(&mut self.glow, &painter, &ctx, &cfg, &f, layout, t, rect, atlas, ppp);
            self.glow.finish(&ctx, &painter, Vec::new(), atlas, ppp);
            return out;
        }
        if f.anim_mode == LyricAnimMode::Chat {
            let out = paint_chat_view(&painter, &ctx, &cfg, &f, layout, t, rect, atlas, ppp);
            self.glow.finish(&ctx, &painter, Vec::new(), atlas, ppp);
            return out;
        }

        for (i, b) in layout.blocks.iter().enumerate() {
            let mut dy = 0.0;
            for a in &self.scrolls {
                let rel = i as isize - a.active as isize;
                let d = scroll_duration(rel, a.delta);
                let p = ((f.now - a.start) / d).clamp(0.0, 1.0) as f32;
                dy += a.delta * (1.0 - SCROLL_EASE.at(p));
            }
            if let Some(shifts) = self.shifts.get(i) {
                for s in shifts {
                    let p = ((f.now - s.start) / DECO_ENTRY).clamp(0.0, 1.0) as f32;
                    dy += s.delta * (1.0 - DECO_EASE.at(p));
                }
            }
            let top = origin_y + b.top + dy;
            if top > rect.bottom() + 40.0 || top + b.height < rect.top() - 40.0 {
                continue;
            }
            let block_rect = Rect::from_min_size(pos2(x0, top), vec2(line_w, b.height));
            let content = pos2(x0 + cfg.size * 0.25, top);

            if let Some((text, font)) = &b.credits {
                let a = if credits_focused { 0.85 } else { 0.2 };
                let a = theme::anim(&ctx, ("lyr-credits", f.key), a, 0.35);
                let o = transform_origin(block_rect, b.align, content.x);
                let g = ctx.fonts_mut(|fv| fv.layout(text.clone(), font.clone(), Color32::WHITE, line_w - cfg.size * 0.5));
                let xf = Xf { origin: o, scale: cfg.scale, content };
                paint_galley(&painter, &g, pos2(0.0, cfg.padding), &xf, theme::with_alpha(Color32::WHITE, a), atlas);
                continue;
            }
            let li = b.line.unwrap();
            let hovered = pointer.is_some_and(|p| block_rect.contains(p));
            let st = self.states[i];
            let sung_now = st.selected && t >= b.start;

            // Whole-line opacity and blur. A theme that styles lines by state (its CSS
            // `.blyrics--active` rules) gets those values and their transitions; the
            // others get the preset's approximations.
            let (dim, opacity, blur) = if let Some(css) = &cfg.css {
                let on = active.contains(&i) && active_real || st.selected;
                let target = if hovered || on {
                    (1.0, 0.0)
                } else {
                    match first_active {
                        Some(a) if i < a && !user => css.past.map_or((1.0, 0.0), |p| (p[0], p[1])),
                        Some(a) if i > a => css.upcoming.map_or((1.0, 0.0), |p| (p[0], p[1])),
                        _ => (1.0, 0.0),
                    }
                };
                let line_ms = b.dur;
                let (op_t, blur_t) = if hovered {
                    // `:hover { transition: all 0.3s ease }`
                    ((0.3, 0.0), (0.3, 0.0))
                } else {
                    let o = css.opacity_transition.unwrap_or([0.0, 0.0]);
                    let fl = css.filter_transition.unwrap_or([0.0, 0.0]);
                    ((line_ms * o[0] as f64, o[1] as f64), (line_ms * fl[0] as f64, fl[1] as f64))
                };
                let s = &mut self.states[i];
                s.opacity.set(target.0, f.now, op_t.0, op_t.1);
                s.blur.set(target.1, f.now, blur_t.0, blur_t.1);
                animating |= s.opacity.moving(f.now) || s.blur.moving(f.now);
                (cfg.inactive_alpha, s.opacity.value(f.now), s.blur.value(f.now))
            } else {
                let rel = active_line.map_or(1, |a| i as isize - a as isize);
                let past = rel < 0;
                let dist = rel.unsigned_abs() as f32;
                let upcoming = (cfg.upcoming_alpha - dist.min(6.0) * cfg.falloff).max(cfg.min_alpha.min(cfg.upcoming_alpha));
                let reveal = theme::anim_bool(&ctx, "lyr-reveal", user, 0.3);
                let dim = if sung_now || rel == 0 {
                    cfg.inactive_alpha
                } else if past {
                    lerp(cfg.past_alpha.unwrap_or(upcoming), upcoming, reveal)
                } else {
                    upcoming
                };
                let dim = theme::anim(&ctx, ("lyr-dim", f.key, i), dim, 0.3);
                let blur_on = match cfg.blur_scope {
                    BlurScope::All => true,
                    BlurScope::Past => past,
                    BlurScope::Upcoming => !past || reveal > 0.0,
                };
                let target = if cfg.blur > 0.0 && blur_on && !sung_now && rel != 0 { (cfg.blur * (1.0 + 0.35 * (dist - 1.0).clamp(0.0, 3.0))).min(9.0) } else { 0.0 };
                let s = &mut self.states[i];
                s.blur.set(target, f.now, 0.35, 0.0);
                animating |= s.blur.moving(f.now);
                (dim, 1.0, s.blur.value(f.now))
            };

            // Line scale: 0.95 at rest, 1 while sung (enter at its start, exit when it ends);
            // a theme's hover scale on lines not being sung.
            let mut scale = cfg.scale;
            if st.selected && t >= b.start {
                scale = lerp(cfg.scale, cfg.active_scale, EASE.at(((t - b.start) / SCALE_DURATION) as f32));
            } else if let Some(e) = st.exit {
                scale = lerp(cfg.active_scale, cfg.scale, EASE.at(((f.now - e) / SCALE_DURATION) as f32));
            } else if let Some(h) = cfg.css.as_ref().and_then(|c| c.hover_scale) {
                let k = theme::anim_bool(&ctx, ("lyr-hover", f.key, i), hovered, 0.3);
                scale = lerp(cfg.scale, h, EASE.at(k));
            }
            if st.selected && t >= b.start && t < b.start + SCALE_DURATION {
                animating = true;
            }
            let origin = transform_origin(block_rect, b.align, content.x);
            let xf = Xf { origin, scale, content };

            let text_lit = if let Some(e) = st.exit {
                1.0 - EASE.at(((f.now - e) / FADE_OUT) as f32)
            } else if sung_now {
                EASE.at(((t - b.start) / FADE_IN) as f32)
            } else {
                0.0
            };
            let text_alpha = 0.6 * lerp(dim, 1.0, text_lit);
            if b.instrumental {
                animating |= paint_instrumental(&painter, &cfg, b, &xf, t, line_w - cfg.size * 0.5);
            } else if blur > 0.05 {
                // `filter: blur()` on the whole line: its text, romanization and translation.
                let r = (blur * 4.0).round() / 4.0;
                let deco_words: Vec<(&Word, f32)> = b
                    .decos
                    .iter()
                    .flat_map(|d| match &d.body {
                        DecoBody::Timed { words, .. } => words.iter().map(|w| (w, dim)).collect::<Vec<_>>(),
                        DecoBody::Text(..) => vec![],
                    })
                    .collect();
                let texts: Vec<(Arc<Galley>, Pos2, f32)> = b
                    .decos
                    .iter()
                    .filter_map(|d| match &d.body {
                        DecoBody::Text(text, font) => {
                            let g = ctx.fonts_mut(|fv| fv.layout(text.clone(), font.clone(), Color32::WHITE, d.rect.width() + 1.0));
                            let pad = if d.pill { vec2(cfg.rem() * 0.75 + 1.0, cfg.rem() * 0.25 + 1.0) } else { Vec2::ZERO };
                            Some((g, d.rect.min + pad, text_alpha))
                        }
                        DecoBody::Timed { .. } => None,
                    })
                    .collect();
                let words: Vec<(&Word, f32)> = b.words.iter().map(|w| (w, if w.bg { dim * 0.78 } else { dim })).chain(deco_words).collect();
                // The romanization pill is part of the line, so it's blurred with it.
                let pills: Vec<(Rect, f32)> = b.decos.iter().filter(|d| d.pill).map(|d| (d.rect, cfg.rem())).collect();
                if let Some((tex, rr, uv)) = self.glow.blurred_line(&ctx, (f.key, i), r, &words, &texts, &pills, scale, atlas, ppp) {
                    // Rendered at the line's scale; snapped so it lands 1:1 on screen pixels.
                    let to = xf.rect(rr);
                    let snapped = (to.min.to_vec2() * ppp).round() / ppp;
                    painter.image(tex, to.translate(snapped - to.min.to_vec2()), uv, theme::with_alpha(Color32::WHITE, opacity));
                }
                animating |= st.exit.is_some();
            } else {
                let lit = LitState::new(&cfg, st, t, f.now, sung_now || st.selected);
                if f.anim_mode == LyricAnimMode::KineticPop {
                    paint_words_kinetic(&painter, &ctx, &cfg, &b.words, &xf, &lit, dim, opacity, atlas, &mut glow_work, (f.key, i, 0), &mut animating);
                } else {
                    paint_words(&painter, &ctx, &cfg, &b.words, &b.groups, &xf, &lit, dim, opacity, atlas, &mut glow_work, (f.key, i, 0));
                }
                for (k, d) in b.decos.iter().enumerate() {
                    let dxf = xf;
                    let a = opacity;
                    if d.pill {
                        let r = dxf.rect(d.rect);
                        let radius = (cfg.rem() * scale).min(255.0) as u8;
                        painter.rect(r, radius, theme::with_alpha(Color32::WHITE, 0.05 * a), egui::Stroke::new(1.0, theme::with_alpha(Color32::WHITE, 0.1 * a)), egui::StrokeKind::Inside);
                    }
                    match &d.body {
                        DecoBody::Timed { words, groups } => {
                            if f.anim_mode == LyricAnimMode::KineticPop {
                                paint_words_kinetic(&painter, &ctx, &cfg, words, &dxf, &lit, dim, a, atlas, &mut glow_work, (f.key, i, k as u32 + 1), &mut animating);
                            } else {
                                paint_words(&painter, &ctx, &cfg, words, groups, &dxf, &lit, dim, a, atlas, &mut glow_work, (f.key, i, k as u32 + 1));
                            }
                        }
                        DecoBody::Text(text, font) => {
                            let wrap = d.rect.width() + 1.0;
                            let g = ctx.fonts_mut(|fv| fv.layout(text.clone(), font.clone(), Color32::WHITE, wrap));
                            let pad = if d.pill { vec2(cfg.rem() * 0.75 + 1.0, cfg.rem() * 0.25 + 1.0) } else { Vec2::ZERO };
                            paint_galley(&painter, &g, d.rect.min + pad, &dxf, theme::with_alpha(Color32::WHITE, text_alpha * a), atlas);
                        }
                    }
                }
                animating |= lit.animating(b, t);
            }

            // Click a line to seek to it (the extension seeks to the line's time).
            let hit = block_rect.intersect(rect);
            if hit.is_positive() {
                let resp = ui.interact(hit, ui.scope_id().with(("lyr-line", f.key, i)), egui::Sense::click());
                if resp.clicked() && f.lyrics.sync != Sync::None {
                    seek = Some(f.lyrics.lines[li].start as f64 / 1000.0);
                }
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
            }
        }
        // Glows (drop-shadows of the lit text) were queued while painting; draw them under
        // the lit text they belong to by inserting at the recorded positions.
        self.glow.finish(&ctx, &painter, glow_work, atlas, ppp);
        fade_edges(&ctx, painter.layer_id(), first_shape, rect, cfg.size);
        if f.playing {
            animating = true;
        }
        Out { seek, animating, user_scrolling: user, ..Default::default() }
    }

    fn relayout(&mut self, ui: &mut egui::Ui, cfg: &Cfg, f: &Frame, key: u64, rect: Rect) {
        let width = rect.width() - cfg.pad.0 - cfg.pad.1;
        let lyrics = f.lyrics;
        let mut blocks: Vec<Block> = ui.ctx().fonts_mut(|fonts| {
            lyrics
                .lines
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let agent = if l.instrumental { nearest_agent(lyrics, i) } else { l.agent.clone() };
                    let roman = f.roman.get(i).and_then(|r| r.as_ref()).map(|r| match r {
                        Roman::Timed(p, t) => Roman::Timed(p, t),
                        Roman::Text(t) => Roman::Text(t),
                    });
                    let trans = f.trans.get(i).copied().flatten();
                    build_block(fonts, cfg, l, i, width, roman, trans, agent.as_deref())
                })
                .collect()
        });
        if !lyrics.songwriters.is_empty() {
            let names = &lyrics.songwriters;
            let text = if names.len() > 1 { format!("Written by {} & {}", names[..names.len() - 1].join(", "), names[names.len() - 1]) } else { format!("Written by {}", names[0]) };
            let font = theme::regular((cfg.size * 0.4).max(12.0));
            let h = ui.ctx().fonts_mut(|fv| fv.layout(text.clone(), font.clone(), Color32::WHITE, width - cfg.size * 0.5).size().y);
            blocks.push(Block {
                line: None,
                start: 0.0,
                dur: 0.0,
                instrumental: false,
                align: if cfg.centered { Align::Center } else { Align::Left },
                top: 0.0,
                height: h + cfg.padding * 2.0,
                words: vec![],
                groups: vec![],
                decos: vec![],
                credits: Some((text, font)),
            });
        }
        let mut y = 0.0;
        for b in &mut blocks {
            b.top = y;
            y += b.height;
        }
        let view_h = rect.height();
        // `computeScrollPadding`
        let first_h = blocks.first().map_or(0.0, |b| b.height);
        let pad_top = (view_h * cfg.target_ratio - first_h / 2.0).max(0.0);
        let last_centre = blocks.iter().rev().find(|b| b.line.is_some()).map(|b| b.top + b.height / 2.0);
        let last_h = blocks.iter().rev().find(|b| b.line.is_some()).map_or(0.0, |b| b.height);
        let content = y;
        let trailing = blocks.last().filter(|b| b.credits.is_some()).map_or(0.0, |b| b.height) + last_h / 2.0;
        let want = last_centre.map_or(view_h, |c| c + view_h * (1.0 - cfg.target_ratio));
        let pad_bottom = (want - content).max(view_h - content).max(view_h * (1.0 - cfg.target_ratio) - trailing).max(0.0);
        let richsync = lyrics.sync == Sync::Word;
        let same_song = self.layout.as_ref().is_some_and(|l| l.blocks.len() == blocks.len() && self.states.len() == blocks.len() && l.richsync == richsync && same_lines(l, &blocks));
        if same_song {
            // Decorations arrived (or the window resized): lines slide to their new places.
            let old = self.layout.as_ref().unwrap();
            let now = ui.input(|i| i.time);
            self.shifts.resize_with(blocks.len(), Vec::new);
            for (i, b) in blocks.iter().enumerate() {
                let delta = (old.pad_top + old.blocks[i].top) - (pad_top + b.top);
                if delta.abs() > 0.5 && (old.width - width).abs() < 1.0 {
                    self.shifts[i].push(Shift { start: now, delta });
                }
            }
        } else {
            let glow = std::mem::take(&mut self.glow);
            *self = Self { glow, ..Default::default() };
            self.states = vec![LineState::default(); blocks.len()];
            self.shifts = vec![Vec::new(); blocks.len()];
        }
        self.layout = Some(Layout { key, width, blocks, pad_top, content_h: pad_top + content + pad_bottom, richsync });
    }
}

fn same_lines(l: &Layout, blocks: &[Block]) -> bool {
    l.blocks.iter().zip(blocks).all(|(a, b)| a.line == b.line && a.start == b.start && a.words.len() == b.words.len())
}

/// `findNearestAgent`: an instrumental break takes the side of the singer nearest it.
fn nearest_agent(l: &Lyrics, i: usize) -> Option<String> {
    l.lines[i + 1..].iter().chain(l.lines[..i].iter().rev()).find(|x| !x.instrumental && x.agent.is_some()).and_then(|x| x.agent.clone())
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Duration of one line's part in a scroll, by its distance from the active line
/// (`LINE_SCROLL_AFTER_FUNCTION` and the `above` variant).
fn scroll_duration(rel: isize, delta: f32) -> f64 {
    let up = delta < 0.0;
    let above = (rel < 0 && !up) || (rel > 0 && up);
    let i = rel.unsigned_abs() as f64;
    if above { (750.0 + i.min(6.0) * 20.0) / 1000.0 } else { (750.0 + (i + 2.0).ln() * 80.0 + (i + 1.0) * 20.0) / 1000.0 }
}

fn transform_origin(block: Rect, align: Align, content_left: f32) -> Pos2 {
    match align {
        Align::Left => pos2(block.left(), block.center().y),
        Align::Right => pos2(block.right(), block.center().y),
        Align::Center => pos2(block.center().x.max(content_left), block.center().y),
    }
}

/// The line's transform: content-relative positions → screen, scaled about `origin`.
#[derive(Clone, Copy)]
struct Xf {
    origin: Pos2,
    scale: f32,
    content: Pos2,
}

impl Xf {
    fn point(&self, p: Pos2) -> Pos2 {
        let s = self.content + p.to_vec2();
        self.origin + (s - self.origin) * self.scale
    }
    fn rect(&self, r: Rect) -> Rect {
        Rect::from_min_max(self.point(r.min), self.point(r.max))
    }
}

/// Lines fade out over the top and bottom edges of the view instead of being cut off
/// there. Geometry crossing an edge zone is sliced into thin horizontal bands, each
/// dimmed by its own height, so the fade is smooth and reaches zero exactly at the edge
/// (per-vertex alpha alone interpolates across a glyph and leaves a visible cut).
fn fade_edges(ctx: &egui::Context, layer: egui::LayerId, from: ShapeIdx, rect: Rect, size: f32) {
    let zone = (size * 1.6).min(rect.height() * 0.15).max(1.0);
    let edge = EdgeFade { rect, zone };
    ctx.graphics_mut(|g| {
        let list = g.entry(layer);
        for i in from.0..list.next_idx().0 {
            list.mutate_shape(ShapeIdx(i), |c| edge.shape(&mut c.shape));
        }
    });
}

struct EdgeFade {
    rect: Rect,
    zone: f32,
}

impl EdgeFade {
    fn k(&self, y: f32) -> f32 {
        let s = |d: f32| {
            let x = (d / self.zone).clamp(0.0, 1.0);
            x * x * (3.0 - 2.0 * x)
        };
        s(y - self.rect.top()) * s(self.rect.bottom() - y)
    }

    fn shape(&self, shape: &mut Shape) {
        match shape {
            Shape::Mesh(m) => {
                let (top, bottom) = (self.rect.top() + self.zone, self.rect.bottom() - self.zone);
                if m.vertices.iter().any(|v| v.pos.y < top || v.pos.y > bottom) {
                    *m = Arc::new(self.mesh(m));
                }
            }
            Shape::Rect(r) => {
                let f = self.k(r.rect.center().y);
                r.fill = r.fill.gamma_multiply(f);
                r.stroke.color = r.stroke.color.gamma_multiply(f);
            }
            Shape::Text(t) => t.opacity_factor *= self.k(t.pos.y + t.galley.size().y / 2.0),
            Shape::Vec(v) => v.iter_mut().for_each(|s| self.shape(s)),
            _ => {}
        }
    }

    fn mesh(&self, m: &Mesh) -> Mesh {
        use egui::epaint::Vertex;
        const BAND: f32 = 2.0;
        let (r, z) = (self.rect, self.zone);
        let mut cuts = Vec::new();
        for (a, b) in [(r.top(), r.top() + z), (r.bottom() - z, r.bottom())] {
            let n = ((b - a) / BAND).ceil().max(1.0) as usize;
            cuts.extend((0..=n).map(|i| a + (b - a) * i as f32 / n as f32));
        }
        let lerp = |a: &Vertex, b: &Vertex, t: f32| Vertex {
            pos: a.pos + (b.pos - a.pos) * t,
            uv: a.uv + (b.uv - a.uv) * t,
            color: a.color.lerp_to_gamma(b.color, t),
        };
        // The part of a polygon on one side of a horizontal line.
        let clip = |poly: &[Vertex], y: f32, keep_below: bool| -> Vec<Vertex> {
            let inside = |v: &Vertex| if keep_below { v.pos.y >= y } else { v.pos.y <= y };
            let mut out = Vec::with_capacity(poly.len() + 2);
            for i in 0..poly.len() {
                let (a, b) = (&poly[i], &poly[(i + 1) % poly.len()]);
                if inside(a) {
                    out.push(*a);
                }
                if inside(a) != inside(b) {
                    out.push(lerp(a, b, (y - a.pos.y) / (b.pos.y - a.pos.y)));
                }
            }
            out
        };
        let mut out = Mesh::with_texture(m.texture_id);
        for tri in m.indices.as_chunks::<3>().0 {
            let poly = [m.vertices[tri[0] as usize], m.vertices[tri[1] as usize], m.vertices[tri[2] as usize]];
            let lo = poly.iter().map(|v| v.pos.y).fold(f32::INFINITY, f32::min);
            let hi = poly.iter().map(|v| v.pos.y).fold(f32::NEG_INFINITY, f32::max);
            if hi <= r.top() || lo >= r.bottom() {
                continue;
            }
            let mut ys: Vec<f32> = cuts.iter().copied().filter(|&y| y > lo && y < hi).collect();
            ys.insert(0, lo.max(r.top()));
            ys.push(hi.min(r.bottom()));
            for w in ys.windows(2) {
                let piece = clip(&clip(&poly, w[0], true), w[1], false);
                if piece.len() < 3 {
                    continue;
                }
                let base = out.vertices.len() as u32;
                out.vertices.extend(piece.iter().map(|v| Vertex { color: v.color.gamma_multiply(self.k(v.pos.y)), ..*v }));
                for i in 1..piece.len() as u32 - 1 {
                    out.indices.extend([base, base + i, base + i + 1]);
                }
            }
        }
        out
    }
}

/// A laid-out galley drawn through a line transform, as glyph quads on the font texture.
fn paint_galley(painter: &egui::Painter, g: &Galley, at: Pos2, xf: &Xf, color: Color32, atlas: [usize; 2]) {
    let mut mesh = Mesh::with_texture(TextureId::default());
    let none: [f32; 0] = [];
    for row in &g.rows {
        for gl in &row.row.glyphs {
            let uv = gl.uv_rect;
            if uv.is_nothing() {
                continue;
            }
            let r = Rect::from_min_size(at + row.pos.to_vec2() + gl.pos.to_vec2() + uv.offset, uv.size);
            let uvr = Rect::from_min_max(
                pos2(uv.min[0] as f32 / atlas[0] as f32, uv.min[1] as f32 / atlas[1] as f32),
                pos2(uv.max[0] as f32 / atlas[0] as f32, uv.max[1] as f32 / atlas[1] as f32),
            );
            push_quad(&mut mesh, xf.rect(r), uvr, color, &|_| 1.0, &none);
        }
    }
    painter.add(Shape::mesh(mesh));
}

// ------------------------------------------------------------------ words

/// Where a line's highlight animations are this frame.
struct LitState {
    /// Line still running its animations (song time drives them).
    live: bool,
    /// Exit: wall time since it started, and the song time it happened at.
    exit: Option<(f32, f64)>,
    t: f64,
    fill: Fill,
}

impl LitState {
    fn new(cfg: &Cfg, st: LineState, t: f64, now: f64, live: bool) -> Self {
        Self { live: live && st.exit.is_none(), exit: st.exit.map(|e| ((now - e) as f32, st.exit_t)), t, fill: cfg.fill }
    }

    fn animating(&self, b: &Block, t: f64) -> bool {
        if self.exit.is_some() {
            return true;
        }
        self.live && b.words.iter().any(|w| t >= w.start - 0.5 && t < w.start + w.dur.max(0.0) * SWIPE_DURATION + GLOW_MIN.max(w.dur * GLOW_RATIO) + 1.0)
    }
}

/// What a word looks like this frame.
struct WordLook {
    /// Lit layer opacity.
    opacity: f32,
    /// Gradient start/end, as fractions of the word (or letter-index space).
    start: f32,
    end: f32,
    /// Glow radius in points (0 = none).
    glow: f32,
    /// Alpha of this word's `--blyrics-glow-color`.
    glow_alpha: f32,
}

fn word_look(w: &Word, lit: &LitState, cfg: &Cfg) -> WordLook {
    let glow_alpha = if w.long { cfg.long_glow_alpha } else { cfg.glow_alpha };
    let none = WordLook { opacity: 0.0, start: SWIPE_START.0, end: SWIPE_END.0, glow: 0.0, glow_alpha };
    let full = |opacity: f32| WordLook { opacity, start: SWIPE_START.1, end: SWIPE_END.1, glow: 0.0, glow_alpha };
    if let Some((since, exit_t)) = lit.exit {
        // Words already sung fade back over 0.5 s; the rest were reset.
        if exit_t < w.start {
            return none;
        }
        return full(1.0 - EASE.at(since / FADE_OUT as f32));
    }
    if !lit.live {
        return none;
    }
    let e = lit.t - w.start;
    if e < 0.0 {
        return none;
    }
    // `resolveLineGlowSuppression`: a glow colour too faint to see isn't animated at all.
    let glow_rem = if glow_alpha < GLOW_INVISIBLE { 0.0 } else { cfg.glow_rem * cfg.rem() };
    match lit.fill {
        Fill::Line | Fill::Word => {
            // Word: a colour flip with a short transition (`data-word-state`);
            // Line: the line-synced fade.
            let d = if lit.fill == Fill::Word { 0.18 } else { FADE_IN };
            let p = EASE.at((e / d) as f32);
            WordLook { glow: glow_radius(e, GLOW_MIN, glow_rem), ..full(p) }
        }
        Fill::Sweep if w.line_synced => {
            let p = EASE.at((e / FADE_IN) as f32);
            WordLook { glow: glow_radius(e, GLOW_MIN, glow_rem), ..full(p) }
        }
        Fill::Sweep => {
            let d = w.dur.max(0.0);
            let sd = d * SWIPE_DURATION;
            let p = if sd <= 0.0 { 1.0 } else { ((e + d * SWIPE_LEAD) / sd).clamp(0.0, 1.0) as f32 };
            WordLook {
                opacity: 1.0,
                start: lerp(SWIPE_START.0, SWIPE_START.1, p),
                end: lerp(SWIPE_END.0, SWIPE_END.1, p),
                glow: glow_radius(e, (d * GLOW_RATIO).max(GLOW_MIN), glow_rem),
                glow_alpha,
            }
        }
    }
}

/// `drop-shadow` radius: 0.8rem → 0 with `ease`, then gone.
fn glow_radius(e: f64, dur: f64, from: f32) -> f32 {
    if e < 0.0 || e >= dur {
        return 0.0;
    }
    from * (1.0 - EASE.at((e / dur) as f32))
}

/// `planLetterWave` on the default swipe ramp: (delay, duration) per letter, seconds.
fn letter_waves(n: usize, d: f64, float: f64) -> Vec<(f64, f64)> {
    let sd = d * SWIPE_DURATION;
    let lead = d * SWIPE_LEAD;
    let rise = float * LETTER_WAVE_CREST as f64;
    (0..n)
        .map(|i| {
            if sd <= 0.0 {
                return (0.0, float);
            }
            let (sf, st) = (SWIPE_START.0 as f64, SWIPE_START.1 as f64);
            let (ef, et) = (SWIPE_END.0 as f64, SWIPE_END.1 as f64);
            let begin = (sd * (i as f64 / n as f64 - ef) / (et - ef)).clamp(0.0, sd);
            let finish = (sd * ((i + 1) as f64 / n as f64 - sf) / (st - sf)).clamp(0.0, sd);
            let dur = (finish - begin).max(0.001);
            let touched = begin - lead;
            let lit = touched + dur;
            if lit - rise > touched { (touched, (lit - touched) / LETTER_WAVE_CREST as f64) } else { (lit - rise, float) }
        })
        .collect()
}

/// A letter's float this frame: (lift in em, scale).
fn letter_float(e: f64, wave: (f64, f64), long: bool, cfg: &Cfg) -> (f32, f32) {
    let p = (e - wave.0) / wave.1;
    if p < 0.0 {
        return (0.0, 1.0);
    }
    let p = p.min(1.0) as f32;
    let peak = if long { cfg.emphasis } else { 1.0 };
    if p < LETTER_WAVE_CREST {
        let k = EASE_IN_OUT.at(p / LETTER_WAVE_CREST);
        (cfg.wave_crest * k, lerp(1.0, peak, k))
    } else {
        let k = EASE_OUT.at((p - LETTER_WAVE_CREST) / (1.0 - LETTER_WAVE_CREST));
        (lerp(cfg.wave_crest, cfg.wave_settle, k), lerp(peak, 1.0, k))
    }
}

/// The group's sideways pop: (translateX in em, scaleX).
fn wobble(e: f64) -> (f32, f32) {
    if !(0.0..WOBBLE_DURATION).contains(&e) {
        return (0.0, 1.0);
    }
    let p = EASE.at((e / WOBBLE_DURATION) as f32);
    let peak = (0.05, 1.025);
    if p < 0.125 {
        let k = p / 0.125;
        (peak.0 * k, lerp(1.0, peak.1, k))
    } else if p < 0.75 {
        let k = EASE_IN_OUT.at((p - 0.125) / 0.625);
        (lerp(peak.0, 0.0, k), lerp(peak.1, 1.0, k))
    } else {
        (0.0, 1.0)
    }
}

/// One glyph of a word, positioned for this frame.
struct GlyphQuad {
    rect: Rect,
    uv: Rect,
    /// Letter index and count, for the letter-space gradient.
    letter: usize,
    letters: usize,
}

/// A word's glyphs this frame, in content coordinates (before the line transform):
/// letter floats applied, then the group's wobble. Laid out once, at the font's size,
/// and scaled with the line: a second size would put a second copy of every glyph in
/// the shared font atlas, which only ever grows.
fn word_glyphs(ctx: &egui::Context, w: &Word, lit: &LitState, cfg: &Cfg, wob: Option<(f32, f32, f32)>, atlas: [usize; 2]) -> Vec<GlyphQuad> {
    let g = ctx.fonts_mut(|fv| fv.layout_no_wrap(w.text.clone(), w.font.clone(), Color32::WHITE));
    let n: usize = g.rows.iter().map(|r| r.row.glyphs.len()).sum();
    let waves = if w.letters && lit.live && lit.exit.is_none() { Some(letter_waves(n, w.dur.max(0.0), cfg.wave_duration)) } else { None };
    let e = lit.t - w.start;
    let em = w.font.size;
    let mut out = Vec::with_capacity(n);
    let mut k = 0;
    for row in &g.rows {
        for gl in &row.row.glyphs {
            let idx = k;
            k += 1;
            let uv = gl.uv_rect;
            if uv.is_nothing() {
                continue;
            }
            let mut r = Rect::from_min_size(w.pos + row.pos.to_vec2() + gl.pos.to_vec2() + uv.offset, uv.size);
            if let Some(waves) = &waves {
                let (lift, s) = letter_float(e, waves[idx], w.long, cfg);
                // Letters scale about their own centre (`.blyrics--letter` box).
                let c = pos2(w.pos.x + row.pos.x + gl.pos.x + gl.advance_width * 0.5, r.center().y);
                r = Rect::from_min_max(c + (r.min - c) * s, c + (r.max - c) * s).translate(vec2(0.0, -lift * em));
            }
            if let Some((cx, tx, sx)) = wob {
                let f = |x: f32| cx + (x - cx) * sx + tx;
                r = Rect::from_x_y_ranges(f(r.left())..=f(r.right()), r.y_range());
            }
            let uvr = Rect::from_min_max(
                pos2(uv.min[0] as f32 / atlas[0] as f32, uv.min[1] as f32 / atlas[1] as f32),
                pos2(uv.max[0] as f32 / atlas[0] as f32, uv.max[1] as f32 / atlas[1] as f32),
            );
            out.push(GlyphQuad { rect: r, uv: uvr, letter: idx, letters: n });
        }
    }
    out
}

/// Draw a glyph quad, alpha following the lit gradient: 1 left of `a`, 0 right of `b`
/// (screen x), linear between, so the soft edge is exact and never a box.
fn push_quad(mesh: &mut Mesh, r: Rect, uv: Rect, color: Color32, alpha: &dyn Fn(f32) -> f32, cuts: &[f32]) {
    let mut xs = vec![r.left()];
    xs.extend(cuts.iter().copied().filter(|&c| c > r.left() && c < r.right()));
    xs.push(r.right());
    for s in xs.windows(2) {
        let (x0, x1) = (s[0], s[1]);
        if x1 - x0 <= 0.0 {
            continue;
        }
        let (a0, a1) = (alpha(x0), alpha(x1));
        if a0 <= 0.0 && a1 <= 0.0 {
            continue;
        }
        let u = |x: f32| uv.left() + (x - r.left()) / r.width().max(1e-6) * uv.width();
        let base = mesh.vertices.len() as u32;
        let c = |a: f32| theme::with_alpha(color, a);
        mesh.vertices.push(Vertex { pos: pos2(x0, r.top()), uv: pos2(u(x0), uv.top()), color: c(a0) });
        mesh.vertices.push(Vertex { pos: pos2(x1, r.top()), uv: pos2(u(x1), uv.top()), color: c(a1) });
        mesh.vertices.push(Vertex { pos: pos2(x1, r.bottom()), uv: pos2(u(x1), uv.bottom()), color: c(a1) });
        mesh.vertices.push(Vertex { pos: pos2(x0, r.bottom()), uv: pos2(u(x0), uv.bottom()), color: c(a0) });
        mesh.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
}

/// The lit gradient over one glyph, in screen x: (alpha function, cut points).
/// Letter mode follows the per-letter masks (index space); otherwise the word box.
/// `rtl`: the word reads right to left, so it's lit from the right.
fn lit_gradient(q: &Rect, letter: usize, letters: usize, word: Rect, look: &WordLook, by_letter: bool, rtl: bool) -> (Box<dyn Fn(f32) -> f32>, Vec<f32>) {
    let (s, e) = (look.start, look.end);
    if rtl && !by_letter {
        let xs = word.right() - word.width() * s;
        let xe = word.right() - word.width() * e - 1.0;
        let f = move |x: f32| if x >= xs { 1.0 } else if x <= xe { 0.0 } else { 1.0 - (xs - x) / (xs - xe) };
        return (Box::new(f), vec![xe, xs]);
    }
    if by_letter {
        let n = letters.max(1) as f32;
        // In letter units across this glyph: X = letter + (x - left) / width.
        let (l, w) = (q.left(), q.width().max(1e-6));
        let i = letter as f32;
        let x_at = |v: f32| l + (v * n - i) * w; // where X/n == v
        let (xs, xe) = (x_at(s), x_at(e) + 1.0);
        let f = move |x: f32| if x <= xs { 1.0 } else if x >= xe { 0.0 } else { 1.0 - (x - xs) / (xe - xs) };
        (Box::new(f), vec![xs, xe])
    } else {
        let xs = word.left() + word.width() * s;
        let xe = word.left() + word.width() * e + 1.0;
        let f = move |x: f32| if x <= xs { 1.0 } else if x >= xe { 0.0 } else { 1.0 - (x - xs) / (xe - xs) };
        (Box::new(f), vec![xs, xe])
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_words(
    painter: &egui::Painter,
    ctx: &egui::Context,
    cfg: &Cfg,
    words: &[Word],
    groups: &[Group],
    xf: &Xf,
    lit: &LitState,
    dim: f32,
    opacity: f32,
    atlas: [usize; 2],
    glow: &mut Vec<GlowJob>,
    key: (u64, usize, u32),
) {
    let no_cut: [f32; 0] = [];
    let mut base = Mesh::with_texture(TextureId::default());
    let mut per_word = Vec::with_capacity(words.len());
    for w in words {
        let gr = &groups[w.group];
        // Wobble: the group pops with its first word's timing (transform-origin: centre).
        let wob = if cfg.wobble && gr.wobbles && lit.live && lit.exit.is_none() {
            let first = &words[gr.first];
            let (tx, sx) = wobble(lit.t - first.start);
            let ws = &words[gr.words.clone()];
            let l = ws.iter().map(|w| w.pos.x).fold(f32::INFINITY, f32::min);
            let r = ws.iter().map(|w| w.pos.x + w.w).fold(f32::NEG_INFINITY, f32::max);
            ((sx - 1.0).abs() > 1e-4 || tx.abs() > 1e-4).then_some(((l + r) * 0.5, tx * first.font.size, sx))
        } else {
            None
        };
        let quads = word_glyphs(ctx, w, lit, cfg, wob, atlas);
        let dim_c = if w.bg { dim * 0.78 } else { dim } * opacity;
        for q in &quads {
            push_quad(&mut base, xf.rect(q.rect), q.uv, theme::with_alpha(Color32::WHITE, dim_c), &|_| 1.0, &no_cut);
        }
        per_word.push((quads, wob));
    }
    // The dim run, then the highlight run over it (`.blyrics-highlight-run`, z-index 1):
    // each word's drop-shadow, then its lit text.
    painter.add(Shape::mesh(base));
    for (wi, (w, (quads, wob))) in words.iter().zip(per_word).enumerate() {
        let look = word_look(w, lit, cfg);
        if look.opacity <= 0.0 || look.end <= 0.0 {
            continue;
        }
        let word_box = {
            let (cx, tx, sx) = wob.unwrap_or((0.0, 0.0, 1.0));
            let f = |x: f32| cx + (x - cx) * sx + tx;
            xf.rect(Rect::from_min_size(pos2(f(w.pos.x), w.pos.y), vec2(w.w * sx, w.h)))
        };
        let mut m = Mesh::with_texture(TextureId::default());
        let color = theme::with_alpha(cfg.active, look.opacity * opacity);
        let mut screen_quads = Vec::with_capacity(quads.len());
        for q in &quads {
            let r = xf.rect(q.rect);
            let (alpha, cuts) = lit_gradient(&r, q.letter, q.letters, word_box, &look, w.letters, w.rtl);
            push_quad(&mut m, r, q.uv, color, alpha.as_ref(), &cuts);
            screen_quads.push((r, q.uv, q.letter, q.letters, 1.0));
        }
        if look.glow > 0.05 && look.glow_alpha >= GLOW_INVISIBLE && !dev_off("glow") {
            glow.push(GlowJob {
                key: (key.0, key.1, key.2, wi as u32),
                quads: screen_quads,
                word: word_box,
                by_letter: w.letters,
                rtl: w.rtl,
                radius: look.glow * xf.scale,
                pills: Vec::new(),
                color: theme::with_alpha(cfg.glow, look.glow_alpha * look.opacity * opacity),
                look,
                layer: painter.add(Shape::Noop),
            });
        }
        if !m.is_empty() {
            painter.add(Shape::mesh(m));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_words_kinetic(
    painter: &egui::Painter,
    ctx: &egui::Context,
    cfg: &Cfg,
    words: &[Word],
    xf: &Xf,
    lit: &LitState,
    dim: f32,
    opacity: f32,
    atlas: [usize; 2],
    glow: &mut Vec<GlowJob>,
    key: (u64, usize, u32),
    animating: &mut bool,
) {
    let no_cut: [f32; 0] = [];
    if !lit.live {
        let mut base = Mesh::with_texture(TextureId::default());
        for w in words {
            let quads = word_glyphs(ctx, w, lit, cfg, None, atlas);
            let dim_c = if w.bg { dim * 0.78 } else { dim } * opacity;
            let c = if lit.t < w.start {
                theme::with_alpha(Color32::WHITE, dim_c)
            } else {
                theme::with_alpha(Color32::WHITE, (dim_c * 1.5).min(opacity * 0.85))
            };
            for q in &quads {
                push_quad(&mut base, xf.rect(q.rect), q.uv, c, &|_| 1.0, &no_cut);
            }
        }
        painter.add(Shape::mesh(base));
        return;
    }

    let mut base_mesh = Mesh::with_texture(TextureId::default());

    for (wi, w) in words.iter().enumerate() {
        let quads = word_glyphs(ctx, w, lit, cfg, None, atlas);
        let word_c = pos2(w.pos.x + w.w * 0.5, w.pos.y + w.h * 0.5);
        let dim_c = if w.bg { dim * 0.78 } else { dim } * opacity;

        // Case 1: Upcoming word in this live line -> render in standby dimmed state so it never vanishes
        if !w.line_synced && lit.t < w.start {
            let standby_color = theme::with_alpha(Color32::WHITE, dim_c);
            for q in &quads {
                push_quad(&mut base_mesh, xf.rect(q.rect), q.uv, standby_color, &|_| 1.0, &no_cut);
            }
            continue;
        }

        // Case 2: Active or passed word
        let dt = (lit.t - w.start).max(0.0) as f32;
        let is_popping = !w.line_synced && dt < 0.18;
        if is_popping {
            *animating = true;
        }

        let (pop_scale, pop_dy, pop_alpha) = if is_popping {
            let p = (dt / 0.18).clamp(0.0, 1.0);
            let s = if p < 0.50 {
                egui::lerp(1.0..=1.16, p / 0.50)
            } else {
                egui::lerp(1.16..=1.0, (p - 0.50) / 0.50)
            };
            let dy = (1.0 - p) * -3.5;
            let a = egui::lerp(dim_c..=1.0, (p / 0.45).min(1.0));
            (s, dy, a)
        } else {
            (1.0, 0.0, 1.0)
        };

        let active = if w.line_synced { true } else { dt <= w.dur.max(0.12) as f32 };
        if active {
            *animating = true;
        }

        let text_color = if active {
            Color32::WHITE
        } else {
            theme::with_alpha(Color32::WHITE, 0.90)
        };
        let color = theme::with_alpha(text_color, pop_alpha * opacity);

        let mut m = Mesh::with_texture(TextureId::default());
        let mut screen_quads = Vec::with_capacity(quads.len());
        for q in &quads {
            let r_local = Rect::from_min_max(
                word_c + (q.rect.min - word_c) * pop_scale,
                word_c + (q.rect.max - word_c) * pop_scale,
            ).translate(vec2(0.0, pop_dy));
            let r_screen = xf.rect(r_local);
            push_quad(&mut m, r_screen, q.uv, color, &|_| 1.0, &no_cut);
            screen_quads.push((r_screen, q.uv, q.letter, q.letters, 1.0));
        }
        painter.add(Shape::mesh(m));

        if active && !dev_off("glow") {
            let word_box = xf.rect(Rect::from_min_size(w.pos, vec2(w.w, w.h)));
            let look = WordLook {
                start: 0.0,
                end: 1.0,
                glow: 16.0 * xf.scale,
                glow_alpha: 0.95,
                opacity: pop_alpha,
            };
            glow.push(GlowJob {
                key: (key.0, key.1, key.2, wi as u32),
                quads: screen_quads,
                word: word_box,
                by_letter: false,
                rtl: w.rtl,
                radius: 16.0 * xf.scale,
                pills: Vec::new(),
                color: theme::with_alpha(cfg.glow, 0.85 * pop_alpha * opacity),
                look,
                layer: painter.add(Shape::Noop),
            });
        }
    }

    if !base_mesh.is_empty() {
        painter.add(Shape::mesh(base_mesh));
    }
}

fn paint_constellation_view(
    painter: &egui::Painter,
    ctx: &egui::Context,
    cfg: &Cfg,
    f: &Frame,
    layout: &Layout,
    t: f64,
    rect: Rect,
) -> Out {
    let lines_n = layout.blocks.iter().filter(|b| b.line.is_some()).count();
    if lines_n == 0 {
        return Out { seek: None, animating: false, user_scrolling: false, ..Default::default() };
    }

    let active_idx = (0..lines_n)
        .find(|&i| t >= layout.blocks[i].start && t < layout.blocks[i].start + layout.blocks[i].dur)
        .or_else(|| (0..lines_n).find(|&i| t < layout.blocks[i].start))
        .unwrap_or(lines_n.saturating_sub(1));

    let mut seek = None;
    let pointer = ctx.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p));
    let clicked = ctx.input(|i| i.pointer.primary_clicked());

    // Context: Previous line at the top
    if active_idx > 0 {
        let prev_b = &layout.blocks[active_idx - 1];
        let prev_text: String = prev_b.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join("");
        if !prev_text.trim().is_empty() {
            let prev_rect = Rect::from_center_size(pos2(rect.center().x, rect.top() + 32.0), vec2(rect.width() - 80.0, 36.0));
            let hover = pointer.is_some_and(|p| prev_rect.contains(p));
            if hover {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                if clicked {
                    seek = Some(prev_b.start);
                }
            }
            let font = theme::lyric_font_for(&cfg.font, Weight::Regular, 14.5, &prev_text);
            let col = theme::with_alpha(Color32::WHITE, if hover { 0.75 } else { 0.30 });
            painter.text(prev_rect.center(), egui::Align2::CENTER_CENTER, prev_text, font, col);
        }
    }

    // Context: Next line at the bottom
    if active_idx + 1 < lines_n {
        let next_b = &layout.blocks[active_idx + 1];
        let next_text: String = next_b.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join("");
        if !next_text.trim().is_empty() {
            let next_rect = Rect::from_center_size(pos2(rect.center().x, rect.bottom() - 36.0), vec2(rect.width() - 80.0, 36.0));
            let hover = pointer.is_some_and(|p| next_rect.contains(p));
            if hover {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                if clicked {
                    seek = Some(next_b.start);
                }
            }
            let font = theme::lyric_font_for(&cfg.font, Weight::Regular, 14.5, &next_text);
            let col = theme::with_alpha(Color32::WHITE, if hover { 0.75 } else { 0.30 });
            painter.text(next_rect.center(), egui::Align2::CENTER_CENTER, next_text, font, col);
        }
    }

    let b = &layout.blocks[active_idx];
    let words: Vec<&Word> = b.words.iter().filter(|w| !w.bg).collect();
    let n = words.len();

    if n == 0 {
        let center = rect.center();
        let pulse = (f.now * 2.0).sin().abs() as f32;
        painter.circle_filled(center, 22.0 + pulse * 10.0, Color32::from_rgba_unmultiplied(255, 105, 180, 40));
        painter.circle_filled(center, 6.0, Color32::WHITE);
        let label = if b.instrumental { "Instrumental" } else { "• • •" };
        let font = theme::lyric_font_for(&cfg.font, Weight::Semibold, 16.0, label);
        painter.text(center + vec2(0.0, 26.0), egui::Align2::CENTER_TOP, label, font, theme::with_alpha(Color32::WHITE, 0.6));
        return Out { seek, animating: true, user_scrolling: false, ..Default::default() };
    }

    let bounds = rect.shrink2(vec2(60.0, 110.0));
    let mut node_positions = Vec::with_capacity(n);
    for i in 0..n {
        let x = if n == 1 {
            bounds.center().x
        } else {
            bounds.left() + (i as f32 / (n - 1) as f32) * bounds.width()
        };
        let alt = if i % 2 == 0 { -1.0 } else { 1.0 };
        let wave = (i as f32 * 1.85 + (f.key % 19) as f32 * 0.1).sin();
        let y_offset = (alt * 0.55 + wave * 0.45) * (bounds.height() * 0.35);
        let y = bounds.center().y + y_offset;
        node_positions.push(pos2(x, y));
    }

    // Constellation lines
    for i in 0..n.saturating_sub(1) {
        let p0 = node_positions[i];
        let p1 = node_positions[i + 1];
        painter.line_segment([p0, p1], egui::Stroke::new(3.0, Color32::from_rgba_unmultiplied(255, 110, 195, 45)));
        painter.line_segment([p0, p1], egui::Stroke::new(1.2, Color32::from_rgba_unmultiplied(255, 215, 240, 130)));
    }

    // Star nodes & words
    for i in 0..n {
        let w = words[i];
        let p = node_positions[i];
        let sung = t >= w.start;
        let active = sung && (t <= w.start + w.dur.max(0.18));

        if pointer.is_some_and(|ptr| ptr.distance(p) < 28.0) {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            if clicked {
                seek = Some(w.start);
            }
        }

        if !sung {
            painter.circle_filled(p, 3.0, Color32::from_rgba_unmultiplied(255, 150, 200, 100));
        } else {
            painter.circle_filled(p, 9.0, Color32::from_rgba_unmultiplied(255, 100, 190, 80));
            painter.circle_filled(p, 3.5, Color32::WHITE);
            let flare_r = if active { 7.0 } else { 4.0 };
            painter.line_segment([p - vec2(flare_r, 0.0), p + vec2(flare_r, 0.0)], egui::Stroke::new(1.0, Color32::WHITE));
            painter.line_segment([p - vec2(0.0, flare_r), p + vec2(0.0, flare_r)], egui::Stroke::new(1.0, Color32::WHITE));
        }

        let is_above = p.y >= bounds.center().y;
        let text_pos = if is_above {
            pos2(p.x, p.y - 18.0)
        } else {
            pos2(p.x, p.y + 18.0)
        };
        let align = if is_above { egui::Align2::CENTER_BOTTOM } else { egui::Align2::CENTER_TOP };

        let font_size = (cfg.size * 0.85).clamp(18.0, 34.0);
        let font = theme::lyric_font_for(&cfg.font, if active { Weight::Bold } else { Weight::Semibold }, font_size, &w.text);

        if active {
            painter.text(text_pos + vec2(0.0, 1.0), align, &w.text, font.clone(), Color32::from_rgba_unmultiplied(255, 90, 180, 160));
            painter.text(text_pos, align, &w.text, font, Color32::WHITE);
        } else if sung {
            painter.text(text_pos, align, &w.text, font, theme::with_alpha(Color32::WHITE, 0.88));
        } else {
            painter.text(text_pos, align, &w.text, font, theme::with_alpha(Color32::WHITE, 0.30));
        }
    }

    // Traveling Star Cursor
    let (star_pos, star_frac) = if t <= words[0].start {
        (node_positions[0], 0.0)
    } else if t >= words[n - 1].start + words[n - 1].dur {
        (node_positions[n - 1], 1.0)
    } else {
        let mut k = 0;
        for idx in 0..n {
            if words[idx].start <= t {
                k = idx;
            } else {
                break;
            }
        }
        let t0 = words[k].start;
        let t1 = if k + 1 < n { words[k + 1].start } else { words[k].start + words[k].dur.max(0.2) };
        let raw_frac = ((t - t0) / (t1 - t0).max(0.01)).clamp(0.0, 1.0) as f32;
        let s = raw_frac * raw_frac * (3.0 - 2.0 * raw_frac);
        let p0 = node_positions[k];
        let p1 = if k + 1 < n { node_positions[k + 1] } else { p0 };
        (pos2(egui::lerp(p0.x..=p1.x, s), egui::lerp(p0.y..=p1.y, s)), raw_frac)
    };

    painter.circle_filled(star_pos, 22.0, Color32::from_rgba_unmultiplied(255, 60, 170, 50));
    painter.circle_filled(star_pos, 11.0, Color32::from_rgba_unmultiplied(255, 120, 210, 120));
    painter.circle_filled(star_pos, 4.0, Color32::WHITE);
    painter.rect_filled(Rect::from_center_size(star_pos, vec2(36.0, 1.8)), 0.9, Color32::from_rgba_unmultiplied(255, 235, 250, 230));
    painter.rect_filled(Rect::from_center_size(star_pos, vec2(1.8, 36.0)), 0.9, Color32::from_rgba_unmultiplied(255, 235, 250, 230));
    painter.circle_filled(star_pos, 6.0, Color32::from_rgba_unmultiplied(255, 200, 240, 160));

    if star_frac < 0.25 {
        let r_rip = (star_frac / 0.25) * 22.0;
        let a_rip = (1.0 - star_frac / 0.25) * 0.65;
        painter.circle_stroke(star_pos, r_rip, egui::Stroke::new(1.5, theme::with_alpha(Color32::from_rgb(255, 105, 180), a_rip)));
    }

    if let Some(tr) = f.trans.get(active_idx).copied().flatten() {
        let tr_rect = Rect::from_center_size(pos2(rect.center().x, bounds.bottom() + 20.0), vec2(rect.width() - 80.0, 28.0));
        let tr_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 14.5, tr);
        painter.text(tr_rect.center(), egui::Align2::CENTER_CENTER, tr, tr_font, theme::with_alpha(Color32::WHITE, 0.75));
    }

    Out {
        seek,
        animating: true,
        user_scrolling: false,
        ..Default::default()
    }
}

fn paint_convex_lens_view(
    glow: &mut Glow,
    painter: &egui::Painter,
    ctx: &egui::Context,
    cfg: &Cfg,
    f: &Frame,
    layout: &Layout,
    t: f64,
    rect: Rect,
    atlas: [usize; 2],
    ppp: f32,
) -> Out {
    struct LensWord {
        text: String,
        start: f64,
        dur: f64,
        line_idx: usize,
    }

    let mut all_words: Vec<LensWord> = Vec::new();
    for (li, b) in layout.blocks.iter().enumerate() {
        if b.line.is_none() || b.instrumental {
            continue;
        }
        if !b.groups.is_empty() {
            for g in &b.groups {
                let slice = &b.words[g.words.clone()];
                let non_bg: Vec<&Word> = slice.iter().filter(|w| !w.bg).collect();
                if non_bg.is_empty() {
                    continue;
                }
                let text: String = non_bg.iter().map(|w| w.text.as_str()).collect();
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                let start = non_bg.first().map(|w| w.start).unwrap_or(b.start);
                let end = non_bg.last().map(|w| w.start + w.dur.max(0.0)).unwrap_or(start + 0.3);
                let dur = (end - start).max(0.1);
                all_words.push(LensWord {
                    text,
                    start,
                    dur,
                    line_idx: li,
                });
            }
        } else {
            for w in b.words.iter().filter(|w| !w.bg) {
                let text = w.text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                all_words.push(LensWord {
                    text,
                    start: w.start,
                    dur: w.dur.max(0.1),
                    line_idx: li,
                });
            }
        }
    }

    let mut seek = None;
    let pointer = ctx.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p));
    let clicked = ctx.input(|i| i.pointer.primary_clicked());
    let center = rect.center();

    if all_words.is_empty() {
        let pulse = (f.now * 2.0).sin().abs() as f32;
        painter.circle_filled(center, 24.0 + pulse * 8.0, Color32::from_rgba_unmultiplied(255, 180, 50, 45));
        painter.circle_filled(center, 6.0, Color32::WHITE);
        let label = "Instrumental";
        let font = theme::lyric_font_for(&cfg.font, Weight::Semibold, 16.0, label);
        painter.text(center + vec2(0.0, 32.0), egui::Align2::CENTER_TOP, label, font, theme::with_alpha(Color32::WHITE, 0.65));
        return Out { seek: None, animating: true, user_scrolling: false, ..Default::default() };
    }

    let n = all_words.len();
    let mut k = 0;
    for (idx, w) in all_words.iter().enumerate() {
        if t >= w.start {
            k = idx;
        } else {
            break;
        }
    }

    // Hero active size and slot height (massive, high-impact focus typography)
    let hero_size = ((cfg.size * 5.0).clamp(118.0, 210.0)).min(rect.height() * 0.36).round();
    let slot_size = (hero_size * 0.50).round();
    let slot_h = hero_size * 1.15;

    let curr = &all_words[k];

    // Helper: Ken Perlin's Smootherstep (zero velocity and zero acceleration at endpoints)
    let smootherstep = |p: f32| -> f32 {
        let p = p.clamp(0.0, 1.0);
        p * p * p * (p * (p * 6.0 - 15.0) + 10.0)
    };

    // Slide transition calculation (moves upward from bottom to center)
    let (slide, is_transitioning) = if t < all_words[0].start {
        // Pre-song lead-in: word 0 slides up into center just before singing begins
        let lead = (all_words[0].start - t) as f32;
        if lead <= 0.40 {
            let p = ((0.40 - lead) / 0.40).clamp(0.0, 1.0);
            (smootherstep(p), true)
        } else {
            (0.0, false)
        }
    } else {
        // Active word transition duration: luxurious fluid glide (0.28s to 0.38s)
        let tau = (curr.dur * 0.48).clamp(0.28, 0.38) as f32;
        let elapsed = (t - curr.start).max(0.0) as f32;
        let p = (elapsed / tau).clamp(0.0, 1.0);
        (smootherstep(p), p < 1.0)
    };

    // Vertical animation offset: when slide goes from 0 -> 1, words move UP by slot_h
    let anim_dy = (1.0 - slide) * slot_h;

    // Render candidate words in the visible focal window [k-2 ..= k+2]
    let start_i = (k as isize - 2).max(0) as usize;
    let end_i = ((k + 2) as usize).min(n - 1);

    for i in start_i..=end_i {
        let w = &all_words[i];
        let rel_idx = i as isize - k as isize;
        let y = center.y + (rel_idx as f32) * slot_h + anim_dy;

        // Relative distance from canvas center in units of slot_h
        let dist = (y - center.y) / slot_h;
        let abs_d = dist.abs();

        if abs_d >= 1.85 {
            continue;
        }

        // Compute size, alpha, and blur radius continuously
        let (target_size, mut alpha, blur_r) = if abs_d <= 1.0 {
            let factor = (1.0 - abs_d).clamp(0.0, 1.0);
            let s = egui::lerp(slot_size..=hero_size, factor).round();
            let a = egui::lerp(0.35..=1.0, factor);
            let b = (1.0 - factor) * 2.5;
            (s, a, b)
        } else {
            let fade = ((1.85 - abs_d) / 0.85).clamp(0.0, 1.0);
            (slot_size, 0.35 * fade, 2.5)
        };

        // Click-to-seek hit detection
        let hit_box = Rect::from_center_size(pos2(center.x, y), vec2(rect.width() - 40.0, slot_h * 0.85));
        let is_hover = pointer.is_some_and(|p| hit_box.contains(p));
        if is_hover && abs_d <= 1.25 {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            alpha = (alpha * 1.35).min(1.0);
            if clicked {
                seek = Some(w.start);
            }
        }

        if alpha < 0.01 {
            continue;
        }

        // Dynamic horizontal fit so long words don't clip
        let mut cur_size = target_size;
        let probe_font = theme::lyric_font_for(&cfg.font, Weight::Bold, cur_size, &w.text);
        let text_w = painter.layout_no_wrap(w.text.clone(), probe_font, Color32::WHITE).size().x;
        let max_w = (rect.width() - 48.0).max(40.0);
        if text_w > max_w {
            cur_size = (cur_size * (max_w / text_w)).clamp(22.0, target_size).round();
        }

        let font = theme::lyric_font_for(&cfg.font, Weight::Bold, cur_size, &w.text);

        // Gaussian drop-shadow glow matching Kinetic Pop fidelity (cached per word for 60 FPS)
        let glow_strength = ((0.65 - abs_d) / 0.65).clamp(0.0, 1.0);
        let total_glow = glow_strength * glow_strength;
        if total_glow > 0.05 && !dev_off("glow") {
            let glow_radius = (hero_size * 0.16).clamp(16.0, 28.0);
            let hero_font = theme::lyric_font_for(&cfg.font, Weight::Bold, hero_size, &w.text);
            if let Some((tex_id, content_rect, uv)) = glow.focal_word_glow(ctx, (f.key, i), &w.text, hero_font, glow_radius, atlas, ppp) {
                let glow_base = if cfg.glow.a() > 20 {
                    cfg.glow
                } else if f.accent.a() > 20 {
                    f.accent
                } else {
                    cfg.active
                };
                let glow_col = theme::with_alpha(glow_base, 0.90 * total_glow * alpha);
                let scale = cur_size / hero_size;
                let dest = Rect::from_center_size(pos2(center.x, y), content_rect.size() * scale);
                painter.image(tex_id, dest, uv, glow_col);
            }
        }

        // Draw text: multi-tap optical blur for outer slots, sharp 100% white for center
        if blur_r > 0.6 {
            let blur_taps = [
                vec2(-blur_r, 0.0),
                vec2(blur_r, 0.0),
                vec2(0.0, -blur_r),
                vec2(0.0, blur_r),
            ];
            let tap_alpha = (alpha * 0.25).clamp(0.005, 1.0);
            let tap_color = theme::with_alpha(Color32::WHITE, tap_alpha);
            for &offset in &blur_taps {
                painter.text(pos2(center.x, y) + offset, egui::Align2::CENTER_CENTER, &w.text, font.clone(), tap_color);
            }
        } else {
            painter.text(pos2(center.x, y), egui::Align2::CENTER_CENTER, &w.text, font, theme::with_alpha(Color32::WHITE, alpha));
        }
    }

    // Translation or Romanization below the focal area if enabled
    if let Some(tr) = f.trans.get(curr.line_idx).copied().flatten() {
        let tr_y = (center.y + slot_h * 1.58).min(rect.bottom() - 28.0);
        let tr_rect = Rect::from_center_size(pos2(center.x, tr_y), vec2(rect.width() - 80.0, 32.0));
        let tr_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 16.0, tr);
        painter.text(tr_rect.center(), egui::Align2::CENTER_CENTER, tr, tr_font, theme::with_alpha(Color32::WHITE, 0.70));
    }

    // Native monitor refresh rate sync (120Hz/144Hz/VSync) during playback or slide transition
    if f.playing || is_transitioning {
        ctx.request_repaint();
    }

    Out {
        seek,
        animating: f.playing || is_transitioning,
        user_scrolling: false,
        ..Default::default()
    }
}

// ------------------------------------------------------------------ physics throw

fn paint_physics_throw_view(
    glow: &mut Glow,
    painter: &egui::Painter,
    ctx: &egui::Context,
    cfg: &Cfg,
    f: &Frame,
    layout: &Layout,
    t: f64,
    rect: Rect,
    atlas: [usize; 2],
    ppp: f32,
) -> Out {
    use std::f32::consts::PI;

    // ── word list (identical builder as ConvexLens) ──────────────────────────
    struct ThrowWord {
        text: String,
        start: f64,
        dur: f64,
        line_idx: usize,
    }

    let mut all_words: Vec<ThrowWord> = Vec::new();
    for (li, b) in layout.blocks.iter().enumerate() {
        if b.line.is_none() || b.instrumental {
            continue;
        }
        if !b.groups.is_empty() {
            for g in &b.groups {
                let slice = &b.words[g.words.clone()];
                let non_bg: Vec<&Word> = slice.iter().filter(|w| !w.bg).collect();
                if non_bg.is_empty() {
                    continue;
                }
                let text: String = non_bg.iter().map(|w| w.text.as_str()).collect();
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                let start = non_bg.first().map(|w| w.start).unwrap_or(b.start);
                let end = non_bg.last().map(|w| w.start + w.dur.max(0.0)).unwrap_or(start + 0.3);
                let dur = (end - start).max(0.1);
                all_words.push(ThrowWord { text, start, dur, line_idx: li });
            }
        } else {
            for w in b.words.iter().filter(|w| !w.bg) {
                let text = w.text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                all_words.push(ThrowWord {
                    text,
                    start: w.start,
                    dur: w.dur.max(0.1),
                    line_idx: li,
                });
            }
        }
    }

    let mut seek = None;
    let pointer = ctx.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p));
    let clicked = ctx.input(|i| i.pointer.primary_clicked());
    let center = rect.center();

    // ── empty / instrumental placeholder ────────────────────────────────────
    if all_words.is_empty() {
        let pulse = (f.now * 2.0).sin().abs() as f32;
        painter.circle_filled(center, 24.0 + pulse * 8.0, Color32::from_rgba_unmultiplied(80, 200, 120, 45));
        painter.circle_filled(center, 6.0, Color32::WHITE);
        let label = "Instrumental";
        let font = theme::lyric_font_for(&cfg.font, Weight::Semibold, 16.0, label);
        painter.text(center + vec2(0.0, 32.0), egui::Align2::CENTER_TOP, label, font, theme::with_alpha(Color32::WHITE, 0.65));
        return Out { seek: None, animating: true, user_scrolling: false, ..Default::default() };
    }

    // ── find current word index k (last word whose start <= t) ────────────────
    let n = all_words.len();
    let mut k = 0usize;
    for (idx, w) in all_words.iter().enumerate() {
        if t >= w.start {
            k = idx;
        } else {
            break;
        }
    }

    // ── font size (identical to ConvexLens) ──────────────────────────────────
    let hero_size = ((cfg.size * 5.0).clamp(118.0, 210.0)).min(rect.height() * 0.36).round();

    // ── physics constants ─────────────────────────────────────────────────────
    const ENTRY_DUR: f64 = 0.42;   // seconds: word is thrown upward
    const G_FALL:    f32 = 2800.0; // px/s² gravity acceleration during free-fall

    // Helper: derive per-word deterministic signature from text hash
    let word_hash = |text: &str| -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in text.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    };

    struct ActiveItem {
        text: String,
        font: egui::FontId,
        cur_size: f32,
        hero_size: f32,
        pos: Pos2,
        roll_rad: f32,
        yaw_rad: f32,
        pitch_rad: f32,
        glow_factor: f32,
        word_idx: usize,
        is_hoverable: bool,
        start_time: f64,
    }

    let mut active_items: Vec<ActiveItem> = Vec::new();
    let center_x = rect.center().x;
    let center_y = rect.center().y;

    // ── Candidate window around k: [k - 6 ..= k + 6] ─────────────────────────
    let scan_start = k.saturating_sub(6);
    let scan_end = (k + 6).min(n);

    // 5 distinct spatial sectors across the canvas (kiri atas, kanan bawah, tengah, kiri bawah, kanan atas)
    const SECTORS: [(f32, f32); 5] = [
        (0.25, 0.26), // 0: Top-Left (agak kiri atas)
        (0.75, 0.72), // 1: Bottom-Right (agak kanan bawah)
        (0.50, 0.46), // 2: Center/Mid (tengah layar)
        (0.26, 0.70), // 3: Bottom-Left (agak kiri bawah)
        (0.74, 0.28), // 4: Top-Right (agak kanan atas)
    ];

    for i in scan_start..scan_end {
        let w = &all_words[i];
        let hash = word_hash(&w.text);

        // ── 1. Dynamic 2D spatial area distribution ───────────────────────────
        let (base_sx, base_sy) = SECTORS[i % 5];
        // Organic pseudo-jitter within each sector (±13% X, ±11% Y)
        let j_x = (((hash >> 13) % 1000) as f32 / 1000.0 * 2.0 - 1.0) * 0.13;
        let j_y = (((hash >> 21) % 1000) as f32 / 1000.0 * 2.0 - 1.0) * 0.11;
        let norm_x = (base_sx + j_x).clamp(0.12, 0.88);
        let norm_y = (base_sy + j_y).clamp(0.12, 0.88);

        // Safe coordinates within rect
        let min_peak_y = rect.top() + hero_size * 0.75;
        let max_peak_y = rect.bottom() - hero_size * 0.85;
        let y_peak = (rect.top() + norm_y * rect.height()).clamp(min_peak_y, max_peak_y);

        // Compute probe font & width for horizontal fitting & clamping
        let mut cur_size = hero_size;
        let probe_font = theme::lyric_font_for(&cfg.font, Weight::Bold, cur_size, &w.text);
        let text_w = painter.layout_no_wrap(w.text.clone(), probe_font, Color32::WHITE).size().x;
        let max_w = (rect.width() - 48.0).max(40.0);
        if text_w > max_w {
            cur_size = (cur_size * (max_w / text_w)).clamp(22.0, hero_size).round();
        }
        let font = theme::lyric_font_for(&cfg.font, Weight::Bold, cur_size, &w.text);

        let min_x = rect.left() + text_w * 0.5 + 20.0;
        let max_x = rect.right() - text_w * 0.5 - 20.0;
        let target_x = rect.left() + norm_x * rect.width();
        let x_peak = if max_x > min_x {
            target_x.clamp(min_x, max_x)
        } else {
            center_x
        };

        // 2D Launch Origin: arcs naturally from bottom towards x_peak
        let launch_drift_x = (x_peak - center_x) * 0.22;
        let x_launch = x_peak - launch_drift_x;
        let y_launch = rect.bottom() + hero_size + 40.0;

        // ── 2. 3D Rotational momentum profiles (Roll, Yaw, Pitch) ────────────
        let base_spin = ((hash >> 7) % 1000) as f32 / 1000.0 * 2.0 - 1.0;     // -1.0 to +1.0
        let base_yaw = ((hash >> 15) % 1000) as f32 / 1000.0 * 2.0 - 1.0;    // -1.0 to +1.0
        let base_pitch = ((hash >> 23) % 1000) as f32 / 1000.0 * 2.0 - 1.0;  // -1.0 to +1.0

        // Roll (Z-axis tilt in screen plane)
        let rot_launch_deg = base_spin * 14.0;                             // initial throw roll tilt (-14° to +14°)
        let rot_rest_deg = base_spin * 4.0;                                // natural resting roll tilt in air (-4° to +4°)
        let wobble_extra = (((hash >> 20) % 500) as f32 / 500.0 * 2.0 - 1.0) * 3.0;

        // Yaw (Y-axis 3D rotation, viewing side extrusion walls)
        let yaw_launch_deg = base_yaw * 16.0;                              // twist during launch (-16° to +16°)
        let yaw_rest_deg = base_yaw * 6.0;                                 // slight natural angle in air (-6° to +6°)

        // Pitch (X-axis 3D tilt, viewing top/bottom extrusion)
        let pitch_launch_deg = -12.0 + base_pitch * 5.0;                   // thrown leaning slightly back (-17° to -7°)
        let pitch_rest_deg = base_pitch * 3.0;                             // resting pitch (-3° to +3°)

        let t_launch = w.start - ENTRY_DUR;
        let t_apex = w.start;
        let t_end = w.start + w.dur;

        // ── 3. Calculate 3D physics trajectory (with weightless apex float) ──
        let (x_pos, y_pos, roll_rad, yaw_rad, pitch_rad, glow_factor, is_hoverable) = if t < t_launch {
            // Not yet thrown — deep below screen
            continue;
        } else if t < t_apex {
            // ── PHASE 1: BALLISTIC THROW UPWARD (Deceleration into Weightlessness)
            let p = ((t - t_launch) / ENTRY_DUR).clamp(0.0, 1.0) as f32;
            // Physical ballistic curve: f(p) = 2p - p²
            // Speed smoothly decelerates to zero at apex
            let ballistic_p = 2.0 * p - p * p;
            let y = y_launch + (y_peak - y_launch) * ballistic_p;
            let x = x_launch + (x_peak - x_launch) * p;

            // 3D Rotations develop with momentum and ease towards resting orientation
            let rot_flight = rot_launch_deg * (1.0 - p) + rot_rest_deg * p + wobble_extra * (p * PI).sin() * (1.0 - p * 0.5);
            let yaw_flight = yaw_launch_deg * (1.0 - p) + yaw_rest_deg * p;
            let pitch_flight = pitch_launch_deg * (1.0 - p) + pitch_rest_deg * p;

            let roll_rad = rot_flight * PI / 180.0;
            let yaw_rad = yaw_flight * PI / 180.0;
            let pitch_rad = pitch_flight * PI / 180.0;
            let glow = p * p;
            (x, y, roll_rad, yaw_rad, pitch_rad, glow, false)
        } else if t < t_end {
            // ── PHASE 2: WEIGHTLESS APEX FLOAT & DRIFT (Natural momentum subsiding)
            let t_hold = (t - t_apex) as f32;
            let settle_damp = (-t_hold * 3.5).exp();
            // Gentle continuous microgravity breathing & subtle drift
            let float_y = (t_hold * 1.8).sin() * 2.2;
            let float_x = (t_hold * 1.4).sin() * 1.5;
            let y = y_peak + (launch_drift_x * 0.04) * settle_damp + float_y;
            let x = x_peak + float_x;

            // Settle into natural 3D floating posture with subtle ambient swaying
            let rot_settle = rot_rest_deg + wobble_extra * 0.35 * settle_damp * (t_hold * 6.0).sin();
            let yaw_settle = yaw_rest_deg + (t_hold * 1.5).sin() * 1.5 * settle_damp;
            let pitch_settle = pitch_rest_deg + (t_hold * 1.7).cos() * 1.2 * settle_damp;

            let roll_rad = rot_settle * PI / 180.0;
            let yaw_rad = yaw_settle * PI / 180.0;
            let pitch_rad = pitch_settle * PI / 180.0;
            (x, y, roll_rad, yaw_rad, pitch_rad, 1.0_f32, true)
        } else {
            // ── PHASE 3: FREE-FALL GRAVITATIONAL ACCELERATION ─────────────────
            // Exactly when duration finishes, gravity smoothly plunges word downward
            let t_exit = (t - t_end) as f32;
            let y = y_peak + 0.5 * G_FALL * t_exit * t_exit;
            let x = x_peak + (base_spin * 24.0 * t_exit);

            // Aerodynamic drag: tips forward and yaws as it plunges downward
            let fall_rot_deg = rot_rest_deg + base_spin * (t_exit * 20.0 + t_exit * t_exit * 18.0);
            let fall_pitch_deg = pitch_rest_deg + t_exit * 32.0;
            let fall_yaw_deg = yaw_rest_deg + base_yaw * (t_exit * 16.0);

            let roll_rad = fall_rot_deg.clamp(-40.0, 40.0) * PI / 180.0;
            let yaw_rad = fall_yaw_deg.clamp(-45.0, 45.0) * PI / 180.0;
            let pitch_rad = fall_pitch_deg.clamp(-30.0, 50.0) * PI / 180.0;
            let glow = (1.0 - t_exit * 3.2).max(0.0);
            (x, y, roll_rad, yaw_rad, pitch_rad, glow, false)
        };

        // ── Strict clipping: skip if completely outside viewport ─────────────
        if y_pos > rect.bottom() + hero_size + 40.0 || y_pos < rect.top() - hero_size - 40.0 {
            continue;
        }

        active_items.push(ActiveItem {
            text: w.text.clone(),
            font,
            cur_size,
            hero_size,
            pos: pos2(x_pos, y_pos),
            roll_rad,
            yaw_rad,
            pitch_rad,
            glow_factor,
            word_idx: i,
            is_hoverable,
            start_time: w.start,
        });
    }

    // ── 4. STABLE FORWARD LAYERING (Newest word strictly on top / foreground) ─
    // Sort ascending by start_time so earlier words are background, newer words are foreground.
    active_items.sort_by(|a, b| a.start_time.partial_cmp(&b.start_time).unwrap_or(std::cmp::Ordering::Equal));

    // Pass 1: Render glows of all active words (underneath all text)
    for item in &active_items {
        if item.glow_factor > 0.04 && !dev_off("glow") {
            let glow_radius = (item.hero_size * 0.16).clamp(16.0, 28.0);
            let hero_font = theme::lyric_font_for(&cfg.font, Weight::Bold, item.hero_size, &item.text);
            if let Some((tex_id, content_rect, uv)) = glow.focal_word_glow(ctx, (f.key, 90000 + item.word_idx), &item.text, hero_font, glow_radius, atlas, ppp) {
                let glow_base = if cfg.glow.a() > 20 {
                    cfg.glow
                } else if f.accent.a() > 20 {
                    f.accent
                } else {
                    Color32::from_rgb(80, 200, 120)
                };
                let glow_col = theme::with_alpha(glow_base, 0.90 * item.glow_factor);
                let scale = item.cur_size / item.hero_size;
                let dest = Rect::from_center_size(item.pos, content_rect.size() * scale);
                painter.image(tex_id, dest, uv, glow_col);
            }
        }
    }

    // Pass 2: Render 3D Volumetric Extruded texts in strict forward order (newest word drawn on top)
    for item in &active_items {
        let galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(item.text.clone(), item.font.clone(), Color32::WHITE));
        let half_size = galley.size() * 0.5;

        struct LocalGlyph {
            rect: Rect,
            uv: Rect,
        }
        let mut glyphs = Vec::with_capacity(item.text.len());
        for row in &galley.rows {
            for gl in &row.row.glyphs {
                let uv = gl.uv_rect;
                if uv.is_nothing() {
                    continue;
                }
                let min_p = row.pos.to_vec2() + gl.pos.to_vec2() + uv.offset - half_size;
                let rect = Rect::from_min_size(pos2(min_p.x, min_p.y), uv.size);
                let uvr = Rect::from_min_max(
                    pos2(uv.min[0] as f32 / atlas[0] as f32, uv.min[1] as f32 / atlas[1] as f32),
                    pos2(uv.max[0] as f32 / atlas[0] as f32, uv.max[1] as f32 / atlas[1] as f32),
                );
                glyphs.push(LocalGlyph { rect, uv: uvr });
            }
        }

        if glyphs.is_empty() {
            continue;
        }

        let (sin_y, cos_y) = item.yaw_rad.sin_cos();
        let (sin_p, cos_p) = item.pitch_rad.sin_cos();
        let (sin_r, cos_r) = item.roll_rad.sin_cos();
        let focal_dist = 850.0_f32;

        let project_3d = |rx: f32, ry: f32, rz: f32| -> Pos2 {
            // 1. Yaw around Y axis
            let x1 = rx * cos_y + rz * sin_y;
            let y1 = ry;
            let z1 = -rx * sin_y + rz * cos_y;

            // 2. Pitch around X axis
            let x2 = x1;
            let y2 = y1 * cos_p - z1 * sin_p;
            let z2 = y1 * sin_p + z1 * cos_p;

            // 3. Roll around Z axis (in-plane tilt)
            let x3 = x2 * cos_r - y2 * sin_r;
            let y3 = x2 * sin_r + y2 * cos_r;
            let z3 = z2;

            // 4. Perspective projection
            let scale = focal_dist / (focal_dist - z3).max(120.0);
            pos2(item.pos.x + x3 * scale, item.pos.y + y3 * scale)
        };

        // Extrusion depth & slices
        let thickness = (item.cur_size * 0.10).clamp(7.0, 15.0);
        const SLICES: usize = 9;

        // Directional light from top-left front (-0.35, -0.55, 0.75)
        let light_dir_factor = ((-item.yaw_rad * 0.5 - item.pitch_rad * 0.6).sin() * 0.5 + 0.5).clamp(0.0, 1.0);

        let has_vibrant_accent = f.accent.a() > 20 && (f.accent.r() > 30 || f.accent.g() > 30 || f.accent.b() > 30);
        let accent_rgb = if has_vibrant_accent {
            (f.accent.r() as f32, f.accent.g() as f32, f.accent.b() as f32)
        } else {
            (115.0, 135.0, 165.0)
        };

        let mut mesh = Mesh::with_texture(TextureId::default());

        for k in 0..SLICES {
            let t_slice = k as f32 / (SLICES - 1) as f32;
            let z_k = -thickness * 0.5 + t_slice * thickness;

            let is_front = k == SLICES - 1;
            let slice_color = if is_front {
                // Front face: pure crisp white
                Color32::WHITE
            } else {
                // Body extrusion slice: depth gradient + directional light shading
                let depth_ambient = 0.22 + 0.46 * t_slice;
                let shade = (depth_ambient + 0.24 * light_dir_factor).clamp(0.18, 0.88);

                let r = (24.0 + (accent_rgb.0 * 0.65 + 35.0) * shade).min(235.0).round() as u8;
                let g = (26.0 + (accent_rgb.1 * 0.65 + 38.0) * shade).min(235.0).round() as u8;
                let b = (32.0 + (accent_rgb.2 * 0.65 + 44.0) * shade).min(245.0).round() as u8;
                Color32::from_rgba_unmultiplied(r, g, b, 255)
            };

            for gl in &glyphs {
                let p0 = project_3d(gl.rect.left(), gl.rect.top(), z_k);
                let p1 = project_3d(gl.rect.right(), gl.rect.top(), z_k);
                let p2 = project_3d(gl.rect.right(), gl.rect.bottom(), z_k);
                let p3 = project_3d(gl.rect.left(), gl.rect.bottom(), z_k);

                let base = mesh.vertices.len() as u32;
                mesh.vertices.push(Vertex { pos: p0, uv: gl.uv.left_top(), color: slice_color });
                mesh.vertices.push(Vertex { pos: p1, uv: gl.uv.right_top(), color: slice_color });
                mesh.vertices.push(Vertex { pos: p2, uv: gl.uv.right_bottom(), color: slice_color });
                mesh.vertices.push(Vertex { pos: p3, uv: gl.uv.left_bottom(), color: slice_color });

                mesh.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }

        painter.add(Shape::mesh(mesh));

        // Click-to-seek when hovering active word
        if item.is_hoverable {
            let hit_box = Rect::from_center_size(item.pos, vec2(rect.width() - 40.0, item.hero_size * 1.1));
            if pointer.is_some_and(|p| hit_box.contains(p)) {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                if clicked {
                    seek = Some(item.start_time);
                }
            }
        }
    }

    // ── Translation below focal area ─────────────────────────────────────────
    let curr = &all_words[k];
    if let Some(tr) = f.trans.get(curr.line_idx).copied().flatten() {
        let tr_y = (rect.bottom() - 36.0).max(center_y + hero_size * 0.85);
        let tr_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 16.0, tr);
        painter.text(pos2(center_x, tr_y), egui::Align2::CENTER_BOTTOM, tr, tr_font, theme::with_alpha(Color32::WHITE, 0.70));
    }

    // ── Native monitor refresh rate sync (120Hz/144Hz/VSync) ─────────────────
    if f.playing {
        ctx.request_repaint();
    }

    Out {
        seek,
        animating: f.playing,
        user_scrolling: false,
        ..Default::default()
    }
}

// ------------------------------------------------------------------ code editor view

fn paint_code_editor_view(
    _glow: &mut Glow,
    painter: &egui::Painter,
    ctx: &egui::Context,
    cfg: &Cfg,
    f: &Frame,
    layout: &Layout,
    t: f64,
    rect: Rect,
    atlas: [usize; 2],
    _ppp: f32,
) -> Out {
    use egui::epaint::Vertex;
    use egui::TextureId;
    use egui::Stroke;
    use std::f32::consts::PI;

    let mut seek = None;
    let clicked = ctx.input(|i| i.pointer.primary_clicked());
    let center = rect.center();

    // Fill background (pitch black IDE canvas)
    painter.rect_filled(rect, 0.0, Color32::from_rgb(9, 10, 13));

    // Empty / instrumental placeholder
    if layout.blocks.is_empty() {
        let pulse = (f.now * 2.0).sin().abs() as f32;
        painter.circle_filled(center, 24.0 + pulse * 8.0, Color32::from_rgba_unmultiplied(255, 51, 85, 40));
        painter.circle_filled(center, 6.0, Color32::WHITE);
        let label = "Instrumental";
        let font = theme::lyric_font_for(&cfg.font, Weight::Semibold, 16.0, label);
        painter.text(center + vec2(0.0, 32.0), egui::Align2::CENTER_TOP, label, font, theme::with_alpha(Color32::WHITE, 0.65));
        return Out { seek: None, animating: true, user_scrolling: false, ..Default::default() };
    }

    // ── Configuration for Code Editor View ────────────────────────────────
    struct CodeEditorConfig {
        tab_bg: Color32,
        tab_accent_top: Color32,
        gutter_text_dim: Color32,
        gutter_text_active: Color32,
        active_line_band: Color32,
        comment_color: Color32,
        keyword_color: Color32,     // Purple #C678DD
        method_color: Color32,      // Cyan/Blue #61AFEF
        obj_color: Color32,         // Coral #E06C75
        gold_color: Color32,        // Gold #E5C07B
        number_color: Color32,      // Orange #D19A66
        punct_color: Color32,       // Silver #ABB2BF
        cursor_red: Color32,        // Bright red block cursor (#ff3355)
        cursor_accent: Color32,     // Orange collaborator line cursor (#ff9900)
        accent_color: Color32,      // Orange collaborator text & tag (#ff9900)
        selection_blue: Color32,    // Blue word completion highlight (#264f78)
        zoom_in_scale: f32,         // 1.85x
        zoom_out_scale: f32,        // 0.95x
        tilt_deg: f32,              // -2.8 deg
    }

    let conf = CodeEditorConfig {
        tab_bg: Color32::from_rgb(14, 15, 19),
        tab_accent_top: Color32::from_rgb(255, 51, 85),
        gutter_text_dim: Color32::from_rgb(60, 68, 80),
        gutter_text_active: Color32::from_rgb(220, 230, 245),
        active_line_band: Color32::from_rgba_unmultiplied(255, 255, 255, 9),
        comment_color: Color32::from_rgb(92, 103, 115),
        keyword_color: Color32::from_rgb(198, 120, 221),
        method_color: Color32::from_rgb(97, 175, 239),
        obj_color: Color32::from_rgb(224, 108, 117),
        gold_color: Color32::from_rgb(229, 192, 123),
        number_color: Color32::from_rgb(209, 154, 102),
        punct_color: Color32::from_rgb(171, 178, 191),
        cursor_red: Color32::from_rgb(255, 51, 85),
        cursor_accent: Color32::from_rgb(255, 153, 0),
        accent_color: Color32::from_rgb(255, 153, 0),
        selection_blue: Color32::from_rgba_unmultiplied(38, 79, 120, 210),
        zoom_in_scale: 1.85,
        zoom_out_scale: 0.95,
        tilt_deg: -2.8,
    };

    let raw_title = f.title.unwrap_or("just us").trim();
    let mut title_slug = String::new();
    for ch in raw_title.chars() {
        if ch.is_alphanumeric() {
            title_slug.push(ch.to_ascii_lowercase());
        } else if ch.is_whitespace() || ch == '-' {
            if !title_slug.ends_with('_') && !title_slug.is_empty() {
                title_slug.push('_');
            }
        }
    }
    if title_slug.is_empty() {
        title_slug = "us".to_string();
    }
    let file_tab_name = format!("{}.js", title_slug);
    let comment_text = format!("// {}", if raw_title.is_empty() { "just us" } else { raw_title });

    struct EditorToken {
        ident: String,
        syntax_suffix: String,
        is_object: bool,
        is_accent: bool,
        start: f64,
        dur: f64,
        char_offset: usize,
    }

    struct EditorLine {
        line_num: usize,
        line_idx: usize,
        tokens: Vec<EditorToken>,
        start: f64,
        end: f64,
    }

    let mut editor_lines: Vec<EditorLine> = Vec::new();

    for (li, b) in layout.blocks.iter().enumerate() {
        if b.line.is_none() || b.instrumental {
            continue;
        }
        let words: Vec<&Word> = b.words.iter().filter(|w| !w.text.trim().is_empty()).collect();
        if words.is_empty() {
            continue;
        }

        let m = words.len();
        let mut tokens: Vec<EditorToken> = Vec::with_capacity(m);
        let mut char_offset = 0usize;

        for (j, w) in words.iter().enumerate() {
            let raw_trimmed = w.text.trim();
            let mut ident = String::new();
            for ch in raw_trimmed.chars() {
                if ch.is_alphanumeric() || ch == '_' || ch == '$' || !ch.is_ascii() {
                    ident.push(ch.to_ascii_lowercase());
                }
            }
            if ident.is_empty() {
                ident = raw_trimmed.to_lowercase();
            }

            let is_object = (j % 2) == 0;
            let is_last = j == m - 1;

            let syntax_suffix = if !is_last {
                if is_object { ".".to_string() } else { "(".to_string() }
            } else {
                if m % 2 == 1 {
                    // Odd word count: last word is object (even index)
                    // Close all opened parens (m / 2) plus semicolon
                    let close_count = m / 2;
                    let mut s = ")".repeat(close_count);
                    s.push(';');
                    s
                } else {
                    // Even word count: last word is method (odd index)
                    // Empty call '()' plus remaining open parens ((m - 2) / 2) plus semicolon
                    let close_count = (m - 2) / 2;
                    let mut s = "()".to_string();
                    s.push_str(&")".repeat(close_count));
                    s.push(';');
                    s
                }
            };

            let is_accent = w.bg
                || raw_trimmed.starts_with('(')
                || raw_trimmed.ends_with(')')
                || ident == "you"
                || ident == "me";

            let token_chars = ident.chars().count() + syntax_suffix.chars().count();
            tokens.push(EditorToken {
                ident,
                syntax_suffix,
                is_object,
                is_accent,
                start: w.start,
                dur: w.dur.max(0.08),
                char_offset,
            });
            char_offset += token_chars;
        }

        let start = tokens.first().map(|tok| tok.start).unwrap_or(b.start);
        let end = tokens.last().map(|tok| tok.start + tok.dur).unwrap_or(start + 0.3);
        let line_num = editor_lines.len() + 3; // line 1: comment, line 2: blank, line 3+: lyrics

        editor_lines.push(EditorLine {
            line_num,
            line_idx: li,
            tokens,
            start,
            end,
        });
    }

    if editor_lines.is_empty() {
        return Out { seek: None, animating: f.playing, user_scrolling: false, ..Default::default() };
    }

    // Base monospace typography scale in editor world coordinates
    let base_font_size = 28.0_f32;
    let base_font = FontId::monospace(base_font_size);
    let sample_galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap("M".to_string(), base_font.clone(), Color32::WHITE));
    let char_w = sample_galley.size().x;
    let line_h = base_font_size * 2.15;

    // Find active line and transition state
    let mut active_line_idx = 0usize;
    let mut in_line_trans = false;
    let mut trans_q = 0.0_f32;
    let num_lines = editor_lines.len();

    if t < editor_lines[0].start {
        // Pre-song lead-in
        active_line_idx = 0;
    } else if t > editor_lines[num_lines - 1].end {
        // Post-song
        active_line_idx = num_lines - 1;
    } else {
        for (idx, line) in editor_lines.iter().enumerate() {
            if t >= line.start && t <= line.end {
                active_line_idx = idx;
                in_line_trans = false;
                break;
            } else if idx + 1 < num_lines && t > line.end && t < editor_lines[idx + 1].start {
                active_line_idx = idx;
                in_line_trans = true;
                let gap = (editor_lines[idx + 1].start - line.end).max(0.10);
                trans_q = ((t - line.end) / gap).clamp(0.0, 1.0) as f32;
                break;
            } else if t >= line.start {
                active_line_idx = idx;
            }
        }
    }

    let curr_line = &editor_lines[active_line_idx];
    let next_line = if active_line_idx + 1 < num_lines {
        &editor_lines[active_line_idx + 1]
    } else {
        curr_line
    };

    let curr_line_y = (curr_line.line_num - 1) as f32 * line_h;
    let next_line_y = (next_line.line_num - 1) as f32 * line_h;

    let mut cursor_char_idx = 0usize;
    let mut cursor_subpixel_x = 0.0_f32;
    let mut cursor_is_accent = false;
    let mut active_token_progress = 0.0_f32;
    let mut active_token_is_active = false;

    for tok in &curr_line.tokens {
        let total_tok_chars = tok.ident.chars().count() + tok.syntax_suffix.chars().count();
        if t >= tok.start + tok.dur {
            // Token is already fully typed
            cursor_char_idx = tok.char_offset + total_tok_chars;
            cursor_subpixel_x = cursor_char_idx as f32 * char_w;
        } else if t >= tok.start {
            // Token is currently being typed
            active_token_is_active = true;
            cursor_is_accent = tok.is_accent;
            let p = ((t - tok.start) / tok.dur).clamp(0.0, 1.0) as f32;
            active_token_progress = p;
            let typed = ((p * total_tok_chars as f32).floor() as usize).min(total_tok_chars);
            cursor_char_idx = tok.char_offset + typed;

            // Continuous sub-pixel x position (fluid, no 16px stair-stepping!)
            let continuous_chars = tok.char_offset as f32 + (p * total_tok_chars as f32);
            cursor_subpixel_x = continuous_chars * char_w;
            break;
        } else {
            // Not yet typed
            break;
        }
    }

    let cursor_world_x = cursor_char_idx as f32 * char_w;

    // ── Dynamic Zoom Varieties ───────────────────────────────────────────────
    let base_zoom = conf.zoom_in_scale;

    // 1. Word Punch-In Impact:
    // When a word starts being typed (p in [0.0, 0.38]), camera gives a snappy punch-in (+0.32x)
    let word_punch = if active_token_is_active && active_token_progress < 0.38 {
        let punch_p = active_token_progress / 0.38;
        (1.0 - punch_p).powf(2.0) * 0.32
    } else {
        0.0
    };

    // 2. Cadence Breathing:
    // Between words or during vocal lulls, camera pulls back subtly (-0.14x)
    let breathing = if active_token_is_active && active_token_progress > 0.75 {
        ((active_token_progress - 0.75) / 0.25) * 0.14
    } else if !active_token_is_active && !in_line_trans {
        0.16
    } else {
        0.0
    };

    let (target_cam_x, target_cam_y, target_zoom, _whoosh_intensity) = if in_line_trans {
        // Fast snappy zoom-out whoosh between lines
        let smooth_q = trans_q * trans_q * (3.0 - 2.0 * trans_q); // Hermite smoothstep
        let cx = egui::lerp(cursor_subpixel_x..=0.0, smooth_q);
        let cy = egui::lerp(curr_line_y..=next_line_y, smooth_q);
        let dip = (trans_q * PI).sin();
        let z = egui::lerp(base_zoom..=conf.zoom_out_scale, dip);
        (cx, cy, z, dip)
    } else {
        let current_target_zoom = (base_zoom + word_punch - breathing).clamp(1.40, 2.30);
        (cursor_subpixel_x, curr_line_y, current_target_zoom, 0.0_f32)
    };

    // ── Camera Momentum and Spring/Exponential Smoothing ─────────────────────
    #[derive(Clone, Copy)]
    struct CodeCamState {
        x: f32,
        y: f32,
        zoom: f32,
        last_t: f64,
    }

    let cam_id = egui::Id::unique("code-editor-smooth-cam");
    let mut cam = ctx.data(|d| d.get_temp::<CodeCamState>(cam_id)).unwrap_or(CodeCamState {
        x: target_cam_x,
        y: target_cam_y,
        zoom: target_zoom,
        last_t: f.now,
    });

    let dt = (f.now - cam.last_t).clamp(0.001, 0.05) as f32;
    cam.last_t = f.now;

    // Detect hard seek or sudden jump:
    if (target_cam_x - cam.x).abs() > 700.0 || (target_cam_y - cam.y).abs() > 350.0 {
        cam.x = target_cam_x;
        cam.y = target_cam_y;
        cam.zoom = target_zoom;
    } else {
        // Smooth exponential damping (buttery smooth glides!)
        let lambda_x = if in_line_trans { 14.0 } else { 11.0 };
        let lambda_y = 12.0;
        let lambda_z = 9.0;
        cam.x += (target_cam_x - cam.x) * (1.0 - (-lambda_x * dt).exp());
        cam.y += (target_cam_y - cam.y) * (1.0 - (-lambda_y * dt).exp());
        cam.zoom += (target_zoom - cam.zoom) * (1.0 - (-lambda_z * dt).exp());
    }

    ctx.data_mut(|d| d.insert_temp(cam_id, cam));

    let cam_x = cam.x;
    let cam_y = cam.y;
    let cam_zoom = cam.zoom;

    let tilt_rad = conf.tilt_deg * PI / 180.0;
    let (sin_t, cos_t) = tilt_rad.sin_cos();

    // Center focal point on screen (slightly below header)
    let screen_focus = pos2(rect.center().x - 60.0, rect.center().y + 16.0);

    let to_screen = |wx: f32, wy: f32| -> Pos2 {
        let rel_x = (wx - cam_x) * cam_zoom;
        let rel_y = (wy - cam_y) * cam_zoom;
        let tx = rel_x * cos_t - rel_y * sin_t;
        let ty = rel_x * sin_t + rel_y * cos_t;
        pos2(screen_focus.x + tx, screen_focus.y + ty)
    };

    // Active line band (subtle horizontal strip across line)
    if !in_line_trans {
        let band_wy0 = curr_line_y - line_h * 0.18;
        let band_wy1 = curr_line_y + line_h * 0.82;
        let p0 = to_screen(-180.0, band_wy0);
        let p1 = to_screen(2400.0, band_wy0);
        let p2 = to_screen(2400.0, band_wy1);
        let p3 = to_screen(-180.0, band_wy1);
        painter.add(Shape::convex_polygon(vec![p0, p1, p2, p3], conf.active_line_band, Stroke::NONE));
    }

    let atlas_w = (atlas[0] as f32).max(1.0);
    let atlas_h = (atlas[1] as f32).max(1.0);
    let mut text_mesh = Mesh::with_texture(TextureId::default());

    let draw_galley_to_mesh = |g: &Galley, world_x: f32, world_y: f32, color: Color32, mesh: &mut Mesh| {
        for row in &g.rows {
            for gl in &row.row.glyphs {
                let uv = gl.uv_rect;
                if uv.is_nothing() {
                    continue;
                }
                let offset = row.pos.to_vec2() + gl.pos.to_vec2() + uv.offset;
                let wx0 = world_x + offset.x;
                let wy0 = world_y + offset.y;
                let wx1 = wx0 + uv.size.x;
                let wy1 = wy0 + uv.size.y;

                let p0 = to_screen(wx0, wy0);
                let p1 = to_screen(wx1, wy0);
                let p2 = to_screen(wx1, wy1);
                let p3 = to_screen(wx0, wy1);

                let uvr = Rect::from_min_max(
                    pos2(uv.min[0] as f32 / atlas_w, uv.min[1] as f32 / atlas_h),
                    pos2(uv.max[0] as f32 / atlas_w, uv.max[1] as f32 / atlas_h),
                );

                let base = mesh.vertices.len() as u32;
                mesh.vertices.push(Vertex { pos: p0, uv: uvr.left_top(), color });
                mesh.vertices.push(Vertex { pos: p1, uv: uvr.right_top(), color });
                mesh.vertices.push(Vertex { pos: p2, uv: uvr.right_bottom(), color });
                mesh.vertices.push(Vertex { pos: p3, uv: uvr.left_bottom(), color });
                mesh.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }
    };

    // ── Render Line 1 (Comment: // just us) ──────────────────────────────────
    let line1_y = 0.0_f32;
    let line1_num_galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap("1".to_string(), base_font.clone(), conf.gutter_text_dim));
    let line1_text_galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(comment_text.clone(), base_font.clone(), conf.comment_color));
    draw_galley_to_mesh(&line1_num_galley, -18.0 - line1_num_galley.size().x, line1_y, conf.gutter_text_dim, &mut text_mesh);
    draw_galley_to_mesh(&line1_text_galley, 0.0, line1_y, conf.comment_color, &mut text_mesh);

    let token_color = |tok: &EditorToken| -> Color32 {
        if tok.is_accent {
            conf.accent_color
        } else if matches!(tok.ident.as_str(), "if" | "else" | "const" | "let" | "var" | "function" | "fn" | "return" | "async" | "await" | "try" | "catch" | "throw" | "new" | "import" | "export") {
            conf.keyword_color
        } else if tok.ident.chars().all(|c| c.is_ascii_digit()) {
            conf.number_color
        } else if tok.is_object {
            if (tok.char_offset / 5) % 2 == 0 { conf.obj_color } else { conf.gold_color }
        } else {
            conf.method_color
        }
    };

    // ── Render Lyric Lines ───────────────────────────────────────────────────
    for line in &editor_lines {
        let line_y = (line.line_num - 1) as f32 * line_h;

        // Skip lines that are far above or below screen
        let test_screen_y = to_screen(0.0, line_y).y;
        if test_screen_y < rect.top() - 100.0 || test_screen_y > rect.bottom() + 100.0 {
            continue;
        }

        // Gutter line number
        let is_active_line = !in_line_trans && line.line_num == curr_line.line_num;
        let num_color = if is_active_line { conf.gutter_text_active } else { conf.gutter_text_dim };
        let num_str = format!("{}", line.line_num);
        let num_galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(num_str, base_font.clone(), num_color));
        draw_galley_to_mesh(&num_galley, -18.0 - num_galley.size().x, line_y, num_color, &mut text_mesh);

        // Tokens
        for tok in &line.tokens {
            let is_completed = t >= tok.start + tok.dur;
            let is_active = t >= tok.start && t < tok.start + tok.dur;

            if is_completed {
                // Blue selection flash right after typing completes
                let elapsed_done = (t - (tok.start + tok.dur)) as f32;
                if elapsed_done >= 0.0 && elapsed_done < 0.38 {
                    let sel_fade = (1.0 - elapsed_done / 0.38).powf(1.6);
                    let sel_col = theme::with_alpha(conf.selection_blue, sel_fade);
                    let sel_x0 = tok.char_offset as f32 * char_w - 2.0;
                    let sel_x1 = (tok.char_offset + tok.ident.chars().count()) as f32 * char_w + 2.0;
                    let sel_y0 = line_y - line_h * 0.10;
                    let sel_y1 = line_y + line_h * 0.85;
                    let q0 = to_screen(sel_x0, sel_y0);
                    let q1 = to_screen(sel_x1, sel_y0);
                    let q2 = to_screen(sel_x1, sel_y1);
                    let q3 = to_screen(sel_x0, sel_y1);
                    painter.add(Shape::convex_polygon(vec![q0, q1, q2, q3], sel_col, Stroke::NONE));
                }

                // Render full ident
                let ident_col = token_color(tok);
                let ident_galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(tok.ident.clone(), base_font.clone(), ident_col));
                draw_galley_to_mesh(&ident_galley, tok.char_offset as f32 * char_w, line_y, ident_col, &mut text_mesh);

                // Render syntax suffix
                let suffix_x = (tok.char_offset + tok.ident.chars().count()) as f32 * char_w;
                let suffix_galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(tok.syntax_suffix.clone(), base_font.clone(), conf.punct_color));
                draw_galley_to_mesh(&suffix_galley, suffix_x, line_y, conf.punct_color, &mut text_mesh);
            } else if is_active {
                let p = ((t - tok.start) / tok.dur).clamp(0.0, 1.0) as f32;
                let total_tok_chars = tok.ident.chars().count() + tok.syntax_suffix.chars().count();
                let typed = ((p * total_tok_chars as f32).floor() as usize).min(total_tok_chars);
                let ident_len = tok.ident.chars().count();

                if typed <= ident_len {
                    let sub_ident: String = tok.ident.chars().take(typed).collect();
                    if !sub_ident.is_empty() {
                        let ident_col = token_color(tok);
                        let ident_galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(sub_ident, base_font.clone(), ident_col));
                        draw_galley_to_mesh(&ident_galley, tok.char_offset as f32 * char_w, line_y, ident_col, &mut text_mesh);
                    }
                } else {
                    // Full ident
                    let ident_col = token_color(tok);
                    let ident_galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(tok.ident.clone(), base_font.clone(), ident_col));
                    draw_galley_to_mesh(&ident_galley, tok.char_offset as f32 * char_w, line_y, ident_col, &mut text_mesh);

                    // Substring suffix
                    let sub_suffix: String = tok.syntax_suffix.chars().take(typed - ident_len).collect();
                    if !sub_suffix.is_empty() {
                        let suffix_x = (tok.char_offset + ident_len) as f32 * char_w;
                        let suffix_galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(sub_suffix, base_font.clone(), conf.punct_color));
                        draw_galley_to_mesh(&suffix_galley, suffix_x, line_y, conf.punct_color, &mut text_mesh);
                    }
                }
            }
        }
    }

    // Submit all code text in a single GPU mesh draw call
    painter.add(Shape::mesh(text_mesh));

    // ── Render Caret / Cursor ────────────────────────────────────────────────
    if !in_line_trans {
        if cursor_is_accent {
            // Collaborator Orange Cursor line
            let cur_w = 2.5 / cam_zoom;
            let c0 = to_screen(cursor_world_x, curr_line_y - line_h * 0.12);
            let c1 = to_screen(cursor_world_x + cur_w, curr_line_y - line_h * 0.12);
            let c2 = to_screen(cursor_world_x + cur_w, curr_line_y + line_h * 0.88);
            let c3 = to_screen(cursor_world_x, curr_line_y + line_h * 0.88);
            painter.add(Shape::convex_polygon(vec![c0, c1, c2, c3], conf.cursor_accent, Stroke::NONE));

            // Small yellow/orange flag badge "you" above cursor
            let badge_pos = to_screen(cursor_world_x - 4.0, curr_line_y - line_h * 0.45);
            let badge_font = theme::lyric_font_for(&cfg.font, Weight::Bold, 11.0, "you");
            let badge_galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap("you".to_string(), badge_font, Color32::from_rgb(20, 20, 20)));
            let badge_rect = Rect::from_min_size(badge_pos, badge_galley.size() + vec2(10.0, 4.0));
            painter.rect_filled(badge_rect, 3.0, Color32::from_rgb(255, 175, 40));
            painter.galley(badge_pos + vec2(5.0, 2.0), badge_galley, Color32::from_rgb(20, 20, 20));
        } else {
            // Standard Thick Red Block Cursor
            let cur_w = char_w * 0.92;
            let c0 = to_screen(cursor_world_x, curr_line_y - line_h * 0.08);
            let c1 = to_screen(cursor_world_x + cur_w, curr_line_y - line_h * 0.08);
            let c2 = to_screen(cursor_world_x + cur_w, curr_line_y + line_h * 0.85);
            let c3 = to_screen(cursor_world_x, curr_line_y + line_h * 0.85);
            painter.add(Shape::convex_polygon(vec![c0, c1, c2, c3], conf.cursor_red, Stroke::NONE));
        }
    }

    // ── Pulsing Collaborator Ring under accent token ─────────────────────────
    for line in &editor_lines {
        let line_y = (line.line_num - 1) as f32 * line_h;
        for tok in &line.tokens {
            if tok.is_accent && t >= tok.start - 0.40 && t <= tok.start + tok.dur + 0.30 {
                let pulse = (f.now * 4.0).sin().abs() as f32;
                let r_base = 18.0 + pulse * 5.0;
                let ring_center_w = pos2((tok.char_offset + tok.ident.chars().count() / 2) as f32 * char_w, line_y + line_h * 0.60);
                let ring_screen = to_screen(ring_center_w.x, ring_center_w.y);
                painter.circle(ring_screen, r_base * cam_zoom, Color32::from_rgba_unmultiplied(255, 153, 0, 30), Stroke::new(2.0, conf.accent_color));

                // Pin pole & flag "you"
                let pole_top = to_screen(ring_center_w.x, ring_center_w.y - 20.0);
                painter.line_segment([ring_screen, pole_top], Stroke::new(2.0, conf.accent_color));
                let flag_rect = Rect::from_min_size(pole_top + vec2(2.0, -12.0), vec2(28.0 * cam_zoom, 14.0 * cam_zoom));
                painter.rect_filled(flag_rect, 2.0, Color32::from_rgb(255, 175, 40));
                let flag_font = theme::lyric_font_for(&cfg.font, Weight::Bold, 9.5, "you");
                painter.text(flag_rect.center(), egui::Align2::CENTER_CENTER, "you", flag_font, Color32::from_rgb(20, 20, 20));
            }
        }
    }

    // ── Fixed Top Editor Tab Bar ─────────────────────────────────────────────
    let tab_bar_rect = Rect::from_min_size(rect.left_top(), vec2(rect.width(), 38.0));
    painter.rect_filled(tab_bar_rect, 0.0, conf.tab_bg);

    // ── Back Button (Return to previous lyric animation mode) ─────────────────
    let back_btn_rect = Rect::from_min_size(rect.left_top(), vec2(48.0, 38.0));
    let pointer_hover = ctx.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p));
    let is_back_hover = pointer_hover.is_some_and(|p| back_btn_rect.contains(p));
    let mut return_to_prev_mode = false;
    if is_back_hover {
        painter.rect_filled(back_btn_rect, 0.0, Color32::from_rgb(34, 38, 50));
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        if clicked {
            return_to_prev_mode = true;
        }
    }
    let back_col = if is_back_hover { Color32::WHITE } else { Color32::from_rgb(160, 172, 190) };
    let back_font = theme::lyric_font_for(&cfg.font, Weight::Bold, 17.0, "←");
    painter.text(back_btn_rect.center(), egui::Align2::CENTER_CENTER, "←", back_font, back_col);

    // Divider after back button
    painter.line_segment(
        [rect.left_top() + vec2(48.0, 6.0), rect.left_top() + vec2(48.0, 32.0)],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 18)),
    );

    // Active Tab button (starts right after back button)
    let tab_start_x = rect.left() + 49.0;
    let tab_w = 145.0_f32;
    let tab_rect = Rect::from_min_size(pos2(tab_start_x, rect.top()), vec2(tab_w, 38.0));
    painter.rect_filled(tab_rect, 0.0, Color32::from_rgb(18, 20, 26));

    // Red thin stripe at the very top of the active tab
    painter.rect_filled(Rect::from_min_size(pos2(tab_start_x, rect.top()), vec2(tab_w, 2.0)), 0.0, conf.tab_accent_top);

    // JS file icon (amber square)
    let icon_rect = Rect::from_center_size(pos2(tab_start_x + 18.0, rect.top() + 20.0), vec2(14.0, 14.0));
    painter.rect_filled(icon_rect, 2.0, Color32::from_rgb(247, 183, 49));
    let js_font = theme::lyric_font_for(&cfg.font, Weight::Bold, 8.5, "JS");
    painter.text(icon_rect.center(), egui::Align2::CENTER_CENTER, "JS", js_font, Color32::from_rgb(25, 20, 10));

    // File tab name
    let tab_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 13.0, &file_tab_name);
    painter.text(pos2(tab_start_x + 31.0, rect.top() + 19.0), egui::Align2::LEFT_CENTER, &file_tab_name, tab_font, Color32::from_rgb(220, 226, 235));

    // Close 'x' button
    let close_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 12.0, "×");
    painter.text(pos2(tab_start_x + tab_w - 14.0, rect.top() + 19.0), egui::Align2::CENTER_CENTER, "×", close_font, Color32::from_rgb(120, 130, 145));

    // Red dot badge "1 here" (Live Share indicator)
    let badge_dot_pos = pos2(tab_start_x + tab_w + 20.0, rect.top() + 19.0);
    painter.circle_filled(badge_dot_pos, 4.0, Color32::from_rgb(255, 51, 85));
    let here_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 12.0, "1 here");
    painter.text(badge_dot_pos + vec2(10.0, 0.0), egui::Align2::LEFT_CENTER, "1 here", here_font, Color32::from_rgb(190, 198, 210));

    // Bottom border line of tab bar
    painter.line_segment(
        [rect.left_top() + vec2(0.0, 38.0), rect.right_top() + vec2(0.0, 38.0)],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 16)),
    );

    // ── Breadcrumbs bar below tab bar ────────────────────────────────────────
    let breadcrumb_rect = Rect::from_min_size(pos2(rect.left(), rect.top() + 38.0), vec2(rect.width(), 22.0));
    painter.rect_filled(breadcrumb_rect, 0.0, Color32::from_rgb(18, 20, 26));
    let bc_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 11.0, "src > lyrics");
    let bc_text = format!("src  ›  tracks  ›  {}  ›  #line_{}", file_tab_name, curr_line.line_num);
    painter.text(pos2(rect.left() + 20.0, rect.top() + 49.0), egui::Align2::LEFT_CENTER, bc_text, bc_font, Color32::from_rgb(130, 140, 155));
    painter.line_segment(
        [pos2(rect.left(), rect.top() + 60.0), pos2(rect.right(), rect.top() + 60.0)],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 12)),
    );

    // ── Minimap on the right edge ────────────────────────────────────────────
    let minimap_w = 42.0_f32;
    let minimap_rect = Rect::from_min_size(pos2(rect.right() - minimap_w, rect.top() + 60.0), vec2(minimap_w, rect.height() - 84.0));
    painter.rect_filled(minimap_rect, 0.0, Color32::from_rgba_unmultiplied(12, 14, 18, 140));
    painter.line_segment([minimap_rect.left_top(), minimap_rect.left_bottom()], Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 10)));
    let num_lines = editor_lines.len().max(1);
    let line_mini_h = (minimap_rect.height() / (num_lines as f32 * 1.4)).clamp(2.0, 5.0);
    for (i, l) in editor_lines.iter().enumerate() {
        let my = minimap_rect.top() + 8.0 + i as f32 * (line_mini_h + 2.0);
        if my > minimap_rect.bottom() - 8.0 { break; }
        let is_cur = l.line_num == curr_line.line_num;
        let bar_w = (l.tokens.len() as f32 * 4.0).clamp(6.0, minimap_w - 10.0);
        let bar_col = if is_cur { Color32::from_rgb(255, 51, 85) } else { Color32::from_rgba_unmultiplied(255, 255, 255, 35) };
        painter.rect_filled(Rect::from_min_size(pos2(minimap_rect.left() + 5.0, my), vec2(bar_w, line_mini_h)), 1.0, bar_col);
    }

    // ── Status bar at bottom ─────────────────────────────────────────────────
    let status_rect = Rect::from_min_size(pos2(rect.left(), rect.bottom() - 22.0), vec2(rect.width(), 22.0));
    painter.rect_filled(status_rect, 0.0, Color32::from_rgb(14, 15, 19));
    let st_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 11.0, "sample");
    let st_left = format!("Ln {}, Col {}  |  Spaces: 2  |  UTF-8  |  TypeScript", curr_line.line_num, curr_line.tokens.len() * 4);
    painter.text(pos2(rect.left() + 16.0, rect.bottom() - 11.0), egui::Align2::LEFT_CENTER, st_left, st_font.clone(), Color32::from_rgb(140, 150, 165));
    let st_right = "git: main*  |  BitChord Live";
    painter.text(pos2(rect.right() - minimap_w - 16.0, rect.bottom() - 11.0), egui::Align2::RIGHT_CENTER, st_right, st_font, Color32::from_rgb(110, 120, 135));

    // ── Translation below code area ──────────────────────────────────────────
    if let Some(tr) = f.trans.get(curr_line.line_idx).copied().flatten() {
        let tr_y = rect.bottom() - 32.0;
        let tr_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 15.0, tr);
        painter.text(pos2(rect.center().x, tr_y), egui::Align2::CENTER_BOTTOM, tr, tr_font, theme::with_alpha(Color32::WHITE, 0.65));
    }

    // Click-to-seek: ONLY within code lines, NEVER on back button or tab bar!
    if clicked && !is_back_hover && pointer_hover.is_some_and(|p| p.y > rect.top() + 42.0) {
        let p_screen = pointer_hover.unwrap();
        let curr_y_screen = to_screen(0.0, curr_line_y).y;
        if (p_screen.y - curr_y_screen).abs() < line_h * cam_zoom * 0.8 {
            seek = Some(curr_line.start);
        }
    }

    // Double safety: if returning to previous mode, NEVER seek!
    if return_to_prev_mode {
        seek = None;
    }

    if f.playing {
        ctx.request_repaint();
    }

    Out {
        seek,
        animating: f.playing,
        user_scrolling: false,
        return_to_prev_mode,
        ..Default::default()
    }
}

// ------------------------------------------------------------------ chat lyrics

#[allow(clippy::too_many_arguments)]
fn paint_chat_view(
    painter: &egui::Painter,
    ctx: &egui::Context,
    cfg: &Cfg,
    f: &Frame,
    layout: &Layout,
    t: f64,
    rect: Rect,
    _atlas: [usize; 2],
    _ppp: f32,
) -> Out {
    use egui::{Stroke, StrokeKind};

    let pointer = ctx.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p));
    let clicked = ctx.input(|i| i.pointer.primary_clicked());
    let now = f.now;

    let mut return_to_prev_mode = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    let mut toggle_play = false;
    let mut next_track = false;
    let mut prev_track = false;
    let mut seek = None;

    // 1. Dark Backdrop
    painter.rect_filled(rect, 0.0, Color32::from_rgb(7, 9, 13));

    // 2. Extract Lines and Words
    struct ChatWord {
        text: String,
        start: f64,
        dur: f64,
    }

    struct ChatLine {
        words: Vec<ChatWord>,
        start: f64,
        end: f64,
        trans: Option<String>,
        full_text: String,
    }

    let mut chat_lines: Vec<ChatLine> = Vec::new();
    for (li, b) in layout.blocks.iter().enumerate() {
        if b.line.is_none() || b.instrumental {
            continue;
        }
        let words: Vec<&Word> = b.words.iter().filter(|w| !w.text.trim().is_empty()).collect();
        if words.is_empty() {
            continue;
        }
        let start = words.first().map(|w| w.start).unwrap_or(0.0);
        let end = words.last().map(|w| w.start + w.dur.max(0.3)).unwrap_or(start + 2.0);
        let mut c_words = Vec::new();

        let is_line_synced = words.iter().all(|w| w.dur <= 0.0);
        let line_dur = (end - start).max(1.0);
        let total_chars: usize = words.iter().map(|w| w.text.trim().chars().count()).sum();
        let mut char_acc = 0usize;

        for w in words {
            let text = w.text.trim().to_string();
            let (w_start, w_dur) = if is_line_synced {
                let n = text.chars().count();
                let s = start + if total_chars > 0 { line_dur * (char_acc as f64) / (total_chars as f64) } else { 0.0 };
                let d = if total_chars > 0 { line_dur * (n as f64) / (total_chars as f64) } else { 0.2 };
                char_acc += n;
                (s, d)
            } else {
                (w.start, w.dur.max(0.12))
            };
            c_words.push(ChatWord { text, start: w_start, dur: w_dur });
        }

        let full_text = c_words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        let trans = f.trans.get(li).copied().flatten().map(|s| s.to_string());
        chat_lines.push(ChatLine {
            words: c_words,
            start,
            end,
            trans,
            full_text,
        });
    }

    let num_lines = chat_lines.len();

    // Determine current active line index
    let mut active_line_idx: Option<usize> = None;
    for (i, l) in chat_lines.iter().enumerate() {
        if t >= l.start && t <= l.end {
            active_line_idx = Some(i);
            break;
        } else if t < l.start {
            if i > 0 {
                active_line_idx = Some(i - 1);
            }
            break;
        }
    }
    if active_line_idx.is_none() && num_lines > 0 && t >= chat_lines[num_lines - 1].end {
        active_line_idx = Some(num_lines - 1);
    }

    // Check if receiver typing indicator is active for upcoming odd line (Receiver)
    let next_line_idx = active_line_idx.map_or(0, |i| i + 1);
    let is_receiver_typing = if next_line_idx < num_lines && (next_line_idx % 2 == 1) {
        let next_start = chat_lines[next_line_idx].start;
        let prev_end = active_line_idx.map_or(0.0, |i| chat_lines[i].end);
        let lead_in_start = (next_start - 2.0).max(prev_end);
        t >= lead_in_start && t < next_start
    } else {
        false
    };

    // Scaled-up layout metrics
    let col_w = (rect.width() * 0.58).clamp(620.0, 940.0).min(rect.width() - 80.0);
    let col_left = rect.center().x - col_w * 0.5;
    let col_right = rect.center().x + col_w * 0.5;

    let font_chat = theme::lyric_font_for(&cfg.font, Weight::Semibold, 27.0, "sample");
    let font_name = theme::lyric_font_for(&cfg.font, Weight::Regular, 14.5, "sample");
    let font_avatar = theme::lyric_font_for(&cfg.font, Weight::Bold, 18.0, "A");

    let pad_x = 26.0_f32;
    let pad_y = 18.0_f32;
    let avatar_size = 46.0_f32;
    let avatar_margin = 14.0_f32;
    let receiver_offset_x = avatar_size + avatar_margin;
    let max_bubble_w = col_w * 0.78;
    let max_text_w = max_bubble_w - pad_x * 2.0;

    let raw_artist = f.artist.unwrap_or("Artist");
    let artist_parts: Vec<&str> = raw_artist.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();

    // 3. Measure & Position All Lines Chronologically (Top-to-Bottom)
    struct ComputedBubble {
        line_idx: usize,
        is_sender: bool,
        bubble_rect: Rect,
        galley: std::sync::Arc<egui::Galley>,
        contact_name: String,
        avatar_pos: Pos2,
        name_pos: Pos2,
        trans_pos: Option<Pos2>,
        trans_text: Option<String>,
        is_typing_only: bool,
    }

    let max_visible_line = if is_receiver_typing { next_line_idx } else { active_line_idx.unwrap_or(0) };
    let mut computed_items: Vec<ComputedBubble> = Vec::new();

    let mut current_world_y = rect.top() + 75.0; // Start comfortably below header
    let normal_gap = 14.0_f32;
    let speaker_switch_gap = 26.0_f32;

    for i in 0..=max_visible_line {
        if i >= num_lines {
            break;
        }
        let line = &chat_lines[i];
        let is_sender = i % 2 == 0;
        let is_typing_only = i == next_line_idx && is_receiver_typing;

        if i > 0 {
            let prev_sender = (i - 1) % 2 == 0;
            if is_sender != prev_sender {
                current_world_y += speaker_switch_gap;
            } else {
                current_world_y += normal_gap;
            }
        }

        let contact_name = if !is_sender {
            let part_idx = (i / 2) % artist_parts.len().max(1);
            artist_parts.get(part_idx).copied().unwrap_or(raw_artist).to_string()
        } else {
            String::new()
        };

        let name_h = if !is_sender { 20.0_f32 } else { 0.0_f32 };
        let name_pos = pos2(col_left + receiver_offset_x + 6.0, current_world_y + 2.0);
        let bubble_y = current_world_y + name_h;

        if is_typing_only {
            let ind_w = 82.0_f32;
            let ind_h = 50.0_f32;
            let bubble_rect = Rect::from_min_size(pos2(col_left + receiver_offset_x + 6.0, bubble_y), vec2(ind_w, ind_h));
            let avatar_pos = pos2(col_left + avatar_size * 0.5, bubble_rect.bottom() - avatar_size * 0.5);

            let dummy_galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(String::new(), font_chat.clone(), Color32::WHITE));
            computed_items.push(ComputedBubble {
                line_idx: i,
                is_sender: false,
                bubble_rect,
                galley: dummy_galley,
                contact_name,
                avatar_pos,
                name_pos,
                trans_pos: None,
                trans_text: None,
                is_typing_only: true,
            });
            current_world_y = bubble_rect.bottom();
        } else {
            let galley = ctx.fonts_mut(|fonts| fonts.layout(line.full_text.clone(), font_chat.clone(), Color32::WHITE, max_text_w));
            let g_size = galley.size();

            let bubble_w = (g_size.x + pad_x * 2.0).clamp(60.0, max_bubble_w);
            let bubble_h = g_size.y + pad_y * 2.0;

            let bubble_x = if is_sender {
                col_right - 6.0 - bubble_w
            } else {
                col_left + receiver_offset_x + 6.0
            };

            let bubble_rect = Rect::from_min_size(pos2(bubble_x, bubble_y), vec2(bubble_w, bubble_h));
            let avatar_pos = pos2(col_left + avatar_size * 0.5, bubble_rect.bottom() - avatar_size * 0.5);

            let (trans_pos, trans_text) = if let Some(tr) = line.trans.as_ref() {
                let tr_pos = if is_sender {
                    pos2(bubble_rect.right(), bubble_rect.bottom() + 6.0)
                } else {
                    pos2(bubble_rect.left(), bubble_rect.bottom() + 6.0)
                };
                (Some(tr_pos), Some(tr.clone()))
            } else {
                (None, None)
            };

            let extra_trans_h = if trans_text.is_some() { 24.0 } else { 0.0 };

            computed_items.push(ComputedBubble {
                line_idx: i,
                is_sender,
                bubble_rect,
                galley,
                contact_name,
                avatar_pos,
                name_pos,
                trans_pos,
                trans_text,
                is_typing_only: false,
            });

            current_world_y = bubble_rect.bottom() + extra_trans_h;
        }
    }

    // 4. Downward Camera Tracking
    let target_bottom_screen = rect.bottom() - 130.0;
    let latest_bottom = computed_items.last().map(|it| it.bubble_rect.bottom()).unwrap_or(rect.top() + 100.0);
    let target_cam_scroll = (latest_bottom - target_bottom_screen).max(0.0);

    let cam_scroll = ctx.animate_value_with_time(egui::Id::unique("chat_cam_scroll_y"), target_cam_scroll, 0.45 * theme::SPEED);

    // 5. Render Bubbles
    let top_fade_start = rect.top() + 180.0;
    let top_fade_end = rect.top() + 80.0;

    for item in &computed_items {
        let screen_bubble_rect = item.bubble_rect.translate(vec2(0.0, -cam_scroll));

        if screen_bubble_rect.bottom() < top_fade_end || screen_bubble_rect.top() > rect.bottom() {
            continue;
        }

        let alpha_fade = if screen_bubble_rect.top() < top_fade_start {
            ((screen_bubble_rect.top() - top_fade_end) / (top_fade_start - top_fade_end)).clamp(0.0, 1.0)
        } else {
            1.0_f32
        };

        if item.is_typing_only {
            // Receiver 3-Dot Typing Indicator
            let bubble_bg = theme::with_alpha(Color32::from_rgb(38, 37, 42), alpha_fade);
            let rounding = egui::CornerRadius { nw: 22, ne: 22, sw: 6, se: 22 };
            painter.rect_filled(screen_bubble_rect, rounding, bubble_bg);
            painter.rect_stroke(screen_bubble_rect, rounding, Stroke::new(1.0, theme::with_alpha(Color32::from_white_alpha(15), alpha_fade)), StrokeKind::Inside);

            let bx0 = screen_bubble_rect.left();
            let by1 = screen_bubble_rect.bottom();
            let p1 = pos2(bx0 + 3.0, by1);
            let p2 = pos2(bx0 - 5.0, by1 + 1.2);
            let p3 = pos2(bx0, by1 - 8.0);
            painter.add(Shape::convex_polygon(vec![p1, p2, p3], bubble_bg, egui::Stroke::NONE));

            let dot_cy = screen_bubble_rect.center().y;
            let dot_cx = screen_bubble_rect.center().x;
            for di in 0..3 {
                let offset_x = (di as f32 - 1.0) * 12.0;
                let bounce = (-(now * 5.5 + di as f64 * 0.75).sin().max(0.0) * 4.5) as f32;
                let dot_pos = pos2(dot_cx + offset_x, dot_cy + bounce);
                painter.circle_filled(dot_pos, 3.8, theme::with_alpha(Color32::from_rgb(142, 142, 147), alpha_fade));
            }

            let screen_av_pos = item.avatar_pos - vec2(0.0, cam_scroll);
            let av_bg = theme::with_alpha(Color32::from_rgb(175, 82, 222), alpha_fade);
            painter.circle_filled(screen_av_pos, avatar_size * 0.5, av_bg);
            let initial = item.contact_name.chars().next().unwrap_or('?').to_string();
            painter.text(screen_av_pos, egui::Align2::CENTER_CENTER, initial, font_avatar.clone(), theme::with_alpha(Color32::WHITE, alpha_fade));

            let screen_name_pos = item.name_pos - vec2(0.0, cam_scroll);
            painter.text(screen_name_pos, egui::Align2::LEFT_CENTER, &item.contact_name, font_name.clone(), theme::with_alpha(Color32::from_rgb(142, 142, 147), alpha_fade));
            continue;
        }

        let is_active = Some(item.line_idx) == active_line_idx;
        let line = &chat_lines[item.line_idx];

        if item.is_sender {
            // SENDER BUBBLE (Apple Blue, Right-aligned, Live Typewriter without horizontal shifting)
            let bubble_bg = theme::with_alpha(Color32::from_rgb(11, 132, 254), alpha_fade);
            let rounding = egui::CornerRadius { nw: 22, ne: 22, sw: 22, se: 6 };
            painter.rect_filled(screen_bubble_rect, rounding, bubble_bg);

            let bx1 = screen_bubble_rect.right();
            let by1 = screen_bubble_rect.bottom();
            let p1 = pos2(bx1 - 3.0, by1);
            let p2 = pos2(bx1 + 5.0, by1 + 1.2);
            let p3 = pos2(bx1, by1 - 8.0);
            painter.add(Shape::convex_polygon(vec![p1, p2, p3], bubble_bg, egui::Stroke::NONE));

            let text_pos = pos2(screen_bubble_rect.left() + pad_x, screen_bubble_rect.top() + pad_y);

            if is_active {
                let mut revealed_text = String::new();
                let mut is_typing_now = false;
                for w in &line.words {
                    if t >= w.start {
                        if !revealed_text.is_empty() {
                            revealed_text.push(' ');
                        }
                        if t <= w.start + w.dur {
                            let q = ((t - w.start) / w.dur.max(0.05)).clamp(0.0, 1.0) as f32;
                            let char_count = (w.text.chars().count() as f32 * q).ceil() as usize;
                            revealed_text.push_str(&w.text.chars().take(char_count).collect::<String>());
                            is_typing_now = true;
                            break;
                        } else {
                            revealed_text.push_str(&w.text);
                        }
                    }
                }

                let cur_galley = ctx.fonts_mut(|fonts| fonts.layout(revealed_text, font_chat.clone(), Color32::WHITE, max_text_w));
                let cur_size = cur_galley.size();
                painter.galley(text_pos, cur_galley, theme::with_alpha(Color32::WHITE, alpha_fade));

                if is_typing_now && (now * 5.0).sin() > -0.2 {
                    let cursor_x = text_pos.x + cur_size.x + 2.0;
                    let cursor_y = text_pos.y + cur_size.y * 0.5;
                    painter.rect_filled(
                        Rect::from_center_size(pos2(cursor_x, cursor_y), vec2(2.5, font_chat.size * 0.9)),
                        1.0,
                        theme::with_alpha(Color32::WHITE, alpha_fade * 0.9),
                    );
                }
            } else {
                painter.galley(text_pos, item.galley.clone(), theme::with_alpha(Color32::WHITE, alpha_fade));
            }
        } else {
            // RECEIVER BUBBLE (Dark Mode #26252A, White text, Avatar & Contact Name)
            let bubble_bg = theme::with_alpha(Color32::from_rgb(38, 37, 42), alpha_fade);
            let rounding = egui::CornerRadius { nw: 22, ne: 22, sw: 6, se: 22 };
            painter.rect_filled(screen_bubble_rect, rounding, bubble_bg);
            painter.rect_stroke(screen_bubble_rect, rounding, Stroke::new(1.0, theme::with_alpha(Color32::from_white_alpha(15), alpha_fade)), StrokeKind::Inside);

            let bx0 = screen_bubble_rect.left();
            let by1 = screen_bubble_rect.bottom();
            let p1 = pos2(bx0 + 3.0, by1);
            let p2 = pos2(bx0 - 5.0, by1 + 1.2);
            let p3 = pos2(bx0, by1 - 8.0);
            painter.add(Shape::convex_polygon(vec![p1, p2, p3], bubble_bg, egui::Stroke::NONE));

            let screen_av_pos = item.avatar_pos - vec2(0.0, cam_scroll);
            let av_colors = [
                Color32::from_rgb(255, 100, 130), // Pink/Coral
                Color32::from_rgb(48, 176, 199),  // Teal
                Color32::from_rgb(175, 82, 222),  // Purple
                Color32::from_rgb(52, 199, 89),   // Green
            ];
            let av_bg = theme::with_alpha(av_colors[(item.line_idx / 2) % av_colors.len()], alpha_fade);
            painter.circle_filled(screen_av_pos, avatar_size * 0.5, av_bg);
            let initial = item.contact_name.chars().next().unwrap_or('?').to_string();
            painter.text(screen_av_pos, egui::Align2::CENTER_CENTER, initial, font_avatar.clone(), theme::with_alpha(Color32::WHITE, alpha_fade));

            let screen_name_pos = item.name_pos - vec2(0.0, cam_scroll);
            painter.text(screen_name_pos, egui::Align2::LEFT_CENTER, &item.contact_name, font_name.clone(), theme::with_alpha(Color32::from_rgb(142, 142, 147), alpha_fade));

            let text_pos = pos2(screen_bubble_rect.left() + pad_x, screen_bubble_rect.top() + pad_y);
            painter.galley(text_pos, item.galley.clone(), theme::with_alpha(Color32::from_rgb(242, 242, 247), alpha_fade));
        }

        let is_hover = pointer.is_some_and(|p| screen_bubble_rect.contains(p));
        if is_hover && !is_active {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            if clicked {
                seek = Some(line.start);
            }
        }

        if is_active && let (Some(tr_pos), Some(tr)) = (item.trans_pos, item.trans_text.as_ref()) {
            let tr_screen_pos = tr_pos - vec2(0.0, cam_scroll);
            let tr_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 14.5, tr);
            let align = if item.is_sender { egui::Align2::RIGHT_TOP } else { egui::Align2::LEFT_TOP };
            painter.text(tr_screen_pos, align, tr, tr_font, theme::with_alpha(Color32::WHITE, 0.70 * alpha_fade));
        }
    }

    // 6. Title Card (Morphs from Hero Card to Compact Top Pill)
    let is_hero = active_line_idx.map_or(true, |idx| idx == 0);
    let morph = theme::anim_bool(ctx, "chat_title_morph", !is_hero, 0.35);

    let raw_title = f.title.unwrap_or("Unknown Title");

    if morph < 0.99 {
        let hero_alpha = 1.0 - morph;
        let hero_w = (col_w * 0.92).clamp(420.0, 580.0);
        let hero_h = 80.0_f32;
        let hero_y = rect.top() + 68.0 - morph * 30.0;
        let hero_rect = Rect::from_center_size(pos2(rect.center().x, hero_y), vec2(hero_w, hero_h));

        let card_bg = theme::with_alpha(Color32::from_black_alpha(200), hero_alpha);
        let card_stroke = theme::with_alpha(Color32::from_white_alpha(30), hero_alpha);
        painter.rect_filled(hero_rect, 20.0, card_bg);
        painter.rect_stroke(hero_rect, 20.0, Stroke::new(1.0, card_stroke), StrokeKind::Inside);

        let thumb_rect = Rect::from_min_size(hero_rect.left_top() + vec2(14.0, 14.0), vec2(52.0, 52.0));
        let mut drew_thumb = false;
        if let Some(thumb) = f.thumbs.first() {
            if let Ok(egui::load::TexturePoll::Ready { texture }) = egui::Image::new(&thumb.url).load_for_size(ctx, thumb_rect.size()) {
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                let mut mesh = egui::Mesh::with_texture(texture.id);
                mesh.add_rect_with_uv(thumb_rect, uv, theme::with_alpha(Color32::WHITE, hero_alpha));
                painter.add(Shape::mesh(mesh));
                drew_thumb = true;
            }
        }
        if !drew_thumb {
            painter.rect_filled(thumb_rect, 10.0, theme::with_alpha(Color32::from_rgb(28, 32, 42), hero_alpha));
            painter.text(thumb_rect.center(), egui::Align2::CENTER_CENTER, "🎵", egui::FontId::proportional(24.0), theme::with_alpha(Color32::WHITE, hero_alpha));
        }

        let text_x = hero_rect.left() + 78.0;
        let title_font = theme::lyric_font_for(&cfg.font, Weight::Bold, 17.5, raw_title);
        let artist_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 14.0, raw_artist);

        painter.text(pos2(text_x, hero_rect.top() + 26.0), egui::Align2::LEFT_CENTER, raw_title, title_font, theme::with_alpha(Color32::WHITE, hero_alpha));
        painter.text(pos2(text_x, hero_rect.top() + 52.0), egui::Align2::LEFT_CENTER, raw_artist, artist_font, theme::with_alpha(Color32::from_white_alpha(170), hero_alpha));
    }

    if morph > 0.01 {
        let pill_alpha = morph;
        let pill_h = 34.0_f32;
        let pill_font = theme::lyric_font_for(&cfg.font, Weight::Regular, 13.5, "sample");
        let pill_text = format!("{raw_title} · {raw_artist}");
        let pill_g = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(pill_text.clone(), pill_font.clone(), Color32::WHITE));
        let pill_w = (pill_g.size().x + 32.0).clamp(180.0, col_w);
        let pill_y = rect.top() + 28.0;
        let pill_rect = Rect::from_center_size(pos2(rect.center().x, pill_y), vec2(pill_w, pill_h));

        painter.rect_filled(pill_rect, 17.0, theme::with_alpha(Color32::from_black_alpha(190), pill_alpha));
        painter.rect_stroke(pill_rect, 17.0, Stroke::new(1.0, theme::with_alpha(Color32::from_white_alpha(26), pill_alpha)), StrokeKind::Inside);
        painter.text(pill_rect.center(), egui::Align2::CENTER_CENTER, pill_text, pill_font, theme::with_alpha(Color32::from_white_alpha(210), pill_alpha));
    }

    // 7. Top-Left Back Button (44x44px accessible touch target)
    let back_btn_rect = Rect::from_min_size(rect.left_top() + vec2(24.0, 22.0), vec2(44.0, 44.0));
    let is_back_hover = pointer.is_some_and(|p| back_btn_rect.contains(p));
    let back_h = theme::anim_bool(ctx, "chat_back_h", is_back_hover, 0.15);
    let back_bg = Color32::from_white_alpha((18.0 + back_h * 24.0) as u8);
    painter.rect_filled(back_btn_rect, 22.0, back_bg);
    painter.rect_stroke(back_btn_rect, 22.0, Stroke::new(1.0, Color32::from_white_alpha((28.0 + back_h * 30.0) as u8)), StrokeKind::Inside);

    let back_font = theme::lyric_font_for(&cfg.font, Weight::Bold, 18.0, "←");
    let back_col = theme::lerp_color(Color32::from_white_alpha(190), Color32::WHITE, back_h);
    painter.text(back_btn_rect.center(), egui::Align2::CENTER_CENTER, "←", back_font, back_col);

    if is_back_hover {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        if clicked {
            return_to_prev_mode = true;
        }
    }

    // 8. Bottom Playback Controls Floating Glass Pill (190x44px)
    let ctrl_w = 190.0_f32;
    let ctrl_h = 44.0_f32;
    let ctrl_rect = Rect::from_center_size(pos2(rect.center().x, rect.bottom() - 44.0), vec2(ctrl_w, ctrl_h));
    painter.rect_filled(ctrl_rect, 22.0, Color32::from_black_alpha(210));
    painter.rect_stroke(ctrl_rect, 22.0, Stroke::new(1.0, Color32::from_white_alpha(32)), StrokeKind::Inside);

    let prev_rect = Rect::from_center_size(pos2(ctrl_rect.center().x - 52.0, ctrl_rect.center().y), Vec2::splat(32.0));
    let play_rect = Rect::from_center_size(ctrl_rect.center(), Vec2::splat(34.0));
    let next_rect = Rect::from_center_size(pos2(ctrl_rect.center().x + 52.0, ctrl_rect.center().y), Vec2::splat(32.0));

    if pointer.is_some_and(|p| prev_rect.contains(p)) {
        painter.circle_filled(prev_rect.center(), 16.0, Color32::from_white_alpha(28));
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        if clicked {
            prev_track = true;
        }
    }
    painter.text(prev_rect.center(), egui::Align2::CENTER_CENTER, "⏮", egui::FontId::proportional(16.0), Color32::WHITE);

    if pointer.is_some_and(|p| play_rect.contains(p)) {
        painter.circle_filled(play_rect.center(), 17.0, Color32::from_white_alpha(38));
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        if clicked {
            toggle_play = true;
        }
    }
    let play_sym = if f.playing { "❚❚" } else { "▶" };
    painter.text(play_rect.center(), egui::Align2::CENTER_CENTER, play_sym, egui::FontId::proportional(16.0), Color32::WHITE);

    if pointer.is_some_and(|p| next_rect.contains(p)) {
        painter.circle_filled(next_rect.center(), 16.0, Color32::from_white_alpha(28));
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        if clicked {
            next_track = true;
        }
    }
    painter.text(next_rect.center(), egui::Align2::CENTER_CENTER, "⏭", egui::FontId::proportional(16.0), Color32::WHITE);

    if return_to_prev_mode {
        seek = None;
    }

    if f.playing {
        ctx.request_repaint();
    }

    Out {
        seek,
        animating: f.playing,
        user_scrolling: false,
        return_to_prev_mode,
        toggle_play,
        next_track,
        prev_track,
    }
}

// ------------------------------------------------------------------ glow

/// A word's drop-shadow to rasterize once this frame's glyphs are known.
pub struct GlowJob {
    key: (u64, usize, u32, u32),
    quads: Vec<(Rect, Rect, usize, usize, f32)>,
    word: Rect,
    look: WordLook,
    by_letter: bool,
    rtl: bool,
    /// Blur radius in points (CSS: std deviation = radius / 2).
    radius: f32,
    /// Romanization pills under the text (box, corner radius): `rgba(255,255,255,.05)` with
    /// a 1px `rgba(255,255,255,.1)` border.
    pills: Vec<(Rect, f32)>,
    color: Color32,
    /// Placeholder in the paint list, under the word's lit text.
    layer: ShapeIdx,
}

/// A GL texture sized in 64 px steps. A new image that fits is written into it in place
/// (`glTexSubImage2D`); a new texture or a new size makes the driver allocate, and at 144
/// frames a second it kept over 100 MB of those allocations (a new texture per blur step of
/// every line was the bulk of it).
struct Slot {
    tex: TextureHandle,
    size: [usize; 2],
    /// The part the current image covers.
    used: [usize; 2],
    /// Of the current image: an unchanged one (a glow that has stopped moving) isn't sent again.
    hash: u64,
}

const SLOT_STEP: usize = 64;
/// Unused slots kept for the next glow or line to write into.
const SPARE_SLOTS: usize = 8;

impl Slot {
    fn uv(&self) -> Rect {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(self.used[0] as f32 / self.size[0] as f32, self.used[1] as f32 / self.size[1] as f32))
    }
}

/// A blurred line's radius in 1/4 px and scale in 1/1000.
type LineStep = (i32, i32, u64);

#[derive(Default)]
struct Glow {
    /// A copy of the font atlas, refreshed when glyphs we need aren't in it yet.
    atlas: Option<Arc<Coverage>>,
    textures: HashMap<(u64, usize, u32, u32), (Slot, f64)>,
    /// Per line: its blurred image, the radius (in 1/4 px) and scale (in 1/1000) it was
    /// drawn at, where it goes.
    lines: HashMap<(u64, usize), (Slot, LineStep, Rect, f64)>,
    spare: Vec<Slot>,
    scratch: Vec<f32>,
}

impl Glow {
    fn atlas(&mut self, ctx: &egui::Context, size: [usize; 2], need: &[Rect]) -> Arc<Coverage> {
        let stale = match &self.atlas {
            None => true,
            Some(a) => a.size != size || need.iter().any(|uv| glyph_blank(a, *uv)),
        };
        if stale {
            // Only the coverage (alpha) is kept: a quarter of the RGBA image egui hands out.
            let img = ctx.fonts(|f| f.image());
            self.atlas = Some(Arc::new(Coverage { size: img.size, alpha: img.pixels.iter().map(|p| p.a()).collect() }));
        }
        self.atlas.clone().unwrap()
    }

    /// Puts `img` in a texture: `old` if it fits, else a spare one that does, else a new one.
    fn upload(&mut self, ctx: &egui::Context, old: Option<Slot>, img: ColorImage, name: &str) -> Slot {
        let used = img.size;
        let image_hash = hash((used, &img.pixels));
        if let Some(s) = &old
            && s.used == used
            && s.hash == image_hash
        {
            return old.unwrap();
        }
        // The image plus a transparent column and row, so filtering at its edge reads what it
        // did on a texture of its own; whatever an earlier, bigger image left past them is
        // never sampled.
        let (w, h) = (used[0] + 1, used[1] + 1);
        let bucket = [w.next_multiple_of(SLOT_STEP), h.next_multiple_of(SLOT_STEP)];
        let fits = |s: &Slot| s.size[0] >= w && s.size[1] >= h && s.size[0] * s.size[1] <= 2 * bucket[0] * bucket[1];
        let slot = match old {
            Some(s) if fits(&s) => Some(s),
            Some(s) => {
                self.recycle(s);
                None
            }
            None => None,
        };
        let slot = slot.or_else(|| self.spare.iter().position(fits).map(|k| self.spare.swap_remove(k)));
        let size = slot.as_ref().map_or(bucket, |s| s.size);
        // A new texture gets its full size; an old one only the part being drawn.
        let (pw, ph) = if slot.is_some() { (w, h) } else { (size[0], size[1]) };
        let mut pixels = vec![Color32::TRANSPARENT; pw * ph];
        for (y, row) in img.pixels.chunks_exact(used[0]).enumerate() {
            pixels[y * pw..y * pw + used[0]].copy_from_slice(row);
        }
        let img = ColorImage { size: [pw, ph], source_size: vec2(pw as f32, ph as f32), pixels };
        match slot {
            Some(mut s) => {
                s.tex.set_partial([0, 0], img, TextureOptions::LINEAR);
                s.used = used;
                s.hash = image_hash;
                s
            }
            None => Slot { tex: ctx.load_texture(name, img, TextureOptions::LINEAR), size, used, hash: image_hash },
        }
    }

    fn recycle(&mut self, s: Slot) {
        if self.spare.len() >= SPARE_SLOTS {
            self.spare.remove(0);
        }
        self.spare.push(s);
    }

    fn finish(&mut self, ctx: &egui::Context, painter: &egui::Painter, jobs: Vec<GlowJob>, atlas_size: [usize; 2], ppp: f32) {
        let now = ctx.input(|i| i.time);
        if !jobs.is_empty() {
            let uvs: Vec<Rect> = jobs.iter().flat_map(|j| j.quads.iter().map(|q| q.1)).collect();
            let atlas = self.atlas(ctx, atlas_size, &uvs);
            for j in jobs {
                let Some((img, rect)) = rasterize_glow(&atlas, &j, ppp, &mut self.scratch) else { continue };
                let old = self.textures.remove(&j.key).map(|(s, _)| s);
                let slot = self.upload(ctx, old, img, "lyric-glow");
                let mut mesh = Mesh::with_texture(slot.tex.id());
                mesh.add_rect_with_uv(rect, slot.uv(), j.color);
                self.textures.insert(j.key, (slot, now));
                painter.set(j.layer, Shape::mesh(mesh));
            }
        }
        let gone: Vec<Slot> = (self.textures.extract_if(|_, (_, used)| now - *used >= 1.0).map(|(_, (s, _))| s))
            .chain(self.lines.extract_if(|_, (.., used)| now - *used >= 4.0).map(|(_, (s, ..))| s))
            .collect();
        for s in gone {
            self.recycle(s);
        }
    }

    /// A whole line blurred (`filter: blur()`): its words, each at its own alpha, its text
    /// decorations and pills, in content coordinates, rendered at the line's `scale` so the
    /// image isn't resampled on screen. Cached per line at its latest radius and scale.
    #[allow(clippy::too_many_arguments)]
    fn blurred_line(
        &mut self,
        ctx: &egui::Context,
        key: (u64, usize),
        radius: f32,
        words: &[(&Word, f32)],
        texts: &[(Arc<Galley>, Pos2, f32)],
        pills: &[(Rect, f32)],
        scale: f32,
        atlas_size: [usize; 2],
        ppp: f32,
    ) -> Option<(TextureId, Rect, Rect)> {
        if dev_off("blur") {
            return None;
        }
        let now = ctx.input(|i| i.time);
        let weights: Vec<u8> = words.iter().map(|(_, a)| (a.clamp(0.0, 1.0) * 255.0).round() as u8)
            .chain(texts.iter().map(|(_, _, a)| (a.clamp(0.0, 1.0) * 255.0).round() as u8))
            .chain(pills.iter().map(|(_, a)| (a.clamp(0.0, 1.0) * 255.0).round() as u8))
            .collect();
        let step = ((radius * 4.0) as i32, (scale * 1000.0).round() as i32, hash(&weights));
        if let Some((s, r, rect, used)) = self.lines.get_mut(&key)
            && *r == step
        {
            *used = now;
            return Some((s.tex.id(), *rect, s.uv()));
        }
        let uv_of = |min: [u16; 2], max: [u16; 2]| {
            Rect::from_min_max(
                pos2(min[0] as f32 / atlas_size[0] as f32, min[1] as f32 / atlas_size[1] as f32),
                pos2(max[0] as f32 / atlas_size[0] as f32, max[1] as f32 / atlas_size[1] as f32),
            )
        };
        let mut quads = Vec::new();
        let push_galley = |g: &Galley, origin: Pos2, weight: f32, quads: &mut Vec<(Rect, Rect, usize, usize, f32)>| {
            for row in &g.rows {
                for gl in &row.row.glyphs {
                    let uv = gl.uv_rect;
                    if uv.is_nothing() {
                        continue;
                    }
                    let r = Rect::from_min_size(origin + row.pos.to_vec2() + gl.pos.to_vec2() + uv.offset, uv.size);
                    quads.push((r, uv_of(uv.min, uv.max), 0, 1, weight));
                }
            }
        };
        for (w, weight) in words {
            let g = ctx.fonts_mut(|fv| fv.layout_no_wrap(w.text.clone(), w.font.clone(), Color32::WHITE));
            push_galley(&g, w.pos, *weight, &mut quads);
        }
        for (g, at, weight) in texts {
            push_galley(g, *at, *weight, &mut quads);
        }
        if quads.is_empty() {
            return None;
        }
        let uvs: Vec<Rect> = quads.iter().map(|q| q.1).collect();
        let atlas = self.atlas(ctx, atlas_size, &uvs);
        let bounds = quads.iter().fold(Rect::NOTHING, |a, q| a.union(q.0));
        let job = GlowJob {
            key: (0, 0, 0, 0),
            quads,
            word: bounds,
            look: WordLook { opacity: 1.0, start: 2.0, end: 3.0, glow: 0.0, glow_alpha: 0.0 },
            by_letter: false,
            rtl: false,
            // CSS `blur(r)` is a Gaussian with σ = r.
            radius: radius * 2.0,
            pills: pills.to_vec(),
            color: Color32::WHITE,
            layer: ShapeIdx(0),
        };
        let (img, content_rect) = rasterize_glow(&atlas, &job, ppp * scale, &mut self.scratch)?;
        let old = self.lines.remove(&key).map(|(s, ..)| s);
        let slot = self.upload(ctx, old, img, "lyric-blur");
        let out = (slot.tex.id(), content_rect, slot.uv());
        self.lines.insert(key, (slot, step, content_rect, now));
        Some(out)
    }

    fn focal_word_glow(
        &mut self,
        ctx: &egui::Context,
        key: (u64, usize),
        text: &str,
        font: FontId,
        radius: f32,
        atlas_size: [usize; 2],
        ppp: f32,
    ) -> Option<(TextureId, Rect, Rect)> {
        if dev_off("glow") {
            return None;
        }
        let now = ctx.input(|i| i.time);
        let step: LineStep = ((radius * 4.0) as i32, (font.size * 10.0) as i32, hash(text));
        if let Some((s, r, rect, used)) = self.lines.get_mut(&key)
            && *r == step
        {
            *used = now;
            return Some((s.tex.id(), *rect, s.uv()));
        }

        let g = ctx.fonts_mut(|fv| fv.layout_no_wrap(text.to_string(), font.clone(), Color32::WHITE));
        let uv_of = |min: [u16; 2], max: [u16; 2]| {
            Rect::from_min_max(
                pos2(min[0] as f32 / atlas_size[0] as f32, min[1] as f32 / atlas_size[1] as f32),
                pos2(max[0] as f32 / atlas_size[0] as f32, max[1] as f32 / atlas_size[1] as f32),
            )
        };
        let mut quads = Vec::new();
        for row in &g.rows {
            for gl in &row.row.glyphs {
                let uv = gl.uv_rect;
                if uv.is_nothing() {
                    continue;
                }
                let r = Rect::from_min_size(Pos2::ZERO + row.pos.to_vec2() + gl.pos.to_vec2() + uv.offset, uv.size);
                quads.push((r, uv_of(uv.min, uv.max), 0, 1, 1.0));
            }
        }
        if quads.is_empty() {
            return None;
        }
        let uvs: Vec<Rect> = quads.iter().map(|q| q.1).collect();
        let atlas = self.atlas(ctx, atlas_size, &uvs);
        let bounds = quads.iter().fold(Rect::NOTHING, |a, q| a.union(q.0));
        let job = GlowJob {
            key: (0, 0, 0, 0),
            quads,
            word: bounds,
            look: WordLook { opacity: 1.0, start: 2.0, end: 3.0, glow: radius, glow_alpha: 0.95 },
            by_letter: false,
            rtl: false,
            radius,
            pills: Vec::new(),
            color: Color32::WHITE,
            layer: ShapeIdx(0),
        };
        let (img, content_rect) = rasterize_glow(&atlas, &job, ppp, &mut self.scratch)?;
        let old = self.lines.remove(&key).map(|(s, ..)| s);
        let slot = self.upload(ctx, old, img, "focal-glow");
        let out = (slot.tex.id(), content_rect, slot.uv());
        self.lines.insert(key, (slot, step, content_rect, now));
        Some(out)
    }
}

/// Whether a glyph's texels are all empty in our atlas copy (it was added since).
/// The font atlas's coverage, one byte per texel.
struct Coverage {
    size: [usize; 2],
    alpha: Vec<u8>,
}

fn glyph_blank(a: &Coverage, uv: Rect) -> bool {
    let (w, h) = (a.size[0] as f32, a.size[1] as f32);
    let (x0, y0, x1, y1) = ((uv.left() * w) as usize, (uv.top() * h) as usize, (uv.right() * w) as usize, (uv.bottom() * h) as usize);
    if x1 <= x0 || y1 <= y0 || x1 > a.size[0] || y1 > a.size[1] {
        return true;
    }
    for y in y0..y1 {
        for x in x0..x1 {
            if a.alpha[y * a.size[0] + x] > 0 {
                return false;
            }
        }
    }
    true
}

/// The lit glyph pixels (coverage × gradient) blurred: CSS `drop-shadow(0 0 r color)`
/// is a Gaussian with σ = r / 2 of the element's alpha, drawn under it. `ppp`: bitmap
/// pixels per unit of the quads' coordinates.
fn rasterize_glow(atlas: &Coverage, j: &GlowJob, ppp: f32, scratch: &mut Vec<f32>) -> Option<(ColorImage, Rect)> {
    let sigma = (j.radius * 0.5 * ppp).max(0.3);
    let pad = (sigma * 3.0).ceil() + 2.0;
    let bounds = j.quads.iter().map(|q| q.0).chain(j.pills.iter().map(|p| p.0)).fold(Rect::NOTHING, |a, r| a.union(r));
    if !bounds.is_positive() {
        return None;
    }
    let origin = bounds.min - Vec2::splat(pad / ppp);
    let w = (bounds.width() * ppp + pad * 2.0).ceil() as usize;
    let h = (bounds.height() * ppp + pad * 2.0).ceil() as usize;
    if w == 0 || h == 0 || w * h > 4_000_000 {
        return None;
    }
    scratch.clear();
    scratch.resize(w * h, 0.0);
    for &(r, radius) in &j.pills {
        paint_pill(scratch, w, h, (r.min - origin) * ppp, r.size() * ppp, radius * ppp, ppp);
    }
    let (aw, ah) = (atlas.size[0] as f32, atlas.size[1] as f32);
    for &(r, uv, letter, letters, weight) in &j.quads {
        let (alpha, _) = lit_gradient(&r, letter, letters, j.word, &j.look, j.by_letter, j.rtl);
        let tx0 = (uv.left() * aw).round() as isize;
        let ty0 = (uv.top() * ah).round() as isize;
        let tw = ((uv.right() - uv.left()) * aw).round() as isize;
        let th = ((uv.bottom() - uv.top()) * ah).round() as isize;
        if tw <= 0 || th <= 0 {
            continue;
        }
        // Destination box in bitmap pixels (glyphs may be scaled a little by the line).
        let dx0 = (r.left() - origin.x) * ppp;
        let dy0 = (r.top() - origin.y) * ppp;
        let dw = r.width() * ppp;
        let dh = r.height() * ppp;
        let (ix0, iy0) = (dx0.floor().max(0.0) as usize, dy0.floor().max(0.0) as usize);
        let (ix1, iy1) = (((dx0 + dw).ceil() as usize).min(w), ((dy0 + dh).ceil() as usize).min(h));
        // Bilinear, with nothing outside the glyph's own texels: at another scale than the
        // atlas's, nearest-texel picks drop or double rows and the edges go jagged.
        let texel = |x: isize, y: isize| -> f32 {
            if x < 0 || y < 0 || x >= tw || y >= th {
                return 0.0;
            }
            atlas.alpha.get((ty0 + y) as usize * atlas.size[0] + (tx0 + x) as usize).map_or(0.0, |&p| p as f32 / 255.0)
        };
        for y in iy0..iy1 {
            let fy = (y as f32 + 0.5 - dy0) / dh * th as f32 - 0.5;
            let (sy, ky) = (fy.floor() as isize, fy - fy.floor());
            for x in ix0..ix1 {
                let fx = (x as f32 + 0.5 - dx0) / dw * tw as f32 - 0.5;
                let (sx, kx) = (fx.floor() as isize, fx - fx.floor());
                let top = texel(sx, sy) * (1.0 - kx) + texel(sx + 1, sy) * kx;
                let bottom = texel(sx, sy + 1) * (1.0 - kx) + texel(sx + 1, sy + 1) * kx;
                let cov = top * (1.0 - ky) + bottom * ky;
                if cov <= 0.0 {
                    continue;
                }
                let screen_x = origin.x + (x as f32 + 0.5) / ppp;
                let a = cov * alpha(screen_x) * weight;
                let d = &mut scratch[y * w + x];
                *d = (*d + a - *d * a).min(1.0);
            }
        }
    }
    gaussian(scratch, w, h, sigma);
    let pixels = scratch
        .iter()
        .map(|v| {
            let a = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            Color32::from_rgba_premultiplied(a, a, a, a)
        })
        .collect();
    let img = ColorImage { size: [w, h], source_size: vec2(w as f32, h as f32), pixels };
    Some((img, Rect::from_min_size(origin, vec2(w as f32, h as f32) / ppp)))
}

/// Gaussian blur: a true kernel for small σ (Skia's cut-off is 2), else three box passes
/// per axis (what browsers use for large blurs). Boxes for a σ under 1 come out as one
/// 3-tap box, which looks blocky instead of soft.
fn gaussian(buf: &mut [f32], w: usize, h: usize, sigma: f32) {
    if sigma < 2.0 {
        let r = (sigma * 3.0).ceil() as usize;
        let mut k: Vec<f32> = (0..=2 * r).map(|i| (-(i as f32 - r as f32).powi(2) / (2.0 * sigma * sigma)).exp()).collect();
        let sum: f32 = k.iter().sum();
        k.iter_mut().for_each(|v| *v /= sum);
        let mut tmp = vec![0.0f32; buf.len()];
        convolve(buf, &mut tmp, w, h, &k, true);
        convolve(&tmp, buf, w, h, &k, false);
        return;
    }
    // Box sizes for n = 3 (W3C filter effects / "boxes for gauss").
    let n = 3.0f32;
    let ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut wl = ideal.floor() as i32;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wu = wl + 2;
    let m_ideal = (12.0 * sigma * sigma - n * (wl * wl) as f32 - 4.0 * n * wl as f32 - 3.0 * n) / (-4.0 * wl as f32 - 4.0);
    let m = m_ideal.round() as i32;
    let mut tmp = vec![0.0f32; buf.len()];
    for i in 0..3 {
        let size = if i < m { wl } else { wu };
        let r = ((size - 1) / 2).max(0) as usize;
        if r == 0 {
            continue;
        }
        box_pass(buf, &mut tmp, w, h, r, true);
        box_pass(&tmp, buf, w, h, r, false);
    }
}

/// One axis of a separable kernel (odd length, centred), zero past the edges.
fn convolve(src: &[f32], dst: &mut [f32], w: usize, h: usize, k: &[f32], horizontal: bool) {
    let r = (k.len() / 2) as isize;
    let (n, lines) = if horizontal { (w, h) } else { (h, w) };
    let at = |line: usize, i: usize| if horizontal { line * w + i } else { i * w + line };
    for line in 0..lines {
        for i in 0..n {
            let mut acc = 0.0;
            for (j, kv) in k.iter().enumerate() {
                let s = i as isize + j as isize - r;
                if s >= 0 && (s as usize) < n {
                    acc += src[at(line, s as usize)] * kv;
                }
            }
            dst[at(line, i)] = acc;
        }
    }
}

/// A romanization pill into an alpha bitmap, anti-aliased like egui's rect: the 5% fill
/// with its 1-point 10% border inside (`min`/`size`/`radius` in bitmap pixels).
fn paint_pill(buf: &mut [f32], w: usize, h: usize, min: Vec2, size: Vec2, radius: f32, ppp: f32) {
    let radius = radius.min(size.x * 0.5).min(size.y * 0.5);
    let half = size * 0.5;
    let centre = min + half;
    let (x0, y0) = (min.x.floor().max(0.0) as usize, min.y.floor().max(0.0) as usize);
    let (x1, y1) = (((min.x + size.x).ceil() as usize).min(w), ((min.y + size.y).ceil() as usize).min(h));
    let over = |d: &mut f32, a: f32| *d = (*d + a - *d * a).min(1.0);
    for y in y0..y1 {
        for x in x0..x1 {
            // Signed distance to the rounded box, negative inside.
            let p = vec2(x as f32 + 0.5, y as f32 + 0.5) - centre;
            let q = vec2(p.x.abs(), p.y.abs()) - half + Vec2::splat(radius);
            let d = vec2(q.x.max(0.0), q.y.max(0.0)).length() + q.x.max(q.y).min(0.0) - radius;
            let inside = (0.5 - d).clamp(0.0, 1.0);
            let border = inside - (0.5 - (d + ppp)).clamp(0.0, 1.0);
            let px = &mut buf[y * w + x];
            over(px, 0.05 * inside);
            over(px, 0.1 * border);
        }
    }
}

fn box_pass(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize, horizontal: bool) {
    let (n, lines) = if horizontal { (w, h) } else { (h, w) };
    let idx = |line: usize, i: usize| if horizontal { line * w + i } else { i * w + line };
    let norm = 1.0 / (2 * r + 1) as f32;
    for line in 0..lines {
        let mut acc = 0.0;
        for i in 0..=r.min(n - 1) {
            acc += src[idx(line, i)];
        }
        for i in 0..n {
            dst[idx(line, i)] = acc * norm;
            if i + r + 1 < n {
                acc += src[idx(line, i + r + 1)];
            }
            if i >= r {
                acc -= src[idx(line, i - r)];
            }
        }
    }
}

// ------------------------------------------------------------------ instrumental

/// Bits of a break, its beads' alpha at rest, and its beats.
const GAP_DOTS: usize = 3;
const GAP_DOT_REST: f32 = 0.55;
const GAP_REST_SCALE: f32 = 0.76;
const GAP_BREATH: f32 = 0.20;
const GAP_PUFF: f32 = 0.35;
/// The break's shape in seconds: the beads settle in, fill one per beat, and leave.
const GAP_ENTER: f64 = 0.620;
const GAP_EXIT: f64 = 0.350;
const GAP_SPLIT: f32 = 0.35;
const GAP_COLLAPSE: f64 = 0.500;

/// Where a break's gesture sits in its own span. The exit is cut out of the end of the
/// silence and runs backwards from the next line, and the beads spend everything ahead
/// of it: `(exit start, exit length, one bead's beat)`.
fn gap_times(start: f64, dur: f64) -> (f64, f64, f64) {
    let until = start + dur.max(0.001);
    let exit_start = start + GAP_ENTER.max(until - start - GAP_COLLAPSE - GAP_EXIT);
    let exit_len = (until.min(exit_start + GAP_EXIT) - exit_start).max(0.001);
    let step = ((exit_start - start) / GAP_DOTS as f64).max(0.001);
    (exit_start, exit_len, step)
}

/// An intermission: three beads that light one after another as the silence runs out,
/// breathe once per bead, then puff out and vanish before the next line. `true` while
/// it is moving.
fn paint_instrumental(painter: &egui::Painter, cfg: &Cfg, b: &Block, xf: &Xf, t: f64, content_w: f32) -> bool {
    let em = cfg.size;
    // The beads' proportions are the BitChord player's: 13 dp across with 5 dp between
    // them at a 34 sp lyric size.
    let dot = em * 13.0 / 34.0;
    let gap = em * 5.0 / 34.0;
    let total = dot * GAP_DOTS as f32 + gap * (GAP_DOTS - 1) as f32;
    let x0 = match b.align {
        Align::Left => 0.0,
        Align::Right => content_w - total,
        Align::Center => (content_w - total) * 0.5,
    };
    let cy = cfg.padding + em * 1.333 * 0.5;
    let mid = x0 + total * 0.5;
    let into = t - b.start;
    let (exit_start, exit_len, step) = gap_times(b.start, b.dur);
    let until = b.start + b.dur.max(0.001);
    // Opening: the row settles from just under its size rather than appearing.
    let swell = EASE.at(((into / GAP_ENTER) as f32).clamp(0.0, 1.0));
    // Closing: a push over the first [GAP_SPLIT], then a pull into the point the beads
    // grew from and out to nothing.
    let e = ((t - exit_start) / exit_len).clamp(0.0, 1.0) as f32;
    let pull = e > GAP_SPLIT;
    let half = if pull { (e - GAP_SPLIT) / (1.0 - GAP_SPLIT) } else { e / GAP_SPLIT };
    let bend = half * half * (3.0 - 2.0 * half);
    let grow = if e <= 0.0 {
        GAP_REST_SCALE + (1.0 - GAP_REST_SCALE) * swell
    } else if !pull {
        1.0 + GAP_PUFF * bend
    } else {
        (1.0 + GAP_PUFF) * (1.0 - bend)
    };
    let alpha = if e <= 0.0 { swell } else if !pull { 1.0 } else { 1.0 - bend };
    if alpha <= 0.001 || grow <= 0.001 {
        return false;
    }
    // One breath to each bead it lights, all three on the same phase; once the third is
    // lit the row holds still and waits for the exit. Raised cosine, so every turn round
    // is the same movement as the last.
    let beat = if into < step * GAP_DOTS as f64 { ((into % step) / step) as f32 } else { 1.0 };
    let pulse = 1.0 - GAP_BREATH * 0.5 * (1.0 - (std::f32::consts::TAU * beat).cos());
    for i in 0..GAP_DOTS {
        // Beads and the space between them scale together about the middle of the row,
        // which is what makes the outer dots drift apart as they fill.
        let x = x0 + dot * 0.5 + i as f32 * (dot + gap);
        let cx = mid + (x - mid) * grow * pulse;
        let c = xf.point(pos2(cx, cy));
        let r = dot * 0.5 * grow * pulse * xf.scale;
        // Brightness alone is per bead: the count has to read as three arrivals, and
        // only one of them can be happening at a time.
        let k = ((t - b.start - i as f64 * step) / step).clamp(0.0, 1.0) as f32;
        let lit = k * k * (3.0 - 2.0 * k);
        painter.circle_filled(c, r, theme::with_alpha(Color32::WHITE, GAP_DOT_REST * alpha));
        if lit > 0.0 {
            painter.circle_filled(c, r, theme::with_alpha(cfg.active, lit * alpha));
        }
    }
    (into >= 0.0 && into <= GAP_ENTER) || t < b.start + step * GAP_DOTS as f64 || (t >= exit_start && t < until)
}

/// Dev hook: YTM_LYR_OFF=glow,blur,view leaves those parts out (memory breakdowns).
pub fn dev_off(what: &str) -> bool {
    static OFF: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    OFF.get_or_init(|| std::env::var("YTM_LYR_OFF").unwrap_or_default()).split(',').any(|x| x == what)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_rewritten_in_place() {
        let ctx = egui::Context::default();
        let img = |w: usize, h: usize| ColorImage::filled([w, h], Color32::WHITE);
        let mut g = Glow::default();
        let a = g.upload(&ctx, None, img(100, 40), "t");
        assert_eq!(a.size, [128, 64]);
        let id = a.tex.id();
        // Smaller, and a little bigger within the slot: the same texture, drawn in part.
        let b = g.upload(&ctx, Some(a), img(60, 30), "t");
        assert_eq!((b.tex.id(), b.used), (id, [60, 30]));
        assert_eq!(b.uv().max, pos2(60.0 / 128.0, 30.0 / 64.0));
        let c = g.upload(&ctx, Some(b), img(120, 60), "t");
        assert_eq!(c.tex.id(), id);
        // Outgrown: a new texture; the old one is kept for the next image that fits it.
        let d = g.upload(&ctx, Some(c), img(300, 40), "t");
        assert_ne!(d.tex.id(), id);
        assert_eq!(g.spare.len(), 1);
        let e = g.upload(&ctx, None, img(90, 50), "t");
        assert_eq!(e.tex.id(), id);
        // Far too big for a small image: not reused.
        let f = g.upload(&ctx, Some(d), img(10, 10), "t");
        assert_eq!(f.size, [64, 64]);
    }

    #[test]
    fn bezier_matches_css_ease() {
        // Reference values of CSS `ease` (cubic-bezier(0.25, 0.1, 0.25, 1)).
        assert!((EASE.at(0.5) - 0.8024).abs() < 0.002);
        assert!((EASE.at(0.25) - 0.4094).abs() < 0.002);
        assert!((EASE_IN.at(0.5) - 0.3153).abs() < 0.002);
        assert_eq!(EASE.at(0.0), 0.0);
        assert_eq!(EASE.at(1.0), 1.0);
    }

    #[test]
    fn parts_split_on_spaces_with_prorated_time() {
        let p = Part { start: 1000, dur: 1000, text: "ab cd".into(), bg: false };
        let t = split_part(&p);
        assert_eq!(t.len(), 3);
        assert!((t[0].start - 1.0).abs() < 1e-9 && (t[0].dur - 0.5).abs() < 1e-9);
        assert!(t[1].space);
        assert!((t[2].start - 1.5).abs() < 1e-9);
    }

    #[test]
    fn rtl_lines_run_from_the_right() {
        let mut fonts = egui::epaint::text::Fonts::new(Default::default(), egui::FontDefinitions::default());
        let mut fv = fonts.with_pixels_per_point(1.0);
        let font = FontId::proportional(20.0);
        let line = |text: &str| line_synced_parts(&Line { start: 0, dur: 1000, text: text.into(), ..Default::default() }).iter().flat_map(split_part).collect::<Vec<_>>();
        let x = |f: &Flow, word: &str| f.words.iter().find(|w| w.text == word).unwrap().pos.x;
        // Hebrew reads from the right; the English inside it keeps its order.
        let f = flow(&mut fv, line("שלום עולם hello world"), &font, 1000.0, 27.0, Align::Right, true);
        assert!(x(&f, "שלום") > x(&f, "עולם"));
        assert!(x(&f, "עולם") > x(&f, "world"));
        assert!(x(&f, "world") > x(&f, "hello"));
        // An English line with a Hebrew phrase: only the phrase turns around.
        let f = flow(&mut fv, line("my song שלום עולם remix"), &font, 1000.0, 27.0, Align::Left, false);
        assert!(x(&f, "song") < x(&f, "עולם") && x(&f, "עולם") < x(&f, "שלום") && x(&f, "שלום") < x(&f, "remix"));
        // A long line wraps instead of running past the edge.
        let f = flow(&mut fv, line(&"word ".repeat(40)), &font, 300.0, 27.0, Align::Left, false);
        assert!(f.height > 27.0 && f.words.iter().all(|w| w.pos.x + w.w <= 300.0 + 0.5));
    }

    /// Where each word of a two-word RTL phrase lands, laid out as before and as now, with
    /// the app's fonts: the first word belongs on the right (the old layout put it left).
    /// Needs the system fonts the app falls back to for Hebrew and Arabic.
    /// `cargo test --release -- --ignored rtl_word_positions --nocapture`
    #[test]
    #[ignore]
    fn rtl_word_positions() {
        let ctx = shot_ctx();
        let font = crate::theme::semibold(26.0);
        for (a, b) in [("אבג", "דהו"), ("عمر", "دياب")] {
            let text = format!("{a} {b}");
            let mean = |g: &Galley, w: &str| {
                let xs: Vec<f32> = g.rows.iter().flat_map(|r| r.row.glyphs.iter()).filter(|gl| w.contains(gl.chr)).map(|gl| gl.pos.x).collect();
                xs.iter().sum::<f32>() / xs.len().max(1) as f32
            };
            let (before, after) = ctx.fonts_mut(|f| (f.layout_no_wrap(text.clone(), font.clone(), Color32::WHITE), f.layout_job(crate::bidi::job(&text, font.clone(), Color32::WHITE))));
            println!("{text}: before first word x {:.0}, second {:.0} | after first {:.0}, second {:.0}", mean(&before, a), mean(&before, b), mean(&after, a), mean(&after, b));
            assert!(mean(&after, a) > mean(&after, b), "{text}: the first word should be on the right");
        }
    }

    /// The app's fonts in a headless egui, for the pictures below.
    fn shot_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx, "Inter");
        for _ in 0..2 {
            let _ = ctx.run_ui(Default::default(), |_| {});
        }
        ctx
    }

    /// Draw a galley's glyphs from the font atlas into `img`, only between `clip` (y).
    fn blit(img: &mut image::RgbaImage, atlas: &ColorImage, at: Pos2, g: &Galley, rgb: [u8; 3], clip: (f32, f32)) {
        let (w, h) = (img.width() as i64, img.height() as i64);
        for row in &g.rows {
            for gl in &row.row.glyphs {
                let uv = gl.uv_rect;
                if uv.is_nothing() {
                    continue;
                }
                let o = at + row.pos.to_vec2() + gl.pos.to_vec2() + uv.offset;
                for (dy, ty) in (uv.min[1]..uv.max[1]).enumerate() {
                    for (dx, tx) in (uv.min[0]..uv.max[0]).enumerate() {
                        let a = atlas.pixels[ty as usize * atlas.size[0] + tx as usize].a() as f32 / 255.0;
                        let (x, y) = ((o.x + dx as f32).round() as i64, (o.y + dy as f32).round() as i64);
                        if a <= 0.0 || x < 0 || y < 0 || x >= w || y >= h || (y as f32) < clip.0 || (y as f32) >= clip.1 {
                            continue;
                        }
                        let p = img.get_pixel_mut(x as u32, y as u32);
                        for c in 0..3 {
                            p[c] = (p[c] as f32 * (1.0 - a) + rgb[c] as f32 * a) as u8;
                        }
                    }
                }
            }
        }
    }

    /// Before/after picture of a lyric line taller than the view, while its 6th row is
    /// sung: the old view stayed at the line's top; the new one pages down with it. The
    /// view positions are the scroll code's (the line's top, then the paging rule).
    /// `YTM_SHOT=<file.png> cargo test --release -- --ignored paging_before_after_png`
    #[test]
    #[ignore]
    fn paging_before_after_png() {
        let ctx = shot_ctx();
        let lyric = "And I keep on running down the same old road that takes me back to you every single night, \
            through the rain and the lights of a city that never learned your name, and I still hear you calling out to me";
        let (width, view_h, font) = (470.0, 230.0, crate::theme::lyric_font_for("Inter", crate::preset::Weight::Bold, 30.0, lyric));
        let lh = 30.0 * 1.333;
        let toks = line_synced_parts(&Line { start: 0, dur: 1000, text: lyric.into(), ..Default::default() }).iter().flat_map(split_part).collect();
        let (words, galleys) = ctx.fonts_mut(|f| {
            let fl = flow(f, toks, &font, width, lh, Align::Left, false);
            let gs: Vec<Arc<Galley>> = fl.words.iter().map(|w| f.layout_no_wrap(w.text.clone(), font.clone(), Color32::WHITE)).collect();
            (fl.words, gs)
        });
        let labels = ctx.fonts_mut(|f| {
            ["Before (0.2.28): the 6th row is being sung, off the bottom", "After: the view pages down with it"].map(|t| f.layout_no_wrap(t.into(), crate::theme::semibold(16.0), Color32::WHITE))
        });
        let atlas = ctx.fonts(|f| f.image());
        let sung_row = 5.0;
        let y_row = sung_row * lh;
        // Old: the line's top. New: the sung row and the one after it in view.
        let tops = [0.0f32, (y_row + lh * 2.0 - view_h).max(0.0)];
        let (col, pad) = (width + 70.0, 30.0);
        let (iw, ih) = ((col * 2.0) as u32, (view_h + 110.0) as u32);
        let mut img = image::RgbaImage::from_pixel(iw, ih, image::Rgba([24, 22, 30, 255]));
        for (k, top) in tops.iter().enumerate() {
            let x0 = pad + col * k as f32;
            let (v0, v1) = (60.0, 60.0 + view_h);
            blit(&mut img, &atlas, pos2(x0, 20.0), &labels[k], [200, 196, 210], (0.0, ih as f32));
            // The view's frame.
            for x in (x0 - 10.0) as u32..(x0 + width + 10.0) as u32 {
                for y in [v0 as u32 - 1, v1 as u32] {
                    img.put_pixel(x, y, image::Rgba([90, 84, 104, 255]));
                }
            }
            for (w, g) in words.iter().zip(&galleys) {
                let row = (w.pos.y / lh).floor();
                let rgb = if row < sung_row { [242, 240, 245] } else if row == sung_row { [196, 160, 255] } else { [110, 106, 120] };
                blit(&mut img, &atlas, pos2(x0 + w.pos.x, v0 + w.pos.y - top), g, rgb, (v0, v1));
            }
        }
        let out = std::env::var("YTM_SHOT").unwrap_or_else(|_| "paging-before-after.png".into());
        img.save(&out).unwrap();
        println!("wrote {out}");
    }

    /// Before/after picture of right-to-left text with the app's fonts (#29), to a PNG.
    /// `YTM_SHOT=<file.png> cargo test --release -- --ignored rtl_before_after_png`
    #[test]
    #[ignore]
    fn rtl_before_after_png() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx, "Inter");
        for _ in 0..2 {
            let _ = ctx.run_ui(Default::default(), |_| {});
        }
        let titles = ["عمرو دياب - تملي معاك", "עומר אדם - תגידו לה", "Fairuz - نسم علينا الهوى (Live)"];
        let lyric = "يا ليل الصب متى غده أقيام الساعة موعده رقد السمار فأرقه أسف للبين يردده";
        let (col, width) = (560.0, 470.0);
        let title_font = crate::theme::semibold(26.0);
        let lyric_font = crate::theme::lyric_font_for("Inter", crate::preset::Weight::Bold, 30.0, lyric);
        // (position, galley) for everything drawn.
        let mut draw: Vec<(Pos2, Arc<Galley>)> = Vec::new();
        ctx.fonts_mut(|f| {
            for (i, label) in ["Before (0.2.28)", "After"].into_iter().enumerate() {
                draw.push((pos2(30.0 + col * i as f32, 20.0), f.layout_no_wrap(label.into(), crate::theme::bold(20.0), Color32::WHITE)));
            }
            for (k, t) in titles.iter().enumerate() {
                let y = 70.0 + k as f32 * 50.0;
                draw.push((pos2(30.0, y), f.layout_no_wrap(t.to_string(), title_font.clone(), Color32::WHITE)));
                draw.push((pos2(30.0 + col, y), f.layout_job(crate::bidi::job(t, title_font.clone(), Color32::WHITE))));
            }
            // Lyrics: the old view put the words left to right, wrapping at the width; the
            // new one from the right.
            let y = 240.0;
            let (lh, space) = (30.0 * 1.333, text_size(f, " ", &lyric_font).x);
            let mut at = vec2(0.0, 0.0);
            for word in lyric.split(' ') {
                let g = f.layout_no_wrap(word.into(), lyric_font.clone(), Color32::WHITE);
                if at.x > 0.0 && at.x + g.size().x > width {
                    at = vec2(0.0, at.y + lh);
                }
                let w = g.size().x;
                draw.push((pos2(30.0, y) + at, g));
                at.x += w + space;
            }
            let toks = line_synced_parts(&Line { start: 0, dur: 1000, text: lyric.into(), ..Default::default() }).iter().flat_map(split_part).collect();
            let fl = flow(f, toks, &lyric_font, width, 30.0 * 1.333, Align::Right, true);
            for w in fl.words {
                draw.push((pos2(30.0 + col, y) + w.pos.to_vec2(), f.layout_no_wrap(w.text.clone(), lyric_font.clone(), Color32::WHITE)));
            }
        });
        let atlas = ctx.fonts(|f| f.image());
        let (w, h) = ((col * 2.0) as u32, 470u32);
        let mut img = image::RgbaImage::from_pixel(w, h, image::Rgba([24, 22, 30, 255]));
        // The column divider.
        for y in 0..h {
            img.put_pixel(col as u32 - 15, y, image::Rgba([70, 66, 80, 255]));
        }
        for (at, g) in &draw {
            blit(&mut img, &atlas, *at, g, [242, 240, 245], (0.0, h as f32));
        }
        let out = std::env::var("YTM_SHOT").unwrap_or_else(|_| "rtl-before-after.png".into());
        img.save(&out).unwrap();
        println!("wrote {out}");
    }

    #[test]
    fn line_synced_words_are_staggered() {
        let l = Line { start: 2000, dur: 3000, text: "one two  three".into(), ..Default::default() };
        let p = line_synced_parts(&l);
        let words: Vec<_> = p.iter().filter(|p| !p.text.trim().is_empty()).collect();
        assert_eq!(words[1].start, 2050);
        assert_eq!(words[2].start, 2100);
    }

    #[test]
    fn letter_waves_crest_when_lit() {
        // A 1 s word with 4 letters: the last letter starts later than the first.
        let w = letter_waves(4, 1.0, 0.9);
        assert!(w[0].0 < w[3].0);
        // Zero-duration words float all letters together.
        assert!(letter_waves(3, 0.0, 0.9).iter().all(|w| w.0 == 0.0 && (w.1 - 0.9).abs() < 1e-9));
    }

    #[test]
    fn scroll_durations_follow_the_extension() {
        assert!((scroll_duration(0, 10.0) - (0.750 + 2f64.ln() * 0.08 + 0.02)).abs() < 1e-9);
        assert!((scroll_duration(-3, 10.0) - 0.810).abs() < 1e-9);
        assert!((scroll_duration(2, 10.0) - (0.750 + 4f64.ln() * 0.08 + 0.06)).abs() < 1e-9);
    }

    #[test]
    fn gradient_is_soft_and_never_a_box() {
        let look = WordLook { opacity: 1.0, start: 0.5, end: 0.6, glow: 0.0, glow_alpha: 0.5 };
        let word = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 40.0));
        let (f, cuts) = lit_gradient(&word, 0, 1, word, &look, false, false);
        assert_eq!(f(10.0), 1.0);
        assert!(f(55.0) > 0.0 && f(55.0) < 1.0);
        assert_eq!(f(70.0), 0.0);
        assert_eq!(cuts.len(), 2);
    }

    #[test]
    fn a_break_finishes_before_the_next_line() {
        // A 6 s break: the exit starts 0.85 s before the next line and lasts 0.35 s.
        let (exit, len, step) = gap_times(10.0, 6.0);
        assert!((exit - 15.15).abs() < 1e-9);
        assert!((len - 0.35).abs() < 1e-9);
        assert!((step - 5.15 / 3.0).abs() < 1e-9);
        // A break too short to count still opens, then uses its whole exit.
        let (exit, len, _) = gap_times(10.0, 1.0);
        assert!((exit - 10.62).abs() < 1e-9);
        assert!((len - 0.35).abs() < 1e-9);
    }
}
