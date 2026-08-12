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

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

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

    let tools = ffmpeg::resolve(&exe_dir, &search, !args.skip_ffmpeg_download, &mut |line| {
        println!("  {line}");
    })
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
