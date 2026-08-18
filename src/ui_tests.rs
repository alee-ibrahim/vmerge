//! Render tests for every screen and overlay.
//!
//! These draw into a TestBackend and read the resulting characters back, which
//! catches the two things that are otherwise only found by running the program:
//! a layout that panics at an awkward terminal size, and a value that silently
//! stops appearing on screen.

use super::*;
use crate::app::{AppEvent, Entry, FetchView, MergeView};
use crate::convert;
use crate::fetch::FetchEvent;
use crate::ffmpeg::Tools;
use crate::merge::{MergeEvent, Outcome};
use crate::probe::ClipInfo;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

fn clip(name: &str, w: u32, h: u32, fps: f64, secs: f64) -> ClipInfo {
    ClipInfo {
        path: PathBuf::from(format!("C:/clips/{name}")),
        name: name.into(),
        has_video: true,
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

/// Sound with no picture: a legitimate thing to convert, and nothing a merge can
/// use.
fn soundtrack(name: &str, codec: &str, secs: f64) -> ClipInfo {
    ClipInfo {
        has_video: false,
        video_codec: String::new(),
        width: 0,
        height: 0,
        pix_fmt: String::new(),
        fps: 0.0,
        frame_rate_raw: String::new(),
        audio_codec: codec.into(),
        ..clip(name, 0, 0, 0.0, secs)
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

    app.overlay = Overlay::Confirm(Confirm::CancelFetch);
    assert!(render(&app, 100, 30).contains("STOP THE DOWNLOAD?"));

    app.overlay = Overlay::Confirm(Confirm::StopRecording);
    assert!(render(&app, 100, 30).contains("FINISH THE RECORDING?"));

    app.prompt_fetch();
    let screen = render(&app, 100, 30);
    assert!(screen.contains("DOWNLOAD A VIDEO"), "got:\n{screen}");
    assert!(screen.contains("Paste a link"), "got:\n{screen}");
}

/// The picker is the second half of asking for a link, so a link that parses
/// has to lead straight to it - and one that does not must say so instead.
#[test]
fn a_link_opens_the_stream_picker_and_a_path_does_not() {
    let (mut app, _rx) = app_with(vec![]);

    app.prompt_fetch();
    if let Overlay::Prompt(prompt) = &mut app.overlay {
        prompt.buffer = "youtu.be/dQw4w9WgXcQ".into();
    }
    app.submit_prompt();

    let screen = render(&app, 100, 30);
    assert!(screen.contains("HOW MUCH OF IT"), "got:\n{screen}");
    for choice in ["best available", "4K", "1080p", "720p", "480p", "audio only"] {
        assert!(screen.contains(choice), "{choice:?} has to be offered:\n{screen}");
    }

    // The picker is a fixed 72 columns and its rows do not wrap, so a note one
    // word too long is silently cut in half. What going above 1080p costs is the
    // whole reason 4K is named rather than left to "best available", so that is
    // the line which must survive being drawn.
    assert!(screen.contains("no H.264"), "the caveat has to be there:
{screen}");
    assert!(screen.contains("re-encodes it"), "and must not be cut off:
{screen}");
    // Numbered downwards from most to least, so pressing 2 gets 4K.
    let row = screen.lines().find(|l| l.contains("4K")).expect("a 4K row");
    assert!(row.contains(" 2 "), "4K has to be the second choice: {row:?}");

    // A dropped clip is not a link, and must not start a download.
    app.close_overlay();
    app.prompt_fetch();
    if let Overlay::Prompt(prompt) = &mut app.overlay {
        prompt.buffer = r"C:\Users\me\Videos\clip.mp4".into();
    }
    app.submit_prompt();
    assert!(matches!(app.overlay, Overlay::None), "no picker for a path");
    assert!(render(&app, 100, 30).contains("does not look like a link"));
}

/// Backing out of the picker has to drop the link with it, or it would attach
/// itself to whatever the next picker was opened for.
#[test]
fn abandoning_the_picker_abandons_the_link() {
    let (mut app, _rx) = app_with(vec![]);
    app.prompt_fetch();
    if let Overlay::Prompt(prompt) = &mut app.overlay {
        prompt.buffer = "https://example.com/v/1".into();
    }
    app.submit_prompt();
    app.close_overlay();

    // The quality picker is a setting, and picking from it must now change only
    // that setting rather than starting a download nobody asked for.
    app.menu_quality();
    app.menu_pick(Some(0));
    assert!(matches!(app.screen, Screen::Browse), "nothing should have started");
}

#[test]
fn the_download_screen_reports_what_is_arriving() {
    let (mut app, _rx) = app_with(vec![]);
    app.screen = Screen::Fetching(FetchView::new("https://example.com/watch?v=abc".into()));

    // Before the site answers there is only the link to show.
    assert!(render(&app, 100, 30).contains("example.com"), "the link stands in for the title");

    app.handle_event(AppEvent::Fetch(FetchEvent::Stage(crate::fetch::Stage::Downloading)));
    app.handle_event(AppEvent::Fetch(FetchEvent::Stream(1)));
    app.handle_event(AppEvent::Fetch(FetchEvent::Title("How to make bread".into())));
    app.handle_event(AppEvent::Fetch(FetchEvent::Progress {
        done: 5 * 1024 * 1024,
        total: Some(20 * 1024 * 1024),
        rate: 2.0 * 1024.0 * 1024.0,
        eta: Some(8.0),
        fragments: None,
    }));

    let screen = render(&app, 100, 30);
    assert!(screen.contains("How to make bread"), "got:\n{screen}");
    assert!(screen.contains("DOWNLOADING"), "got:\n{screen}");
    assert!(screen.contains("25%"), "expected a bar:\n{screen}");
    assert!(screen.contains('█'), "expected a filled bar:\n{screen}");
    assert!(screen.contains("5.0 MB / 20.0 MB"), "got:\n{screen}");
    assert!(screen.contains("2.0 MB/s"), "got:\n{screen}");
    assert!(screen.contains("0:08"), "expected the estimate:\n{screen}");
    assert!(screen.contains("stop the download"), "got:\n{screen}");
}

/// A bar needs something to be a fraction of. Some sites declare no length, and
/// drawing one anyway would be a guess dressed up as a measurement.
#[test]
fn a_download_of_unknown_size_draws_no_bar() {
    let (mut app, _rx) = app_with(vec![]);
    app.screen = Screen::Fetching(FetchView::new("https://example.com/live".into()));
    app.handle_event(AppEvent::Fetch(FetchEvent::Stream(1)));
    app.handle_event(AppEvent::Fetch(FetchEvent::Progress {
        done: 3 * 1024 * 1024,
        total: None,
        rate: 512.0 * 1024.0,
        eta: None,
        fragments: None,
    }));

    let screen = render(&app, 100, 30);
    assert!(screen.contains("no size given"), "got:\n{screen}");
    assert!(!screen.contains('%'), "no percentage without a total:\n{screen}");
    // The byte count is still the truth, and still moves.
    assert!(screen.contains("3.0 MB"), "got:\n{screen}");
    assert!(screen.contains("512 KB/s"), "got:\n{screen}");
}

/// Best video and best audio arrive as two files, so the bar runs 0..100 twice.
/// That has to read as a second stream rather than as progress being lost.
#[test]
fn a_second_stream_restarts_the_bar_and_says_which_one_it_is() {
    let (mut app, _rx) = app_with(vec![]);
    app.screen = Screen::Fetching(FetchView::new("https://example.com/v".into()));
    app.handle_event(AppEvent::Fetch(FetchEvent::Stream(1)));
    app.handle_event(AppEvent::Fetch(FetchEvent::Progress {
        done: 100,
        total: Some(100),
        rate: 10.0,
        eta: None,
        fragments: None,
    }));
    assert!(render(&app, 100, 30).contains("100%"));

    app.handle_event(AppEvent::Fetch(FetchEvent::Stream(2)));
    let screen = render(&app, 100, 30);
    assert!(screen.contains("STREAM 2"), "got:\n{screen}");
    assert!(!screen.contains("100%"), "the second stream starts from nothing:\n{screen}");
}

/// Installing yt-dlp is an 18 MB transfer of its own, and the tail after the
/// last byte is ffmpeg joining the streams. Both look like a hang unless the
/// screen says which one it is.
#[test]
fn the_quiet_phases_of_a_download_still_say_what_they_are() {
    let (mut app, _rx) = app_with(vec![]);
    app.screen = Screen::Fetching(FetchView::new("https://example.com/v".into()));

    app.handle_event(AppEvent::Fetch(FetchEvent::Note("Downloading yt-dlp 2026.07.04".into())));
    let screen = render(&app, 100, 30);
    assert!(screen.contains("SETTING UP"), "got:\n{screen}");
    assert!(screen.contains("Downloading yt-dlp"), "got:\n{screen}");

    app.handle_event(AppEvent::Fetch(FetchEvent::Stage(crate::fetch::Stage::Finishing)));
    let screen = render(&app, 100, 30);
    assert!(screen.contains("PUTTING IT TOGETHER"), "got:\n{screen}");
    assert!(screen.contains("finishing off"), "got:\n{screen}");
}

/// A broadcast still on air is a different job from a download, and the screen
/// has to say so. There is no total to be a fraction of and no arrival to wait
/// for, so what it shows instead is how much is captured and the fact that this
/// carries on until somebody stops it.
#[test]
fn a_live_broadcast_says_it_is_live_and_that_it_runs_until_stopped() {
    let (mut app, _rx) = app_with(vec![]);
    app.screen = Screen::Fetching(FetchView::new("https://example.com/watch?v=abc".into()));

    app.handle_event(AppEvent::Fetch(FetchEvent::Title("26th Sitting".into())));
    app.handle_event(AppEvent::Fetch(FetchEvent::Note(
        "This is live, so it is taken from the start of the broadcast rather than from now."
            .into(),
    )));
    app.handle_event(AppEvent::Fetch(FetchEvent::Stage(crate::fetch::Stage::Recording)));
    app.handle_event(AppEvent::Fetch(FetchEvent::Progress {
        done: 96 * 1024 * 1024,
        total: None,
        rate: 130.0 * 1024.0,
        eta: None,
        fragments: Some((322, 1877)),
    }));

    let screen = render(&app, 100, 30);
    assert!(screen.contains("26th Sitting"), "got:\n{screen}");
    assert!(screen.contains("RECORDING  LIVE"), "got:\n{screen}");
    assert!(screen.contains("This is live"), "the note explains itself:\n{screen}");
    // A note wider than the window loses its own ending, which is how the one
    // that used to be here stopped saying "or you stop it" at all.
    assert!(
        screen.contains("rather than from now"),
        "the note must fit the window:\n{screen}"
    );
    // 322 of 1877 pieces. A real fraction of what has been broadcast so far,
    // which is the only thing a live download can honestly be measured against -
    // and it has to say that is what it is, or 17% reads as "nearly nothing yet"
    // rather than "seventeen minutes of the first hundred".
    assert!(screen.contains("17%"), "expected a measured position:\n{screen}");
    assert!(screen.contains("of the broadcast so far"), "got:\n{screen}");
    assert!(screen.contains('█'), "expected a drawn bar:\n{screen}");
    assert!(screen.contains("captured"), "got:\n{screen}");
    assert!(screen.contains("96.0 MB"), "got:\n{screen}");
    // Never an estimate: there is no telling when a sitting will end.
    assert!(!screen.contains("estimating"), "got:\n{screen}");
    assert!(screen.contains("when you stop it"), "got:\n{screen}");
    // Stopping a recording keeps it, so esc cannot be labelled as throwing it
    // away the way it is on a download.
    assert!(screen.contains("finish the recording"), "got:\n{screen}");
}

/// Stopping is how a recording is meant to end, so the question esc raises has
/// to promise what it actually does - which is the opposite of what stopping a
/// download promises.
#[test]
fn stopping_a_recording_offers_to_keep_it() {
    let (mut app, _rx) = app_with(vec![]);
    app.screen = Screen::Fetching(FetchView::new("https://example.com/v".into()));
    app.handle_event(AppEvent::Fetch(FetchEvent::Stage(crate::fetch::Stage::Recording)));

    app.request_cancel();
    let screen = render(&app, 100, 30);
    assert!(screen.contains("FINISH THE RECORDING?"), "got:\n{screen}");
    assert!(screen.contains("is kept"), "got:\n{screen}");
    assert!(screen.contains("keep recording"), "got:\n{screen}");

    // A download in the same place still promises the opposite, because that is
    // still what it does.
    app.close_overlay();
    app.handle_event(AppEvent::Fetch(FetchEvent::Stage(crate::fetch::Stage::Downloading)));
    app.request_cancel();
    let screen = render(&app, 100, 30);
    assert!(screen.contains("STOP THE DOWNLOAD?"), "got:\n{screen}");
    assert!(screen.contains("thrown away"), "got:\n{screen}");
}

/// A recording that the user stopped produced exactly what it was asked for.
/// Reporting that as a cancelled download would call a successful capture a
/// loss, which is the one thing the report must not do.
#[test]
fn a_stopped_recording_is_reported_as_a_file_and_not_as_a_loss() {
    let (mut app, _rx) = app_with(vec![]);
    app.screen = Screen::Fetching(FetchView::new("https://example.com/v".into()));

    let mut done = outcome(true);
    done.recorded = true;
    done.cancelled = true;
    done.output = PathBuf::from("C:/clips/26th Sitting.mp4");
    app.handle_event(AppEvent::Fetch(FetchEvent::Finished(Box::new(done))));

    let screen = render(&app, 100, 30);
    assert!(screen.contains("DONE"), "got:\n{screen}");
    assert!(screen.contains("26th Sitting.mp4"), "got:\n{screen}");
    assert!(app.status.iter().any(|(text, _)| text.contains("Recorded")), "got {:?}", app.status);
}

/// A download reports through the same screen a merge does, so a file that
/// arrived from a link reads exactly like one that was built here.
#[test]
fn a_finished_download_reports_through_the_same_screen() {
    let (mut app, _rx) = app_with(vec![]);
    let mut done = outcome(true);
    done.output = PathBuf::from("C:/clips/How to make bread.mp4");
    app.screen = Screen::Fetched(Box::new(done));

    let screen = render(&app, 100, 30);
    assert!(screen.contains("DONE"));
    assert!(screen.contains("How to make bread.mp4"), "got:\n{screen}");
    assert!(screen.contains("show the file"), "got:\n{screen}");
}

/// The merge output name belongs to the merge. A download wrote a file of its
/// own choosing and never went near merged.mp4, so it must not renumber it.
#[test]
fn a_download_leaves_the_merge_output_name_alone() {
    let (mut app, _rx) = app_with(vec![clip("a.mp4", 1920, 1080, 30.0, 5.0)]);
    app.output_name = "holiday.mp4".into();

    app.screen = Screen::Fetched(Box::new(outcome(true)));
    app.dismiss_result();
    assert_eq!(app.output_name, "holiday.mp4", "a download must not touch it");

    // A finished merge still advances it, which is what stops the next merge
    // asking about overwriting the last one.
    app.screen = Screen::Result(Box::new(outcome(true)));
    app.dismiss_result();
    assert_ne!(app.output_name, "holiday.mp4", "a merge still renumbers");
}

#[test]
fn merge_screen_shows_progress_per_clip() {
    let (mut app, _rx) = app_with(vec![
        clip("one.mp4", 1920, 1080, 30.0, 10.0),
        clip("two.mp4", 1280, 720, 25.0, 10.0),
    ]);
    app.screen =
        Screen::Merging(MergeView::new(Path::new("C:/clips/merged.mp4"), &app.clips));

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
        Screen::Merging(MergeView::new(Path::new("C:/clips/merged.mp4"), &app.clips));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentEnd {
        index: 0,
        step: Step::Convert,
        ok: false,
        elapsed: 1.0,
    }));
    assert!(render(&app, 100, 30).contains("failed"));
}

// ------------------------------------------------------------------- convert

#[test]
fn the_convert_picker_lists_the_formats_and_says_what_it_will_do() {
    let (mut app, _rx) = app_with(vec![
        clip("one.mp4", 1920, 1080, 30.0, 10.0),
        clip("two.mov", 1280, 720, 25.0, 10.0),
    ]);
    app.menu_convert();

    let screen = render(&app, 100, 34);
    assert!(screen.contains("CONVERT 2 FILE(S) TO"), "got:\n{screen}");
    // Video and audio both, because "any format" is the point.
    for format in ["MKV", "WEBM", "GIF", "MP3", "FLAC"] {
        assert!(screen.contains(format), "expected {format}:\n{screen}");
    }
    // The two things worth knowing before pressing it: where the files go, and
    // that this is not a merge.
    assert!(screen.contains("beside the original"), "got:\n{screen}");
    assert!(screen.contains("Nothing is merged"), "got:\n{screen}");
}

/// Marking is how a subset gets picked out everywhere else in the program, so it
/// has to mean the same thing here - and the picker has to say so before the job
/// starts rather than after.
#[test]
fn marking_narrows_what_a_conversion_touches() {
    let (mut app, _rx) = app_with(vec![
        clip("one.mp4", 1920, 1080, 30.0, 10.0),
        clip("two.mov", 1280, 720, 25.0, 10.0),
        clip("three.mkv", 1280, 720, 25.0, 10.0),
    ]);
    app.clips[1].marked = true;

    app.menu_convert();
    let screen = render(&app, 100, 34);
    assert!(screen.contains("CONVERT 1 MARKED FILE(S) TO"), "got:\n{screen}");
}

#[test]
fn the_convert_screen_counts_files_and_names_the_format() {
    let clips =
        vec![clip("one.mp4", 1920, 1080, 30.0, 10.0), clip("two.mov", 1280, 720, 25.0, 10.0)];
    let (mut app, _rx) = app_with(clips.clone());
    app.screen = Screen::Converting(MergeView::converting(convert::Target::Mkv, &clips));

    app.handle_event(AppEvent::Merge(MergeEvent::Plan("2 files to MKV".into())));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentEnd {
        index: 0,
        step: Step::Copy,
        ok: true,
        elapsed: 0.3,
    }));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentStart {
        index: 1,
        name: "two.mov".into(),
        step: Step::Convert,
        duration: 10.0,
    }));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentProgress { index: 1, done: 5.0 }));

    let screen = render(&app, 100, 30);
    assert!(screen.contains("1/2 files"), "files, not clips - nothing is joined:\n{screen}");
    assert!(screen.contains("as MKV"), "got:\n{screen}");
    assert!(screen.contains("CONVERTING"), "the phase is not 'preparing clips':\n{screen}");
    assert!(screen.contains("stop converting"), "got:\n{screen}");
    // One file of two done plus half of the second, and no join to hold room
    // for: 75%, where the same progress on the merge screen reads 64%.
    assert!(screen.contains("75%"), "expected overall progress:\n{screen}");
}

/// A merge keeps the last slice of the bar for the join. A conversion has no
/// join, so a finished batch must read 100% rather than sitting at 85%.
#[test]
fn a_conversion_bar_is_not_held_back_for_a_join_that_never_comes() {
    let clips = vec![clip("one.mp4", 1920, 1080, 30.0, 10.0)];
    let (mut app, _rx) = app_with(clips.clone());

    fn finish(app: &mut App) {
        app.handle_event(AppEvent::Merge(MergeEvent::SegmentEnd {
            index: 0,
            step: Step::Copy,
            ok: true,
            elapsed: 1.0,
        }));
    }

    app.screen = Screen::Converting(MergeView::converting(convert::Target::Mp3, &clips));
    finish(&mut app);
    let Screen::Converting(view) = &app.screen else { panic!("expected the convert screen") };
    assert!((view.overall() - 1.0).abs() < 1e-9, "got {}", view.overall());

    app.screen = Screen::Merging(MergeView::new(Path::new("C:/clips/merged.mp4"), &app.clips));
    finish(&mut app);
    let Screen::Merging(view) = &app.screen else { panic!("expected the merge screen") };
    assert!((view.overall() - 0.85).abs() < 1e-9, "got {}", view.overall());
}

#[test]
fn the_report_describes_a_batch_by_the_count_and_not_by_one_file() {
    let (mut app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 10.0)]);
    let mut done = outcome(true);
    done.output = PathBuf::from("C:/clips/clip1.mkv");
    done.outputs = (1..=10).map(|n| PathBuf::from(format!("C:/clips/clip{n}.mkv"))).collect();
    app.screen = Screen::Converted(Box::new(done));

    let screen = render(&app, 100, 34);
    assert!(screen.contains("10 written"), "got:\n{screen}");
    assert!(screen.contains("clips"), "where they went:\n{screen}");
    assert!(screen.contains("clip1.mkv"), "got:\n{screen}");
    // Eight names and then a count: a report, not a directory listing.
    assert!(!screen.contains("clip9.mkv"), "got:\n{screen}");
    assert!(screen.contains("and 2 more"), "got:\n{screen}");
}

/// A finished conversion must not renumber the merge output: it never went near
/// merged.mp4, and the name may be one set for a merge still to come.
#[test]
fn a_conversion_leaves_the_merge_output_name_alone() {
    let (mut app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 10.0)]);
    app.output_name = "holiday.mp4".into();
    app.screen = Screen::Converted(Box::new(outcome(true)));

    app.dismiss_result();
    assert_eq!(app.output_name, "holiday.mp4");
    assert!(matches!(app.screen, Screen::Browse));
}

#[test]
fn stopping_a_conversion_says_the_finished_files_are_kept() {
    let (mut app, _rx) = app_with(vec![clip("one.mp4", 1920, 1080, 30.0, 10.0)]);
    app.overlay = Overlay::Confirm(Confirm::CancelConvert);

    let screen = render(&app, 100, 30);
    assert!(screen.contains("STOP CONVERTING?"), "got:\n{screen}");
    assert!(screen.contains("already converted are kept"), "got:\n{screen}");
}

/// An mp3 in the list is a conversion waiting to happen. It is also the one thing
/// a merge cannot use, and the list has to show both facts.
#[test]
fn a_file_with_no_picture_reads_as_sound_and_is_refused_by_the_merge() {
    let (mut app, _rx) = app_with(vec![
        clip("one.mp4", 1920, 1080, 30.0, 10.0),
        soundtrack("song.mp3", "mp3", 200.0),
    ]);

    let screen = render(&app, 100, 30);
    assert!(screen.contains("song.mp3"), "got:\n{screen}");
    assert!(screen.contains("\u{2014}\u{00b7}mp3"), "no codec and no zero:\n{screen}");
    assert!(!screen.contains("0\u{00d7}0"), "a measurement that never happened:\n{screen}");

    app.request_merge();
    let (message, kind) = app.status.clone().expect("a refusal");
    assert!(message.contains("song.mp3"), "got {message}");
    assert!(message.contains("no picture"), "got {message}");
    assert_eq!(kind, Kind::Bad);
    // And it says which key does work on it.
    assert!(message.contains('V'), "got {message}");
}

fn outcome(ok: bool) -> Outcome {
    Outcome {
        ok,
        output: PathBuf::from("C:/clips/merged.mp4"),
        outputs: if ok { vec![PathBuf::from("C:/clips/merged.mp4")] } else { Vec::new() },
        size: 5 * 1024 * 1024,
        out_duration: if ok { 65.0 } else { 0.0 },
        out_format: if ok { Some((1920, 1080, 29.97)) } else { None },
        elapsed: 12.0,
        warnings: Vec::new(),
        error: if ok { None } else { Some("ffmpeg said no".into()) },
        cancelled: false,
        recorded: false,
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
            Screen::Merging(MergeView::new(Path::new("C:/clips/merged.mp4"), &app.clips));
        render(&app, w, h);

        app.screen = Screen::Result(Box::new(outcome(true)));
        render(&app, w, h);

        app.screen = Screen::Fetching(FetchView::new(
            "https://example.com/a/very/long/link/that/overflows?v=abcdefghijklmnop".into(),
        ));
        render(&app, w, h);
        app.handle_event(AppEvent::Fetch(FetchEvent::Progress {
            done: 1024,
            total: Some(4096),
            rate: 512.0,
            eta: Some(6.0),
            fragments: None,
        }));
        render(&app, w, h);

        app.screen = Screen::Fetched(Box::new(outcome(true)));
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
        ("download", Click::Command('u')),
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

    // So is fetching one: a link needs no clips to work on, and with an empty
    // list it is the second thing worth doing.
    let (column, row) = centre_of(&empty, 100, 30, "download");
    assert_eq!(click_at(&empty, 100, 30, column, row), Some(Click::Command('u')));

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
        Screen::Merging(MergeView::new(Path::new("C:/clips/merged.mp4"), &app.clips));

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

    app.screen = Screen::Merging(MergeView::new(Path::new("C:/clips/merged.mp4"), &app.clips));
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

    app.screen = Screen::Browse;
    app.menu_convert();
    println!("
=== format picker ===
{}", render(&app, 96, 30));
    app.close_overlay();

    let clips: Vec<ClipInfo> = app.clips.iter().map(|e| e.clip.clone()).collect();
    app.screen = Screen::Converting(MergeView::converting(convert::Target::Mkv, &clips));
    app.handle_event(AppEvent::Merge(MergeEvent::Plan(
        "4 files to MKV, each written beside the original.".into(),
    )));
    app.handle_event(AppEvent::Merge(MergeEvent::Plan(
        "3 are remuxed as they are; 1 is re-encoded with CPU (libx264), quality high.".into(),
    )));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentEnd {
        index: 0,
        step: Step::Copy,
        ok: true,
        elapsed: 0.3,
    }));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentStart {
        index: 1,
        name: "beach clip 2.mov".into(),
        step: Step::Copy,
        duration: 64.0,
    }));
    app.handle_event(AppEvent::Merge(MergeEvent::SegmentProgress { index: 1, done: 22.0 }));
    println!("
=== converting ===
{}", render(&app, 96, 26));

    let mut batch = outcome(true);
    batch.output = PathBuf::from("C:/clips/intro.mkv");
    batch.outputs = ["intro", "beach clip 2", "drone_10", "outro"]
        .iter()
        .map(|n| PathBuf::from(format!("C:/clips/{n}.mkv")))
        .collect();
    app.screen = Screen::Converted(Box::new(batch));
    println!("
=== converted ===
{}", render(&app, 96, 26));

    app.screen = Screen::Browse;
    app.prompt_fetch();
    if let Overlay::Prompt(prompt) = &mut app.overlay {
        prompt.buffer = "https://www.youtube.com/watch?v=aqz-KE-bpKQ".into();
    }
    println!("
=== link prompt ===
{}", render(&app, 96, 26));
    app.submit_prompt();
    println!("
=== stream picker ===
{}", render(&app, 96, 26));
    app.close_overlay();

    app.screen =
        Screen::Fetching(FetchView::new("https://www.youtube.com/watch?v=aqz-KE-bpKQ".into()));
    app.handle_event(AppEvent::Fetch(FetchEvent::Note(
        "Downloading yt-dlp 2026.07.04 - this happens once".into(),
    )));
    app.handle_event(AppEvent::Fetch(FetchEvent::Stream(2)));
    app.handle_event(AppEvent::Fetch(FetchEvent::Title(
        "Big Buck Bunny 60fps 4K - Official Blender Foundation Short Film".into(),
    )));
    app.handle_event(AppEvent::Fetch(FetchEvent::Progress {
        done: 17 * 1024 * 1024,
        total: Some(29 * 1024 * 1024),
        rate: 10.7 * 1024.0 * 1024.0,
        eta: Some(1.0),
        fragments: None,
    }));
    println!("
=== downloading ===
{}", render(&app, 96, 26));

    app.overlay = Overlay::Confirm(Confirm::CancelFetch);
    println!("
=== stop the download ===
{}", render(&app, 96, 26));
    app.close_overlay();

    let (empty, _rx) = app_with(vec![]);
    println!("
=== nothing loaded yet ===
{}", render(&empty, 96, 26));
}
