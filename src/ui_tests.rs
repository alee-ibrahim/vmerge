//! Render tests for every screen and overlay.
//!
//! These draw into a TestBackend and read the resulting characters back, which
//! catches the two things that are otherwise only found by running the program:
//! a layout that panics at an awkward terminal size, and a value that silently
//! stops appearing on screen.

use super::*;
use crate::app::{AppEvent, Entry, MergeView};
use crate::ffmpeg::Tools;
use crate::merge::{MergeEvent, Outcome};
use crate::probe::ClipInfo;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

fn clip(name: &str, w: u32, h: u32, fps: f64, secs: f64) -> ClipInfo {
    ClipInfo {
        path: PathBuf::from(format!("C:/clips/{name}")),
        name: name.into(),
        video_codec: "h264".into(),
        width: w,
        height: h,
        pix_fmt: "yuv420p".into(),
        fps,
        frame_rate_raw: "30/1".into(),
        rotation: 0,
        has_audio: true,
        audio_codec: "aac".into(),
        sample_rate: 48_000,
        channels: 2,
        duration: secs,
        size_bytes: 1024 * 1024,
    }
}

/// The receiver has to be held: dropping it makes every send fail.
fn app_with(clips: Vec<ClipInfo>) -> (App, Receiver<AppEvent>) {
    let (tx, rx) = mpsc::channel();
    let tools = Arc::new(Tools {
        ffmpeg: PathBuf::from("ffmpeg.exe"),
        ffprobe: PathBuf::from("ffprobe.exe"),
    });
    let mut app = App::new(tools, PathBuf::from("C:/clips"), tx);
    app.clips = clips.into_iter().map(|c| Entry { clip: c, marked: false }).collect();
    (app, rx)
}

/// Renders and reads the screen back as plain text.
fn render(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut ui = UiState::default();
    terminal.draw(|frame| draw(frame, app, &mut ui)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn empty_list_asks_for_files() {
    let (app, _rx) = app_with(vec![]);
    let screen = render(&app, 100, 30);
    assert!(screen.contains("DROP YOUR CLIPS HERE"), "got:\n{screen}");
    assert!(screen.contains("START MERGE"));
}

#[test]
fn clip_table_shows_each_clip_and_the_plan() {
    let (app, _rx) = app_with(vec![
        clip("intro.mp4", 1920, 1080, 30.0, 12.0),
        clip("beach clip 2.mov", 3840, 2160, 60.0, 64.0),
    ]);
    let screen = render(&app, 100, 30);
    assert!(screen.contains("intro.mp4"));
    assert!(screen.contains("beach clip 2.mov"));
    assert!(screen.contains("1920×1080"), "dimensions use a multiplication sign");
    // Mixed sizes, so the plan has to name the conversion target. The 4K clip
    // carries most of the footage here, so it wins.
    assert!(screen.contains("Convert to 3840×2160"), "got:\n{screen}");
    assert!(screen.contains("need converting"));
}

#[test]
fn identical_clips_advertise_the_fast_path() {
    let (app, _rx) = app_with(vec![
        clip("a.mp4", 1920, 1080, 30.0, 5.0),
        clip("b.mp4", 1920, 1080, 30.0, 5.0),
    ]);
    assert!(render(&app, 100, 30).contains("Fast join"));
}

#[test]
fn marked_clips_are_counted_in_the_title() {
    let (mut app, _rx) = app_with(vec![clip("a.mp4", 1920, 1080, 30.0, 5.0)]);
    app.toggle_mark();
    assert!(render(&app, 100, 30).contains("1 marked"));
}

#[test]
fn the_output_path_is_shown_before_starting() {
    let (app, _rx) = app_with(vec![clip("a.mp4", 1920, 1080, 30.0, 5.0)]);
    let screen = render(&app, 100, 30);
    assert!(screen.contains("merged.mp4"), "got:\n{screen}");
    assert!(screen.contains("high"), "the quality setting must be visible");
}

#[test]
fn every_overlay_renders() {
    let (mut app, _rx) = app_with(vec![clip("a.mp4", 1920, 1080, 30.0, 5.0)]);

    app.prompt_add_with("C:/clips/dropped file.mp4".into());
    assert!(render(&app, 100, 30).contains("dropped file.mp4"));

    app.prompt_output();
    assert!(render(&app, 100, 30).contains("OUTPUT FILE NAME"));

    app.menu_quality();
    let screen = render(&app, 100, 30);
    assert!(screen.contains("visually-lossless") && screen.contains("small"), "got:\n{screen}");

    app.menu_target();
    let screen = render(&app, 100, 30);
    assert!(screen.contains("Auto") && screen.contains("1280×720"), "got:\n{screen}");

    app.overlay = Overlay::Confirm(Confirm::Overwrite(PathBuf::from("C:/clips/merged.mp4")));
    assert!(render(&app, 100, 30).contains("ALREADY EXISTS"));

    app.overlay = Overlay::Confirm(Confirm::CancelMerge);
    assert!(render(&app, 100, 30).contains("STOP THE MERGE?"));
}

#[test]
fn merge_screen_shows_progress_per_clip() {
    let (mut app, _rx) = app_with(vec![
        clip("one.mp4", 1920, 1080, 30.0, 10.0),
        clip("two.mp4", 1280, 720, 25.0, 10.0),
    ]);
    app.screen =
        Screen::Merging(MergeView::new(PathBuf::from("C:/clips/merged.mp4"), &app.clips));

    app.handle_event(AppEvent::Merge(MergeEvent::Plan("Common format: 1920x1080".into())));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentStart {
        index: 0,
        name: "one.mp4".into(),
        step: Step::Copy,
        duration: 10.0,
    }));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentEnd {
        index: 0,
        step: Step::Copy,
        ok: true,
        elapsed: 0.4,
    }));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentStart {
        index: 1,
        name: "two.mp4".into(),
        step: Step::Convert,
        duration: 10.0,
    }));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentProgress { index: 1, done: 6.4 }));

    let screen = render(&app, 100, 30);
    assert!(screen.contains("copied"), "got:\n{screen}");
    assert!(screen.contains("converting"), "got:\n{screen}");
    assert!(screen.contains("64%"), "expected the per-clip bar:\n{screen}");
    assert!(screen.contains('█'), "expected a filled bar:\n{screen}");
    // One clip of two done plus 64% of the second, over a prepare phase worth
    // 85% of the bar: (1.0 + 0.64) / 2 * 0.85 = 70%.
    assert!(screen.contains("70%"), "expected overall progress:\n{screen}");
}

#[test]
fn a_failed_clip_is_visible_on_the_merge_screen() {
    let (mut app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 10.0)]);
    app.screen =
        Screen::Merging(MergeView::new(PathBuf::from("C:/clips/merged.mp4"), &app.clips));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentEnd {
        index: 0,
        step: Step::Convert,
        ok: false,
        elapsed: 1.0,
    }));
    assert!(render(&app, 100, 30).contains("failed"));
}

fn outcome(ok: bool) -> Outcome {
    Outcome {
        ok,
        output: PathBuf::from("C:/clips/merged.mp4"),
        size: 5 * 1024 * 1024,
        out_duration: if ok { 65.0 } else { 0.0 },
        out_format: if ok { Some((1920, 1080, 29.97)) } else { None },
        elapsed: 12.0,
        warnings: Vec::new(),
        error: if ok { None } else { Some("ffmpeg said no".into()) },
        cancelled: false,
    }
}

#[test]
fn result_screen_reports_a_finished_merge() {
    let (mut app, _rx) = app_with(vec![clip("a.mp4", 1920, 1080, 30.0, 5.0)]);
    let mut done = outcome(true);
    done.warnings.push("Inputs add up to more than the output.".into());
    app.screen = Screen::Result(Box::new(done));

    let screen = render(&app, 100, 30);
    assert!(screen.contains("DONE"));
    assert!(screen.contains("merged.mp4"));
    assert!(screen.contains("00:01:05"));
    assert!(screen.contains("29.97"), "an NTSC rate must survive display:\n{screen}");
    assert!(screen.contains("Inputs add up"));
    assert!(screen.contains("5.0 MB"));
}

#[test]
fn result_screen_reports_a_failure_and_a_cancellation() {
    let (mut app, _rx) = app_with(vec![clip("a.mp4", 1920, 1080, 30.0, 5.0)]);

    app.screen = Screen::Result(Box::new(outcome(false)));
    let screen = render(&app, 100, 30);
    assert!(screen.contains("FAILED"));
    assert!(screen.contains("ffmpeg said no"));

    let mut stopped = outcome(false);
    stopped.cancelled = true;
    app.screen = Screen::Result(Box::new(stopped));
    assert!(render(&app, 100, 30).contains("STOPPED"));
}

/// A window dragged down to nothing must not take the program with it.
#[test]
fn awkward_terminal_sizes_do_not_panic() {
    let (mut app, _rx) = app_with(vec![
        clip("a-really-long-clip-name-that-overflows-every-column.mp4", 1920, 1080, 30.0, 5.0),
        clip("b.mp4", 640, 480, 25.0, 5.0),
    ]);

    for (w, h) in [(1, 1), (2, 2), (4, 3), (20, 6), (40, 10), (80, 24), (300, 80)] {
        render(&app, w, h);

        app.menu_target();
        render(&app, w, h);
        app.close_overlay();

        app.prompt_add_with("C:/some/very/long/path/to/a/dropped/clip.mp4".into());
        render(&app, w, h);
        app.close_overlay();

        app.screen =
            Screen::Merging(MergeView::new(PathBuf::from("C:/clips/merged.mp4"), &app.clips));
        render(&app, w, h);

        app.screen = Screen::Result(Box::new(outcome(true)));
        render(&app, w, h);
        app.screen = Screen::Browse;
    }
}

/// Renders, then reports what a click at that cell would do.
///
/// This is the check that matters for the mouse: the hit regions are computed
/// during drawing from width arithmetic that has to agree with what the spans
/// actually occupy. If the two drift apart, buttons stop matching their labels.
fn click_at(app: &App, width: u16, height: u16, column: u16, row: u16) -> Option<Click> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut ui = UiState::default();
    terminal.draw(|frame| draw(frame, app, &mut ui)).unwrap();
    ui.hit(column, row)
}

/// The terminal column where `needle` starts.
///
/// Not `str::find`, which counts bytes: the arrow glyphs in the key bar are
/// three bytes each, so a byte offset lands ten columns to the right of the text
/// it was meant to point at.
fn column_of(line: &str, needle: &str) -> u16 {
    let byte = line
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is not in {line:?}"));
    line[..byte].chars().count() as u16
}

/// The middle of `needle` on its row, which is where a person would click.
fn centre_of(app: &App, width: u16, height: u16, needle: &str) -> (u16, u16) {
    let row = row_of(app, width, height, needle);
    let screen = render(app, width, height);
    let line = screen.lines().nth(row as usize).unwrap().to_string();
    let column = column_of(&line, needle) + needle.chars().count() as u16 / 2;
    (column, row)
}

/// Finds the screen row containing `needle`, so the tests do not hard-code
/// layout arithmetic they are meant to be checking.
fn row_of(app: &App, width: u16, height: u16, needle: &str) -> u16 {
    let screen = render(app, width, height);
    screen
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{screen}")) as u16
}

#[test]
fn clicking_a_clip_selects_it() {
    let (app, _rx) = app_with(vec![
        clip("one.mp4", 1920, 1080, 30.0, 5.0),
        clip("two.mp4", 1920, 1080, 30.0, 5.0),
        clip("three.mp4", 1920, 1080, 30.0, 5.0),
    ]);

    for (name, expected) in [("one.mp4", 0), ("two.mp4", 1), ("three.mp4", 2)] {
        let row = row_of(&app, 100, 30, name);
        assert_eq!(
            click_at(&app, 100, 30, 20, row),
            Some(Click::Row(expected)),
            "clicking the {name} row"
        );
    }
}

#[test]
fn the_bottom_bar_buttons_match_their_labels() {
    let (app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 5.0)]);

    // Walk both key rows and check every labelled button reports itself.
    for (label, expected) in [
        ("START MERGE", Click::Command('s')),
        ("output", Click::Command('o')),
        ("quality", Click::Command('q')),
        ("target", Click::Command('t')),
        ("encoder", Click::Command('e')),
        ("exit", Click::Command('x')),
        ("mark", Click::Mark),
        ("remove", Click::Remove),
        ("add", Click::Command('a')),
        ("clear", Click::Command('c')),
    ] {
        let (column, row) = centre_of(&app, 100, 30, label);
        assert_eq!(
            click_at(&app, 100, 30, column, row),
            Some(expected),
            "clicking {label:?} at column {column}, row {row}"
        );
    }
}

/// The whole button is the target, caps included.
///
/// `chip_width` and the spans it places have to agree exactly. They cannot be
/// checked by clicking labels alone: one cell of disagreement moves every hit
/// region after the first, so the drift only shows at the ends of a row.
#[test]
fn a_button_is_clickable_right_up_to_its_edges() {
    let (app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 5.0)]);

    // The last chip on its row, so it has accumulated any drift from the ones
    // before it.
    let chip = "▐ x exit ▌";
    let row = row_of(&app, 100, 30, chip);
    let screen = render(&app, 100, 30);
    let left = column_of(screen.lines().nth(row as usize).unwrap(), chip);
    let right = left + chip.chars().count() as u16 - 1;

    for column in [left, left + 1, right - 1, right] {
        assert_eq!(
            click_at(&app, 100, 30, column, row),
            Some(Click::Command('x')),
            "column {column} is part of the button drawn from {left} to {right}"
        );
    }
    assert_eq!(
        click_at(&app, 100, 30, left - 1, row),
        None,
        "the gap between two buttons belongs to neither"
    );
    assert_eq!(click_at(&app, 100, 30, right + 1, row), None, "nor does the gap after one");
}

/// Hover is the only feedback a terminal can give before the click lands.
#[test]
fn a_button_lights_up_under_the_pointer() {
    let (app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 5.0)]);
    let (column, row) = centre_of(&app, 100, 30, "output");

    let face = |pointer: Option<(u16, u16)>| {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut ui = UiState::default();
        if let Some((c, r)) = pointer {
            ui.set_pointer(c, r);
        }
        terminal.draw(|frame| draw(frame, &app, &mut ui)).unwrap();
        terminal.backend().buffer()[(column, row)].bg
    };

    assert_ne!(
        face(Some((column, row))),
        face(None),
        "the button under the pointer has to look different from the same button cold"
    );
    assert_eq!(
        face(Some((column, row + 1))),
        face(None),
        "and a pointer on another row must leave it alone"
    );
}

/// Rows are clickable, so they light up too: hover has to mean one thing
/// everywhere, or it teaches nothing.
#[test]
fn a_clip_row_lights_up_under_the_pointer() {
    let (app, _rx) = app_with(vec![
        clip("one.mp4", 1920, 1080, 30.0, 5.0),
        clip("two.mp4", 1920, 1080, 30.0, 5.0),
    ]);
    // Not the row under the cursor: that one carries its own wash already.
    let row = row_of(&app, 100, 30, "two.mp4");

    let face = |pointer: Option<(u16, u16)>| {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut ui = UiState::default();
        if let Some((c, r)) = pointer {
            ui.set_pointer(c, r);
        }
        terminal.draw(|frame| draw(frame, &app, &mut ui)).unwrap();
        terminal.backend().buffer()[(30, row)].bg
    };

    assert_ne!(face(Some((30, row))), face(None), "the row under the pointer");
    assert_eq!(face(Some((30, row + 1))), face(None), "any other row");
}

/// Buttons with nothing to act on are drawn dead rather than answering a click
/// with a complaint.
#[test]
fn buttons_with_nothing_to_act_on_cannot_be_pressed() {
    let (empty, _rx) = app_with(vec![]);
    let screen = render(&empty, 100, 30);

    for label in ["START MERGE", "mark", "remove", "clear"] {
        assert!(screen.contains(label), "{label:?} still has to be listed:\n{screen}");
        let (column, row) = centre_of(&empty, 100, 30, label);
        assert_eq!(
            click_at(&empty, 100, 30, column, row),
            None,
            "{label:?} has no clips to work on, so clicking it must do nothing"
        );
    }
    // Adding some is the exception, and the drop zone is a button for it too.
    let (column, row) = centre_of(&empty, 100, 30, "ADD CLIPS");
    assert_eq!(click_at(&empty, 100, 30, column, row), Some(Click::Command('a')));

    // With clips loaded, the same buttons come back to life.
    let (loaded, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 5.0)]);
    let (column, row) = centre_of(&loaded, 100, 30, "START MERGE");
    assert_eq!(click_at(&loaded, 100, 30, column, row), Some(Click::Command('s')));
}

/// Buttons need air above them, or they read as one more line of the panel they
/// are sitting under.
#[test]
fn the_button_bar_is_not_pressed_against_the_text_above_it() {
    let (app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 5.0)]);
    let screen = render(&app, 100, 30);
    let lines: Vec<&str> = screen.lines().collect();
    let buttons = row_of(&app, 100, 30, "START MERGE") as usize;

    assert!(buttons > 0);
    assert!(
        lines[buttons - 1].trim().is_empty(),
        "the row above the buttons has to be blank, got {:?}",
        lines[buttons - 1]
    );
}

#[test]
fn menu_items_are_clickable_where_they_are_drawn() {
    let (mut app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 5.0)]);
    app.menu_quality();

    // The numbered prefix is the unique part: "medium" is followed by the note
    // "noticeably smaller", which contains "small".
    for (index, needle) in ["1 visually-lossless", "2 high", "3 medium", "4 small"]
        .iter()
        .enumerate()
    {
        let (column, row) = centre_of(&app, 100, 30, needle);
        assert_eq!(
            click_at(&app, 100, 30, column, row),
            Some(Click::MenuItem(index)),
            "clicking {needle:?}"
        );
    }
}

#[test]
fn an_open_dialog_swallows_clicks_meant_for_what_is_behind_it() {
    let (mut app, _rx) = app_with(vec![
        clip("one.mp4", 1920, 1080, 30.0, 5.0),
        clip("two.mp4", 1920, 1080, 30.0, 5.0),
    ]);

    // With no dialog, that cell selects a clip.
    let row = row_of(&app, 100, 30, "one.mp4");
    assert_eq!(click_at(&app, 100, 30, 20, row), Some(Click::Row(0)));

    // With one open, the clip underneath must not be reachable.
    app.prompt_output();
    assert_eq!(
        click_at(&app, 100, 30, 20, row),
        None,
        "a click behind a dialog has to do nothing"
    );
}

#[test]
fn the_confirm_dialog_has_a_button_per_outcome() {
    let (mut app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 5.0)]);
    app.overlay = Overlay::Confirm(Confirm::Overwrite(PathBuf::from("C:/clips/merged.mp4")));

    // Three outcomes here, all of them different: overwrite, write a second
    // file, or merge nothing at all.
    let (yes, row) = centre_of(&app, 100, 30, "overwrite it");
    let (no, _) = centre_of(&app, 100, 30, "write alongside it");
    let (out, _) = centre_of(&app, 100, 30, "do not merge");
    assert_eq!(click_at(&app, 100, 30, yes, row), Some(Click::Answer(true)));
    assert_eq!(click_at(&app, 100, 30, no, row), Some(Click::Answer(false)));
    assert_eq!(click_at(&app, 100, 30, out, row), Some(Click::Cancel));

    // Stopping a merge has two, and Esc means the same as one of them, so it
    // does not get a button of its own.
    app.overlay = Overlay::Confirm(Confirm::CancelMerge);
    let (stop, row) = centre_of(&app, 100, 30, "stop it");
    let (keep, _) = centre_of(&app, 100, 30, "keep going");
    assert_eq!(click_at(&app, 100, 30, stop, row), Some(Click::Answer(true)));
    assert_eq!(click_at(&app, 100, 30, keep, row), Some(Click::Answer(false)));
}

/// A path arrives in the add prompt by being dropped, which is a mouse gesture:
/// finishing the job should not have to be a keystroke.
#[test]
fn a_prompt_can_be_accepted_and_cancelled_with_the_mouse() {
    let (mut app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 5.0)]);
    app.prompt_add_with("C:/clips/dropped.mp4".into());

    let (accept, row) = centre_of(&app, 100, 30, "ADD THESE");
    assert_eq!(click_at(&app, 100, 30, accept, row), Some(Click::Submit));
    let (cancel, row) = centre_of(&app, 100, 30, "cancel");
    assert_eq!(click_at(&app, 100, 30, cancel, row), Some(Click::Cancel));

    // With nothing typed there is nothing to accept, so the button is inert.
    app.prompt_add();
    let (accept, row) = centre_of(&app, 100, 30, "ADD THESE");
    assert_eq!(click_at(&app, 100, 30, accept, row), None);
}

/// Clicking off a picker dismisses it, and clicking a bare part of it does not.
#[test]
fn a_picker_closes_when_clicked_off() {
    let (mut app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 5.0)]);
    app.menu_quality();

    assert_eq!(click_at(&app, 100, 30, 1, 1), Some(Click::Cancel), "the page behind it");

    // The note at the top of the panel is part of the dialog, not off it.
    let (column, row) = centre_of(&app, 100, 30, "Only matters for clips");
    assert_eq!(click_at(&app, 100, 30, column, row), Some(Click::Ignore));

    let (column, row) = centre_of(&app, 100, 30, "esc cancel");
    assert_eq!(click_at(&app, 100, 30, column, row), Some(Click::Cancel));
}

#[test]
fn the_mouse_button_advertises_what_it_will_do() {
    let (mut app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 5.0)]);
    assert!(app.mouse, "the mouse starts captured");
    assert!(render(&app, 100, 30).contains("mouse off"), "offers to turn it off");

    app.mouse = false;
    assert!(render(&app, 100, 30).contains("mouse on"), "offers to turn it back on");
}

/// Strip every colour and the screen must still say the same things.
///
/// This is the check the previous design would have failed: it leaned on colour
/// to tell the selected row, the marked rows and the primary action apart. Here
/// the cursor is a solid bar, a mark is a bullet, and the shapes survive.
#[test]
fn hierarchy_survives_monochrome() {
    let (mut app, _rx) = app_with(vec![
        clip("one.mp4", 1920, 1080, 30.0, 5.0),
        clip("two.mp4", 1280, 720, 25.0, 5.0),
        clip("three.mp4", 1920, 1080, 30.0, 5.0),
    ]);
    app.cursor = 1;
    app.clips[2].marked = true;

    // Render with the plain palette, then throw the palette away entirely: only
    // the characters are left.
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut ui = UiState {
        theme: crate::theme::Theme::new(crate::theme::Tier::Ansi),
        ..Default::default()
    };
    terminal.draw(|frame| draw(frame, &app, &mut ui)).unwrap();

    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect();
    let screen = rows.join("\n");

    // The cursor is a shape, not a colour.
    let cursor_row = rows.iter().find(|r| r.contains("two.mp4")).expect("the clip is listed");
    assert!(
        cursor_row.contains(crate::theme::glyph::CURSOR),
        "the selected row needs a visible marker without colour:\n{screen}"
    );
    let other_row = rows.iter().find(|r| r.contains("one.mp4")).unwrap();
    assert!(
        !other_row.contains(crate::theme::glyph::CURSOR),
        "an unselected row must not carry the cursor marker"
    );

    // So is a mark, and it is a different shape from the cursor.
    let marked_row = rows.iter().find(|r| r.contains("three.mp4")).unwrap();
    assert!(
        marked_row.contains(crate::theme::glyph::MARK),
        "a marked row needs its own marker without colour:\n{screen}"
    );
    assert_ne!(
        crate::theme::glyph::MARK,
        crate::theme::glyph::CURSOR,
        "focus and marking must not be the same shape"
    );

    // The primary action still reads as one thing, spaced apart from the rest.
    assert!(screen.contains(" S START MERGE "), "got:\n{screen}");

    // And the data is all still there.
    for expected in ["one.mp4", "two.mp4", "three.mp4", "1920×1080", "OUTPUT", "PLAN"] {
        assert!(screen.contains(expected), "{expected:?} vanished without colour:\n{screen}");
    }
}

/// A fallback pass has to start the rows over. When the fast join fails after
/// remuxing every clip, the retry used to inherit those finished rows and open
/// at nearly 100% before doing any of the work.
#[test]
fn a_retry_pass_starts_the_rows_over() {
    let (mut app, _rx) = app_with(vec![
        clip("one.mp4", 1920, 1080, 30.0, 10.0),
        clip("two.mp4", 1920, 1080, 30.0, 10.0),
    ]);
    app.screen =
        Screen::Merging(MergeView::new(PathBuf::from("C:/clips/merged.mp4"), &app.clips));

    // Pass one gets through both clips, then the join fails.
    for index in 0..2 {
        app.handle_event(AppEvent::Merge(MergeEvent::SegmentEnd {
            index,
            step: Step::Copy,
            ok: true,
            elapsed: 0.3,
        }));
    }
    app.handle_event(AppEvent::Merge(MergeEvent::JoinStart));
    let Screen::Merging(view) = &app.screen else { panic!("still merging") };
    assert!(view.overall() > 0.8, "pass one really did finish its work");

    // The fallback pass reports itself, and the screen resets.
    app.handle_event(AppEvent::Merge(MergeEvent::Pass { total: 2, attempt: 2 }));
    let Screen::Merging(view) = &app.screen else { panic!("still merging") };
    assert_eq!(view.overall(), 0.0, "a retry starts from nothing");
    assert!(!view.joining);

    let screen = render(&app, 100, 30);
    assert!(screen.contains("retry 2"), "the retry has to be visible:\n{screen}");
    assert!(screen.contains("queued"));
}

/// A design aid, not a check: prints the screens so the layout can be looked
/// at without launching the program.
///     cargo test dump_screens -- --ignored --nocapture
#[test]
#[ignore]
fn dump_screens() {
    let (mut app, _rx) = app_with(vec![
        clip("intro.mp4", 1920, 1080, 30.0, 12.0),
        clip("beach clip 2.mov", 3840, 2160, 60.0, 64.0),
        clip("drone_10.mp4", 1920, 1080, 30.0, 151.0),
        clip("outro.mp4", 1280, 720, 25.0, 8.0),
    ]);
    app.clips[3].clip.has_audio = false;
    app.clips[3].clip.audio_codec = "none".into();
    app.clips[2].clip.fps = 29.97;
    app.cursor = 1;
    app.clips[3].marked = true;
    app.say("Added 4 clips.", Kind::Good);

    println!("
=== the list ===
{}", render(&app, 96, 26));

    app.menu_target();
    println!("
=== target picker ===
{}", render(&app, 96, 26));
    app.close_overlay();

    app.prompt_add_with("C:\\Users\\me\\Videos\\holiday\\clip 7.mp4".into());
    println!("
=== add prompt ===
{}", render(&app, 96, 26));
    app.close_overlay();

    app.overlay = Overlay::Confirm(Confirm::Overwrite(PathBuf::from("C:/clips/merged.mp4")));
    println!("
=== overwrite dialog ===
{}", render(&app, 96, 26));
    app.close_overlay();

    app.toggle_help();
    println!("
=== key reference ===
{}", render(&app, 96, 26));
    app.close_overlay();

    app.screen = Screen::Merging(MergeView::new(PathBuf::from("C:/clips/merged.mp4"), &app.clips));
    app.handle_event(AppEvent::Merge(MergeEvent::Plan(
        "Common format: 1920×1080 @ 30 fps, H.264 + AAC stereo 48000 Hz".into(),
    )));
    app.handle_event(AppEvent::Merge(MergeEvent::Plan(
        "2 of 4 clips are copied as they are; the other 2 get converted.".into(),
    )));
    for (i, step, secs) in [(0usize, Step::Copy, 0.4), (1, Step::Convert, 41.0)] {
        app.handle_event(AppEvent::Merge(MergeEvent::SegmentStart {
            index: i,
            name: app.clips[i].clip.name.clone(),
            step,
            duration: app.clips[i].clip.duration,
        }));
        app.handle_event(AppEvent::Merge(MergeEvent::SegmentEnd {
            index: i,
            step,
            ok: true,
            elapsed: secs,
        }));
    }
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentStart {
        index: 2,
        name: "drone_10.mp4".into(),
        step: Step::Convert,
        duration: 151.0,
    }));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentProgress { index: 2, done: 96.0 }));
    println!("
=== merging ===
{}", render(&app, 96, 26));

    app.screen = Screen::Result(Box::new(outcome(true)));
    println!("
=== result ===
{}", render(&app, 96, 26));

    let (empty, _rx) = app_with(vec![]);
    println!("
=== nothing loaded yet ===
{}", render(&empty, 96, 26));
}
