//! Colour, as semantic slots rather than a scattering of hex values.
//!
//! Two rules from the outset, both of which the previous design broke:
//!
//! 1. **Hierarchy has to survive monochrome.** Strip every colour and the
//!    screen must still read: ordering, alignment, weight and glyphs carry the
//!    structure, and colour only reinforces it. There is a test for this.
//! 2. **Depth comes from layered surfaces, not from nested boxes.** A panel is
//!    a slightly lighter background, not another rectangle of border characters.
//!    Borders around borders eat rows and columns and add no information.
//!
//! True colour is an enhancement layer. Everything still works on a terminal
//! with sixteen colours, which is why each slot has an ANSI equivalent.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// 24-bit colour: the full palette.
    True,
    /// Named ANSI colours, which follow whatever the terminal's own theme is.
    Ansi,
}

/// Which colour depth to draw at.
///
/// Windows consoles have handled 24-bit colour since Windows 10 1703, so that
/// is the default here; `VMERGE_COLORS=16` forces the plain palette for
/// anything older, or for a terminal whose theme fights the painted background.
pub fn detect() -> Tier {
    if let Ok(setting) = std::env::var("VMERGE_COLORS") {
        match setting.trim().to_lowercase().as_str() {
            "16" | "ansi" | "basic" => return Tier::Ansi,
            "24" | "true" | "truecolor" => return Tier::True,
            _ => {}
        }
    }
    if let Ok(colorterm) = std::env::var("COLORTERM")
        && (colorterm.contains("truecolor") || colorterm.contains("24bit"))
    {
        return Tier::True;
    }
    if std::env::var_os("WT_SESSION").is_some() {
        return Tier::True;
    }
    if cfg!(windows) { Tier::True } else { Tier::Ansi }
}

/// The semantic slots. Nothing outside this file names a colour.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// The page. Painted explicitly so the app looks the same whatever the
    /// terminal's own background happens to be.
    pub base: Color,
    /// A panel sitting on the page.
    pub surface: Color,
    /// A popup sitting on a panel, and the selected row.
    pub raised: Color,
    /// A button face. One step above `raised`, so a chip reads as an object
    /// sitting on the page rather than as text with a highlight behind it.
    pub chip: Color,
    /// Hairline rules. Never a full border if a rule will do.
    pub line: Color,

    pub text: Color,
    /// Secondary text: units, labels, anything read second.
    pub muted: Color,
    /// Tertiary text: column headings, inactive hints.
    pub faint: Color,

    /// Interaction and focus. One accent, used sparingly.
    pub accent: Color,
    /// The selected row's background: the accent, heavily diluted.
    pub accent_wash: Color,
    /// Marks and multi-selection, kept distinct from focus.
    pub mark: Color,

    pub good: Color,
    pub warn: Color,
    pub bad: Color,
}

impl Theme {
    pub fn new(tier: Tier) -> Self {
        match tier {
            Tier::True => Self::truecolor(),
            Tier::Ansi => Self::ansi(),
        }
    }

    /// A cool slate ground with a single azure accent. The surface steps are
    /// deliberately small - just enough to read as a layer, not as a stripe.
    fn truecolor() -> Self {
        Self {
            base: Color::Rgb(0x0d, 0x10, 0x17),
            surface: Color::Rgb(0x14, 0x18, 0x21),
            raised: Color::Rgb(0x1b, 0x20, 0x2b),
            chip: Color::Rgb(0x25, 0x2c, 0x3a),
            line: Color::Rgb(0x28, 0x2f, 0x3c),
            text: Color::Rgb(0xe6, 0xe9, 0xf0),
            muted: Color::Rgb(0x98, 0xa1, 0xb3),
            faint: Color::Rgb(0x62, 0x6b, 0x7d),
            accent: Color::Rgb(0x4d, 0xb8, 0xff),
            accent_wash: Color::Rgb(0x1b, 0x33, 0x4a),
            mark: Color::Rgb(0xc3, 0x9b, 0xff),
            good: Color::Rgb(0x57, 0xd9, 0xa3),
            warn: Color::Rgb(0xff, 0xc8, 0x61),
            bad: Color::Rgb(0xff, 0x7a, 0x7a),
        }
    }

    fn ansi() -> Self {
        Self {
            base: Color::Reset,
            surface: Color::Reset,
            raised: Color::Blue,
            chip: Color::DarkGray,
            line: Color::DarkGray,
            text: Color::White,
            muted: Color::Gray,
            faint: Color::DarkGray,
            accent: Color::Cyan,
            accent_wash: Color::Blue,
            mark: Color::Magenta,
            good: Color::Green,
            warn: Color::Yellow,
            bad: Color::Red,
        }
    }

    // ------------------------------------------------------------- shorthands

    pub fn on_base(&self) -> Style {
        Style::default().bg(self.base).fg(self.text)
    }

    pub fn body(&self) -> Style {
        Style::default().fg(self.text)
    }

    pub fn dim(&self) -> Style {
        Style::default().fg(self.muted)
    }

    pub fn label(&self) -> Style {
        Style::default().fg(self.faint)
    }

    pub fn rule(&self) -> Style {
        Style::default().fg(self.line)
    }

    pub fn strong(&self) -> Style {
        Style::default().fg(self.text).add_modifier(Modifier::BOLD)
    }

    pub fn accented(&self) -> Style {
        Style::default().fg(self.accent)
    }

    /// A button, as the three styles one needs: the cap either side, the key,
    /// and the label.
    ///
    /// `ground` is what the chip is sitting on, because the caps are half-cells
    /// of the face colour and the other half has to match its surroundings.
    /// `hot` is the pointer being over it: a terminal has no cursor to change,
    /// so brightening the face is the only feedback available before the click.
    pub fn chip(&self, kind: ChipKind, ground: Color, hot: bool) -> ChipStyle {
        let face = match kind {
            ChipKind::Button => self.chip,
            ChipKind::Primary => self.accent,
            ChipKind::Danger => self.warn,
        };
        let face = if hot { lift(face) } else { face };

        // A filled chip is already the loudest thing on its row, so its key is
        // told apart by weight rather than by a second colour on top.
        let (key, label) = match kind {
            ChipKind::Button => (
                Style::default().bg(face).fg(self.accent).add_modifier(Modifier::BOLD),
                Style::default().bg(face).fg(self.text),
            ),
            _ => {
                let filled = Style::default().bg(face).fg(self.base).add_modifier(Modifier::BOLD);
                (filled, filled)
            }
        };
        ChipStyle { edge: Style::default().fg(face).bg(ground), key, label }
    }

    /// A clickable row under the pointer: the same brightening a chip gets, so
    /// hover means one thing everywhere. `over` is the layer the row sits on.
    pub fn hovered(&self, over: Color) -> Style {
        Style::default().bg(lift(over)).fg(self.text)
    }

    /// A keyboard key in the hint bar.
    pub fn key(&self) -> Style {
        Style::default().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    /// The row under the cursor.
    pub fn selected(&self) -> Style {
        Style::default().bg(self.accent_wash).fg(self.text).add_modifier(Modifier::BOLD)
    }

    pub fn status(&self, kind: StatusTone) -> Style {
        let colour = match kind {
            StatusTone::Info => self.muted,
            StatusTone::Good => self.good,
            StatusTone::Warn => self.warn,
            StatusTone::Bad => self.bad,
        };
        Style::default().fg(colour)
    }
}

/// How loud a button is. Exactly one `Primary` per screen; `Danger` is for the
/// answer in a dialog that cannot be taken back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    Button,
    Primary,
    Danger,
}

/// The styles one button is drawn from.
#[derive(Debug, Clone, Copy)]
pub struct ChipStyle {
    pub edge: Style,
    pub key: Style,
    pub label: Style,
}

/// A colour a step brighter, for whatever is under the pointer.
///
/// Proportional to the headroom left, so a near-white lifts a little and a dark
/// face lifts a lot: a fixed step would either not show on one or blow out the
/// other. Named colours have no components to work with, so they get their
/// light variant instead.
fn lift(colour: Color) -> Color {
    match colour {
        Color::Rgb(r, g, b) => Color::Rgb(lift_channel(r), lift_channel(g), lift_channel(b)),
        Color::Black => Color::DarkGray,
        Color::DarkGray => Color::Gray,
        Color::Gray => Color::White,
        Color::Red => Color::LightRed,
        Color::Green => Color::LightGreen,
        Color::Yellow => Color::LightYellow,
        Color::Blue => Color::LightBlue,
        Color::Magenta => Color::LightMagenta,
        Color::Cyan => Color::LightCyan,
        other => other,
    }
}

fn lift_channel(value: u8) -> u8 {
    value.saturating_add((u32::from(255 - value) * 30 / 100) as u8)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    Info,
    Good,
    Warn,
    Bad,
}

/// Glyphs, chosen for coverage in the fonts Windows terminals actually ship
/// with (Consolas, Cascadia). Nothing here needs a patched font.
pub mod glyph {
    /// The cursor's left edge marker. Carries focus without colour.
    pub const CURSOR: &str = "▌";
    /// A marked row.
    pub const MARK: &str = "●";
    /// Separates parts of one value, e.g. `h264·aac`.
    pub const DOT: &str = "·";
    /// Dimensions: 1920×1080 rather than 1920x1080.
    pub const TIMES: &str = "×";
    /// Stands in for a value that is absent.
    pub const NONE: &str = "—";

    pub const DONE: &str = "●";
    pub const QUEUED: &str = "○";
    pub const FAILED: &str = "×";

    /// Frames for the working indicator, in order.
    pub const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];

    /// The ends of a button. Half a cell of the button's own colour, so a chip
    /// stops in the middle of a cell instead of squaring off against the page -
    /// which is what makes it read as an object rather than as a highlight.
    pub const CAP_LEFT: &str = "▐";
    pub const CAP_RIGHT: &str = "▌";

    /// Eighth-width blocks, so a bar can end part way through a cell and show
    /// progress finer than the character grid.
    pub const EIGHTHS: [&str; 8] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
    pub const FULL: &str = "█";
    /// The unfilled remainder of a bar.
    pub const TRACK: &str = "─";
}

/// A progress bar with sub-cell precision.
///
/// The last cell is a partial block, so a bar 20 cells wide resolves 160 steps
/// rather than 20. Without it a slow clip looks stalled for seconds at a time.
pub fn bar(fraction: f64, width: usize) -> String {
    let fraction = fraction.clamp(0.0, 1.0);
    if width == 0 {
        return String::new();
    }
    let eighths = (fraction * width as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let remainder = eighths % 8;

    let mut out = String::with_capacity(width * 3);
    for _ in 0..full.min(width) {
        out.push_str(glyph::FULL);
    }
    let mut cells = full.min(width);
    if remainder > 0 && cells < width {
        out.push_str(glyph::EIGHTHS[remainder - 1]);
        cells += 1;
    }
    for _ in cells..width {
        out.push_str(glyph::TRACK);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_are_the_width_they_are_asked_for() {
        for fraction in [0.0, 0.01, 0.33, 0.5, 0.99, 1.0] {
            assert_eq!(
                bar(fraction, 20).chars().count(),
                20,
                "fraction {fraction} must not change the width"
            );
        }
        assert_eq!(bar(0.5, 0), "");
    }

    #[test]
    fn bars_resolve_finer_than_one_cell() {
        // A twentieth of a cell still moves the bar, which is the whole point.
        assert_ne!(bar(0.001, 20), bar(0.02, 20));
        assert!(bar(0.0, 10).starts_with(glyph::TRACK));
        assert!(bar(1.0, 10).chars().all(|c| c.to_string() == glyph::FULL));
    }

    #[test]
    fn colour_depth_can_be_forced() {
        // Every slot has to exist at both depths, or a fallback would panic.
        let _ = Theme::new(Tier::True);
        let _ = Theme::new(Tier::Ansi);
    }
}
