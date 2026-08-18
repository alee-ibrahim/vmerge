//! The non-interactive path: no screen, just plain lines and an exit code.
//! Ported from Start-OneShot.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, bail};

use crate::collect;
use crate::convert;
use crate::encoder::{EncoderPref, Quality};
use crate::fetch::{self, FetchEvent, FetchQuality, Stage};
use crate::ffmpeg::Tools;
use crate::format;
use crate::proc;
use crate::merge::{self, MergeEvent};
use crate::probe::{self, ClipInfo};

pub struct Options {
    pub files: Vec<PathBuf>,
    pub folder: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub quality: Quality,
    pub encoder: EncoderPref,
    pub force_reencode: bool,
}

/// Returns false when the merge did not produce a usable file, which the
/// caller turns into a non-zero exit code.
pub fn run(tools: Arc<Tools>, root: &Path, options: Options) -> Result<bool> {
    let (selected, source_folder) = choose_inputs(root, &options)?;

    if selected.is_empty() {
        println!();
        println!("  No video clips found.");
        println!();
        println!("  Folder checked: {}", source_folder.display());
        println!("  Formats read:   {}", collect::VIDEO_EXTENSIONS.join(" "));
        println!();
        println!("  Copy your clips into that folder, then run this again.");
        return Ok(false);
    }

    let output = match &options.output {
        Some(given) => {
            let mut path =
                if given.is_absolute() { given.clone() } else { source_folder.join(given) };
            if path.extension().is_none() {
                path.set_extension("mp4");
            }
            path
        }
        None => collect::default_output_path(&source_folder),
    };

    println!();
    println!("  Found {} clip(s) in: {}", selected.len(), source_folder.display());
    println!();

    let mut clips: Vec<ClipInfo> = Vec::new();
    for (i, path) in selected.iter().enumerate() {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        print!("  {:>3}. {}", i + 1, format::pad(&name, 40));
        let _ = std::io::stdout().flush();
        match probe::clip_info(&tools.ffprobe, path) {
            Some(info) => {
                println!(
                    "  {:>9}  {:>5} fps  {:<6} {:<6} {}",
                    info.dimensions(),
                    format::fps(info.fps),
                    info.video_codec,
                    info.audio_label(),
                    format::duration(info.duration)
                );
                clips.push(info);
            }
            None => println!("  [SKIPPED - not a readable video]"),
        }
    }

    if clips.is_empty() {
        bail!("None of the files could be read as video.");
    }

    let total_duration: f64 = clips.iter().map(|c| c.duration).sum();
    let total_size: u64 = clips.iter().map(|c| c.size_bytes).sum();
    println!();
    println!(
        "  Total input : {} clips, {}, {}",
        clips.len(),
        format::duration(total_duration),
        format::size(total_size)
    );
    println!("  Output file : {}", output.display());
    println!();

    let job = merge::Job {
        tools,
        clips,
        output,
        quality: options.quality,
        encoder: options.encoder,
        force_reencode: options.force_reencode,
        target_override: None,
    };

    let cancel = AtomicBool::new(false);
    let total = job.clips.len();
    let tty = std::io::stdout().is_terminal();
    let mut current_duration = 0.0f64;
    let mut current_label = String::new();
    let mut last_percent = u32::MAX;

    let outcome = merge::run(&job, &cancel, &mut |event| match event {
        MergeEvent::Plan(line) => println!("  {line}"),
        MergeEvent::Pass { attempt, .. } => {
            if attempt > 1 {
                println!();
                println!("  Retrying (pass {attempt})...");
            }
        }
        MergeEvent::SegmentStart { index, name, step, duration } => {
            current_duration = duration;
            last_percent = u32::MAX;
            current_label =
                format!("  [{}/{}] {:<8} {}", index + 1, total, step.verb(), format::pad(&name, 34));
            redraw(&current_label, false, tty);
        }
        MergeEvent::SegmentProgress { done, .. } => {
            if current_duration <= 0.0 {
                return;
            }
            let percent = ((done / current_duration).clamp(0.0, 1.0) * 100.0) as u32;
            if percent != last_percent {
                last_percent = percent;
                redraw(&format!("{current_label} {percent:>3}%"), false, tty);
            }
        }
        MergeEvent::SegmentEnd { step, ok, elapsed, .. } => {
            let mark = if ok {
                format!("{} in {}", step.past(), format::short_duration(elapsed))
            } else {
                "FAILED".to_string()
            };
            redraw(&format!("{current_label} {mark}"), true, tty);
        }
        MergeEvent::JoinStart => {
            println!();
            // On a console this line is rewritten in place by the percentages
            // that follow; in a log it has to stand on its own or the join
            // leaves no trace at all.
            redraw("  Joining...", !tty, tty);
        }
        MergeEvent::JoinProgress { done, total } => {
            if total > 0.0 {
                let percent = (done / total).clamp(0.0, 1.0) * 100.0;
                redraw(&format!("  Joining... {percent:>3.0}%"), false, tty);
            }
        }
        MergeEvent::Warning(text) => redraw(&format!("  {text}"), true, tty),
        MergeEvent::Finished(_) => {}
    });

    println!();
    println!();
    if outcome.ok {
        println!("  -------------------------------------------");
        println!("  DONE");
        println!("  -------------------------------------------");
        println!("  File     : {}", outcome.output.display());
        println!("  Size     : {}", format::size(outcome.size));
        println!("  Length   : {}", format::duration(outcome.out_duration));
        if let Some((w, h, fps)) = outcome.out_format {
            println!(
                "  Video    : {w}{}{h} @ {} fps",
                crate::theme::glyph::TIMES,
                format::fps(fps)
            );
        }
        println!("  Took     : {}", format::duration(outcome.elapsed));
    } else {
        println!("  The merge did not finish. Nothing usable was written.");
        if let Some(error) = &outcome.error {
            println!("  Reason: {error}");
        }
    }
    for warning in &outcome.warnings {
        println!("  Note: {warning}");
    }

    Ok(outcome.ok)
}

pub struct Conversion {
    pub files: Vec<PathBuf>,
    pub folder: Option<PathBuf>,
    pub target: convert::Target,
    pub quality: Quality,
    pub encoder: EncoderPref,
    pub force_reencode: bool,
}

/// `--convert-to <FORMAT>` : write every input out again in that format and stop.
///
/// Deliberately not interactive even on a terminal, the same as `--download`: the
/// flag already names the format, which is the only thing the picker would have
/// asked, and a script that has to know whether it worked gets the exit code.
pub fn convert(tools: Arc<Tools>, root: &Path, options: Conversion) -> Result<bool> {
    let (selected, source_folder) = choose_conversion_inputs(root, &options)?;
    if selected.is_empty() {
        println!();
        println!("  Nothing to convert.");
        println!();
        println!("  Folder checked: {}", source_folder.display());
        println!("  Formats read:   {}", collect::VIDEO_EXTENSIONS.join(" "));
        println!("                  {}", collect::AUDIO_EXTENSIONS.join(" "));
        return Ok(false);
    }

    println!();
    println!("  Converting to : {}", options.target.ext().to_uppercase());
    println!("  Folder        : {}", source_folder.display());
    println!();

    let mut clips: Vec<ClipInfo> = Vec::new();
    for (i, path) in selected.iter().enumerate() {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        print!("  {:>3}. {}", i + 1, format::pad(&name, 40));
        let _ = std::io::stdout().flush();
        match probe::clip_info(&tools.ffprobe, path) {
            Some(info) => {
                // What happens to this file is decided before anything runs, so
                // the list doubles as the plan.
                let planned = match convert::decide(&info, options.target, options.force_reencode) {
                    convert::Move::Copy => "remux".to_string(),
                    convert::Move::Encode => "re-encode".to_string(),
                    convert::Move::Skip(why) => format!("SKIPPED - it {why}"),
                };
                println!(
                    "  {:>9}  {:<10} {:>8}  {}",
                    info.dimensions(),
                    info.codec_label(),
                    format::duration(info.duration),
                    planned
                );
                clips.push(info);
            }
            None => println!("  [SKIPPED - neither video nor audio]"),
        }
    }

    if clips.is_empty() {
        bail!("None of the files could be read.");
    }
    println!();

    let job = convert::Job {
        tools,
        clips,
        target: options.target,
        quality: options.quality,
        encoder: options.encoder,
        force_reencode: options.force_reencode,
    };

    // ctrl-c stops after the file in progress rather than killing the process, so
    // a long batch keeps everything it had finished.
    let cancel = proc::stop_on_interrupt();
    let total = job.clips.len();
    let tty = std::io::stdout().is_terminal();
    let mut current_duration = 0.0f64;
    let mut current_label = String::new();
    let mut last_percent = u32::MAX;

    let outcome = convert::run(&job, cancel, &mut |event| match event {
        MergeEvent::Plan(line) => println!("  {line}"),
        MergeEvent::Pass { .. } => println!(),
        MergeEvent::SegmentStart { index, name, step, duration } => {
            current_duration = duration;
            last_percent = u32::MAX;
            current_label = format!(
                "  [{}/{}] {:<8} {}",
                index + 1,
                total,
                step.verb(),
                format::pad(&name, 34)
            );
            redraw(&current_label, false, tty);
        }
        MergeEvent::SegmentProgress { done, .. } => {
            if current_duration <= 0.0 {
                return;
            }
            let percent = ((done / current_duration).clamp(0.0, 1.0) * 100.0) as u32;
            if percent != last_percent {
                last_percent = percent;
                redraw(&format!("{current_label} {percent:>3}%"), false, tty);
            }
        }
        MergeEvent::SegmentEnd { step, ok, elapsed, .. } => {
            let mark = if ok {
                format!("{} in {}", step.past(), format::short_duration(elapsed))
            } else {
                "not written".to_string()
            };
            redraw(&format!("{current_label} {mark}"), true, tty);
        }
        // A conversion never joins anything, so these cannot arrive.
        MergeEvent::JoinStart | MergeEvent::JoinProgress { .. } => {}
        MergeEvent::Warning(text) => redraw(&format!("  {text}"), true, tty),
        MergeEvent::Finished(_) => {}
    });

    println!();
    if outcome.ok {
        println!("  -------------------------------------------");
        println!("  DONE");
        println!("  -------------------------------------------");
        match outcome.outputs.len() {
            1 => println!("  File     : {}", outcome.output.display()),
            n => {
                println!("  Files    : {n}");
                println!("  Folder   : {}", source_folder.display());
                for path in &outcome.outputs {
                    println!(
                        "             {}",
                        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                    );
                }
            }
        }
        println!("  Size     : {}", format::size(outcome.size));
        println!("  Took     : {}", format::duration(outcome.elapsed));
    } else {
        println!("  Nothing was converted.");
        if let Some(error) = &outcome.error {
            println!("  Reason: {error}");
        }
    }
    for warning in &outcome.warnings {
        println!("  Note: {warning}");
    }

    Ok(outcome.ok)
}

/// Which files a conversion runs over: an explicit list wins, and otherwise every
/// video and audio file in the folder, in natural filename order.
fn choose_conversion_inputs(root: &Path, options: &Conversion) -> Result<(Vec<PathBuf>, PathBuf)> {
    if !options.files.is_empty() {
        let mut selected = Vec::new();
        for file in &options.files {
            if file.is_file() {
                selected.push(file.clone());
            } else {
                println!("  Not found, ignoring: {}", file.display());
            }
        }
        let folder = selected
            .first()
            .and_then(|f| f.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
        return Ok((selected, folder));
    }

    let folder = options.folder.clone().unwrap_or_else(|| root.to_path_buf());
    if !folder.is_dir() {
        bail!("Folder not found: {}", folder.display());
    }
    Ok((collect::media_files_in_folder(&folder, None), folder))
}

pub struct Download {
    pub url: String,
    pub folder: PathBuf,
    pub quality: FetchQuality,
    pub install_root: PathBuf,
    pub search: Vec<PathBuf>,
    pub allow_download: bool,
}

/// `--download` : fetch one video and stop. No clip list, no merge, no screen.
///
/// Returns false when nothing usable was written, which the caller turns into a
/// non-zero exit code - so this is usable from a script that has to know.
pub fn download(tools: Arc<Tools>, options: Download) -> Result<bool> {
    println!();
    println!("  Link    : {}", options.url);
    println!("  Quality : {}", options.quality.label());
    println!("  Folder  : {}", options.folder.display());
    println!();

    let job = fetch::Job {
        tools,
        url: options.url,
        folder: options.folder,
        quality: options.quality,
        install_root: options.install_root,
        search: options.search,
        allow_download: options.allow_download,
    };

    // ctrl-c asks the job to stop rather than killing it, so a live recording
    // gets to close its file instead of being cut off mid-write.
    let cancel = proc::stop_on_interrupt();
    let tty = std::io::stdout().is_terminal();
    let mut stream = 0u32;
    let mut last_percent = u32::MAX;
    // A live broadcast reports how much running time is in the file rather than
    // how far through it is, because there is no "through" to be far along.
    let mut recording = false;
    let mut finishing = false;
    // Redirected output gets a line per stream rather than a bar it cannot draw,
    // so a log ends up with a record instead of carriage-return litter.
    let mut logged_percent = 0u32;

    let outcome = fetch::run(&job, cancel, &mut |event| match event {
        FetchEvent::Note(line) => redraw(&format!("  {line}"), true, tty),
        FetchEvent::Title(title) => redraw(&format!("  {title}"), true, tty),
        // Mostly not announced here, unlike on the interactive screen. A stream
        // ending means either that ffmpeg is now joining or that a second
        // stream is about to start, and the plain console cannot take a line
        // back once it has printed it - so it says nothing rather than
        // something it may have to contradict two lines later. Recording is the
        // exception: which of the two jobs is running changes what every line
        // after it means.
        FetchEvent::Stage(stage) => {
            recording = stage == Stage::Recording;
            finishing = stage == Stage::Finishing;
            if recording {
                // What it is doing has already been said, by the note every
                // screen gets. What only this one needs is which key stops it.
                redraw("  Press ctrl-c to stop and keep what has arrived.", true, tty);
                last_percent = u32::MAX;
            }
        }
        FetchEvent::Stream(n) => {
            stream = n;
            last_percent = u32::MAX;
            logged_percent = 0;
        }
        FetchEvent::Progress { done, total, rate, eta, fragments } => {
            if recording {
                let percent = match fragments {
                    Some((at, count)) if count > 0 => {
                        ((at as f64 / count as f64) * 100.0) as u32
                    }
                    _ => 0,
                };
                let line = format!(
                    "  recording  {percent:>3}% of the broadcast so far   {} at {}",
                    format::size(done),
                    format::rate(rate)
                );
                if tty {
                    redraw(&line, false, tty);
                } else if percent >= logged_percent + 5 {
                    // A redirected run would otherwise say nothing at all
                    // between starting and finishing, which for a recording that
                    // can last hours is a log that proves nothing.
                    logged_percent = percent - percent % 5;
                    println!("{}", line.trim_end());
                }
                return;
            }
            let where_ = if finishing {
                "  putting it together".to_string()
            } else if stream > 1 {
                format!("  stream {stream}")
            } else {
                "  downloading".to_string()
            };
            let Some(total) = total.filter(|t| *t > 0) else {
                // No declared length, so there is nothing to be a fraction of.
                redraw(
                    &format!("{where_} {} at {}", format::size(done), format::rate(rate)),
                    false,
                    tty,
                );
                return;
            };
            let percent = ((done as f64 / total as f64).clamp(0.0, 1.0) * 100.0) as u32;
            if percent == last_percent {
                return;
            }
            last_percent = percent;
            let line = format!(
                "{where_} {percent:>3}%   {} / {}   {}   {} left",
                format::size(done),
                format::size(total),
                format::rate(rate),
                eta.map(format::short_duration).unwrap_or_else(|| "--:--".into())
            );
            if tty {
                redraw(&line, false, tty);
            } else if percent >= logged_percent + 25 {
                logged_percent = percent - percent % 25;
                println!("{}", line.trim_end());
            }
        }
        FetchEvent::Finished(_) => {}
    });

    if tty {
        println!();
    }
    println!();
    if outcome.ok {
        println!("  -------------------------------------------");
        println!("  DONE");
        println!("  -------------------------------------------");
        println!("  File     : {}", outcome.output.display());
        println!("  Size     : {}", format::size(outcome.size));
        println!("  Length   : {}", format::duration(outcome.out_duration));
        if let Some((w, h, fps)) = outcome.out_format {
            println!(
                "  Video    : {w}{}{h} @ {} fps",
                crate::theme::glyph::TIMES,
                format::fps(fps)
            );
        }
        println!("  Took     : {}", format::duration(outcome.elapsed));
    } else {
        println!("  The download did not finish. Nothing usable was written.");
        if let Some(error) = &outcome.error {
            println!("  Reason: {error}");
        }
    }

    Ok(outcome.ok)
}

/// Rewrites the current line in place. Padded to a fixed width, because a
/// shorter line would otherwise leave the tail of the longer one it replaced.
///
/// With output redirected to a file there is no cursor to rewind, so only
/// finished lines are printed - otherwise every percentage tick would land in
/// the log as carriage-return litter.
fn redraw(text: &str, finish: bool, tty: bool) {
    if !tty {
        if finish {
            println!("{}", text.trim_end());
        }
        return;
    }
    print!("\r{text:<74}");
    if finish {
        println!();
    }
    let _ = std::io::stdout().flush();
}

/// Which files, in which order: an explicit list wins, then order.txt, then
/// natural filename order.
fn choose_inputs(root: &Path, options: &Options) -> Result<(Vec<PathBuf>, PathBuf)> {
    if !options.files.is_empty() {
        let mut selected = Vec::new();
        for file in &options.files {
            if file.is_file() {
                selected.push(file.clone());
            } else {
                println!("  Not found, ignoring: {}", file.display());
            }
        }
        let folder = selected
            .first()
            .and_then(|f| f.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
        println!("  Using the files you listed, in the order you listed them.");
        return Ok((selected, folder));
    }

    let folder = options.folder.clone().unwrap_or_else(|| root.to_path_buf());
    if !folder.is_dir() {
        bail!("Folder not found: {}", folder.display());
    }

    let output_leaf = options
        .output
        .as_ref()
        .and_then(|o| o.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    let found = collect::video_files_in_folder(&folder, output_leaf.as_deref());

    // order.txt (one filename per line) overrides filename ordering.
    let order_file = folder.join("order.txt");
    if order_file.is_file() {
        println!("  order.txt found - using the order listed in it.");
        let text = std::fs::read_to_string(&order_file).unwrap_or_default();
        let mut selected = Vec::new();
        for line in text.lines() {
            let key = line.trim().trim_matches('"');
            if key.is_empty() || key.starts_with('#') {
                continue;
            }
            match found.iter().find(|f| {
                f.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(key))
            }) {
                Some(hit) => selected.push(hit.clone()),
                None => println!("  order.txt lists a file that is not here: {key}"),
            }
        }
        if selected.is_empty() {
            println!("  order.txt matched nothing; falling back to filename order.");
        } else {
            return Ok((selected, folder));
        }
    }

    Ok((found, folder))
}
