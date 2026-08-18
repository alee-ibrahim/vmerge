//! The merge engine.
//!
//! Ported from Invoke-MergeAll and friends, with one structural change: the
//! PowerShell version called Invoke-CopyMerge, Invoke-PartialConvertMerge and
//! Invoke-ReencodeMerge, none of which were ever defined - every multi-clip
//! merge died in the catch block. Those three collapse into the two functions
//! here, which is what the half-finished Invoke-SegmentedMerge refactor was
//! reaching for:
//!
//! * `copy_merge` - every clip already shares one format, so each is remuxed
//!   untouched and the set is joined. Whatever the codec is.
//! * `segmented` - one target format; each clip is copied if it already matches
//!   and converted only if it does not. With `force_all` this is the full
//!   re-encode fallback.
//!
//! Everything goes through MPEG-TS on the way, even when nothing is being
//! re-encoded, because TS carries a fixed 90 kHz clock. Joining mp4 files
//! directly with a stream copy looks like it works and then silently produces a
//! file whose video runs out long before its audio, because each mp4 keeps its
//! own timebase and the copy does not rescale between them.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use crate::encoder::{self, EncoderChoice, EncoderPref, Quality};
use crate::ffmpeg::Tools;
use crate::plan::{self, Target, TargetOverride};
use crate::probe::{self, ClipInfo};
use crate::proc;

const TEMP_DIR_NAME: &str = "_merge_temp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Remuxed, not re-encoded: no quality loss, limited only by disk speed.
    Copy,
    Convert,
}

impl Step {
    pub fn verb(self) -> &'static str {
        match self {
            Step::Copy => "copy",
            Step::Convert => "convert",
        }
    }

    pub fn past(self) -> &'static str {
        match self {
            Step::Copy => "copied",
            Step::Convert => "converted",
        }
    }
}

/// Progress reports from the worker thread to whatever is drawing.
#[derive(Debug, Clone)]
pub enum MergeEvent {
    /// A line for the plan summary, decided once the target is known.
    Plan(String),
    /// A pass over the clips is starting; `attempt` > 1 means a fallback.
    Pass { total: usize, attempt: u32 },
    SegmentStart { index: usize, name: String, step: Step, duration: f64 },
    SegmentProgress { index: usize, done: f64 },
    SegmentEnd { index: usize, step: Step, ok: bool, elapsed: f64 },
    JoinStart,
    JoinProgress { done: f64, total: f64 },
    Warning(String),
    Finished(Box<Outcome>),
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub ok: bool,
    /// The file to show the user, and the one Explorer is pointed at. A merge and
    /// a download write one file each; a conversion writes one per input, and
    /// this is the first of them.
    pub output: PathBuf,
    /// Every file the job wrote. One entry for a merge or a download, one per
    /// input for a conversion - which is what the report reads to know whether it
    /// is describing a file or a batch.
    pub outputs: Vec<PathBuf>,
    pub size: u64,
    pub out_duration: f64,
    pub out_format: Option<(u32, u32, f64)>,
    pub elapsed: f64,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    pub cancelled: bool,
    /// The file is a live broadcast captured as it happened, rather than a
    /// finished video fetched or clips joined. Stopping one of these on
    /// purpose is how it is *meant* to end, so the report has to be able to
    /// tell that apart from a download the user gave up on.
    pub recorded: bool,
}

pub struct Job {
    pub tools: Arc<Tools>,
    pub clips: Vec<ClipInfo>,
    pub output: PathBuf,
    pub quality: Quality,
    pub encoder: EncoderPref,
    pub force_reencode: bool,
    pub target_override: Option<TargetOverride>,
}

/// Runs the merge on a worker thread so the UI keeps drawing. Every progress
/// report arrives through `tx`, wrapped by `wrap` into the caller's event type.
pub fn spawn<T: Send + 'static>(
    job: Job,
    cancel: Arc<AtomicBool>,
    tx: Sender<T>,
    wrap: fn(MergeEvent) -> T,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut emit = |event: MergeEvent| {
            let _ = tx.send(wrap(event));
        };
        let outcome = run(&job, &cancel, &mut emit);
        emit(MergeEvent::Finished(Box::new(outcome)));
    })
}

/// Do these two paths name the same file? Canonicalising catches the same file
/// reached by different routes (`.\a.mp4`, a mapped drive, a symlink); the
/// fallback compares text, case-insensitively, because Windows paths are.
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        // b usually does not exist yet, which is the normal case.
        _ => {
            let text = |p: &Path| p.display().to_string().replace('/', "\\").to_lowercase();
            text(&absolute(a)) == text(&absolute(b))
        }
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Picks a strategy, runs it, cleans up.
pub fn run(job: &Job, cancel: &AtomicBool, emit: &mut dyn FnMut(MergeEvent)) -> Outcome {
    let started = Instant::now();
    let output = absolute(&job.output);
    let expected_duration: f64 = job.clips.iter().map(|c| c.duration).sum();

    let mut outcome = Outcome {
        ok: false,
        output: output.clone(),
        outputs: Vec::new(),
        size: 0,
        out_duration: 0.0,
        out_format: None,
        elapsed: 0.0,
        warnings: Vec::new(),
        error: None,
        cancelled: false,
        recorded: false,
    };

    if job.clips.is_empty() {
        outcome.error = Some("There are no clips to merge.".into());
        return outcome;
    }

    // A file with sound and no picture is a perfectly good input for a conversion
    // and no use at all here: every strategy below maps a video stream, so it
    // would fail one clip at a time with an ffmpeg error rather than saying what
    // is actually wrong.
    if let Some(soundtrack) = job.clips.iter().find(|c| !c.has_video) {
        outcome.error = Some(format!(
            "{} has no picture in it, so there is nothing to join. \
             Remove it, or convert it instead.",
            soundtrack.name
        ));
        return outcome;
    }

    // Writing the output over one of its own inputs would have ffmpeg reading a
    // file it is in the middle of replacing, destroying the source and the
    // result together. Refuse before anything is touched.
    if let Some(clash) = job.clips.iter().find(|c| same_file(&c.path, &output)) {
        outcome.error = Some(format!(
            "The output would overwrite {}, which is one of the clips being merged. \
             Pick a different name.",
            clash.name
        ));
        return outcome;
    }

    let work_root = output.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    // The process id keeps two merges running in the same folder from
    // overwriting each other's segments half way through.
    let temp_dir = work_root.join(format!("{TEMP_DIR_NAME}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    if let Err(e) = fs::create_dir_all(&temp_dir) {
        outcome.error = Some(format!("Could not create a working folder: {e}"));
        return outcome;
    }
    proc::set_hidden(&temp_dir);

    let result = strategy(job, &output, &temp_dir, cancel, emit, &mut outcome.warnings);

    let _ = fs::remove_dir_all(&temp_dir);

    outcome.elapsed = started.elapsed().as_secs_f64();
    outcome.cancelled = cancel.load(Ordering::Relaxed);

    match result {
        Ok(()) if output.is_file() => {
            outcome.ok = true;
            outcome.outputs = vec![output.clone()];
            outcome.size = fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
            if let Some(info) = probe::clip_info(&job.tools.ffprobe, &output) {
                outcome.out_duration = info.duration;
                outcome.out_format = Some((info.width, info.height, info.fps));
                // Inputs adding up to more than the output means clips were
                // dropped or a join went wrong - worth saying out loud.
                let slack = (expected_duration * 0.02).max(2.0);
                if expected_duration > 0.0 && (expected_duration - info.duration).abs() > slack {
                    outcome.warnings.push(format!(
                        "Inputs add up to {} but the output is {}.",
                        crate::format::duration(expected_duration),
                        crate::format::duration(info.duration)
                    ));
                }
            }
        }
        Ok(()) => {
            outcome.error = Some("ffmpeg reported success but wrote nothing usable.".into());
            let _ = fs::remove_file(&output);
        }
        Err(e) => {
            outcome.error = Some(e);
            // A half-written file is worse than none: it looks playable and is not.
            let _ = fs::remove_file(&output);
        }
    }

    outcome
}

/// Records a warning and shows it straight away: a fallback that takes minutes
/// should not be a surprise revealed only in the final summary.
fn note(text: String, emit: &mut dyn FnMut(MergeEvent), warnings: &mut Vec<String>) {
    emit(MergeEvent::Warning(text.clone()));
    warnings.push(text);
}

fn strategy(
    job: &Job,
    output: &Path,
    temp_dir: &Path,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(MergeEvent),
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let clips = &job.clips;

    // A single clip needs no join at all, just a container change.
    if clips.len() == 1 {
        emit(MergeEvent::Plan("One clip - copying it into an mp4.".into()));
        emit(MergeEvent::Pass { total: 1, attempt: 1 });
        emit(MergeEvent::SegmentStart {
            index: 0,
            name: clips[0].name.clone(),
            step: Step::Copy,
            duration: clips[0].duration,
        });
        let started = Instant::now();
        let args = vec![
            "-i".to_string(),
            clips[0].path.display().to_string(),
            "-c".into(),
            "copy".into(),
            "-movflags".into(),
            "+faststart".into(),
            "--".into(),
            output.display().to_string(),
        ];
        let total = clips[0].duration;
        let result = run_ffmpeg(&job.tools.ffmpeg, &args, cancel, &mut |done| {
            emit(MergeEvent::SegmentProgress { index: 0, done: done.min(total) });
        });
        emit(MergeEvent::SegmentEnd {
            index: 0,
            step: Step::Copy,
            ok: result.is_ok(),
            elapsed: started.elapsed().as_secs_f64(),
        });
        match result {
            Ok(()) => return Ok(()),
            Err(e) if cancel.load(Ordering::Relaxed) => return Err(e),
            Err(_) => {
                note("Direct copy failed; re-encoding instead.".into(), emit, warnings);
                let target = plan::target_format(clips, job.target_override);
                let enc = encoder::select(&job.tools.ffmpeg, job.encoder);
                return segmented(job, &target, &enc, true, output, temp_dir, cancel, emit, warnings, 2);
            }
        }
    }

    // Which pass over the clips this is. A failed fast join means the next
    // attempt starts the rows over, rather than inheriting their finished state.
    let mut pass = 1;

    // Every clip already shares one format: nothing needs the encoder.
    if !job.force_reencode && plan::can_stream_copy(clips) {
        let target = plan::pass_through_target(&clips[0]);
        emit(MergeEvent::Plan(format!(
            "All {} clips are already {} {} - joining without re-encoding.",
            clips.len(),
            target.label(),
            clips[0].video_codec
        )));
        match copy_merge(job, output, temp_dir, cancel, emit) {
            Ok(()) => return Ok(()),
            Err(e) if cancel.load(Ordering::Relaxed) => return Err(e),
            Err(e) => {
                note(format!("The fast join failed ({e}). Re-encoding instead."), emit, warnings);
                pass = 2;
            }
        }
    }

    // Mixed formats: convert only what has to be converted.
    let target = plan::target_format(clips, job.target_override);
    let to_convert = plan::convert_count(clips, &target);
    let enc = if to_convert > 0 || job.force_reencode {
        // Probing a GPU encoder means test-encoding with each candidate, which
        // takes a moment. Say so, or the screen looks stalled.
        if job.encoder == EncoderPref::Auto {
            emit(MergeEvent::Plan("Checking which encoder works here...".into()));
        }
        encoder::select(&job.tools.ffmpeg, job.encoder)
    } else {
        EncoderChoice { name: "libx264".into(), label: "CPU (libx264)".into() }
    };

    emit(MergeEvent::Plan(format!(
        "Common format: {}, H.264 + AAC {} {} Hz",
        target.label(),
        target.channel_layout(),
        target.sample_rate
    )));
    if to_convert == 0 && !job.force_reencode {
        emit(MergeEvent::Plan("Every clip already matches - joining without re-encoding.".into()));
    } else if to_convert < clips.len() && !job.force_reencode {
        emit(MergeEvent::Plan(format!(
            "{} of {} clips are copied as they are; the other {} {} converted with {}, quality {}.",
            clips.len() - to_convert,
            clips.len(),
            to_convert,
            if to_convert == 1 { "gets" } else { "get" },
            enc.label,
            job.quality.label()
        )));
    } else {
        emit(MergeEvent::Plan(format!(
            "Encoder: {}, quality: {}",
            enc.label,
            job.quality.label()
        )));
    }

    let force_all = job.force_reencode;
    match segmented(job, &target, &enc, force_all, output, temp_dir, cancel, emit, warnings, pass) {
        Ok(()) => Ok(()),
        Err(e) if cancel.load(Ordering::Relaxed) => Err(e),
        Err(e) if force_all => Err(e),
        Err(e) => {
            // Last resort: stop trusting the "this clip already matches" test
            // and put every clip through the encoder.
            note(format!("Mixed copy/convert failed ({e}). Re-encoding everything."), emit, warnings);
            segmented(job, &target, &enc, true, output, temp_dir, cancel, emit, warnings, pass + 1)
        }
    }
}

/// Every clip is already identical to every other, so each one is remuxed into
/// a TS segment untouched and the set is joined. No pixels are read or written.
fn copy_merge(
    job: &Job,
    output: &Path,
    temp_dir: &Path,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(MergeEvent),
) -> Result<(), String> {
    let clips = &job.clips;
    emit(MergeEvent::Pass { total: clips.len(), attempt: 1 });

    let mut parts = Vec::with_capacity(clips.len());
    for (index, clip) in clips.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled.".into());
        }
        emit(MergeEvent::SegmentStart {
            index,
            name: clip.name.clone(),
            step: Step::Copy,
            duration: clip.duration,
        });
        let started = Instant::now();
        let part = temp_dir.join(format!("part_{:04}.ts", index + 1));
        let result = remux_to_ts(job, clip, &part, cancel, &mut |done| {
            emit(MergeEvent::SegmentProgress { index, done });
        });
        emit(MergeEvent::SegmentEnd {
            index,
            step: Step::Copy,
            ok: result.is_ok(),
            elapsed: started.elapsed().as_secs_f64(),
        });
        // On this path a failure is not survivable: dropping a clip from a set
        // the user expects joined byte-for-byte would be silent data loss.
        // Bail out and let the caller fall back to a real conversion.
        result?;
        parts.push(part);
    }

    let total: f64 = clips.iter().map(|c| c.duration).sum();
    let has_audio = clips[0].has_audio;
    emit(MergeEvent::JoinStart);
    join_segments(job, &parts, output, temp_dir, has_audio, cancel, &mut |done| {
        emit(MergeEvent::JoinProgress { done, total });
    })
}

/// One target format; each clip is copied if it already matches and converted
/// only if it does not. `force_all` converts everything regardless.
#[allow(clippy::too_many_arguments)]
fn segmented(
    job: &Job,
    target: &Target,
    enc: &EncoderChoice,
    force_all: bool,
    output: &Path,
    temp_dir: &Path,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(MergeEvent),
    warnings: &mut Vec<String>,
    attempt: u32,
) -> Result<(), String> {
    let clips = &job.clips;
    emit(MergeEvent::Pass { total: clips.len(), attempt });

    let mut parts = Vec::with_capacity(clips.len());
    let mut skipped = 0usize;
    let mut prepared_duration = 0.0f64;
    let mut any_audio = false;

    for (index, clip) in clips.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled.".into());
        }
        let copying = !force_all && plan::clip_matches_target(clip, target);
        let step = if copying { Step::Copy } else { Step::Convert };
        emit(MergeEvent::SegmentStart {
            index,
            name: clip.name.clone(),
            step,
            duration: clip.duration,
        });

        let started = Instant::now();
        let part = temp_dir.join(format!("part_{:04}.ts", index + 1));
        let mut report = |done: f64| emit(MergeEvent::SegmentProgress { index, done });

        let mut result = if copying {
            remux_to_ts(job, clip, &part, cancel, &mut report)
        } else {
            convert_to_ts(job, clip, target, enc, &part, cancel, &mut report)
        };

        // Remuxing should not fail, but if it does, encoding is still an option.
        if result.is_err() && copying && !cancel.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&part);
            result = convert_to_ts(job, clip, target, enc, &part, cancel, &mut report);
        }

        let ok = result.is_ok();
        emit(MergeEvent::SegmentEnd {
            index,
            step,
            ok,
            elapsed: started.elapsed().as_secs_f64(),
        });

        match result {
            Ok(()) => {
                prepared_duration += clip.duration;
                any_audio = true; // every segment carries audio by construction
                parts.push(part);
            }
            Err(e) if cancel.load(Ordering::Relaxed) => return Err(e),
            Err(e) => {
                note(format!("Could not prepare {} - leaving it out ({e}).", clip.name), emit, warnings);
                skipped += 1;
            }
        }
    }

    if parts.is_empty() {
        return Err("None of the clips could be prepared.".into());
    }
    if skipped > 0 {
        note(format!("{skipped} of {} clips were left out because of errors.", clips.len()), emit, warnings);
    }

    emit(MergeEvent::JoinStart);
    join_segments(job, &parts, output, temp_dir, any_audio, cancel, &mut |done| {
        emit(MergeEvent::JoinProgress { done, total: prepared_duration });
    })
}

/// Reads and rewrites a clip into a TS segment: no encoding, no quality loss.
fn remux_to_ts(
    job: &Job,
    clip: &ClipInfo,
    part: &Path,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(f64),
) -> Result<(), String> {
    let mut args = vec!["-i".to_string(), clip.path.display().to_string(), "-map".into(), "0:v:0".into()];
    if clip.has_audio {
        args.extend(["-map".to_string(), "0:a:0".to_string()]);
    }
    args.extend([
        "-c".to_string(),
        "copy".into(),
        "-f".into(),
        "mpegts".into(),
        "--".into(),
        part.display().to_string(),
    ]);
    run_ffmpeg(&job.tools.ffmpeg, &args, cancel, on_progress)
}

/// Encodes a clip so it lands exactly on the target format.
fn convert_to_ts(
    job: &Job,
    clip: &ClipInfo,
    target: &Target,
    enc: &EncoderChoice,
    part: &Path,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(f64),
) -> Result<(), String> {
    let mut args: Vec<String> = Vec::new();

    if clip.has_audio {
        args.extend(["-i".to_string(), clip.path.display().to_string()]);
        args.extend(["-map".to_string(), "0:v:0".into(), "-map".into(), "0:a:0".into()]);
    } else {
        // Silent clips still need an audio track, or the join desyncs.
        args.extend([
            "-f".to_string(),
            "lavfi".into(),
            "-i".into(),
            format!(
                "anullsrc=channel_layout={}:sample_rate={}",
                target.channel_layout(),
                target.sample_rate
            ),
        ]);
        args.extend(["-i".to_string(), clip.path.display().to_string()]);
        args.extend([
            "-map".to_string(),
            "1:v:0".into(),
            "-map".into(),
            "0:a:0".into(),
            "-shortest".into(),
        ]);
    }

    args.extend(["-vf".to_string(), target.video_filter()]);
    args.extend(["-c:v".to_string(), enc.name.clone()]);
    args.extend(encoder::quality_args(&enc.name, job.quality));
    args.extend([
        "-c:a".to_string(),
        "aac".into(),
        "-b:a".into(),
        "192k".into(),
        "-ar".into(),
        target.sample_rate.to_string(),
        "-ac".into(),
        target.channels.to_string(),
    ]);
    args.extend([
        "-video_track_timescale".to_string(),
        "90000".into(),
        "-f".into(),
        "mpegts".into(),
        "--".into(),
        part.display().to_string(),
    ]);

    run_ffmpeg(&job.tools.ffmpeg, &args, cancel, on_progress)
}

/// Joins the prepared segments into the final mp4. Nothing is re-encoded here;
/// every segment already carries the same format and the same 90 kHz clock.
fn join_segments(
    job: &Job,
    parts: &[PathBuf],
    output: &Path,
    temp_dir: &Path,
    has_audio: bool,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(f64),
) -> Result<(), String> {
    let list_path = temp_dir.join("concat.txt");
    write_concat_list(parts, &list_path).map_err(|e| format!("writing the join list: {e}"))?;

    let mut args = vec![
        "-f".to_string(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list_path.display().to_string(),
        "-c".into(),
        "copy".into(),
    ];
    if has_audio {
        // Turns the ADTS headers the TS segments carry back into the plain
        // AAC an mp4 expects. With no audio stream it would be an error.
        args.extend(["-bsf:a".to_string(), "aac_adtstoasc".into()]);
    }
    args.extend([
        "-movflags".to_string(),
        "+faststart".into(),
        "-fflags".into(),
        "+genpts".into(),
        "--".into(),
        output.display().to_string(),
    ]);

    run_ffmpeg(&job.tools.ffmpeg, &args, cancel, on_progress)
}

/// The concat demuxer's list format: forward slashes, single quotes escaped,
/// and UTF-8 with no BOM - a BOM makes ffmpeg reject the first entry.
fn write_concat_list(parts: &[PathBuf], list_path: &Path) -> std::io::Result<()> {
    let mut text = String::new();
    for part in parts {
        let escaped = part.display().to_string().replace('\\', "/").replace('\'', "'\\''");
        text.push_str(&format!("file '{escaped}'\n"));
    }
    fs::write(list_path, text.as_bytes())
}

/// Runs ffmpeg, turning its progress stream into callbacks.
///
/// `-progress pipe:1` writes machine-readable key=value lines to stdout, which
/// is what makes a real progress bar possible; `-nostats` silences the human
/// version. stderr is captured so the tail can be reported if ffmpeg gives up.
pub(crate) fn run_ffmpeg(
    ffmpeg: &Path,
    args: &[String],
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(f64),
) -> Result<(), String> {
    let mut command = proc::command(ffmpeg);
    command.args([
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-progress",
        "pipe:1",
        "-nostats",
        "-y",
    ]);
    command.args(args);

    let mut child = command.spawn().map_err(|e| format!("could not start ffmpeg: {e}"))?;

    let stderr = child.stderr.take();
    let drain = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut stderr) = stderr {
            use std::io::Read;
            let _ = stderr.read_to_end(&mut buffer);
        }
        buffer
    });

    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                break;
            }
            if let Some(seconds) = parse_progress_time(&line) {
                on_progress(seconds);
            }
        }
    }

    let status = child.wait().map_err(|e| format!("waiting for ffmpeg: {e}"))?;
    let stderr = drain.join().unwrap_or_default();

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled.".into());
    }
    if status.success() {
        return Ok(());
    }
    let tail = proc::error_tail(&stderr, 3);
    if tail.is_empty() {
        Err(format!("ffmpeg exited with {}", status.code().unwrap_or(-1)))
    } else {
        Err(tail)
    }
}

/// Pulls the elapsed output time out of one `-progress` line, in seconds.
///
/// ffmpeg's own `out_time_ms` key is microseconds despite the name; newer
/// builds also emit `out_time_us`. Both are read the same way.
fn parse_progress_time(line: &str) -> Option<f64> {
    let (key, value) = line.split_once('=')?;
    match key.trim() {
        "out_time_us" | "out_time_ms" => {
            let micros: f64 = value.trim().parse().ok()?;
            (micros >= 0.0).then_some(micros / 1_000_000.0)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore)]
    fn an_output_that_is_also_an_input_is_recognised() {
        let dir = std::env::temp_dir().join(format!("vmerge-same-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("a.mp4");
        fs::write(&clip, b"x").unwrap();

        assert!(same_file(&clip, &dir.join("a.mp4")));
        // Windows paths are case-insensitive, and so is the check.
        assert!(same_file(&clip, &dir.join("A.MP4")));
        assert!(same_file(&clip, &dir.join(".").join("a.mp4")));
        // A file that does not exist yet still compares by path.
        assert!(!same_file(&clip, &dir.join("merged.mp4")));
        assert!(!same_file(&clip, &dir.join("b.mp4")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_lines() {
        assert_eq!(parse_progress_time("out_time_us=2500000"), Some(2.5));
        assert_eq!(parse_progress_time("out_time_ms=1000000"), Some(1.0));
        assert_eq!(parse_progress_time("frame=42"), None);
        assert_eq!(parse_progress_time("out_time_us=N/A"), None);
        assert_eq!(parse_progress_time("progress=continue"), None);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn concat_list_escapes_quotes_and_slashes() {
        let dir = std::env::temp_dir().join(format!("vmerge-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let list = dir.join("concat.txt");
        write_concat_list(&[PathBuf::from(r"C:\a b\it's.ts")], &list).unwrap();
        let text = fs::read_to_string(&list).unwrap();
        assert_eq!(text, "file 'C:/a b/it'\\''s.ts'\n");
        assert!(!text.starts_with('\u{feff}'), "a BOM would break the demuxer");
        let _ = fs::remove_dir_all(&dir);
    }
}
