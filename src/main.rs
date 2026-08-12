//! vmerge - joins video clips into a single .mp4.
//!
//! Input  : mp4, mov, mkv, avi, m4v, webm, wmv, flv, mpg, mpeg, ts, m2ts, mts,
//!          3gp, 3g2, ogv, asf
//! Output : one .mp4 (H.264 + AAC, faststart)
//!
//! Running it with no arguments opens the interactive screen: drag clips onto
//! the window, reorder them with shift+arrows, press S to start. Dropping clips
//! straight onto the executable loads them in the order they were dropped.
//!
//! Clips that already share identical codecs/size/framerate are joined without
//! re-encoding (seconds, zero quality loss). Anything mixed is normalised to a
//! common format first, then joined.

mod app;
mod collect;
mod encoder;
mod ffmpeg;
mod format;
mod input;
mod merge;
mod oneshot;
mod plan;
mod probe;
mod proc;
mod theme;
mod ui;

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use ratatui::crossterm::execute;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};

use crate::app::{App, Screen};
use crate::encoder::{EncoderPref, Quality};

/// How often the screen is redrawn when nothing is happening.
const TICK: Duration = Duration::from_millis(100);

#[derive(Parser)]
#[command(
    name = "vmerge",
    about = "Joins video clips into one .mp4",
    long_about = None,
    version
)]
struct Args {
    /// Clips to load, in order. Files dropped onto the program land here.
    files: Vec<PathBuf>,

    /// Folder holding the clips. Defaults to the folder the program is in.
    #[arg(long)]
    folder: Option<PathBuf>,

    /// Output file name, or a full path. Default: merged.mp4 next to the clips.
    #[arg(long, short)]
    output: Option<PathBuf>,

    /// Text file holding one path per line, in order.
    #[arg(long)]
    file_list: Option<PathBuf>,

    #[arg(long, value_enum, default_value = "high")]
    quality: Quality,

    /// auto uses the GPU if one works; cpu is always libx264.
    #[arg(long, value_enum, default_value = "auto")]
    encoder: EncoderPref,

    /// Re-encode every clip even when they already match.
    #[arg(long)]
    force_reencode: bool,

    /// Never download ffmpeg; fail instead if it is missing.
    #[arg(long)]
    skip_ffmpeg_download: bool,

    /// Merge straight away and print plain text, with no interactive screen.
    #[arg(long)]
    no_tui: bool,

    /// Do not wait for a keypress before closing.
    #[arg(long)]
    no_pause: bool,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let interactive = !args.no_tui;
    let pause = !args.no_pause;

    match real_main(args) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => {
            // The merge ran and failed; it has already said why.
            if pause {
                wait_for_key();
            }
            std::process::ExitCode::FAILURE
        }
        Err(error) => {
            // Anything unexpected must stay on screen. A window that vanishes
            // tells the user nothing.
            eprintln!();
            eprintln!("  STOPPED: {error:#}");
            if pause || interactive {
                wait_for_key();
            }
            std::process::ExitCode::FAILURE
        }
    }
}

fn real_main(args: Args) -> Result<bool> {
    banner();

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    // Where clips are looked for, and where an installed ffmpeg goes.
    let root = args
        .folder
        .clone()
        .filter(|f| f.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| exe_dir.clone());

    // A cargo-built binary sits in target/release, so its own folder is not
    // where a sibling ffmpeg folder lives. Walk up a few levels as well.
    let mut search = vec![exe_dir.clone(), root.clone()];
    let mut walk = exe_dir.as_path();
    for _ in 0..3 {
        match walk.parent() {
            Some(parent) => {
                search.push(parent.to_path_buf());
                walk = parent;
            }
            None => break,
        }
    }

    let mut reporter = SetupReporter::new();
    let tools = ffmpeg::resolve(&exe_dir, &search, !args.skip_ffmpeg_download, &mut reporter)
        .context("setting up ffmpeg")?;
    let tools = Arc::new(tools);

    // Explicit files win; a list file is the same thing from a launcher script.
    let mut files = args.files.clone();
    if let Some(list) = &args.file_list {
        files.extend(read_file_list(list)?);
    }

    // Drawing a full-screen UI into a pipe or a log file helps nobody, and
    // there would be no way to press S to start it either.
    let has_screen = io::stdout().is_terminal();
    if !args.no_tui && !has_screen {
        println!("  Output is not a terminal, so this is running as a plain one-shot merge.");
    }

    if args.no_tui || !has_screen {
        return oneshot::run(
            tools,
            &root,
            oneshot::Options {
                files,
                folder: args.folder,
                output: args.output,
                quality: args.quality,
                encoder: args.encoder,
                force_reencode: args.force_reencode,
            },
        );
    }

    run_tui(tools, root, files, &args)
}

/// Setup progress on the plain console, before the interactive screen exists.
///
/// A hundred-megabyte download with nothing but "Downloading..." on screen is
/// indistinguishable from a hang, which is exactly what it looked like. The bar
/// is the same eighth-block one the merge screen uses, so the two match.
struct SetupReporter {
    tty: bool,
    started: Instant,
    last_drawn: Option<Instant>,
    /// Set once a bar has been drawn, so `finished` knows to end the line.
    drawing: bool,
    last_logged_percent: u64,
}

impl SetupReporter {
    /// Redraw rate. Fast enough to look live, slow enough not to spend the
    /// download flushing the console.
    const REDRAW: Duration = Duration::from_millis(100);

    fn new() -> Self {
        Self {
            tty: io::stdout().is_terminal(),
            started: Instant::now(),
            last_drawn: None,
            drawing: false,
            last_logged_percent: 0,
        }
    }
}

impl ffmpeg::Reporter for SetupReporter {
    fn log(&mut self, line: &str) {
        self.finished();
        println!("  {line}");
    }

    fn progress(&mut self, received: u64, total: Option<u64>) {
        // The clock starts when the bytes do. Counting from program start would
        // fold ffmpeg discovery into the rate and report it far too low.
        if received == 0 {
            self.started = Instant::now();
            // Also reset the logged percentage: it was left at 100 by the
            // download, so unpacking printed nothing at all in a log.
            self.last_logged_percent = 0;
        }
        let elapsed = self.started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.2 { received as f64 / elapsed } else { 0.0 };

        if !self.tty {
            // Redirected output gets a line every 25%, not a bar it cannot draw.
            if let Some(total) = total {
                let percent = received * 100 / total.max(1);
                if percent >= self.last_logged_percent + 25 {
                    self.last_logged_percent = percent - percent % 25;
                    println!("  {}%  {}", self.last_logged_percent, format::size(received));
                }
            }
            return;
        }

        let now = Instant::now();
        let due = self.last_drawn.is_none_or(|last| now.duration_since(last) >= Self::REDRAW);
        let complete = total.is_some_and(|total| received >= total);
        if !due && !complete {
            return;
        }
        self.last_drawn = Some(now);
        self.drawing = true;

        print!("\r{:<78}", progress_line(received, total, rate));
        let _ = io::stdout().flush();
    }

    fn finished(&mut self) {
        if self.drawing {
            self.drawing = false;
            self.last_drawn = None;
            println!();
        }
    }
}

/// The download line, kept pure so it can be checked without a socket.
fn progress_line(received: u64, total: Option<u64>, rate: f64) -> String {
    match total {
        Some(total) => {
            let fraction = (received as f64 / total as f64).clamp(0.0, 1.0);
            // On a slow link the wait is minutes, so say how many. Without a
            // rate yet there is nothing honest to put here.
            let left = if rate > 0.0 && received < total {
                format!("   {} left", format::short_duration((total - received) as f64 / rate))
            } else {
                String::new()
            };
            format!(
                "  {}  {:>3.0}%   {} / {}   {}{}",
                theme::bar(fraction, 24),
                fraction * 100.0,
                format::size(received),
                format::size(total),
                format::rate(rate),
                left
            )
        }
        // No declared length, so there is nothing to be a fraction of.
        None => format!("  {}   {}", format::size(received), format::rate(rate)),
    }
}

fn banner() {
    println!();
    println!("  ===========================================");
    println!("            V I D E O   M E R G E R");
    println!("  ===========================================");
}

fn read_file_list(path: &Path) -> Result<Vec<PathBuf>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the file list {}", path.display()))?;
    Ok(text
        .lines()
        .map(|l| l.trim().trim_matches('"'))
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn run_tui(
    tools: Arc<ffmpeg::Tools>,
    root: PathBuf,
    files: Vec<PathBuf>,
    args: &Args,
) -> Result<bool> {
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(tools, root.clone(), tx);
    app.quality = args.quality;
    app.encoder = args.encoder;
    app.force_reencode = args.force_reencode;
    if let Some(output) = &args.output
        && let Some(name) = output.file_name()
    {
        app.output_name = name.to_string_lossy().into_owned();
    }

    if !files.is_empty() {
        // Files were dropped onto the program: start with exactly those, in the
        // order they were dropped.
        app.add_paths(files.iter().map(|f| f.display().to_string()).collect());
    } else {
        // Otherwise preload whatever is already sitting in the folder - the
        // common case is "the clips are in this folder", and it saves any typing.
        let preload = collect::video_files_in_folder(&root, None);
        if !preload.is_empty() {
            app.add_paths(preload.iter().map(|f| f.display().to_string()).collect());
        }
    }

    let mut terminal = ratatui::init();
    // Terminals that support this send a whole dropped or pasted path as one
    // event, which removes the guesswork in input.rs.
    let _ = execute!(io::stdout(), EnableBracketedPaste, EnableMouseCapture);

    let mut ui_state = ui::UiState::default();
    let result = (|| -> Result<()> {
        let mut dirty = true;
        while !app.quit {
            // A merge screen has a moving clock, so it redraws on every tick;
            // an idle list only redraws when something actually changed.
            if dirty || matches!(app.screen, Screen::Merging(_)) {
                terminal.draw(|frame| ui::draw(frame, &app, &mut ui_state))?;
            }
            dirty = input::pump(&mut app, &ui_state, TICK)?;
            while let Ok(event) = rx.try_recv() {
                app.handle_event(event);
                dirty = true;
            }
        }
        Ok(())
    })();

    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();

    result?;

    // Leave the last outcome on the plain console, so closing the window is not
    // the only record of what happened.
    if let Screen::Result(outcome) = &app.screen {
        println!();
        if outcome.ok {
            println!("  Wrote {}", outcome.output.display());
            println!(
                "  {}, {}, took {}",
                format::size(outcome.size),
                format::duration(outcome.out_duration),
                format::duration(outcome.elapsed)
            );
        } else if let Some(error) = &outcome.error {
            println!("  The last merge did not finish: {error}");
        }
    }
    println!();

    Ok(true)
}

fn wait_for_key() {
    println!();
    println!("  Press Enter to close this window...");
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_download_line_reports_the_numbers() {
        let line = progress_line(20 * 1024 * 1024, Some(40 * 1024 * 1024), 4.5 * 1024.0 * 1024.0);
        assert!(line.contains("50%"), "got {line:?}");
        assert!(line.contains("20.0 MB / 40.0 MB"), "got {line:?}");
        assert!(line.contains("4.5 MB/s"), "got {line:?}");
        assert!(line.contains(theme::glyph::FULL), "expected a filled bar: {line:?}");
        // 20 MB left at 4.5 MB/s is about 4 and a half seconds.
        assert!(line.contains("left"), "a slow download needs an estimate: {line:?}");
    }

    #[test]
    fn a_download_of_unknown_length_still_reports_bytes() {
        // Some mirrors send no Content-Length, and a bar would be a lie.
        let line = progress_line(3 * 1024 * 1024, None, 512.0 * 1024.0);
        assert!(line.contains("3.0 MB"), "got {line:?}");
        assert!(line.contains("512 KB/s"), "got {line:?}");
        assert!(!line.contains('%'), "no percentage without a total: {line:?}");
    }

    #[test]
    fn the_download_line_ends_at_a_hundred() {
        let full = 40 * 1024 * 1024;
        let line = progress_line(full, Some(full), 9.9 * 1024.0 * 1024.0);
        assert!(line.contains("100%"), "got {line:?}");
        // Overshooting a declared length must not produce 103%.
        let over = progress_line(full + 4096, Some(full), 1.0);
        assert!(over.contains("100%"), "got {over:?}");
        assert!(!line.contains("left"), "nothing left to wait for at the end: {line:?}");
    }
}
