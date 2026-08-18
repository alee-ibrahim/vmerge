//! Downloading one video from a link, through yt-dlp.
//!
//! The same shape as merge.rs: a `Job` goes onto a worker thread, progress comes
//! back as events, and a cancel flag kills the child. What it produces is the
//! same `Outcome`, so the finished screen does not have to know which of the two
//! wrote the file.
//!
//! Two decisions are worth the words.
//!
//! **The download is machine-readable, not scraped.** yt-dlp draws a progress bar
//! for people, full of carriage returns and colour, and reading that back is
//! guesswork. `--progress-template` makes it emit exactly the fields wanted, one
//! line at a time, with a prefix nothing else uses - so a parser can be sure what
//! it is looking at. The same goes for where the file ended up: `--print
//! after_move:` is asked for the final path rather than the folder being searched
//! afterwards and the newest thing in it assumed to be ours.
//!
//! **The bytes land in a working folder first.** yt-dlp is told to keep partial
//! files under `temp:` and to move the finished one out to `home:`. A cancelled
//! or failed download therefore leaves its leftovers somewhere we own and can
//! delete wholesale, and the user's folder never contains a half-written video
//! that looks playable and is not. It is the same reason merge.rs works in
//! `_merge_temp` rather than in place.
//!
//! **A live broadcast is recorded, not downloaded.** A sitting that is still on
//! air has no last byte to wait for, so there is nothing for the usual path to
//! finish. yt-dlp hands live streams to ffmpeg and then reports nothing at all
//! about them - no progress hook of its own fires, which is why this used to sit
//! at "got 0 KB" for as long as anyone left it there. So a link is asked what it
//! is *before* anything is fetched, and a live one takes a different route: this
//! program runs ffmpeg itself, and esc means "that is enough, keep it" rather
//! than "throw it away".
//!
//! What makes that safe is the container, not politeness. ffmpeg will close a
//! file tidily if it is sent a `q`, but only when its stdin is a console - given
//! a pipe it never reads the keystroke, which was measured here rather than
//! assumed: a `q` sent to a piped stdin was still being ignored a minute later.
//! So a recording is stopped by ending the process, and it is written to an
//! MPEG-TS stream precisely because that survives being ended. Every packet is
//! flushed as it is written, so what is on disk is playable at every instant and
//! stopping costs at most the packet in flight.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use serde::Deserialize;

use crate::ffmpeg::{Reporter, Tools};
use crate::merge::Outcome;
use crate::probe;
use crate::proc;
use crate::ytdlp;

const TEMP_DIR_NAME: &str = "_download_temp";

/// The prefix on every progress line, chosen so nothing yt-dlp says by accident
/// can be mistaken for one.
const PROGRESS_TAG: &str = "VMPROG";

/// Where yt-dlp is asked to write the finished file's path.
///
/// A file rather than stdout, and that is not fussiness. yt-dlp re-encodes
/// whatever it prints to the console into something the console can render, and
/// on Windows that silently drops characters: a title containing an emoji comes
/// back through stdout with the emoji simply missing. The path it names is then
/// one no file has, and a download that worked perfectly is reported as having
/// vanished. `--print-to-file` writes UTF-8 and changes nothing.
const PATH_RECORD: &str = "finished-path.txt";

/// How many times a refused download is attempted before giving up.
///
/// YouTube answers `403 Forbidden` for links that work perfectly well a moment
/// later. Measured here on one video: the same request failed, then succeeded
/// six times in a row, with the format selection identical throughout. yt-dlp's
/// own `--retries` re-requests the *same* URL, which is no use when that URL is
/// the thing being refused - running the extraction again is what produces
/// fresh ones.
const ATTEMPTS: u32 = 3;

/// What a live recording is written to before it is put into its final
/// container.
///
/// MPEG-TS rather than mp4, and not for tradition. An mp4 only becomes readable
/// when its index is written at the very end, so a recording that ends in any
/// way other than politely - a crash, a power cut, a task manager - would be a
/// file of the right size that nothing can open. A transport stream has no index
/// to miss: it is playable at every instant, and cutting it anywhere leaves the
/// part before the cut intact. The remux afterwards costs a copy of the bytes
/// and no quality at all.
const RECORDING_NAME: &str = "recording.ts";


/// Which stream to take. Not a bitrate or a codec: the point of a picker in a
/// program like this is that "720p" is a thing people already know the meaning
/// of, and every one of these maps to a yt-dlp selector that cannot fail to
/// parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FetchQuality {
    /// Whatever the site has, at the highest resolution.
    Best,
    #[value(name = "1080p")]
    P1080,
    #[value(name = "720p")]
    P720,
    #[value(name = "480p")]
    P480,
    /// No video at all, just the sound.
    Audio,
}

impl FetchQuality {
    pub const ALL: [FetchQuality; 5] = [
        FetchQuality::Best,
        FetchQuality::P1080,
        FetchQuality::P720,
        FetchQuality::P480,
        FetchQuality::Audio,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FetchQuality::Best => "best available",
            FetchQuality::P1080 => "1080p",
            FetchQuality::P720 => "720p",
            FetchQuality::P480 => "480p",
            FetchQuality::Audio => "audio only",
        }
    }

    /// What choosing this costs, for the picker.
    pub fn note(self) -> &'static str {
        match self {
            FetchQuality::Best => "biggest file, slowest download",
            FetchQuality::P1080 => "the sensible default",
            FetchQuality::P720 => "smaller, still sharp",
            FetchQuality::P480 => "smallest, quickest",
            FetchQuality::Audio => "an .m4a, no picture",
        }
    }

    /// The `-f` selector. A height cap is a hard filter rather than a
    /// preference: someone who picks 720p wants 720p, not "720p unless the site
    /// happens to offer something bigger".
    fn selector(self) -> &'static str {
        match self {
            FetchQuality::Best => "bv*+ba/b",
            FetchQuality::P1080 => "bv*[height<=1080]+ba/b[height<=1080]",
            FetchQuality::P720 => "bv*[height<=720]+ba/b[height<=720]",
            FetchQuality::P480 => "bv*[height<=480]+ba/b[height<=480]",
            FetchQuality::Audio => "ba/b",
        }
    }

    pub fn is_audio(self) -> bool {
        matches!(self, FetchQuality::Audio)
    }
}

/// Which part of the job is running. Downloads have a setup phase that can be an
/// 18 MB transfer of its own, and a tail after the last byte where ffmpeg is
/// joining the video and audio streams - both look like a hang unless they say
/// what they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Finding or installing yt-dlp.
    Setup,
    /// Asking the link what it is, before a byte of it is fetched.
    Asking,
    Downloading,
    /// A broadcast that is still on air, being captured as it happens. The one
    /// stage with no end of its own: it lasts until the sitting finishes or the
    /// user stops it.
    Recording,
    /// Every byte is in; ffmpeg is putting the streams together.
    Finishing,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Setup => "getting ready",
            Stage::Asking => "checking the link",
            Stage::Downloading => "downloading",
            Stage::Recording => "recording",
            Stage::Finishing => "finishing off",
        }
    }
}

/// Progress reports from the worker thread to whatever is drawing.
#[derive(Debug, Clone)]
pub enum FetchEvent {
    /// A line for the running notes.
    Note(String),
    /// The video's own name, which only becomes known once the site answers.
    Title(String),
    Stage(Stage),
    /// A separate stream has started. Video and audio arrive one after the other
    /// when the best copy of each is in a different file, so the bar restarts.
    Stream(u32),
    Progress { done: u64, total: Option<u64>, rate: f64, eta: Option<f64> },
    /// How much of a live broadcast is in the file so far, in seconds of
    /// running time. The only honest measure of a recording's progress: there
    /// is no total to be a fraction of, but "37 minutes captured" is a fact.
    Captured(f64),
    Finished(Box<Outcome>),
}

pub struct Job {
    pub tools: Arc<Tools>,
    pub url: String,
    /// Where the finished file goes.
    pub folder: PathBuf,
    pub quality: FetchQuality,
    /// Where yt-dlp is installed if it has to be fetched.
    pub install_root: PathBuf,
    /// Where an existing copy is looked for first.
    pub search: Vec<PathBuf>,
    pub allow_download: bool,
}

/// Runs the download on a worker thread so the UI keeps drawing. Every progress
/// report arrives through `tx`, wrapped by `wrap` into the caller's event type.
pub fn spawn<T: Send + 'static>(
    job: Job,
    cancel: Arc<AtomicBool>,
    tx: Sender<T>,
    wrap: fn(FetchEvent) -> T,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut emit = |event: FetchEvent| {
            let _ = tx.send(wrap(event));
        };
        let outcome = run(&job, &cancel, &mut emit);
        emit(FetchEvent::Finished(Box::new(outcome)));
    })
}

/// Turns the setup reporter's calls into the same events everything else here
/// sends, so installing yt-dlp draws the same bar as downloading a video.
struct SetupProgress<'a> {
    emit: &'a mut dyn FnMut(FetchEvent),
    started: Instant,
}

impl Reporter for SetupProgress<'_> {
    fn log(&mut self, line: &str) {
        (self.emit)(FetchEvent::Note(line.to_string()));
    }

    fn progress(&mut self, received: u64, total: Option<u64>) {
        // The clock starts when the bytes do, or the rate folds in however long
        // the release listing took to answer and reads far too low.
        if received == 0 {
            self.started = Instant::now();
        }
        let elapsed = self.started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.2 { received as f64 / elapsed } else { 0.0 };
        let eta = match total {
            Some(total) if rate > 0.0 && received < total => {
                Some((total - received) as f64 / rate)
            }
            _ => None,
        };
        (self.emit)(FetchEvent::Progress { done: received, total, rate, eta });
    }

    fn finished(&mut self) {}
}

/// Finds yt-dlp, downloads the video, and reports what landed.
pub fn run(job: &Job, cancel: &AtomicBool, emit: &mut dyn FnMut(FetchEvent)) -> Outcome {
    let started = Instant::now();
    let mut outcome = Outcome {
        ok: false,
        output: job.folder.clone(),
        size: 0,
        out_duration: 0.0,
        out_format: None,
        elapsed: 0.0,
        warnings: Vec::new(),
        error: None,
        cancelled: false,
        recorded: false,
    };

    let finish = |mut outcome: Outcome, cancel: &AtomicBool| {
        outcome.elapsed = started.elapsed().as_secs_f64();
        outcome.cancelled = cancel.load(Ordering::Relaxed);
        outcome
    };

    if !job.folder.is_dir() {
        outcome.error = Some(format!("There is no folder called {}", job.folder.display()));
        return finish(outcome, cancel);
    }

    emit(FetchEvent::Stage(Stage::Setup));
    let tool = {
        let mut reporter = SetupProgress { emit, started: Instant::now() };
        match ytdlp::resolve(
            &job.install_root,
            &job.search,
            job.allow_download,
            &mut reporter,
        ) {
            Ok(tool) => tool,
            Err(e) => {
                outcome.error = Some(format!("{e:#}"));
                return finish(outcome, cancel);
            }
        }
    };
    if cancel.load(Ordering::Relaxed) {
        outcome.error = Some("Cancelled.".into());
        return finish(outcome, cancel);
    }
    {
        let mut reporter = SetupProgress { emit, started: Instant::now() };
        ytdlp::refresh_if_stale(&tool, &mut reporter);
    }

    // The process id keeps two downloads running in the same folder from
    // treading on each other's partial files.
    let temp_dir = job.folder.join(format!("{TEMP_DIR_NAME}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    if let Err(e) = fs::create_dir_all(&temp_dir) {
        outcome.error = Some(format!("Could not create a working folder: {e}"));
        return finish(outcome, cancel);
    }
    proc::set_hidden(&temp_dir);

    emit(FetchEvent::Stage(Stage::Asking));
    emit(FetchEvent::Progress { done: 0, total: None, rate: 0.0, eta: None });

    let mut live = Live::No;
    let mut result = attempt(&tool.path, job, &temp_dir, cancel, emit, &mut live);
    let mut attempts = 1;
    while attempts < ATTEMPTS
        && !cancel.load(Ordering::Relaxed)
        && result.as_ref().err().is_some_and(|e| worth_retrying(e))
    {
        attempts += 1;
        emit(FetchEvent::Note(format!(
            "The site refused that one. Asking again for fresh links ({attempts} of {ATTEMPTS})."
        )));
        emit(FetchEvent::Stage(Stage::Asking));
        result = attempt(&tool.path, job, &temp_dir, cancel, emit, &mut live);
    }

    // Whatever happened, the working folder goes: on success it is empty, and on
    // failure it holds a part-file that is no use to anyone.
    let _ = fs::remove_dir_all(&temp_dir);

    let file = match result {
        Ok(file) => file,
        Err(e) => {
            // Explained once, at the end, rather than per attempt: the sentence
            // added below counts the attempts, so it has to know they are over.
            outcome.error = Some(explain(&e));
            return finish(outcome, cancel);
        }
    };

    let Some(file) = file.filter(|f| f.is_file()) else {
        outcome.error =
            Some("yt-dlp finished without leaving a file behind. Nothing was written.".into());
        return finish(outcome, cancel);
    };

    outcome.recorded = live == Live::Now;
    outcome.ok = true;
    outcome.output = file.clone();
    outcome.size = fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    match probe::clip_info(&job.tools.ffprobe, &file) {
        Some(info) => {
            outcome.out_duration = info.duration;
            if info.width > 0 && info.height > 0 {
                outcome.out_format = Some((info.width, info.height, info.fps));
            }
        }
        // An audio-only download has no video stream, so it is not a clip and
        // has no shape to report - but it does have a length, and saying
        // "unknown" for a file we just wrote would be a poor showing.
        None => outcome.out_duration = probe::duration_of(&job.tools.ffprobe, &file),
    }
    finish(outcome, cancel)
}

/// One go at getting the video: ask the link what it is, then either fetch it or
/// sit and record it.
///
/// Both halves are inside the retry, and deliberately. The thing being retried
/// is a site refusing a request it will honour a moment later, and the asking
/// pass makes exactly the same kind of request as the fetching one - so a retry
/// that skipped it would keep handing the recorder the stale URLs of the attempt
/// that had just been refused.
///
/// `live` is written back rather than returned so that the caller still knows
/// what it was dealing with when the attempt failed, which is what lets a
/// stopped recording be reported as a recording rather than as a lost download.
fn attempt(
    ytdlp: &Path,
    job: &Job,
    temp_dir: &Path,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(FetchEvent),
    live: &mut Live,
) -> Result<Option<PathBuf>, String> {
    let source = ask(ytdlp, job, cancel)?;
    *live = source.live;
    if let Some(title) = &source.title {
        emit(FetchEvent::Title(title.clone()));
    }

    match source.live {
        Live::Upcoming => Err("That broadcast has not started yet. There is nothing to \
                               record until it does."
            .into()),
        Live::Now => {
            emit(FetchEvent::Note(
                "This is live. It will keep recording until the broadcast ends or you stop it."
                    .into(),
            ));
            record(job, &source, temp_dir, cancel, emit)
        }
        Live::No => {
            emit(FetchEvent::Stage(Stage::Downloading));
            download(ytdlp, job, temp_dir, cancel, source.title.as_deref(), emit)
        }
    }
}

/// How streams are ranked once the format filter has had its say.
///
/// Resolution first, or this would happily prefer a 360p H.264 stream over a
/// 1080p VP9 one. Within a resolution, H.264 in an mp4 is what the merge side of
/// this program joins without re-encoding anything.
const SORT: &str = "res,vcodec:h264,acodec:aac,ext:mp4:m4a";

/// The arguments that decide *which* stream is wanted.
///
/// Shared by the pass that asks a link what it is and the pass that fetches it,
/// so the format the first one reports on is the format the second one gets.
/// Anything that reads a stream URL out of the probe - which is exactly what
/// recording a live broadcast does - depends on those two agreeing.
fn selection_args(job: &Job) -> Vec<String> {
    let mut args: Vec<String> = vec![
        // A stray yt-dlp.conf could otherwise redirect the output, pick another
        // format, or turn the progress template off, and none of that would be
        // visible from here.
        "--ignore-config".into(),
        // Playlists are not what this offers. A link that carries one alongside
        // a video gets the video; a bare playlist gets its first entry.
        "--no-playlist".into(),
        "--playlist-items".into(),
        "1".into(),
        "-f".into(),
        job.quality.selector().into(),
    ];
    if !job.quality.is_audio() {
        args.extend(["-S".to_string(), SORT.into()]);
    }

    // YouTube hands out video links behind a JavaScript challenge, and yt-dlp
    // has to run an engine to answer it. It enables deno by itself and nothing
    // else, so a machine with Node installed - which is most of them - is
    // treated as having no engine at all, and the download dies with
    // `HTTP Error 403: Forbidden`. Naming what is here costs one flag.
    for runtime in js_runtimes() {
        args.extend(["--js-runtimes".to_string(), runtime.to_string()]);
    }
    args
}

/// The arguments, kept apart from the running so they can be checked without a
/// network.
fn args(job: &Job, temp_dir: &Path, ffmpeg_dir: Option<&Path>) -> Vec<String> {
    let mut args = selection_args(job);
    args.extend([
        // --print implies --simulate, which would download nothing at all.
        "--no-simulate".into(),
        // --print also implies --quiet, so the progress has to be asked for
        // explicitly; --newline makes it whole lines instead of a redrawn bar.
        "--progress".into(),
        "--newline".into(),
        "--color".into(),
        "never".into(),
        "--retries".into(),
        "5".into(),
        "--fragment-retries".into(),
        "5".into(),
        // Windows rejects a handful of characters a video title may well
        // contain, and a 200-character title makes a path nothing can open.
        "--windows-filenames".into(),
        "-o".into(),
        "%(title).120B.%(ext)s".into(),
        "-P".into(),
        format!("temp:{}", temp_dir.display()),
        "-P".into(),
        format!("home:{}", job.folder.display()),
    ]);

    if job.quality.is_audio() {
        args.extend(["-x".to_string(), "--audio-format".into(), "m4a".into()]);
    } else {
        args.extend(["--merge-output-format".to_string(), "mp4".into()]);
    }

    if let Some(dir) = ffmpeg_dir {
        // The copy this program already installed, rather than whatever may or
        // may not be on PATH.
        args.extend(["--ffmpeg-location".to_string(), dir.display().to_string()]);
    }

    args.extend([
        "--progress-template".to_string(),
        format!(
            "download:{PROGRESS_TAG} %(progress.status)s %(progress.downloaded_bytes)s \
             %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s \
             %(progress.eta)s %(info.title)s"
        ),
        "--print-to-file".into(),
        "after_move:%(filepath)s".into(),
        record_path(temp_dir).display().to_string(),
        // Nothing after this is read as an option, so a URL beginning with a
        // dash cannot turn into one.
        "--".into(),
        job.url.clone(),
    ]);
    args
}

// ----------------------------------------------------------- asking the link

/// Whether a link is something to fetch or something to sit and record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Live {
    /// A finished video. Every byte of it already exists.
    No,
    /// On air now. There is no last byte, and there will not be one until
    /// whoever is broadcasting stops.
    Now,
    /// Scheduled, but not started. Nothing to record yet.
    Upcoming,
}

/// One stream ffmpeg is pointed at, with the headers the site expects.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stream {
    url: String,
    headers: Vec<(String, String)>,
}

/// What one extraction pass says about a link, before anything is fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    title: Option<String>,
    live: Live,
    /// Where the chosen streams actually are. Two of them when the best video
    /// and the best audio live in separate files, which ffmpeg then reads side
    /// by side. Only used for a recording; a finished video is left to yt-dlp,
    /// which does the whole job better than a bare URL would.
    streams: Vec<Stream>,
}

/// The handful of fields wanted out of yt-dlp's `-j` dump, which is otherwise
/// an enormous document describing every format the site offers.
#[derive(Debug, Deserialize)]
struct Info {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    live_status: Option<String>,
    /// Older extractors set only this. Read as a fallback so a site that has
    /// not caught up with `live_status` is not silently treated as finished.
    #[serde(default)]
    is_live: Option<bool>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    http_headers: Option<BTreeMap<String, String>>,
    /// Present when the choice was "best video plus best audio" and those turned
    /// out to be two different files.
    #[serde(default)]
    requested_formats: Option<Vec<InfoFormat>>,
}

#[derive(Debug, Deserialize)]
struct InfoFormat {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    http_headers: Option<BTreeMap<String, String>>,
}

/// The headers ffmpeg should send, out of the ones yt-dlp worked out.
///
/// `Accept-Encoding` is dropped on purpose: yt-dlp asks for an identity encoding
/// through its own machinery, and ffmpeg's HTTP client does not undo compression
/// it did not ask for. Passing one on can turn a perfectly good stream into a
/// file of gzip.
fn headers_from(map: Option<&BTreeMap<String, String>>) -> Vec<(String, String)> {
    map.into_iter()
        .flatten()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("Accept-Encoding"))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Reads what yt-dlp said about a link. Kept apart from running it so every rule
/// here can be tested without a network.
fn read_info(json: &str) -> Result<Source, String> {
    // `-j` writes one document per video on its own line, and warnings go to
    // stderr, so the first line that looks like an object is the answer.
    let line = json
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .ok_or_else(|| "yt-dlp said nothing about that link.".to_string())?;
    let info: Info =
        serde_json::from_str(line).map_err(|e| format!("could not read what yt-dlp said: {e}"))?;

    let live = match info.live_status.as_deref() {
        Some("is_live") => Live::Now,
        Some("is_upcoming") => Live::Upcoming,
        // `was_live` and `post_live` are both finished broadcasts: the first is
        // a published recording, the second one still being processed. Neither
        // needs recording, and both download the ordinary way.
        Some(_) => Live::No,
        None if info.is_live == Some(true) => Live::Now,
        None => Live::No,
    };

    let streams = match &info.requested_formats {
        Some(formats) => formats
            .iter()
            .filter_map(|f| {
                Some(Stream {
                    url: f.url.clone()?,
                    headers: headers_from(f.http_headers.as_ref()),
                })
            })
            .collect(),
        None => info
            .url
            .clone()
            .map(|url| vec![Stream { url, headers: headers_from(info.http_headers.as_ref()) }])
            .unwrap_or_default(),
    };

    Ok(Source {
        title: info.title.filter(|t| !t.trim().is_empty()),
        live,
        streams,
    })
}

/// Asks a link what it is, without fetching any of it.
///
/// This costs an extraction that the download is about to do again, and it buys
/// two things worth more than the seconds. A live broadcast has to be spotted
/// *before* anything starts, because recording one is a different job with a
/// different way of stopping - finding out afterwards is finding out too late.
/// And the title arrives here, so the screen names what it is working on from
/// the first moment rather than showing a raw URL until bytes start moving.
fn ask(ytdlp: &Path, job: &Job, cancel: &AtomicBool) -> Result<Source, String> {
    let mut command = proc::command(ytdlp);
    command.args(selection_args(job));
    // -j is a simulation: it selects a format and describes it, and downloads
    // none of it.
    command.args(["-j", "--no-warnings", "--", &job.url]);

    let mut child = command.spawn().map_err(|e| format!("could not start yt-dlp: {e}"))?;
    let group = proc::Group::around(&child);
    // Both pipes are drained on their own threads. A document describing every
    // format YouTube offers is far more than a pipe buffer holds, and a child
    // blocked writing one it cannot finish would never exit.
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());

    // Polled rather than waited on, because esc has to work while a slow site is
    // still thinking - not only once bytes are moving.
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            group.kill(&mut child);
            break child.wait().map_err(|e| format!("waiting for yt-dlp: {e}"))?;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("waiting for yt-dlp: {e}")),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled.".into());
    }
    if !status.success() {
        let tail = proc::error_tail(&stderr, 3);
        return Err(if tail.is_empty() {
            format!("yt-dlp exited with {}", status.code().unwrap_or(-1))
        } else {
            tail
        });
    }
    read_info(&String::from_utf8_lossy(&stdout))
}

// ------------------------------------------------------------ recording live

/// A title turned into something Windows will accept as a filename.
///
/// The same job `--windows-filenames` does for the download path, done here
/// because the recording path names its own output. The reserved characters
/// become underscores rather than vanishing, so two titles that differ only in
/// punctuation do not collapse into one name.
fn file_stem_for(title: &str) -> String {
    let mut stem = String::with_capacity(title.len());
    for c in title.chars() {
        match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => stem.push('_'),
            c if (c as u32) < 0x20 => stem.push('_'),
            c => stem.push(c),
        }
    }
    // Windows silently drops these from the end of a name, so a file would be
    // written under one name and then not be found under it.
    let stem = stem.trim().trim_end_matches('.').trim();

    // Long titles make paths nothing can open. The same 120 bytes the download
    // path asks for, cut on a character boundary so the name stays valid text.
    let mut cut = stem.len().min(120);
    while cut > 0 && !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    let stem = stem[..cut].trim_end();
    if stem.is_empty() { "recording".to_string() } else { stem.to_string() }
}

/// A path in `folder` that no file is using yet.
///
/// A sitting recorded twice in one day would otherwise overwrite the first
/// attempt, and the first attempt is the one with the beginning of it.
fn free_path(folder: &Path, stem: &str, extension: &str) -> PathBuf {
    let first = folder.join(format!("{stem}.{extension}"));
    if !first.exists() {
        return first;
    }
    for n in 2..1000 {
        let candidate = folder.join(format!("{stem} ({n}).{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

/// ffmpeg's `-headers` argument: one header per line, CRLF separated, the way
/// the wire wants them.
fn header_argument(headers: &[(String, String)]) -> String {
    headers.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect()
}

/// What ffmpeg is told in order to capture a broadcast as it happens.
///
/// Every stream is copied rather than encoded. A live capture that cannot keep
/// up is a live capture that loses the end of the sitting, and re-encoding 1080p
/// in real time is exactly how that happens. `-map` is spelled out because the
/// two-file case has the picture in one input and the sound in the other; the
/// trailing `?` on the single-input case is what lets an audio-only recording
/// ask for a video track that is not there without ffmpeg calling it an error.
fn record_args(streams: &[Stream], output: &Path) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    for stream in streams {
        let headers = header_argument(&stream.headers);
        if !headers.is_empty() {
            args.extend(["-headers".to_string(), headers]);
        }
        // A sitting runs for hours and a home connection does not stay up for
        // all of them. Without this, one dropped socket ends the recording;
        // with it, ffmpeg picks the broadcast back up where it left off.
        args.extend([
            "-reconnect".to_string(),
            "1".into(),
            "-reconnect_streamed".into(),
            "1".into(),
            "-reconnect_delay_max".into(),
            "30".into(),
            "-i".into(),
            stream.url.clone(),
        ]);
    }

    if streams.len() > 1 {
        args.extend(["-map".to_string(), "0:v:0".into(), "-map".into(), "1:a:0".into()]);
    } else {
        args.extend(["-map".to_string(), "0:v:0?".into(), "-map".into(), "0:a:0?".into()]);
    }
    args.extend([
        "-c".to_string(),
        "copy".into(),
        "-f".into(),
        "mpegts".into(),
        // Written straight through instead of gathering in a buffer first. A
        // recording ends when the process ends, and whatever was still in that
        // buffer at the time would be the last seconds of the sitting.
        "-flush_packets".into(),
        "1".into(),
        output.display().to_string(),
    ]);
    args
}

/// Records a broadcast that is still on air, until it ends or the user stops it.
///
/// The stopping is the interesting part, and it is why ffmpeg is this program's
/// own child rather than something yt-dlp started out of reach: a recording has
/// to be endable at a moment of the user's choosing, and endable *safely*. What
/// makes it safe is that it is being written as a transport stream, flushed
/// packet by packet - so ending the process costs the packet in flight and
/// nothing else, and what is on disk was already a playable recording of
/// everything up to that instant.
fn record(
    job: &Job,
    source: &Source,
    temp_dir: &Path,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(FetchEvent),
) -> Result<Option<PathBuf>, String> {
    if source.streams.is_empty() {
        return Err("The site did not say where the live stream is.".into());
    }

    let raw = temp_dir.join(RECORDING_NAME);
    let mut command = proc::command(&job.tools.ffmpeg);
    command.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
    command.args(["-progress", "pipe:1", "-nostats", "-y"]);
    command.args(record_args(&source.streams, &raw));

    let mut child = command.spawn().map_err(|e| format!("could not start ffmpeg: {e}"))?;
    let group = proc::Group::around(&child);
    let errors = drain(child.stderr.take());

    emit(FetchEvent::Stage(Stage::Recording));
    let started = Instant::now();
    let stopped = AtomicBool::new(false);
    let done = AtomicBool::new(false);

    // Stopping is watched for on a thread of its own, and that is not tidiness.
    // The obvious place to check is the loop below that reads ffmpeg's progress
    // - but that loop only runs when ffmpeg has something to say, and a stream
    // that has gone quiet is exactly when someone reaches for esc. Watching on a
    // clock instead means stopping never depends on the broadcast still
    // arriving, and takes effect within a tenth of a second either way.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if done.load(Ordering::Relaxed) {
                return;
            }
            stopped.store(true, Ordering::Relaxed);
            // Ended rather than asked. ffmpeg reads a `q` only from a console,
            // and this one has a pipe; the transport stream it has been writing
            // is what makes ending it safe, so there is nothing to gain by
            // waiting for a keystroke that will not be read.
            group.kill_detached();
        });

        if let Some(stdout) = child.stdout.take() {
            let mut announced = false;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if stopped.load(Ordering::Relaxed) && !announced {
                    announced = true;
                    emit(FetchEvent::Stage(Stage::Finishing));
                }
                match parse_recording(&line) {
                    Some(Recorded::Bytes(bytes)) => {
                        let elapsed = started.elapsed().as_secs_f64();
                        let rate = if elapsed > 0.2 { bytes as f64 / elapsed } else { 0.0 };
                        // No total and no estimate, and that is not a gap in the
                        // reporting: a broadcast still running has neither.
                        emit(FetchEvent::Progress { done: bytes, total: None, rate, eta: None });
                    }
                    Some(Recorded::Seconds(seconds)) => emit(FetchEvent::Captured(seconds)),
                    None => {}
                }
            }
        }
        // ffmpeg has closed its output, so it is on its way out. Releasing the
        // watcher here is what stops it counting down a grace period against a
        // recording that has already finished.
        done.store(true, Ordering::Relaxed);
    });

    let status = child.wait().map_err(|e| format!("waiting for ffmpeg: {e}"))?;
    let stderr = errors.join().unwrap_or_default();
    let asked_to_stop = stopped.load(Ordering::Relaxed);

    let captured = fs::metadata(&raw).map(|m| m.len()).unwrap_or(0);
    if captured == 0 {
        let tail = proc::error_tail(&stderr, 3);
        return Err(if tail.is_empty() {
            "The broadcast produced nothing to record.".into()
        } else {
            tail
        });
    }
    // A stopped recording exits non-zero, and so does one that lost the stream
    // half way through a three-hour sitting. Neither is a reason to throw away
    // what is already on disk, because those bytes are there and they play. So
    // what ffmpeg made of the ending is a note, not a failure.
    if !status.success() && !asked_to_stop {
        let tail = proc::error_tail(&stderr, 2);
        if !tail.is_empty() {
            emit(FetchEvent::Note(format!("The broadcast ended early: {tail}")));
        }
    }

    emit(FetchEvent::Stage(Stage::Finishing));
    let extension = if job.quality.is_audio() { "m4a" } else { "mp4" };
    let stem = file_stem_for(source.title.as_deref().unwrap_or("recording"));
    let output = free_path(&job.folder, &stem, extension);
    remux(&job.tools.ffmpeg, &raw, &output, captured, emit)?;
    Ok(Some(output))
}

/// Puts a finished recording into the container it should end up in, without
/// touching a frame of it.
///
/// `-c copy` throughout, so this is a copy of the bytes rather than an encode.
/// The bar it reports is honest for the same reason: a remux writes very nearly
/// the number of bytes it reads, so the source's size is a real total and not a
/// guess dressed up as one.
fn remux(
    ffmpeg: &Path,
    from: &Path,
    to: &Path,
    source_size: u64,
    emit: &mut dyn FnMut(FetchEvent),
) -> Result<(), String> {
    let mut command = proc::command(ffmpeg);
    command.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
    command.args(["-progress", "pipe:1", "-nostats", "-y", "-i"]);
    command.arg(from);
    command.args(["-c", "copy", "-movflags", "+faststart"]);
    command.arg(to);

    let mut child = command.spawn().map_err(|e| format!("could not start ffmpeg: {e}"))?;
    let errors = drain(child.stderr.take());
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(Recorded::Bytes(bytes)) = parse_recording(&line) {
                emit(FetchEvent::Progress {
                    done: bytes,
                    total: Some(source_size.max(bytes)),
                    rate: 0.0,
                    eta: None,
                });
            }
        }
    }
    let status = child.wait().map_err(|e| format!("waiting for ffmpeg: {e}"))?;
    if status.success() && to.is_file() {
        return Ok(());
    }
    let tail = proc::error_tail(&errors.join().unwrap_or_default(), 3);
    Err(if tail.is_empty() {
        "the recording could not be put into its final file".to_string()
    } else {
        format!("the recording could not be put into its final file: {tail}")
    })
}

/// One useful number out of an ffmpeg `-progress` line.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Recorded {
    /// Bytes written to the file so far.
    Bytes(u64),
    /// Running time captured so far, in seconds.
    Seconds(f64),
}

fn parse_recording(line: &str) -> Option<Recorded> {
    let (key, value) = line.split_once('=')?;
    let value = value.trim();
    match key.trim() {
        "total_size" => value.parse().ok().map(Recorded::Bytes),
        // Microseconds, despite what the older of the two names suggests.
        "out_time_us" | "out_time_ms" => {
            let micros: f64 = value.parse().ok()?;
            (micros >= 0.0).then_some(Recorded::Seconds(micros / 1_000_000.0))
        }
        _ => None,
    }
}

/// Reads a child's pipe to the end on a thread of its own.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    })
}

fn record_path(temp_dir: &Path) -> PathBuf {
    temp_dir.join(PATH_RECORD)
}

/// Where the finished file actually is, once yt-dlp has exited.
fn finished_path(temp_dir: &Path, folder: &Path) -> Option<PathBuf> {
    let recorded = fs::read(record_path(temp_dir)).ok().and_then(|bytes| {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        text.lines().map(str::trim).find(|line| !line.is_empty()).map(PathBuf::from)
    });
    match recorded {
        Some(path) if path.is_file() => Some(path),
        // Either yt-dlp is old enough not to have written one, or it named
        // something that is not there. Both leave the folder as the only place
        // left to look.
        _ => newest_file_in(folder),
    }
}

/// The JavaScript engines on PATH that yt-dlp will not find by itself.
///
/// deno is its default, so finding one means there is nothing to say. node and
/// bun work just as well but are ignored unless they are named, which is the
/// whole problem this solves.
fn js_runtimes() -> Vec<&'static str> {
    if proc::find_on_path("deno").is_some() {
        return Vec::new();
    }
    ["node", "bun"].into_iter().filter(|r| proc::find_on_path(r).is_some()).collect()
}

/// Whether a failure is the kind another attempt might get past.
///
/// Deliberately a short list. A private video, an unsupported site or a typo
/// fails identically however many times it is asked, and retrying only makes the
/// user wait three times as long to be told so. The codes here are the ones that
/// have actually been seen to clear on their own.
fn worth_retrying(error: &str) -> bool {
    const TRANSIENT: [&str; 7] = [
        "HTTP Error 403",
        "Forbidden",
        "HTTP Error 429",
        "HTTP Error 500",
        "HTTP Error 502",
        "HTTP Error 503",
        "HTTP Error 504",
    ];
    TRANSIENT.iter().any(|pattern| error.contains(pattern))
}

/// yt-dlp's last words, with a sentence added where they need one.
///
/// Two failures get this treatment, because both send people looking in the
/// wrong place. `403 Forbidden` reads like a blocked network or a private video;
/// it is usually neither. With no JavaScript engine installed it means a missing
/// program, and on its own it often means nothing at all - YouTube simply
/// refuses at random, which is why it has already been retried by the time this
/// is written.
fn explain(tail: &str) -> String {
    // Checked first: this failure reports the 403 as well, and the missing
    // engine is the part worth acting on.
    if tail.contains("JavaScript runtime") {
        return format!(
            "{tail}  --  In plain terms: YouTube locks its video links behind a \
             JavaScript puzzle, and this machine has nothing that can solve one. \
             Installing Node.js (nodejs.org) or Deno fixes it, and this will find \
             either of them on its own afterwards."
        );
    }
    if worth_retrying(tail) {
        return format!(
            "{tail}  --  Tried {ATTEMPTS} times. YouTube refuses perfectly good links \
             at random, so the same video often works if you try it again in a minute."
        );
    }
    tail.to_string()
}

/// Runs yt-dlp, turning its progress lines into events. Returns where the file
/// ended up, if it said.
fn download(
    ytdlp: &Path,
    job: &Job,
    temp_dir: &Path,
    cancel: &AtomicBool,
    // What the asking pass already reported, so the same name is not announced
    // a second time once the progress lines start carrying it.
    known_title: Option<&str>,
    emit: &mut dyn FnMut(FetchEvent),
) -> Result<Option<PathBuf>, String> {
    let ffmpeg_dir = job.tools.ffmpeg.parent();
    let mut command = proc::command(ytdlp);
    command.args(args(job, temp_dir, ffmpeg_dir));

    let mut child = command.spawn().map_err(|e| format!("could not start yt-dlp: {e}"))?;

    let stderr = child.stderr.take();
    let drain = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut stderr) = stderr {
            use std::io::Read;
            let _ = stderr.read_to_end(&mut buffer);
        }
        buffer
    });

    let mut title: Option<String> = known_title.map(str::to_string);
    let mut stream = 0u32;
    let mut last_done = 0u64;

    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout);
        let mut raw = Vec::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                break;
            }
            raw.clear();
            // Read bytes and decode loosely rather than using `lines()`, which
            // hands back an error for anything that is not UTF-8 and would take
            // the rest of the stream with it. One odd byte in a video title must
            // not be able to blind this to every progress line after it.
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&raw);
            let Some(progress) = Progress::parse(&line) else {
                continue;
            };

            if let Some(name) = &progress.title
                && title.as_deref() != Some(name.as_str())
            {
                title = Some(name.clone());
                emit(FetchEvent::Title(name.clone()));
            }

            match progress.status {
                Status::Downloading => {
                    // Best video and best audio usually live in separate files,
                    // so the byte count runs 0..n twice. Restarting is the only
                    // honest thing the bar can do, but it has to say why.
                    if progress.done < last_done || stream == 0 {
                        stream += 1;
                        // Carries the stage with it: a stream starting is the
                        // download starting, and after the first one it is also
                        // what takes the screen back out of "finishing off".
                        emit(FetchEvent::Stream(stream));
                    }
                    last_done = progress.done;
                    emit(FetchEvent::Progress {
                        done: progress.done,
                        total: progress.total,
                        rate: progress.rate,
                        eta: progress.eta,
                    });
                }
                Status::Finished => {
                    last_done = progress.done;
                    emit(FetchEvent::Progress {
                        done: progress.done,
                        total: progress.total.or(Some(progress.done)),
                        rate: progress.rate,
                        eta: None,
                    });
                    // Either another stream follows - which restarts the bar
                    // above - or ffmpeg is now joining what arrived.
                    emit(FetchEvent::Stage(Stage::Finishing));
                }
                Status::Error => {}
            }
        }
    }

    let status = child.wait().map_err(|e| format!("waiting for yt-dlp: {e}"))?;
    let stderr = drain.join().unwrap_or_default();

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled.".into());
    }
    if status.success() {
        return Ok(finished_path(temp_dir, &job.folder));
    }

    // yt-dlp's last words are the useful ones: "Video unavailable", "Sign in to
    // confirm your age", "Unsupported URL". The rest is traceback.
    let tail = proc::error_tail(&stderr, 3);
    Err(if tail.is_empty() {
        format!("yt-dlp exited with {}", status.code().unwrap_or(-1))
    } else {
        tail
    })
}

/// The fallback for a run that downloaded something but did not say where.
///
/// `--print after_move:` is the answer that gets used; this only covers a yt-dlp
/// old enough not to have it, and it is deliberately dumb - the newest file
/// sitting in the destination folder.
fn newest_file_in(folder: &Path) -> Option<PathBuf> {
    fs::read_dir(folder)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let when = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((when, e.path()))
        })
        .max_by_key(|(when, _)| *when)
        .filter(|(when, _)| {
            // Anything older than a minute was already there before this ran.
            when.elapsed().map(|age| age.as_secs() < 60).unwrap_or(false)
        })
        .map(|(_, path)| path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Downloading,
    Finished,
    Error,
}

/// One parsed progress line.
#[derive(Debug, Clone, PartialEq)]
struct Progress {
    status: Status,
    done: u64,
    total: Option<u64>,
    rate: f64,
    eta: Option<f64>,
    title: Option<String>,
}

impl Progress {
    /// Reads one `--progress-template` line.
    ///
    /// yt-dlp writes `NA` for any field it does not know yet, which is most of
    /// them on the first line of a live stream, so every value here is optional
    /// in practice even though the template always produces seven of them.
    fn parse(line: &str) -> Option<Self> {
        let rest = line.trim().strip_prefix(PROGRESS_TAG)?.trim_start();
        // The title goes last precisely because it is the one field that can
        // contain spaces, so the six before it split off cleanly.
        let mut parts = rest.splitn(7, ' ');

        let status = match parts.next()? {
            "downloading" => Status::Downloading,
            "finished" => Status::Finished,
            "error" => Status::Error,
            _ => return None,
        };
        let number = |text: Option<&str>| -> Option<f64> {
            text.and_then(|t| t.trim().parse::<f64>().ok()).filter(|n| n.is_finite() && *n >= 0.0)
        };

        let done = number(parts.next()).unwrap_or(0.0) as u64;
        let declared = number(parts.next());
        let estimated = number(parts.next());
        let rate = number(parts.next()).unwrap_or(0.0);
        let eta = number(parts.next());
        let title = parts
            .next()
            .map(str::trim)
            .filter(|t| !t.is_empty() && *t != "NA")
            .map(str::to_string);

        Some(Self {
            status,
            done,
            // A declared length is exact; an estimate is what a fragmented
            // stream offers instead, and a bar drawn from it is still better
            // than no bar at all.
            total: declared.or(estimated).map(|n| n as u64).filter(|n| *n > 0),
            rate,
            eta,
            title,
        })
    }
}

/// Turns what the user typed into a link, or says it is not one.
///
/// Copying a link out of a browser gives a whole URL; copying it out of a chat
/// message or reading it off a screen often loses the `https://`, and refusing
/// `youtu.be/dQw4w9WgXcQ` over a missing scheme would be pedantry. A Windows
/// path is refused outright - that is a clip to add, not a link to download.
pub fn normalise_url(raw: &str) -> Option<String> {
    let text = raw.trim().trim_matches('"').trim_matches('\'');
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return None;
    }
    if let Some(rest) = text.strip_prefix("https://").or_else(|| text.strip_prefix("http://")) {
        return (!rest.is_empty()).then(|| text.to_string());
    }
    // A bare host, which is the shape a link takes when the scheme is dropped.
    let host = text.split(['/', '?', '#']).next()?;
    let plausible = host.contains('.')
        && !host.contains('\\')
        && !host.contains(':')
        && !host.starts_with('.')
        && !host.ends_with('.');
    plausible.then(|| format!("https://{text}"))
}

/// Whether the destination folder can take a downloaded file, checked before a
/// screen full of progress appears and then fails at the last step.
pub fn folder_is_usable(folder: &Path) -> bool {
    crate::ffmpeg::is_writable(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(quality: FetchQuality) -> Job {
        Job {
            tools: Arc::new(Tools {
                ffmpeg: PathBuf::from("C:/tools/ffmpeg/bin/ffmpeg.exe"),
                ffprobe: PathBuf::from("C:/tools/ffmpeg/bin/ffprobe.exe"),
            }),
            url: "https://example.com/watch?v=abc".into(),
            folder: PathBuf::from("C:/clips"),
            quality,
            install_root: PathBuf::from("C:/tools"),
            search: Vec::new(),
            allow_download: true,
        }
    }

    fn args_for(quality: FetchQuality) -> Vec<String> {
        let job = job(quality);
        args(&job, Path::new("C:/clips/_download_temp_1"), job.tools.ffmpeg.parent())
    }

    fn info(fields: &str) -> String {
        format!("{{\"title\":\"A sitting\",{fields}}}")
    }

    /// The one question the whole live path turns on. Getting it wrong in the
    /// safe direction wastes an extraction; getting it wrong in the other sends
    /// a broadcast with no end down a path that waits for one.
    #[test]
    fn a_link_is_read_as_live_or_finished() {
        let live = read_info(&info(r#""live_status":"is_live","url":"https://x/live.m3u8""#))
            .expect("a readable answer");
        assert_eq!(live.live, Live::Now);
        assert_eq!(live.title.as_deref(), Some("A sitting"));

        // A broadcast that has finished is an ordinary video, whichever of the
        // two words the extractor uses for it.
        for finished in ["was_live", "post_live", "not_live"] {
            let json = info(&format!(r#""live_status":"{finished}","url":"https://x/v.mp4""#));
            assert_eq!(read_info(&json).unwrap().live, Live::No, "{finished}");
        }

        let upcoming = info(r#""live_status":"is_upcoming""#);
        assert_eq!(read_info(&upcoming).unwrap().live, Live::Upcoming);

        // An extractor that never learned about live_status still has to be
        // understood, or its broadcasts are treated as finished videos.
        let old = info(r#""is_live":true,"url":"https://x/live.m3u8""#);
        assert_eq!(read_info(&old).unwrap().live, Live::Now);
        assert_eq!(read_info(&info(r#""url":"https://x/v.mp4""#)).unwrap().live, Live::No);
    }

    /// Where the stream is, in both shapes yt-dlp reports it: one file carrying
    /// both tracks, or the best video and the best audio in separate files.
    #[test]
    fn the_streams_to_record_are_found_in_either_shape() {
        let single = read_info(&info(
            r#""live_status":"is_live","url":"https://x/live.m3u8","http_headers":{"User-Agent":"vm","Accept-Encoding":"gzip"}"#,
        ))
        .unwrap();
        assert_eq!(single.streams.len(), 1);
        assert_eq!(single.streams[0].url, "https://x/live.m3u8");
        // Compression ffmpeg did not ask for and will not undo.
        assert_eq!(single.streams[0].headers, vec![("User-Agent".into(), "vm".into())]);

        let split = read_info(&info(
            r#""live_status":"is_live","url":"https://x/ignored","requested_formats":[{"url":"https://x/video.m3u8","http_headers":{"User-Agent":"vm"}},{"url":"https://x/audio.m3u8"}]"#,
        ))
        .unwrap();
        let urls: Vec<&str> = split.streams.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, ["https://x/video.m3u8", "https://x/audio.m3u8"]);
        assert!(split.streams[1].headers.is_empty());

        // Nothing to point ffmpeg at is not a crash, and not a silent success
        // either - `record` refuses it by name.
        let bare = read_info(&info(r#""live_status":"is_live""#)).unwrap();
        assert!(bare.streams.is_empty());
    }

    #[test]
    fn an_answer_that_is_not_an_answer_is_refused() {
        // Warnings reach stderr, but a stray line on stdout must not stop the
        // document after it from being read.
        let noisy = format!("WARNING: something\n{}\n", info(r#""live_status":"is_live""#));
        assert!(read_info(&noisy).is_ok());
        assert!(read_info("").is_err());
        assert!(read_info("WARNING: only this").is_err());
        assert!(read_info("{not json at all").is_err());
    }

    /// ffmpeg is handed a URL and told where to put what comes back. Both
    /// matter: the wrong map loses a track, and a missing `-c copy` would try to
    /// re-encode a live 1080p stream in real time.
    #[test]
    fn the_recorder_is_told_to_copy_and_to_follow_the_stream() {
        let one = [Stream {
            url: "https://x/live.m3u8".into(),
            headers: vec![("User-Agent".into(), "vm".into())],
        }];
        let args = record_args(&one, Path::new("C:/temp/recording.ts"));
        let joined = args.join(" ");
        assert!(joined.contains("-c copy"), "got {joined}");
        assert!(joined.contains("-f mpegts"), "got {joined}");
        assert!(joined.contains("-reconnect_streamed 1"), "got {joined}");
        assert_eq!(args.last().unwrap(), "C:/temp/recording.ts");
        // Optional, so asking for a picture an audio-only capture does not have
        // is not an error.
        assert!(joined.contains("-map 0:v:0? -map 0:a:0?"), "got {joined}");
        let headers = args[args.iter().position(|a| a == "-headers").expect("headers") + 1].clone();
        assert_eq!(headers, "User-Agent: vm\r\n");

        // Two files: the picture comes from one input and the sound the other.
        let two = [
            Stream { url: "https://x/v.m3u8".into(), headers: Vec::new() },
            Stream { url: "https://x/a.m3u8".into(), headers: Vec::new() },
        ];
        let args = record_args(&two, Path::new("C:/temp/recording.ts"));
        assert!(args.join(" ").contains("-map 0:v:0 -map 1:a:0"), "got {args:?}");
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 2);
    }

    #[test]
    fn recording_progress_is_read_from_ffmpeg() {
        assert_eq!(parse_recording("total_size=4456448"), Some(Recorded::Bytes(4_456_448)));
        assert_eq!(parse_recording("out_time_us=2500000"), Some(Recorded::Seconds(2.5)));
        assert_eq!(parse_recording("out_time_ms=1000000"), Some(Recorded::Seconds(1.0)));
        // Everything else in the block, which is most of it.
        for other in ["frame=42", "progress=continue", "bitrate=1122.5kbits/s", "speed=1x", ""] {
            assert_eq!(parse_recording(other), None, "{other:?}");
        }
        // Written before anything has been, and not a length of -1 seconds.
        assert_eq!(parse_recording("out_time_us=N/A"), None);
        assert_eq!(parse_recording("total_size=N/A"), None);
    }

    /// A recording names its own file, so every rule `--windows-filenames` would
    /// have applied has to be applied here instead.
    #[test]
    fn a_title_becomes_a_filename_windows_will_take() {
        assert_eq!(file_stem_for("20th Majlis - 26th Sitting"), "20th Majlis - 26th Sitting");
        // The characters Windows refuses, including the colon every one of these
        // broadcast titles carries a timestamp in.
        assert_eq!(file_stem_for("Sitting 2026-08-18 10:08"), "Sitting 2026-08-18 10_08");
        assert_eq!(file_stem_for(r#"a<b>c:d"e/f\g|h?i*j"#), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(file_stem_for("line\nbreak"), "line_break");
        // Windows drops these silently, so a file would be written under one
        // name and then looked for under another.
        assert_eq!(file_stem_for("  Sitting...  "), "Sitting");
        // Never nothing: an empty name is not a file.
        for empty in ["", "   ", "..."] {
            assert_eq!(file_stem_for(empty), "recording", "{empty:?}");
        }
        // A title of nothing but control characters becomes a name made of the
        // underscores they turned into. Odd to look at, and still a file that
        // can be written and found again, which is all this has to guarantee.
        assert_eq!(file_stem_for("\u{0}\u{1}"), "__");

        // Long titles make paths nothing can open, and the cut has to leave
        // valid text behind rather than half of a character.
        let long = "\u{1F3DB}".repeat(200);
        let cut = file_stem_for(&long);
        assert!(cut.len() <= 120, "{} bytes", cut.len());
        assert!(!cut.is_empty() && cut.chars().all(|c| c == '\u{1F3DB}'));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_second_recording_does_not_overwrite_the_first() {
        let dir = std::env::temp_dir().join(format!("vmerge-free-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let first = free_path(&dir, "Sitting", "mp4");
        assert_eq!(first.file_name().unwrap(), "Sitting.mp4");
        fs::write(&first, b"x").unwrap();

        let second = free_path(&dir, "Sitting", "mp4");
        assert_eq!(second.file_name().unwrap(), "Sitting (2).mp4");
        fs::write(&second, b"x").unwrap();
        assert_eq!(free_path(&dir, "Sitting", "mp4").file_name().unwrap(), "Sitting (3).mp4");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Both passes have to agree about which stream is wanted, or the recorder
    /// is handed the URL of a format the download would never have picked.
    #[test]
    fn asking_and_fetching_choose_the_same_stream() {
        for quality in FetchQuality::ALL {
            let job = job(quality);
            let asked = selection_args(&job);
            let fetched = args(&job, Path::new("C:/clips/_download_temp_1"), None);
            let format_of = |args: &[String]| {
                args.iter().position(|a| a == "-f").map(|i| args[i + 1].clone())
            };
            assert_eq!(format_of(&asked), format_of(&fetched), "{quality:?}");
            let sort_of = |args: &[String]| {
                args.iter().position(|a| a == "-S").map(|i| args[i + 1].clone())
            };
            assert_eq!(sort_of(&asked), sort_of(&fetched), "{quality:?}");
        }
    }

    #[test]
    fn progress_lines_are_read() {
        let line = "VMPROG downloading 1048576 10485760 NA 524288.0 18 A Video Title";
        let parsed = Progress::parse(line).expect("a progress line");
        assert_eq!(parsed.status, Status::Downloading);
        assert_eq!(parsed.done, 1_048_576);
        assert_eq!(parsed.total, Some(10_485_760));
        assert_eq!(parsed.rate, 524_288.0);
        assert_eq!(parsed.eta, Some(18.0));
        assert_eq!(parsed.title.as_deref(), Some("A Video Title"));
    }

    /// A title is the one field that can contain anything, which is why it is
    /// last. Splitting on every space would cut it into pieces.
    #[test]
    fn a_title_with_spaces_survives_whole() {
        let line = "VMPROG downloading 1 2 NA 3 4 how to make bread - part 2";
        let parsed = Progress::parse(line).unwrap();
        assert_eq!(parsed.title.as_deref(), Some("how to make bread - part 2"));
    }

    /// Nothing is known on the first line of a fragmented stream, and a bar has
    /// to cope with that rather than showing a total of zero.
    #[test]
    fn unknown_fields_become_nothing_rather_than_zero() {
        let parsed = Progress::parse("VMPROG downloading 0 NA NA NA NA NA").unwrap();
        assert_eq!(parsed.done, 0);
        assert_eq!(parsed.total, None, "no total means no bar, not a bar at 0/0");
        assert_eq!(parsed.eta, None);
        assert_eq!(parsed.title, None);

        // An estimate stands in for a declared length when there is not one.
        let parsed = Progress::parse("VMPROG downloading 500 NA 4096 100 5 x").unwrap();
        assert_eq!(parsed.total, Some(4096));
    }

    #[test]
    fn anything_that_is_not_a_progress_line_is_ignored() {
        for line in [
            "",
            "[download] Destination: video.mp4",
            "VMPROG something-else 1 2 3 4 5 x",
            "  [youtube] abc: Downloading webpage",
        ] {
            assert_eq!(Progress::parse(line), None, "{line:?} is not progress");
        }
    }

    /// A path with an emoji in it is exactly the case that broke: yt-dlp prints
    /// a stripped version to the console, so the file has to be believed over
    /// anything named on stdout - and a name that leads nowhere falls back to
    /// looking in the folder rather than reporting the download as lost.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn the_finished_path_is_read_from_the_file_and_checked() {
        let dir = std::env::temp_dir().join(format!("vmerge-fetch-{}", std::process::id()));
        let temp = dir.join("temp");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&temp).unwrap();

        let name = "Chill Mix 🎧 Vol 85.mp4";
        let landed = dir.join(name);
        fs::write(&landed, b"video").unwrap();

        // The happy path: yt-dlp wrote the real name, emoji and all.
        fs::write(record_path(&temp), landed.display().to_string().as_bytes()).unwrap();
        assert_eq!(finished_path(&temp, &dir), Some(landed.clone()));

        // A name that leads nowhere - what a console-mangled path looks like -
        // falls through to the folder rather than reporting nothing at all.
        fs::write(record_path(&temp), dir.join("Chill Mix  Vol 85.mp4").display().to_string().as_bytes())
            .unwrap();
        assert_eq!(finished_path(&temp, &dir), Some(landed.clone()), "the real file is still there");

        // No record written at all, same answer.
        let _ = fs::remove_file(record_path(&temp));
        assert_eq!(finished_path(&temp, &dir), Some(landed));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_finished_status_closes_the_bar() {
        let parsed = Progress::parse("VMPROG finished 10485760 10485760 NA NA NA Clip").unwrap();
        assert_eq!(parsed.status, Status::Finished);
        assert_eq!(parsed.done, parsed.total.unwrap());
    }

    #[test]
    fn a_height_cap_is_a_hard_filter() {
        // Someone who picks 720p gets 720p. A preference would quietly hand back
        // 4K whenever the site happened to offer it.
        assert!(FetchQuality::P720.selector().contains("height<=720"));
        assert!(!FetchQuality::Best.selector().contains("height"));
    }

    #[test]
    fn the_arguments_say_what_they_have_to() {
        let args = args_for(FetchQuality::P1080);
        let joined = args.join(" ");

        // The URL is last, behind the end-of-options marker, so a link starting
        // with a dash cannot be read as a flag.
        assert_eq!(args.last().unwrap(), "https://example.com/watch?v=abc");
        assert_eq!(args[args.len() - 2], "--", "the URL has to be shielded");

        // Partial files stay in the working folder; only the finished one moves.
        assert!(joined.contains("temp:C:/clips/_download_temp_1"), "got {joined}");
        assert!(joined.contains("home:C:/clips"), "got {joined}");

        // The finished path is written to a file, never read off stdout: yt-dlp
        // drops characters the console cannot render, and a path with an emoji
        // stripped out of it names nothing.
        let record = args
            .iter()
            .position(|a| a == "--print-to-file")
            .map(|i| (args[i + 1].clone(), args[i + 2].clone()))
            .expect("the path is recorded to a file");
        assert_eq!(record.0, "after_move:%(filepath)s");
        assert!(record.1.ends_with(PATH_RECORD), "got {}", record.1);
        assert!(
            !args.iter().any(|a| a.starts_with("after_move") && a.contains("VMFILE")),
            "nothing may go back to reading it off stdout"
        );

        // --print-to-file would otherwise turn the whole run into a dry one.
        assert!(args.iter().any(|a| a == "--no-simulate"));
        // ...and would silence the progress the screen is drawn from.
        assert!(args.iter().any(|a| a == "--progress"));
        assert!(args.iter().any(|a| a == "--newline"));
        // A user's own config must not be able to change any of that.
        assert!(args.iter().any(|a| a == "--ignore-config"));

        // The ffmpeg this program already installed, not whatever is on PATH.
        assert!(joined.contains("--ffmpeg-location C:/tools/ffmpeg/bin"), "got {joined}");

        // Resolution has to be the first sort key, or a 360p H.264 stream beats
        // a 1080p VP9 one and the cap becomes meaningless.
        let sort = args
            .iter()
            .position(|a| a == "-S")
            .map(|i| args[i + 1].clone())
            .expect("a sort order");
        assert!(sort.starts_with("res,"), "got {sort}");
    }

    #[test]
    fn audio_only_asks_for_no_video_at_all() {
        let args = args_for(FetchQuality::Audio);
        assert!(args.iter().any(|a| a == "-x"));
        assert!(args.iter().any(|a| a == "m4a"));
        assert!(
            !args.iter().any(|a| a == "--merge-output-format"),
            "there are no streams to merge"
        );
    }

    /// deno is yt-dlp's own default, so naming it again would be noise; node and
    /// bun are invisible to it unless they are named.
    #[test]
    fn only_the_engines_yt_dlp_misses_are_named() {
        // Whatever this machine has, the rule holds: nothing is asked for when
        // deno is present, and never deno itself.
        let named = js_runtimes();
        assert!(!named.contains(&"deno"), "deno is already the default: {named:?}");
        if proc::find_on_path("deno").is_some() {
            assert!(named.is_empty(), "deno covers it: {named:?}");
        }
        assert!(named.iter().all(|r| ["node", "bun"].contains(r)), "got {named:?}");
    }

    /// `403 Forbidden` for a public video is the single most misleading thing
    /// yt-dlp says, and it has two quite different causes.
    #[test]
    fn the_two_faces_of_403_are_told_apart() {
        // No JavaScript engine: a missing program, and the 403 is a symptom.
        let no_engine = "WARNING: [youtube] No supported JavaScript runtime could be found. \
                         Only deno is enabled by default; ERROR: unable to download video \
                         data: HTTP Error 403: Forbidden";
        let said = explain(no_engine);
        assert!(said.starts_with(no_engine), "yt-dlp's own words come first: {said}");
        assert!(said.contains("nodejs.org"), "it has to say what to install: {said}");
        assert!(!said.contains("at random"), "this one is not random: {said}");

        // A bare 403: nothing is wrong, YouTube just said no this time.
        let bare = "ERROR: unable to download video data: HTTP Error 403: Forbidden";
        let said = explain(bare);
        assert!(said.starts_with(bare), "got {said}");
        assert!(said.contains("Tried 3 times"), "the retries have to be accounted for: {said}");
        assert!(!said.contains("nodejs.org"), "nothing here needs installing: {said}");

        // Everything else is passed through untouched: guessing at causes that
        // have not actually been diagnosed is worse than saying nothing.
        for other in [
            "ERROR: [youtube] abc: Video unavailable",
            "ERROR: Unsupported URL: https://example.com/x",
            "ERROR: [youtube] abc: Sign in to confirm your age",
        ] {
            assert_eq!(explain(other), other, "{other:?} must not be embellished");
        }
    }

    /// Retrying is only worth the wait for failures that actually clear.
    #[test]
    fn only_failures_that_might_clear_are_retried() {
        for transient in [
            "ERROR: unable to download video data: HTTP Error 403: Forbidden",
            "ERROR: HTTP Error 503: Service Unavailable",
            "ERROR: HTTP Error 429: Too Many Requests",
        ] {
            assert!(worth_retrying(transient), "{transient:?} is worth another go");
        }
        // Asking three times changes none of these, and only makes the user
        // wait three times as long to hear the same answer.
        for permanent in [
            "ERROR: [youtube] abc: Video unavailable",
            "ERROR: Unsupported URL: https://example.com/x",
            "ERROR: [youtube] abc: Private video. Sign in if you've been granted access",
            "ERROR: unable to open for writing: HTTP Error 404: Not Found",
        ] {
            assert!(!worth_retrying(permanent), "{permanent:?} will never work");
        }
    }

    #[test]
    fn links_are_recognised_and_paths_are_not() {
        assert_eq!(
            normalise_url("https://www.youtube.com/watch?v=abc"),
            Some("https://www.youtube.com/watch?v=abc".into())
        );
        // A scheme dropped by a chat client or read off a screen.
        assert_eq!(normalise_url("youtu.be/dQw4w9WgXcQ"), Some("https://youtu.be/dQw4w9WgXcQ".into()));
        assert_eq!(
            normalise_url("  \"https://vimeo.com/12345\"  "),
            Some("https://vimeo.com/12345".into())
        );

        // These are clips to add, not links to download.
        for not_a_link in [
            r"C:\Users\me\Videos\clip.mp4",
            r"\\server\share\clip.mp4",
            "clip.mp4 other.mp4",
            "merged",
            "",
            "https://",
            "..",
        ] {
            assert_eq!(normalise_url(not_a_link), None, "{not_a_link:?} is not a link");
        }
    }
}
