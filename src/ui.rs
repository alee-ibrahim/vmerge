//! Drawing. Every value shown here comes from App; nothing is decided.
//!
//! The design follows four rules:
//!
//! * **Monochrome first.** Ordering, alignment, weight and glyphs carry the
//!   structure; colour only reinforces it. `hierarchy_survives_monochrome`
//!   checks this rather than trusting it.
//! * **Layers, not boxes.** Panels are a lighter background between hairline
//!   rules. Nested borders spend rows and columns to say nothing.
//! * **Every cell counts.** Numbers are right-aligned in columns sized to their
//!   content, so they can be compared down the screen.
//! * **One primary action.** Exactly one filled chip per screen: `S START MERGE`
//!   with clips loaded, `A ADD CLIPS` when there are none.
//! * **A button looks pressable, or it is not a button.** Anything clickable is
//!   a chip - padded, capped at both ends, lit under the pointer, and clickable
//!   across all of that. Anything that can only be typed stays plain text, and
//!   anything with nothing to act on is greyed out rather than answering a click
//!   with a complaint.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::{
    App, Confirm, HelpSheet, Kind, Menu, Overlay, Prompt, PromptKind, Screen, SegState,
};
use crate::fetch::Stage;
use crate::format;
use crate::merge::{Outcome, Step};
use crate::theme::{self, ChipKind, ChipStyle, StatusTone, Theme, glyph};

/// What clicking somewhere on the screen means. Recorded during drawing,
/// because drawing is the only place that knows where anything ended up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Click {
    Row(usize),
    MenuItem(usize),
    /// Routed through the same handler as the key of that name.
    Command(char),
    Mark,
    Remove,
    Back,
    Answer(bool),
    /// Accept what has been typed into a prompt.
    Submit,
    /// Back out of an overlay, changing nothing.
    Cancel,
    /// Somewhere with nothing on it. Registered so that a dialog which closes
    /// when clicked off does not also close when clicked on.
    Ignore,
}

/// How one entry in a hint bar is drawn.
#[derive(Clone, Copy, PartialEq)]
enum Look {
    /// A keyboard reminder. Plain text, because there is nothing to press here
    /// with a mouse and pretending otherwise is worse than saying nothing.
    Plain,
    /// Nothing for it to act on yet. It keeps its words and its width, so the
    /// bar does not rearrange itself as the list fills up, but it loses the
    /// face that said it could be pressed.
    Off,
    Button,
    Primary,
    Danger,
}

/// One entry in a hint bar: the key, what it does, and whether it can be clicked.
struct Hint<'a> {
    key: &'a str,
    label: &'a str,
    click: Option<Click>,
    look: Look,
}

const fn hint<'a>(key: &'a str, label: &'a str, click: Option<Click>) -> Hint<'a> {
    Hint { key, label, click, look: Look::Button }
}

const fn nav<'a>(key: &'a str, label: &'a str) -> Hint<'a> {
    Hint { key, label, click: None, look: Look::Plain }
}

const fn primary<'a>(key: &'a str, label: &'a str, click: Click) -> Hint<'a> {
    Hint { key, label, click: Some(click), look: Look::Primary }
}

/// The answer a dialog cannot take back.
const fn danger<'a>(key: &'a str, label: &'a str, click: Click) -> Hint<'a> {
    Hint { key, label, click: Some(click), look: Look::Danger }
}

/// The same button, greyed out and inert, when there is nothing for it to do.
///
/// Better than letting it be pressed and answering with a complaint: the answer
/// is on screen before the click rather than after it.
fn only_if(enabled: bool, item: Hint) -> Hint {
    if enabled { item } else { Hint { click: None, look: Look::Off, ..item } }
}

/// Scroll offsets and other view-only state, kept across frames so the list
/// does not jump about.
pub struct UiState {
    pub theme: Theme,
    /// First visible clip. Owned here rather than by a widget, because the hit
    /// regions have to agree with it exactly.
    offset: usize,
    /// Advances once per redraw, to animate the working indicator.
    frame: usize,
    hits: Vec<(Rect, Click)>,
    /// Where the pointer is. Whatever sits under it is drawn a shade brighter,
    /// which is the only way a terminal can say "this one" before the click.
    pointer: Option<(u16, u16)>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            theme: Theme::new(theme::detect()),
            offset: 0,
            frame: 0,
            hits: Vec::new(),
            pointer: None,
        }
    }
}

impl UiState {
    fn add_hit(&mut self, area: Rect, click: Click) {
        if area.width > 0 && area.height > 0 {
            self.hits.push((area, click));
        }
    }

    pub fn set_pointer(&mut self, column: u16, row: u16) {
        self.pointer = Some((column, row));
    }

    /// Whether the pointer is over this region.
    fn hovered(&self, area: Rect) -> bool {
        self.pointer
            .is_some_and(|(column, row)| area.contains(Position::new(column, row)))
    }

    /// What is under the pointer. Later regions win, which is what makes an
    /// overlay's buttons beat whatever it is covering.
    pub fn hit(&self, column: u16, row: u16) -> Option<Click> {
        self.hits
            .iter()
            .rev()
            .find(|(area, _)| area.contains(Position::new(column, row)))
            .map(|(_, click)| *click)
    }

    fn spinner(&self) -> &'static str {
        glyph::SPINNER[(self.frame / 2) % glyph::SPINNER.len()]
    }
}

pub fn draw(frame: &mut Frame, app: &App, ui: &mut UiState) {
    let area = frame.area();
    ui.hits.clear();
    ui.frame = ui.frame.wrapping_add(1);
    // With the mouse released no move events arrive, so the last known pointer
    // would leave a button lit up under nothing.
    if !app.mouse {
        ui.pointer = None;
    }

    // The page is painted rather than inherited, so the app looks the same
    // whatever the terminal's own background is.
    frame.render_widget(Block::default().style(ui.theme.on_base()), area);

    match &app.screen {
        Screen::Browse => draw_browse(frame, app, ui, area),
        Screen::Merging(view) | Screen::Converting(view) => {
            draw_progress(frame, area, view, ui)
        }
        Screen::Fetching(view) => draw_fetching(frame, area, view, ui),
        Screen::Result(outcome) | Screen::Fetched(outcome) | Screen::Converted(outcome) => {
            draw_result(frame, area, outcome, ui)
        }
    }

    if !matches!(app.overlay, Overlay::None) {
        // A dialog owns the surface: nothing behind it should be clickable.
        ui.hits.clear();
    }
    match &app.overlay {
        Overlay::None => {}
        Overlay::Prompt(prompt) => draw_prompt(frame, area, prompt, ui),
        Overlay::Menu(menu) => draw_menu(frame, area, menu, ui),
        Overlay::Confirm(confirm) => draw_confirm(frame, area, confirm, ui),
        Overlay::Help(help) => draw_help(frame, area, help, ui),
    }
}

// --------------------------------------------------------------------- chrome

/// Wordmark on the left, the numbers that change on the right.
fn header(frame: &mut Frame, area: Rect, ui: &UiState, right: &str) {
    let theme = &ui.theme;
    let left = Line::from(vec![
        Span::raw("  "),
        Span::styled("VIDEO", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(" MERGER", theme.strong()),
    ]);
    frame.render_widget(Paragraph::new(left), area);
    frame.render_widget(
        Paragraph::new(Line::styled(format!("{right}  "), theme.dim())).alignment(Alignment::Right),
        area,
    );
}

/// A hairline. One row, and it separates as well as a box does.
fn rule(frame: &mut Frame, area: Rect, ui: &UiState) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::styled("─".repeat(area.width as usize), ui.theme.rule())),
        area,
    );
}

fn fill(frame: &mut Frame, area: Rect, style: Style) {
    frame.render_widget(Block::default().style(style), area);
}

/// How wide the chip for this key and label comes out. Must agree with
/// `chip_spans`, or the click targets drift along the row: every chip after the
/// first inherits the error of the ones before it.
fn chip_width(key: &str, label: &str) -> u16 {
    // cap, space, key, space, label, space, cap
    (key.chars().count() + label.chars().count()) as u16 + 5
}

/// One button: padded, capped at both ends, and a single object to click.
///
/// The padding is the point. A key and some words with nothing around them read
/// as a caption; the same text with a face behind it and air inside reads as
/// something to press.
fn chip_spans(key: &str, label: &str, style: ChipStyle) -> Vec<Span<'static>> {
    vec![
        Span::styled(glyph::CAP_LEFT, style.edge),
        Span::styled(format!(" {key}"), style.key),
        Span::styled(format!(" {label} "), style.label),
        Span::styled(glyph::CAP_RIGHT, style.edge),
    ]
}

/// A row of buttons and keyboard reminders.
///
/// Anything clickable is drawn as a chip whose hit region covers the whole
/// button, padding and caps included - the label is what a person aims at, but
/// the edges are what they hit. Anything that can only be typed stays plain
/// text, so the bar keeps saying which is which.
///
/// `ground` is the surface underneath, needed for the half-cell caps.
fn hint_bar(frame: &mut Frame, area: Rect, ui: &mut UiState, ground: Color, rows: &[&[Hint]]) {
    /// Between two chips. They carry their own padding, so this is the gap
    /// between buttons rather than between words.
    const CHIP_GAP: u16 = 2;
    /// Between two plain hints, which have no face to separate them.
    const TEXT_GAP: u16 = 3;
    let theme = ui.theme;

    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    for (row_index, row) in rows.iter().enumerate() {
        let mut spans = vec![Span::raw("  ")];
        let mut x = area.x + 2;
        let y = area.y + row_index as u16;

        for item in row.iter() {
            // A button nobody can click would be a lie, so anything without a
            // click behind it falls back to being a reminder.
            let look = match item.look {
                Look::Off => Look::Off,
                other if item.click.is_some() => other,
                _ => Look::Plain,
            };
            let kind = match look {
                Look::Off => {
                    // The chip's own shape, in spaces: same words, same width,
                    // nothing to press.
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(format!(" {}", item.key), theme.label()));
                    spans.push(Span::styled(format!(" {} ", item.label), theme.label()));
                    spans.push(Span::raw(" ".repeat(CHIP_GAP as usize + 1)));
                    x += chip_width(item.key, item.label) + CHIP_GAP;
                    continue;
                }
                Look::Plain => {
                    let width = (item.key.chars().count() + 1 + item.label.chars().count()) as u16;
                    spans.push(Span::styled(
                        item.key,
                        Style::default().fg(theme.faint).add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::styled(
                        format!(" {}{}", item.label, " ".repeat(TEXT_GAP as usize)),
                        theme.label(),
                    ));
                    x += width + TEXT_GAP;
                    continue;
                }
                Look::Button => ChipKind::Button,
                Look::Primary => ChipKind::Primary,
                Look::Danger => ChipKind::Danger,
            };

            let width = chip_width(item.key, item.label);
            let button = Rect::new(x, y, clamp_width(width, x, area), 1);
            spans.extend(chip_spans(
                item.key,
                item.label,
                theme.chip(kind, ground, ui.hovered(button)),
            ));
            spans.push(Span::raw(" ".repeat(CHIP_GAP as usize)));
            if let Some(click) = item.click {
                ui.add_hit(button, click);
            }
            x += width + CHIP_GAP;
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn clamp_width(width: u16, x: u16, area: Rect) -> u16 {
    let used = x.saturating_sub(area.x);
    width.min(area.width.saturating_sub(used))
}

/// The measure everything on a screen shares.
///
/// Capped, because on a 200-column terminal a full-width table pushes the name
/// and the numbers so far apart that they stop reading as one row. A fixed
/// measure with empty page beside it looks deliberate; a stretched one does not.
fn page(area: Rect) -> Rect {
    const MAX: u16 = 112;
    Rect { width: area.width.min(MAX), ..area }
}

/// Windows paths with forward slashes in them are legal and look like a mistake.
fn display_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    if cfg!(windows) { text.replace('/', "\\") } else { text }
}

fn status_line(frame: &mut Frame, area: Rect, app: &App, ui: &UiState) {
    let Some((text, kind)) = &app.status else {
        return;
    };
    let tone = match kind {
        Kind::Info => StatusTone::Info,
        Kind::Good => StatusTone::Good,
        Kind::Warn => StatusTone::Warn,
        Kind::Bad => StatusTone::Bad,
    };
    // The bullet is what marks this as a message, so it still reads as one
    // when the colour is gone.
    let line = Line::from(vec![
        Span::styled("  › ", ui.theme.status(tone)),
        Span::styled(text.clone(), ui.theme.status(tone)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// --------------------------------------------------------------------- browse

/// Rows the button bar takes on the list screen.
const BUTTON_ROWS: u16 = 3;

fn draw_browse(frame: &mut Frame, app: &App, ui: &mut UiState, frame_area: Rect) {
    let area = page(frame_area);
    let plan = app.plan_lines();

    // The list hugs what it holds rather than stretching: four clips in a
    // forty-row window should not sit in a thirty-row empty panel. It still
    // takes everything available once there are enough clips to need it.
    let wanted = if app.clips.is_empty() { 9 } else { app.clips.len() as u16 + 1 };
    let fixed = 1 + 1 + 1 + (1 + plan.len() as u16) + 1 + BUTTON_ROWS + 1;
    let list_height = wanted.clamp(4, area.height.saturating_sub(fixed).max(4));

    // A blank row above the buttons, because a toolbar pressed up against the
    // text above it reads as one more line of that text.
    let [head, top_rule, list, mid_rule, info, _, hints, status, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(list_height),
        Constraint::Length(1),
        Constraint::Length(1 + plan.len() as u16),
        Constraint::Length(1),
        Constraint::Length(BUTTON_ROWS),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    // The numbers that change live in the header, marks included: the list
    // itself should not have to carry a running total in its heading.
    let summary = if app.clips.is_empty() {
        String::new()
    } else {
        let marked = app.marked_count();
        let mut parts = vec![format!("{} clips", app.clips.len())];
        if marked > 0 {
            parts.push(format!("{marked} marked"));
        }
        parts.push(format::duration(app.total_duration()));
        parts.push(format::size(app.total_size()));
        parts.join(&format!(" {} ", glyph::DOT))
    };
    header(frame, head, ui, &summary);
    rule(frame, top_rule, ui);

    if app.clips.is_empty() {
        draw_drop_zone(frame, list, ui);
    } else {
        draw_clip_list(frame, list, app, ui);
    }

    rule(frame, mid_rule, ui);
    draw_info(frame, info, app, ui, &plan);

    let mouse_label = if app.mouse { "mouse off" } else { "mouse on" };
    let ground = ui.theme.base;
    // With nothing loaded, the buttons that act on clips have nothing to act on,
    // and the one action worth taking is the drop zone's own ADD CLIPS.
    let any = !app.clips.is_empty();
    hint_bar(
        frame,
        hints,
        ui,
        ground,
        // Three rows rather than two: buttons take the room their padding needs,
        // and a row that runs off a narrow terminal loses its last button
        // entirely. Grouped by what they are for - the merge and how it comes
        // out, then the other jobs and the list they act on, then the program
        // itself.
        &[
            &[
                only_if(any, primary("S", "START MERGE", Click::Command('s'))),
                hint("o", "output", Some(Click::Command('o'))),
                hint("q", "quality", Some(Click::Command('q'))),
                hint("t", "target", Some(Click::Command('t'))),
                hint("e", "encoder", Some(Click::Command('e'))),
            ],
            &[
                only_if(any, hint("v", "convert", Some(Click::Command('v')))),
                // Never greyed out: a link needs no clips to work on, and with
                // an empty list it is the second thing worth doing.
                hint("u", "download", Some(Click::Command('u'))),
                hint("a", "add", Some(Click::Command('a'))),
                only_if(any, hint("space", "mark", Some(Click::Mark))),
                only_if(any, hint("del", "remove", Some(Click::Remove))),
            ],
            &[
                only_if(any, hint("c", "clear", Some(Click::Command('c')))),
                hint("m", mouse_label, Some(Click::Command('m'))),
                hint("?", "more", Some(Click::Command('?'))),
                hint("x", "exit", Some(Click::Command('x'))),
                nav("↑↓", "move"),
                nav("⇧↑↓", "reorder"),
            ],
        ],
    );

    status_line(frame, status, app, ui);
}

/// The empty state. A drop target rather than a paragraph in a box.
fn draw_drop_zone(frame: &mut Frame, area: Rect, ui: &mut UiState) {
    let theme = ui.theme;
    let inner = centred(area, 54, 8);
    fill(frame, inner, Style::default().bg(theme.surface));

    // Nothing else on this screen can be clicked yet, so the whole panel is the
    // button and the chip inside it says so.
    let hot = ui.hovered(inner);
    ui.add_hit(inner, Click::Command('a'));

    const KEY: &str = "A";
    const LABEL: &str = "ADD CLIPS";
    let lines = vec![
        Line::raw(""),
        Line::styled("↓", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Line::raw(""),
        Line::styled("DROP YOUR CLIPS HERE", theme.strong()),
        Line::styled("a folder works too, and so does a pasted path", theme.dim()),
        Line::raw(""),
        Line::from(chip_spans(
            KEY,
            LABEL,
            // Filled: with an empty list this is the one action worth taking, so
            // it is where the screen's single primary chip lives. The ground is
            // the panel rather than the page, because that is what the caps are
            // actually sitting on.
            theme.chip(ChipKind::Primary, theme.surface, hot),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}

/// Column widths for the clip list. Numbers are right-aligned so they can be
/// compared down the screen; the name takes whatever is left.
struct Columns {
    cursor: u16,
    index: u16,
    name: u16,
    size: u16,
    fps: u16,
    codec: u16,
    length: u16,
}

impl Columns {
    const GAP: u16 = 2;

    fn for_width(width: u16) -> Self {
        let mut columns = Self { cursor: 3, index: 3, name: 0, size: 11, fps: 6, codec: 10, length: 7 };
        let fixed = columns.cursor
            + columns.index
            + columns.size
            + columns.fps
            + columns.codec
            + columns.length
            + Self::GAP * 5;
        // The name column absorbs the slack, down to something still readable.
        columns.name = width.saturating_sub(fixed + 2).max(8);
        columns
    }
}

fn draw_clip_list(frame: &mut Frame, area: Rect, app: &App, ui: &mut UiState) {
    let theme = ui.theme;
    let columns = Columns::for_width(area.width);

    let [heading, rows_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);

    // Column headings in the faintest slot: present for reference, never
    // competing with the data.
    let heading_line = Line::from(vec![
        Span::raw(" ".repeat(columns.cursor as usize)),
        Span::styled(format!("{:>w$}", "#", w = columns.index as usize), theme.label()),
        Span::raw("  "),
        Span::styled(format!("{:<w$}", "CLIP", w = columns.name as usize), theme.label()),
        Span::raw("  "),
        Span::styled(format!("{:>w$}", "SIZE", w = columns.size as usize), theme.label()),
        Span::raw("  "),
        Span::styled(format!("{:>w$}", "FPS", w = columns.fps as usize), theme.label()),
        Span::raw("  "),
        Span::styled(format!("{:<w$}", "FORMAT", w = columns.codec as usize), theme.label()),
        Span::raw("  "),
        Span::styled(format!("{:>w$}", "LENGTH", w = columns.length as usize), theme.label()),
    ]);
    frame.render_widget(Paragraph::new(heading_line), heading);

    fill(frame, rows_area, Style::default().bg(theme.surface));

    // Keep the cursor in view with a little context above and below it.
    let visible = rows_area.height as usize;
    let margin = 2usize.min(visible / 3);
    if app.cursor < ui.offset + margin {
        ui.offset = app.cursor.saturating_sub(margin);
    }
    if visible > 0 && app.cursor + margin >= ui.offset + visible {
        ui.offset = (app.cursor + margin + 1).saturating_sub(visible);
    }
    let max_offset = app.clips.len().saturating_sub(visible);
    ui.offset = ui.offset.min(max_offset);

    let mut lines: Vec<Line> = Vec::with_capacity(visible);
    for row in 0..visible {
        let index = ui.offset + row;
        let Some(entry) = app.clips.get(index) else {
            break;
        };
        let clip = &entry.clip;
        let selected = index == app.cursor;
        let line_area = Rect::new(rows_area.x, rows_area.y + row as u16, rows_area.width, 1);

        // Hover, painted under the row rather than into it: the spans below set a
        // colour and leave the background alone, so it shows through them and the
        // selected row's own wash still wins.
        if !selected && ui.hovered(line_area) {
            fill(frame, line_area, theme.hovered(theme.surface));
        }

        let row_style = if selected { theme.selected() } else { Style::default().fg(theme.text) };
        let value_style = if selected { theme.selected() } else { theme.dim() };

        // Focus is a solid bar, marking is a bullet: two different shapes, so
        // they stay distinguishable with no colour at all.
        let (edge, edge_style) = if selected {
            (glyph::CURSOR, Style::default().fg(theme.accent).bg(theme.accent_wash))
        } else {
            (" ", Style::default())
        };
        let (marker, marker_style) = if entry.marked {
            (glyph::MARK, Style::default().fg(theme.mark))
        } else {
            (" ", Style::default())
        };

        // Every one of these reads "—" rather than a zero for a file that has no
        // picture: an mp3 in the list is a conversion waiting to happen, not a
        // clip that measured 0x0 at 0 fps.
        let codec = clip.codec_label();

        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(edge, edge_style),
            Span::styled(marker, if selected { marker_style.bg(theme.accent_wash) } else { marker_style }),
            Span::styled(format!("{:>w$}", index + 1, w = columns.index as usize), if selected { theme.selected() } else { theme.label() }),
            Span::styled("  ", row_style),
            Span::styled(
                format!("{:<w$}", format::ellipsize(&clip.name, columns.name as usize), w = columns.name as usize),
                row_style,
            ),
            Span::styled("  ", row_style),
            Span::styled(
                format!("{:>w$}", clip.dimensions(), w = columns.size as usize),
                value_style,
            ),
            Span::styled("  ", row_style),
            Span::styled(format!("{:>w$}", clip.fps_label(), w = columns.fps as usize), value_style),
            Span::styled("  ", row_style),
            Span::styled(format!("{:<w$}", format::ellipsize(&codec, columns.codec as usize), w = columns.codec as usize), value_style),
            Span::styled("  ", row_style),
            Span::styled(format!("{:>w$}", format::duration(clip.duration), w = columns.length as usize), value_style),
        ]));

        ui.add_hit(line_area, Click::Row(index));
    }
    frame.render_widget(Paragraph::new(lines), rows_area);

    // A count of what is out of sight, rather than a scrollbar nobody can grab.
    let hidden = app.clips.len().saturating_sub(ui.offset + visible);
    if hidden > 0 && rows_area.height > 0 {
        let note = format!(" +{hidden} more ");
        let width = note.chars().count() as u16;
        let spot = Rect::new(
            rows_area.right().saturating_sub(width + 1),
            rows_area.bottom().saturating_sub(1),
            width.min(rows_area.width),
            1,
        );
        frame.render_widget(
            Paragraph::new(Line::styled(note, Style::default().fg(theme.faint).bg(theme.raised))),
            spot,
        );
    }
}

/// Output, settings and what pressing S will actually do.
fn draw_info(frame: &mut Frame, area: Rect, app: &App, ui: &UiState, plan: &[String]) {
    let theme = &ui.theme;
    let output = app
        .resolved_output()
        .map(|p| display_path(&p))
        .unwrap_or_else(|| app.output_name.clone());

    // Uppercase labels throughout this block, matching OUTPUT and PLAN. It also
    // keeps these words distinct from the lowercase ones in the hint bar.
    let mut settings = vec![
        Span::styled("QUALITY ", theme.label()),
        Span::styled(app.quality.label(), theme.dim()),
        Span::styled("   ENCODER ", theme.label()),
        Span::styled(app.encoder.label(), theme.dim()),
    ];
    if app.force_reencode {
        settings.push(Span::styled("   forced re-encode", Style::default().fg(theme.warn)));
    }
    settings.push(Span::raw("  "));

    let [first, rest] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled("OUTPUT  ", theme.label()),
            Span::styled(output, theme.body()),
        ])),
        first,
    );
    frame.render_widget(
        Paragraph::new(Line::from(settings)).alignment(Alignment::Right),
        first,
    );

    let lines: Vec<Line> = plan
        .iter()
        .enumerate()
        .map(|(i, text)| {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(if i == 0 { "PLAN    " } else { "        " }, theme.label()),
                Span::styled(text.clone(), theme.dim()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rest);
}

// -------------------------------------------------------------------- merging

/// The merge screen, and the conversion screen: one row per clip, a bar each, and
/// one bar for the lot. The two jobs differ in three words, so they share the
/// drawing rather than having a copy each that can drift apart.
fn draw_progress(frame: &mut Frame, frame_area: Rect, view: &crate::app::MergeView, ui: &mut UiState) {
    let area = page(frame_area);
    let theme = ui.theme;

    let plan_rows = view.plan.len().min(2) as u16;
    let fixed = 1 + 1 + plan_rows + 2 + 1 + 1;
    let rows_height =
        (view.rows.len() as u16).clamp(2, area.height.saturating_sub(fixed).max(2));

    let [head, top_rule, plan_area, rows_area, bar_area, timing, _, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(plan_rows),
        Constraint::Length(rows_height),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    let done = view.rows.iter().filter(|r| r.state == SegState::Done).count();
    let what = if view.joins { "clips" } else { "files" };
    header(
        frame,
        head,
        ui,
        &format!("{done}/{} {what} {} {}", view.rows.len(), glyph::DOT, view.label),
    );
    rule(frame, top_rule, ui);

    if plan_rows > 0 {
        let lines: Vec<Line> = view
            .plan
            .iter()
            .rev()
            .take(plan_rows as usize)
            .rev()
            .map(|l| Line::styled(format!("  {l}"), theme.dim()))
            .collect();
        frame.render_widget(Paragraph::new(lines), plan_area);
    }

    draw_segment_rows(frame, rows_area, view, ui);

    // The overall bar, unboxed: a label line and the bar itself.
    let fraction = view.overall();
    let [_, bar_label, bar_line] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(bar_area);
    let stage = match (view.joining, view.joins) {
        (true, _) => "joining",
        (false, true) => "preparing clips",
        (false, false) => "converting",
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(stage.to_uppercase(), theme.label()),
        ])),
        bar_label,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!("{:>3.0}%  ", fraction * 100.0),
            theme.strong(),
        ))
        .alignment(Alignment::Right),
        bar_label,
    );
    let width = bar_line.width.saturating_sub(4) as usize;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(theme::bar(fraction, width), Style::default().fg(theme.accent)),
        ])),
        bar_line,
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled("elapsed ", theme.label()),
            Span::styled(format::short_duration(view.elapsed()), theme.dim()),
            Span::styled("   remaining ", theme.label()),
            Span::styled(
                view.remaining().map(format::short_duration).unwrap_or_else(|| "estimating".into()),
                theme.dim(),
            ),
        ])),
        timing,
    );

    // Stopping stays on the keyboard: a stray click must not be able to throw
    // away work already done, so this is a reminder and not a button.
    let ground = ui.theme.base;
    let stop = if view.joins { "stop the merge" } else { "stop converting" };
    hint_bar(frame, hints, ui, ground, &[&[nav("esc", stop)]]);
}

fn draw_segment_rows(frame: &mut Frame, area: Rect, view: &crate::app::MergeView, ui: &mut UiState) {
    let theme = ui.theme;
    let spinner = ui.spinner();
    fill(frame, area, Style::default().bg(theme.surface));

    let visible = area.height as usize;
    // Follow the clip being worked on, keeping a little of what is coming next.
    let active = view
        .active
        .or_else(|| view.rows.iter().position(|r| r.state == SegState::Queued))
        .unwrap_or(0);
    let start = active
        .saturating_sub(visible / 2)
        .min(view.rows.len().saturating_sub(visible.max(1)));

    let name_width = 24usize;
    let bar_width = (area.width as usize).saturating_sub(name_width + 26).clamp(8, 28);

    let lines: Vec<Line> = view
        .rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, row)| {
            let (mark, mark_style) = match row.state {
                SegState::Queued => (glyph::QUEUED, Style::default().fg(theme.faint)),
                SegState::Running => (spinner, Style::default().fg(theme.accent)),
                SegState::Done => (glyph::DONE, Style::default().fg(theme.good)),
                SegState::Failed => (glyph::FAILED, Style::default().fg(theme.bad)),
            };
            let name_style = match row.state {
                SegState::Queued => theme.label(),
                SegState::Running => theme.strong(),
                _ => theme.dim(),
            };

            let mut spans = vec![
                Span::raw("  "),
                Span::styled(mark, mark_style),
                Span::styled(format!(" {:>3} ", i + 1), theme.label()),
                Span::styled(
                    format!("{:<w$}", format::ellipsize(&row.name, name_width), w = name_width),
                    name_style,
                ),
                Span::raw(" "),
            ];

            match row.state {
                SegState::Queued => spans.push(Span::styled("queued", theme.label())),
                SegState::Running => {
                    let fraction = if row.duration > 0.0 { row.done / row.duration } else { 0.0 };
                    spans.push(Span::styled(
                        format!("{:<11}", if row.step == Step::Copy { "copying" } else { "converting" }),
                        theme.dim(),
                    ));
                    spans.push(Span::styled(
                        theme::bar(fraction, bar_width),
                        Style::default().fg(theme.accent),
                    ));
                    spans.push(Span::styled(
                        format!(" {:>3.0}%", fraction.clamp(0.0, 1.0) * 100.0),
                        theme.strong(),
                    ));
                }
                SegState::Done => {
                    spans.push(Span::styled(format!("{:<11}", row.step.past()), theme.dim()));
                    spans.push(Span::styled(
                        format!("{:>6}", format::short_duration(row.elapsed)),
                        theme.label(),
                    ));
                }
                SegState::Failed => {
                    spans.push(Span::styled("failed", Style::default().fg(theme.bad)));
                }
            }
            Line::from(spans)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), area);

    if view.attempt > 1 {
        let note = format!(" retry {} ", view.attempt);
        let width = note.chars().count() as u16;
        frame.render_widget(
            Paragraph::new(Line::styled(note, Style::default().fg(theme.warn).bg(theme.raised))),
            Rect::new(area.right().saturating_sub(width + 1), area.y, width.min(area.width), 1),
        );
    }
}

// ------------------------------------------------------------------ fetching

/// The download screen.
///
/// Simpler than the merge screen because there is only ever one thing happening,
/// but it has the same job: a wait of minutes with nothing moving on it is
/// indistinguishable from a hang. Every phase says which one it is, including the
/// two that are easy to forget - installing yt-dlp the first time, and the tail
/// where ffmpeg joins the video and audio streams after the last byte lands.
fn draw_fetching(frame: &mut Frame, frame_area: Rect, view: &crate::app::FetchView, ui: &mut UiState) {
    let area = page(frame_area);
    let theme = ui.theme;
    let spinner = ui.spinner();

    let note_rows = view.notes.len().min(3) as u16;
    let [head, top_rule, what, _, notes_area, _, bar_label, bar_line, _, timing, _, hints] =
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(note_rows),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);

    header(frame, head, ui, view.stage.label());
    rule(frame, top_rule, ui);

    // The video's own name once the site has answered, and until then the link,
    // so the screen is never showing something the user cannot recognise.
    let room = (what.width as usize).saturating_sub(12);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(spinner, Style::default().fg(theme.accent)),
            Span::raw("  "),
            Span::styled(format::ellipsize(&view.what(), room), theme.strong()),
        ])),
        what,
    );

    if note_rows > 0 {
        let lines: Vec<Line> = view
            .notes
            .iter()
            .rev()
            .take(note_rows as usize)
            .rev()
            .map(|l| Line::styled(format!("  {l}"), theme.dim()))
            .collect();
        frame.render_widget(Paragraph::new(lines), notes_area);
    }

    // Video and audio usually arrive as two separate files, so the bar restarts
    // part way through. Saying which one is running keeps that from reading as
    // progress being lost.
    let label = match (view.stage, view.stream) {
        (Stage::Setup, _) => "SETTING UP".to_string(),
        (Stage::Asking, _) => "CHECKING THE LINK".to_string(),
        (Stage::Recording, _) => "RECORDING  LIVE".to_string(),
        (Stage::Finishing, _) => "PUTTING IT TOGETHER".to_string(),
        (_, 0 | 1) => "DOWNLOADING".to_string(),
        (_, n) => format!("DOWNLOADING  STREAM {n}"),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::raw("  "), Span::styled(label, theme.label())])),
        bar_label,
    );

    let width = bar_line.width.saturating_sub(4) as usize;
    // A live broadcast is drawn against what has been broadcast so far rather
    // than against a finished length, because there is no finished length -
    // which is why the bar creeps rather than filling, and why it never quite
    // arrives while the sitting is still going. Until the first counts come
    // through there is nothing to draw at all, and a sweep says "working" where
    // a bar would have to invent a position.
    if view.is_recording() && view.fraction().is_none() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    theme::sweep(ui.frame / 2, width),
                    Style::default().fg(theme.accent),
                ),
            ])),
            bar_line,
        );
    } else {
    match view.fraction() {
        Some(fraction) => {
            // "84%" of a download means it is nearly here. Of a broadcast still
            // running it means something else entirely - how much of what has
            // gone out so far is on disk - and the two must not read alike.
            let of_what = if view.is_recording() { " of the broadcast so far" } else { "" };
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!("{:>3.0}%{of_what}  ", fraction * 100.0),
                    theme.strong(),
                ))
                .alignment(Alignment::Right),
                bar_label,
            );
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(theme::bar(fraction, width), Style::default().fg(theme.accent)),
                ])),
                bar_line,
            );
        }
        // Some sites declare no length, and a bar drawn without one would be a
        // guess dressed up as a measurement. The byte count below is the truth.
        None => {
            frame.render_widget(
                Paragraph::new(Line::styled("  no size given by the site", theme.label())),
                bar_line,
            );
        }
    }
    }

    let mut stats = vec![
        Span::raw("  "),
        Span::styled(if view.is_recording() { "captured " } else { "got " }, theme.label()),
    ];
    match view.total {
        Some(total) => stats.push(Span::styled(
            format!("{} / {}", format::size(view.done), format::size(total)),
            theme.dim(),
        )),
        None => stats.push(Span::styled(format::size(view.done), theme.dim())),
    }
    stats.extend([
        Span::styled("   at ", theme.label()),
        Span::styled(format::rate(view.rate), theme.dim()),
        Span::styled("   elapsed ", theme.label()),
        Span::styled(format::short_duration(view.elapsed()), theme.dim()),
    ]);
    if view.is_recording() {
        // "Remaining" would be a fiction here, and the honest answer is the one
        // worth the space: this does not stop until somebody stops it.
        stats.extend([
            Span::styled("   ends ", theme.label()),
            Span::styled("when you stop it", theme.dim()),
        ]);
    } else {
        stats.extend([
            Span::styled("   remaining ", theme.label()),
            Span::styled(
                view.eta.map(format::short_duration).unwrap_or_else(|| "estimating".into()),
                theme.dim(),
            ),
        ]);
    }
    frame.render_widget(Paragraph::new(Line::from(stats)), timing);

    // Stopping stays on the keyboard, the same as the merge screen: a stray
    // click must not be able to throw away a download already half done.
    let ground = ui.theme.base;
    let stop = if view.is_recording() { "finish the recording" } else { "stop the download" };
    hint_bar(frame, hints, ui, ground, &[&[nav("esc", stop)]]);
}

// --------------------------------------------------------------------- result

/// How many of a batch's file names the report lists before it starts counting
/// instead. Enough to recognise what happened, few enough to stay a summary.
const RESULT_NAMES: usize = 8;

fn draw_result(frame: &mut Frame, frame_area: Rect, outcome: &Outcome, ui: &mut UiState) {
    let area = page(frame_area);
    let theme = ui.theme;

    let (word, tone) = if outcome.ok {
        ("DONE", theme.good)
    } else if outcome.cancelled {
        ("STOPPED", theme.warn)
    } else {
        ("FAILED", theme.bad)
    };

    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::raw("  "),
        Span::styled(word, Style::default().fg(tone).add_modifier(Modifier::BOLD)),
    ])];
    lines.push(Line::raw(""));

    let mut field = |label: &str, value: String| {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{label:<8}"), theme.label()),
            Span::styled(value, theme.body()),
        ]));
    };

    if outcome.ok && outcome.outputs.len() > 1 {
        // A batch conversion wrote a file per input, so naming one of them would
        // be picking a favourite. The count, where they are and what they came to
        // is the honest summary, and then the names themselves.
        let folder = outcome.output.parent().unwrap_or(&outcome.output);
        field("FILES", format!("{} written", outcome.outputs.len()));
        field("FOLDER", display_path(folder));
        field("SIZE", format::size(outcome.size));
        field("LENGTH", format::duration(outcome.out_duration));
        field("TOOK", format::duration(outcome.elapsed));
        lines.push(Line::raw(""));
        for path in outcome.outputs.iter().take(RESULT_NAMES) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<8}", ""), theme.label()),
                Span::styled(name, theme.dim()),
            ]));
        }
        if outcome.outputs.len() > RESULT_NAMES {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<8}", ""), theme.label()),
                Span::styled(
                    format!("and {} more", outcome.outputs.len() - RESULT_NAMES),
                    theme.label(),
                ),
            ]));
        }
    } else if outcome.ok {
        field("FILE", display_path(&outcome.output));
        field("SIZE", format::size(outcome.size));
        field("LENGTH", format::duration(outcome.out_duration));
        if let Some((w, h, fps)) = outcome.out_format {
            field("VIDEO", format!("{w}{}{h} @ {} fps", glyph::TIMES, format::fps(fps)));
        }
        field("TOOK", format::duration(outcome.elapsed));
    } else {
        if let Some(error) = &outcome.error {
            field("REASON", error.clone());
        }
        if !outcome.cancelled {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "  Nothing usable was written, so no half-finished file was left behind.",
                theme.label(),
            ));
        }
    }

    if !outcome.warnings.is_empty() {
        lines.push(Line::raw(""));
        for warning in &outcome.warnings {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("!  ", Style::default().fg(theme.warn)),
                Span::styled(warning.clone(), Style::default().fg(theme.warn)),
            ]));
        }
    }

    let panel_height = (lines.len() as u16 + 2).min(area.height.saturating_sub(3));
    // The buttons follow the panel rather than sitting on the last row, so on a
    // tall window they stay where the eye already is.
    let [head, top_rule, panel, _, hints, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(panel_height),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    header(frame, head, ui, if outcome.ok { "finished" } else { "did not finish" });
    rule(frame, top_rule, ui);
    fill(frame, panel, Style::default().bg(theme.surface));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect { y: panel.y + 1, height: panel.height.saturating_sub(1), ..panel },
    );

    let ground = theme.base;
    hint_bar(
        frame,
        hints,
        ui,
        ground,
        &[&[
            primary("enter", "BACK TO THE LIST", Click::Back),
            hint("p", "show the file", Some(Click::Command('p'))),
            hint("x", "exit", Some(Click::Command('x'))),
        ]],
    );
}

// -------------------------------------------------------------------- overlays

/// A box of the given size, centred.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let [row] = Layout::vertical([Constraint::Length(height)]).flex(Flex::Center).areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center).areas(row);
    cell
}

/// A popup: a raised surface with a title row and a rule, and no border.
fn panel(frame: &mut Frame, area: Rect, ui: &UiState, title: &str, accent: ratatui::style::Color) -> Rect {
    let theme = &ui.theme;
    frame.render_widget(Clear, area);
    fill(frame, area, Style::default().bg(theme.raised));

    if area.height == 0 {
        return area;
    }
    let [title_row, rule_row, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                title.to_uppercase(),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
        ])),
        title_row,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(rule_row.width as usize),
            Style::default().fg(theme.line),
        )),
        rule_row,
    );
    body
}

fn draw_prompt(frame: &mut Frame, area: Rect, prompt: &Prompt, ui: &mut UiState) {
    let theme = ui.theme;
    let box_area = centred(area, 76, 9);
    let body = panel(frame, box_area, ui, &prompt.title, theme.accent);
    let [text_area, _, buttons, _] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(body);

    // A dropped path is longer than the box, so the tail is what shows - the
    // end of a path is the part that identifies it.
    let room = body.width.saturating_sub(6) as usize;
    let count = prompt.buffer.chars().count();
    let shown: String = if count > room {
        prompt.buffer.chars().skip(count - room).collect()
    } else {
        prompt.buffer.clone()
    };

    let lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("› ", theme.accented()),
            Span::styled(shown, theme.strong()),
            Span::styled("▏", theme.accented()),
        ]),
        Line::raw(""),
        Line::from(vec![Span::raw("  "), Span::styled(prompt.hint.clone(), theme.label())]),
    ];
    frame.render_widget(Paragraph::new(lines), text_area);

    // Something to click. A path arrives here by being dropped, which is a
    // mouse gesture, so finishing the job should not have to be a keystroke.
    let verb = match prompt.kind {
        PromptKind::AddPaths => "ADD THESE",
        PromptKind::OutputName => "USE THIS NAME",
        PromptKind::CustomTarget => "USE THIS SIZE",
        PromptKind::FetchUrl => "GET THIS ONE",
    };
    let typed = !prompt.buffer.trim().is_empty();
    hint_bar(
        frame,
        buttons,
        ui,
        theme.raised,
        &[&[
            only_if(typed, primary("enter", verb, Click::Submit)),
            hint("esc", "cancel", Some(Click::Cancel)),
        ]],
    );
}

fn draw_menu(frame: &mut Frame, area: Rect, menu: &Menu, ui: &mut UiState) {
    const WIDTH: u16 = 72;
    const NOTE_ROWS: u16 = 2;
    let theme = ui.theme;

    let height = NOTE_ROWS + menu.items.len() as u16 + 6;
    let box_area = centred(area, WIDTH, height);
    // A picker is dismissed by clicking off it, the way a menu anywhere else is.
    // Both regions go down before the items, which therefore win over them.
    ui.add_hit(area, Click::Cancel);
    ui.add_hit(box_area, Click::Ignore);
    let body = panel(frame, box_area, ui, &menu.title, theme.accent);

    let [note_area, _, items_area, _, hint_area] = Layout::vertical([
        Constraint::Length(NOTE_ROWS),
        Constraint::Length(1),
        Constraint::Length(menu.items.len() as u16),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(body);

    frame.render_widget(
        Paragraph::new(Line::styled(menu.note.clone(), theme.label())).wrap(Wrap { trim: false }),
        indent(note_area, 2),
    );

    let mut lines = Vec::with_capacity(menu.items.len());
    for (i, (label, extra)) in menu.items.iter().enumerate() {
        let chosen = i == menu.cursor;
        let row = Rect::new(items_area.x, items_area.y + i as u16, items_area.width, 1);
        // Hover, not selection: the row lights up to say a click will land here,
        // while the cursor bar still shows what enter would pick.
        let hot = !chosen && ui.hovered(row);
        let (edge, edge_style) = if chosen {
            (glyph::CURSOR, Style::default().fg(theme.accent).bg(theme.accent_wash))
        } else {
            (" ", if hot { theme.hovered(theme.raised) } else { Style::default() })
        };
        let label_style = match (chosen, hot) {
            (true, _) => theme.selected(),
            (false, true) => theme.hovered(theme.raised),
            (false, false) => theme.body(),
        };
        let index_style = match (chosen, hot) {
            (true, _) => theme.selected(),
            (false, true) => theme.hovered(theme.raised),
            (false, false) => theme.label(),
        };
        let extra_style = if hot { theme.hovered(theme.raised) } else { theme.label() };
        let mut spans = vec![
            Span::styled(" ", edge_style),
            Span::styled(edge, edge_style),
            Span::styled(format!(" {} ", i + 1), index_style),
            Span::styled(format!("{label} "), label_style),
        ];
        if !extra.is_empty() {
            spans.push(Span::styled(format!(" {extra}"), extra_style));
        }
        lines.push(Line::from(spans));

        ui.add_hit(row, Click::MenuItem(i));
    }
    frame.render_widget(Paragraph::new(lines), items_area);

    // Only nine of them can have a key, so a longer list says so rather than
    // promising a number for the tenth.
    let how = if menu.items.len() > 9 {
        "click a line, or press 1-9   "
    } else {
        "click a line, or press its number   "
    };
    frame.render_widget(
        Paragraph::new(Line::styled(how, theme.label())).alignment(Alignment::Right),
        hint_area,
    );
    hint_bar(
        frame,
        hint_area,
        ui,
        theme.raised,
        &[&[hint("esc", "cancel", Some(Click::Cancel))]],
    );
}

fn indent(area: Rect, by: u16) -> Rect {
    Rect { x: area.x + by, width: area.width.saturating_sub(by), ..area }
}

fn draw_confirm(frame: &mut Frame, area: Rect, confirm: &Confirm, ui: &mut UiState) {
    let theme = ui.theme;
    // One button per outcome, and no button for an outcome another one already
    // covers: in the cancel dialog Esc and "keep going" are the same answer, so
    // only the answer is drawn.
    let (title, body_lines, buttons) = match confirm {
        Confirm::Overwrite(path) => (
            "That file already exists",
            vec![
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(display_path(path), theme.body()),
                ]),
                Line::raw(""),
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled("Overwrite it, or write alongside it as merged_2.mp4?", theme.dim()),
                ]),
            ],
            vec![
                // The answer that cannot be undone is the filled one, because
                // enter picks it: a default has to look like the default.
                danger("y", "overwrite it", Click::Answer(true)),
                hint("n", "write alongside it", Some(Click::Answer(false))),
                hint("esc", "do not merge", Some(Click::Cancel)),
            ],
        ),
        Confirm::CancelMerge => (
            "Stop the merge?",
            vec![Line::from(vec![
                Span::raw("  "),
                Span::styled("ffmpeg is stopped and the part-finished file is removed.", theme.dim()),
            ])],
            vec![
                danger("y", "stop it", Click::Answer(true)),
                hint("n", "keep going", Some(Click::Answer(false))),
            ],
        ),
        Confirm::CancelConvert => (
            "Stop converting?",
            vec![
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        "The files already converted are kept - they are finished.",
                        theme.dim(),
                    ),
                ]),
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        "The one in progress is removed, and the rest are not started.",
                        theme.dim(),
                    ),
                ]),
            ],
            vec![
                danger("y", "stop it", Click::Answer(true)),
                hint("n", "keep going", Some(Click::Answer(false))),
            ],
        ),
        Confirm::StopRecording => (
            "Finish the recording?",
            vec![
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        "Everything captured so far is kept and written out as a video.",
                        theme.dim(),
                    ),
                ]),
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        "The rest of the broadcast is not recorded.",
                        theme.dim(),
                    ),
                ]),
            ],
            vec![
                hint("y", "finish it", Some(Click::Answer(true))),
                hint("n", "keep recording", Some(Click::Answer(false))),
            ],
        ),
        Confirm::CancelFetch => (
            "Stop the download?",
            vec![Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "Whatever has arrived so far is thrown away, so nothing half-finished is left.",
                    theme.dim(),
                ),
            ])],
            vec![
                danger("y", "stop it", Click::Answer(true)),
                hint("n", "keep going", Some(Click::Answer(false))),
            ],
        ),
    };

    let box_area = centred(area, 74, body_lines.len() as u16 + 7);
    let body = panel(frame, box_area, ui, title, theme.warn);

    // A blank row between the question and the answers. Without it the buttons
    // read as another line of the question.
    let [text_area, button_row, _] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
            .areas(body);
    let mut lines = vec![Line::raw("")];
    lines.extend(body_lines);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text_area);

    hint_bar(frame, button_row, ui, theme.raised, &[&buttons]);
}

/// Everything the keyboard does, on demand. The hint bar carries the handful
/// that get used constantly; this carries the rest.
/// Two columns, because one is taller than most terminals.
fn draw_help(frame: &mut Frame, area: Rect, help: &HelpSheet, ui: &mut UiState) {
    let theme = ui.theme;

    // Split the groups so the two columns come out as even as they can.
    let total: usize = help.groups.iter().map(|g| g.keys.len() + 2).sum();
    let mut split = help.groups.len();
    let mut running = 0;
    for (i, group) in help.groups.iter().enumerate() {
        running += group.keys.len() + 2;
        if running * 2 >= total {
            split = i + 1;
            break;
        }
    }

    let render_column = |groups: &[crate::app::HelpGroup]| -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for group in groups {
            lines.push(Line::styled(group.title.to_uppercase(), theme.label()));
            for (keys, what) in &group.keys {
                // The key column is wide enough for the longest chord, with a
                // gap: without it "home end  g G" ran into its description.
                lines.push(Line::from(vec![
                    Span::styled(format!("{keys:<15}"), theme.key()),
                    Span::styled(what.clone(), theme.dim()),
                ]));
            }
            lines.push(Line::raw(""));
        }
        lines
    };

    let left = render_column(&help.groups[..split]);
    let right = render_column(&help.groups[split..]);
    let rows = left.len().max(right.len()) as u16;

    let box_area = centred(area, 100, rows + 5);
    // A reference sheet goes away on any key, so it should go away on any click
    // too - including a click on the sheet itself, which has nothing to press.
    ui.add_hit(area, Click::Cancel);
    let body = panel(frame, box_area, ui, "Keys", theme.accent);

    let [columns_area, footer] =
        Layout::vertical([Constraint::Length(rows), Constraint::Min(0)]).areas(body);
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Percentage(52), Constraint::Percentage(48)])
            .areas(indent(columns_area, 2));

    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(Paragraph::new(right), right_area);
    hint_bar(
        frame,
        footer,
        ui,
        theme.raised,
        &[&[hint("esc", "close", Some(Click::Cancel)), nav("?", "does the same")]],
    );
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;
