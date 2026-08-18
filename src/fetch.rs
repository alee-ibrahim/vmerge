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
//! is *before* anything is fetched, and a live one is fetched differently:
//! `--live-from-start`, which asks yt-dlp for the broadcast from its beginning
//! rather than from this moment.
//!
//! That one flag settles three things at once. It is what makes a sitting
//! already an hour old arrive as an hour of video instead of the tail end of
//! one. It puts the download back on yt-dlp's own fragment downloader, which
//! reports progress properly - the silence was never a live stream being
//! unreportable, only ffmpeg being handed the job and saying nothing about it.
//! And because the work is counted in fragments, and the site says how many
//! there are, a live download has a real position to show rather than a
//! spinner: it is a measured fraction of what has been broadcast so far, which
//! creeps as the broadcast goes on and is honest about never quite arriving.
//!
//! Stopping one keeps it. yt-dlp is ended and the part-finished streams it
//! leaves behind are joined into a playable file, so esc means "that is enough"
//! rather than "throw away the hour you just waited for".

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

/// The suffix yt-dlp gives a stream it has not finished writing.
///
/// What a stopped live download leaves behind, and what is salvaged from it.
const PART: &str = ".part";


/// Which stream to take. Not a bitrate or a codec: the point of a picker in a
/// program like this is that "720p" is a thing people already know the meaning
/// of, and every one of these maps to a yt-dlp selector that cannot fail to
/// parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FetchQuality {
    /// Whatever the site has, at the highest resolution.
    Best,
    /// 4K, where the site has it. Nothing above 1080p is published in H.264,
    /// so this arrives as VP9 and joining it to anything re-encodes it.
    #[value(name = "2160p", alias = "4k")]
    P2160,
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
    pub const ALL: [FetchQuality; 6] = [
        FetchQuality::Best,
        FetchQuality::P2160,
        FetchQuality::P1080,
        FetchQuality::P720,
        FetchQuality::P480,
        FetchQuality::Audio,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FetchQuality::Best => "best available",
            FetchQuality::P2160 => "4K",
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
            FetchQuality::P2160 => "if the source has it; slow to merge after",
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
            FetchQuality::P2160 => "bv*[height<=2160]+ba/b[height<=2160]",
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
    Progress {
        done: u64,
        total: Option<u64>,
        rate: f64,
        eta: Option<f64>,
        /// Pieces done and pieces there are, when the stream comes in pieces.
        /// What a live broadcast is measured by, having no byte total.
        fragments: Option<(u64, u64)>,
    },
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
        (self.emit)(FetchEvent::Progress { done: received, total, rate, eta, fragments: None });
    }

    fn finished(&mut self) {}
}

/// Finds yt-dlp, downloads the video, and reports what landed.
pub fn run(job: &Job, cancel: &AtomicBool, emit: &mut dyn FnMut(FetchEvent)) -> Outcome {
    let started = Instant::now();
    let mut outcome = Outcome {
        ok: false,
        output: job.folder.clone(),
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
    emit(FetchEvent::Progress { done: 0, total: None, rate: 0.0, eta: None, fragments: None });

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
    outcome.outputs = vec![file.clone()];
    outcome.size = fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    match probe::clip_info(&job.tools.ffprobe, &file) {
        Some(info) => {
            outcome.out_duration = info.duration;
            if info.width > 0 && info.height > 0 {
                // The size it will be displayed at, which for anamorphic footage is
                // not the size in the file - and is what the plan line promised.
                outcome.out_format = {
                    let (width, height) = info.display_size();
                    Some((width, height, info.fps))
                };
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

    if source.live == Live::Upcoming {
        return Err("That broadcast has not started yet. There is nothing to record \
                    until it does."
            .into());
    }

    let live = source.live == Live::Now;
    if live {
        // Two lines rather than one long one. The notes area draws whole lines
        // and cuts whatever is wider than the window, so a single sentence this
        // long loses its own ending on a terminal of ordinary width.
        emit(FetchEvent::Note(
            "This is live, so it is taken from the start of the broadcast rather than from now."
                .into(),
        ));
        emit(FetchEvent::Note(
            "It keeps going until the broadcast ends or you stop it.".into(),
        ));
    }
    emit(FetchEvent::Stage(if live { Stage::Recording } else { Stage::Downloading }));
    download(ytdlp, job, temp_dir, cancel, live, source.title.as_deref(), emit)
}

/// How streams are ranked once the format filter has had its say.
///
/// Resolution first, or this would happily prefer a 360p H.264 stream over a
/// 1080p VP9 one. Within a resolution, H.264 in an mp4 is what the merge side of
/// this program joins without re-encoding anything.
///
/// Which is also why the named resolutions stop at 1080p: YouTube publishes no
/// H.264 above it. Ask for more and what arrives is necessarily VP9 or AV1 -
/// a perfectly good file, in a perfectly good mp4, that the merge side then has
/// to re-encode to join. That is a real cost and belongs to whoever chose it,
/// so 4K is offered by name rather than only reachable by picking "best".
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
fn args(job: &Job, temp_dir: &Path, ffmpeg_dir: Option<&Path>, live: bool) -> Vec<String> {
    let mut args = selection_args(job);

    if live {
        // From the beginning of the broadcast rather than from this moment.
        //
        // Someone opening this an hour into a sitting wants the hour, not the
        // tail of it - and without the flag that hour is simply gone, because
        // there is no going back for it once the broadcast ends. It also takes
        // the download off ffmpeg and back onto yt-dlp's own fragment
        // downloader, which is the only one of the two that reports progress.
        args.push("--live-from-start".into());
    }

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
        // The two fragment fields are what a live download is measured by. It
        // has no byte total - nobody knows how big a broadcast still running
        // will be - but the site does say how many pieces it is in so far, and
        // that is a real fraction rather than a guess.
        format!(
            "download:{PROGRESS_TAG} %(progress.status)s %(progress.downloaded_bytes)s \
             %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s \
             %(progress.eta)s %(progress.fragment_index)s %(progress.fragment_count)s \
             %(info.title)s"
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

/// What one extraction pass says about a link, before anything is fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    title: Option<String>,
    live: Live,
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

    Ok(Source { title: info.title.filter(|t| !t.trim().is_empty()), live })
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
    // Whether this is a broadcast still on air, which changes both what yt-dlp
    // is asked for and what stopping half way through means.
    live: bool,
    // What the asking pass already reported, so the same name is not announced
    // a second time once the progress lines start carrying it.
    known_title: Option<&str>,
    emit: &mut dyn FnMut(FetchEvent),
) -> Result<Option<PathBuf>, String> {
    let ffmpeg_dir = job.tools.ffmpeg.parent();
    let mut command = proc::command(ytdlp);
    command.args(args(job, temp_dir, ffmpeg_dir, live));

    let mut child = command.spawn().map_err(|e| format!("could not start yt-dlp: {e}"))?;
    let group = proc::Group::around(&child);

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
    // The numbered lines, kept per stream so they can be added up. Empty for a
    // download yt-dlp runs one stream at a time, which takes the path below it.
    let mut streams: BTreeMap<u64, Progress> = BTreeMap::new();

    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout);
        let mut raw = Vec::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                group.kill(&mut child);
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

            // Only when the asking pass came back without one. For a live
            // broadcast yt-dlp stamps the time onto the title it names the file
            // after, while the progress lines carry the bare title without it -
            // so both would be announced, as two spellings of one name, and the
            // shorter would overwrite the one the file is actually called.
            if let Some(name) = &progress.title
                && known_title.is_none()
                && title.as_deref() != Some(name.as_str())
            {
                title = Some(name.clone());
                emit(FetchEvent::Title(name.clone()));
            }

            // Numbered lines are several downloads at once and are reported as
            // their sum; unnumbered ones are one download at a time, where a
            // count that goes backwards means the next stream has started.
            if let Some(n) = progress.stream {
                let finished = progress.status == Status::Finished;
                streams.insert(n, progress);
                if let Some(event) = combined(&streams) {
                    emit(event);
                }
                // Only once every stream is in. The first of two finishing is
                // not the download finishing, and saying so would put "putting
                // it together" on screen with half of it still arriving.
                if finished && streams.values().all(|p| p.status == Status::Finished) {
                    emit(FetchEvent::Stage(Stage::Finishing));
                }
                continue;
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
                        fragments: progress.fragments,
                    });
                }
                Status::Finished => {
                    last_done = progress.done;
                    emit(FetchEvent::Progress {
                        done: progress.done,
                        total: progress.total.or(Some(progress.done)),
                        rate: progress.rate,
                        eta: None,
                        fragments: progress.fragments,
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
        // A stopped download of a finished video is worth nothing - the file is
        // a fragment of something that still exists in full, and can simply be
        // fetched again. A stopped recording is the opposite: the part of the
        // broadcast it holds is gone the moment the broadcast ends, so it is
        // salvaged rather than swept up with the working folder.
        if live {
            emit(FetchEvent::Stage(Stage::Finishing));
            return salvage(job, temp_dir, known_title, emit).map(Some);
        }
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

/// A title turned into something Windows will accept as a filename.
///
/// The same job `--windows-filenames` does for a download that runs to the end,
/// done here because a salvaged recording names its own output rather than being
/// named by yt-dlp. The reserved characters become underscores rather than
/// vanishing, so two titles differing only in punctuation do not collapse into
/// one name.
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
/// A sitting stopped and restarted would otherwise overwrite the first attempt,
/// and the first attempt is the one with the beginning of it.
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

/// Turns what a stopped recording left behind into a file that plays.
///
/// yt-dlp writes each stream to its own `.part` while it works and only joins
/// them at the end, so a recording stopped part way through leaves two of them -
/// the picture in one and the sound in the other - and no finished video at all.
/// Both are complete files as far as they go, which is what makes this possible:
/// joining them copies the streams without re-encoding anything, and what comes
/// out is every minute that had arrived when the user pressed esc.
fn salvage(
    job: &Job,
    temp_dir: &Path,
    known_title: Option<&str>,
    emit: &mut dyn FnMut(FetchEvent),
) -> Result<PathBuf, String> {
    let parts = part_files(temp_dir);
    if parts.is_empty() {
        return Err("Stopped before anything had arrived, so there was nothing to keep.".into());
    }

    let extension = if job.quality.is_audio() { "m4a" } else { "mp4" };
    let stem = file_stem_for(known_title.unwrap_or("recording"));
    let output = free_path(&job.folder, &stem, extension);

    let mut command = proc::command(&job.tools.ffmpeg);
    command.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]);
    for part in &parts {
        command.arg("-i");
        command.arg(part);
    }
    // Named rather than left to ffmpeg's own choice, which would take every
    // stream from the first input only and drop the sound entirely.
    for (n, _) in parts.iter().enumerate() {
        command.args(["-map".to_string(), format!("{n}")]);
    }
    command.args(["-c", "copy"]);
    command.arg(&output);

    let result = proc::run_captured(command).map_err(|e| format!("could not start ffmpeg: {e}"))?;
    if !result.status.success() || !output.is_file() {
        let tail = proc::error_tail(&result.stderr, 3);
        return Err(if tail.is_empty() {
            "What had been recorded could not be written out.".into()
        } else {
            format!("What had been recorded could not be written out: {tail}")
        });
    }
    emit(FetchEvent::Note("Kept everything recorded up to the moment you stopped.".into()));
    Ok(output)
}

/// The part-written streams in the working folder, biggest last so the picture -
/// which is always the larger of the two - is not what the sound is mapped over.
///
/// yt-dlp also leaves a `.part-FragNNN.part` behind for the single fragment it
/// was in the middle of, and that one is a few kilobytes of nothing useful.
fn part_files(temp_dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(temp_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name.ends_with(PART) && !name.contains(".part-Frag")
        })
        .collect();
    found.sort();
    found
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

/// What several streams arriving at once add up to.
///
/// A live recording downloads the picture and the sound side by side, and each
/// reports on itself, so taking the newest line as the truth makes every figure
/// on screen flip between two unrelated ones. Added together they are one
/// download again.
///
/// The position is the exception: it is the *least* complete stream's, not the
/// sum and not the average. What can be salvaged from a recording stopped half
/// way is bounded by whichever of the two has less, so the honest answer to "how
/// much of this sitting do I have" is the smaller of them.
fn combined(streams: &BTreeMap<u64, Progress>) -> Option<FetchEvent> {
    if streams.is_empty() {
        return None;
    }
    let done = streams.values().map(|p| p.done).sum();
    let rate = streams.values().map(|p| p.rate).sum();
    let total = streams
        .values()
        .map(|p| p.total)
        .try_fold(0u64, |sum, total| total.map(|t| sum + t))
        .filter(|t| *t > 0);
    let fragments = streams
        .values()
        .filter_map(|p| p.fragments)
        .min_by(|(a_at, a_of), (b_at, b_of)| {
            let left = *a_at as f64 / *a_of as f64;
            let right = *b_at as f64 / *b_of as f64;
            left.partial_cmp(&right).unwrap_or(std::cmp::Ordering::Equal)
        });
    // No estimate: two streams racing each other have no shared one, and a live
    // broadcast has no end for either of them to be estimating towards.
    Some(FetchEvent::Progress { done, total, rate, eta: None, fragments })
}

/// Splits the `2: ` yt-dlp writes in front of a progress line off the rest.
///
/// It numbers the lines it is keeping on screen when more than one download is
/// running at once, and `--live-from-start` is exactly that case: the picture
/// and the sound arrive together rather than one after the other. The template
/// this program asks for is then no longer the start of the line, and requiring
/// it to be threw away every progress report a live recording produced - which
/// is how a recording that was working perfectly came to show `0 KB` and no bar
/// for as long as anyone watched it.
///
/// The number is worth keeping rather than merely skipping, because it is what
/// says which of two interleaved streams a line is talking about. Only a run of
/// digits followed by `: ` counts, so a title beginning with something similar
/// cannot be eaten by accident.
fn split_stream_number(line: &str) -> (Option<u64>, &str) {
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return (None, line);
    }
    match line[digits..].strip_prefix(": ") {
        Some(rest) => (line[..digits].parse().ok(), rest),
        None => (None, line),
    }
}

/// One parsed progress line.
#[derive(Debug, Clone, PartialEq)]
struct Progress {
    /// Which of several interleaved downloads this line is about, when yt-dlp
    /// is running more than one at a time. None when it is running one, which
    /// is every download of a video that has finished being broadcast.
    stream: Option<u64>,
    status: Status,
    done: u64,
    total: Option<u64>,
    rate: f64,
    eta: Option<f64>,
    /// How far through the pieces the stream is broken into, when it is broken
    /// into pieces at all. The only measure a live broadcast has: it has no
    /// byte total, because nobody knows how big something still happening will
    /// be, but the site does say how many pieces there are so far.
    fragments: Option<(u64, u64)>,
    title: Option<String>,
}

impl Progress {
    /// Reads one `--progress-template` line.
    ///
    /// yt-dlp writes `NA` for any field it does not know yet, which is most of
    /// them on the first line of a live stream, so every value here is optional
    /// in practice even though the template always produces nine of them.
    fn parse(line: &str) -> Option<Self> {
        let (stream, rest) = split_stream_number(line.trim());
        let rest = rest.strip_prefix(PROGRESS_TAG)?.trim_start();
        // The title goes last precisely because it is the one field that can
        // contain spaces, so the eight before it split off cleanly.
        let mut parts = rest.splitn(9, ' ');

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
        let index = number(parts.next());
        let count = number(parts.next());
        let title = parts
            .next()
            .map(str::trim)
            .filter(|t| !t.is_empty() && *t != "NA")
            .map(str::to_string);

        Some(Self {
            stream,
            status,
            done,
            // A declared length is exact; an estimate is what a fragmented
            // stream offers instead, and a bar drawn from it is still better
            // than no bar at all.
            total: declared.or(estimated).map(|n| n as u64).filter(|n| *n > 0),
            rate,
            eta,
            // Both or neither: a position with nothing to be a position within
            // is not a measurement, and would draw a bar that only ever grows.
            fragments: index
                .zip(count)
                .map(|(i, c)| (i as u64, c as u64))
                .filter(|(i, c)| *c > 0 && i <= c),
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
        args(&job, Path::new("C:/clips/_download_temp_1"), job.tools.ffmpeg.parent(), false)
    }

    fn live_args_for(quality: FetchQuality) -> Vec<String> {
        let job = job(quality);
        args(&job, Path::new("C:/clips/_download_temp_1"), job.tools.ffmpeg.parent(), true)
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

    /// The whole difference between watching a sitting from now and keeping the
    /// hour of it that has already gone out. Without this flag that hour cannot
    /// be had at all once the broadcast ends.
    #[test]
    fn a_live_link_is_taken_from_the_start_of_the_broadcast() {
        for quality in FetchQuality::ALL {
            let live = live_args_for(quality);
            assert!(live.iter().any(|a| a == "--live-from-start"), "{quality:?}");
            // A finished video has no start to be taken from, and asking for one
            // makes yt-dlp refuse the link outright.
            let ordinary = args_for(quality);
            assert!(!ordinary.iter().any(|a| a == "--live-from-start"), "{quality:?}");
        }
    }

    /// A live broadcast declares no size, so the fragment counts are the only
    /// thing a bar can honestly be drawn from. They have to be asked for.
    #[test]
    fn the_progress_template_asks_how_many_pieces_there_are() {
        let joined = args_for(FetchQuality::P1080).join(" ");
        assert!(joined.contains("%(progress.fragment_index)s"), "got {joined}");
        assert!(joined.contains("%(progress.fragment_count)s"), "got {joined}");
    }

    /// A salvaged recording names its own file, so every rule
    /// `--windows-filenames` would have applied has to be applied here instead.
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
    fn a_second_salvaged_recording_does_not_overwrite_the_first() {
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
            let fetched = args(&job, Path::new("C:/clips/_download_temp_1"), None, false);
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
        let line = "VMPROG downloading 1048576 10485760 NA 524288.0 18 NA NA A Video Title";
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
        let line = "VMPROG downloading 1 2 NA 3 4 NA NA how to make bread - part 2";
        let parsed = Progress::parse(line).unwrap();
        assert_eq!(parsed.title.as_deref(), Some("how to make bread - part 2"));
    }

    /// Nothing is known on the first line of a fragmented stream, and a bar has
    /// to cope with that rather than showing a total of zero.
    #[test]
    fn unknown_fields_become_nothing_rather_than_zero() {
        let parsed = Progress::parse("VMPROG downloading 0 NA NA NA NA NA NA NA").unwrap();
        assert_eq!(parsed.done, 0);
        assert_eq!(parsed.total, None, "no total means no bar, not a bar at 0/0");
        assert_eq!(parsed.eta, None);
        assert_eq!(parsed.title, None);

        // An estimate stands in for a declared length when there is not one.
        let parsed = Progress::parse("VMPROG downloading 500 NA 4096 100 5 NA NA x").unwrap();
        assert_eq!(parsed.total, Some(4096));
    }

    /// The counts a live broadcast is measured by. It declares no size, so
    /// without these there is nothing to draw a bar from at all.
    #[test]
    fn the_pieces_of_a_live_stream_are_counted() {
        let line = "VMPROG downloading 51503359 NA NA 168546.7 NA 322 1877 A Sitting";
        let parsed = Progress::parse(line).expect("a progress line");
        assert_eq!(parsed.fragments, Some((322, 1877)));
        assert_eq!(parsed.total, None, "a live broadcast has no size to declare");
        assert_eq!(parsed.title.as_deref(), Some("A Sitting"));

        // Neither is any use without the other, and a position past the end is
        // not a position. All three would draw a bar that means nothing.
        for (index, count) in [("NA", "1877"), ("322", "NA"), ("322", "0"), ("1900", "1877")] {
            let line = format!("VMPROG downloading 1 NA NA 1 NA {index} {count} x");
            assert_eq!(Progress::parse(&line).unwrap().fragments, None, "{index}/{count}");
        }
    }

    /// yt-dlp numbers its progress lines when two downloads run at once, which
    /// is every live recording. Missing this threw away every report one made.
    #[test]
    fn a_numbered_progress_line_is_still_a_progress_line() {
        let plain = "VMPROG downloading 3089 NA NA 0 NA 322 1877 A Sitting";
        let numbered = "2: VMPROG downloading 3089 NA NA 0 NA 322 1877 A Sitting";
        // Identical in every respect but the number itself, which is the point:
        // the prefix must change which stream a line is about and nothing else.
        let with = Progress::parse(numbered).expect("the numbered form has to parse");
        let without = Progress::parse(plain).expect("the plain form has to parse");
        assert_eq!(Progress { stream: None, ..with.clone() }, without);
        assert_eq!(Progress::parse(&format!("10: {plain}")).unwrap().done, 3089);

        // The number says which stream, and is kept for that reason.
        assert_eq!(Progress::parse(numbered).unwrap().stream, Some(2));
        assert_eq!(Progress::parse(plain).unwrap().stream, None);

        // Only a number and a colon-space, so nothing that merely looks like one
        // takes a bite out of the line.
        assert_eq!(split_stream_number("2: VMPROG x"), (Some(2), "VMPROG x"));
        assert_eq!(split_stream_number("VMPROG x"), (None, "VMPROG x"));
        assert_eq!(split_stream_number("2:VMPROG x"), (None, "2:VMPROG x"));
        assert_eq!(split_stream_number("2 VMPROG x"), (None, "2 VMPROG x"));
        assert_eq!(split_stream_number(""), (None, ""));
    }

    /// Two streams arriving at once are one download. Reported as they come,
    /// every figure on screen flips between two unrelated ones.
    #[test]
    fn streams_arriving_together_are_added_up() {
        let line = |n: u64, done: u64, at: u64, of: u64| {
            Progress::parse(&format!("{n}: VMPROG downloading {done} NA NA 1000 NA {at} {of} T"))
                .expect("a progress line")
        };
        let mut streams = BTreeMap::new();
        streams.insert(1, line(1, 52_000_000, 560, 1877));
        streams.insert(2, line(2, 37_000_000, 322, 1877));

        let Some(FetchEvent::Progress { done, total, rate, eta, fragments }) = combined(&streams)
        else {
            panic!("two streams have a combined position");
        };
        assert_eq!(done, 89_000_000, "the bytes are the two of them together");
        assert_eq!(rate, 2000.0);
        assert_eq!(total, None, "neither declared a size, so there is no total");
        assert_eq!(eta, None);
        // The lower of the two: what can be salvaged is bounded by whichever
        // stream has less of the sitting, not by the one that is ahead.
        assert_eq!(fragments, Some((322, 1877)));

        assert!(combined(&BTreeMap::new()).is_none(), "nothing yet is not a position");
    }

    #[test]
    fn anything_that_is_not_a_progress_line_is_ignored() {
        for line in [
            "",
            "[download] Destination: video.mp4",
            "VMPROG something-else 1 2 3 4 5 6 7 x",
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
        let parsed = Progress::parse("VMPROG finished 10485760 10485760 NA NA NA NA NA Clip").unwrap();
        assert_eq!(parsed.status, Status::Finished);
        assert_eq!(parsed.done, parsed.total.unwrap());
    }

    #[test]
    fn a_height_cap_is_a_hard_filter() {
        // Someone who picks 720p gets 720p. A preference would quietly hand back
        // 4K whenever the site happened to offer it.
        assert!(FetchQuality::P720.selector().contains("height<=720"));
        assert!(!FetchQuality::Best.selector().contains("height"));

        // Every named resolution is a cap of its own name, and they descend, so
        // asking for less can never come back with more.
        let caps: Vec<u32> = FetchQuality::ALL
            .iter()
            .filter_map(|q| {
                let s = q.selector();
                let at = s.find("height<=")? + "height<=".len();
                s[at..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
            })
            .collect();
        assert_eq!(caps, [2160, 1080, 720, 480], "got {caps:?}");
    }

    /// 4K exists to be asked for by name. It was always reachable through "best
    /// available", which is not the same as being offered.
    #[test]
    fn four_k_is_offered_by_name() {
        assert!(FetchQuality::ALL.contains(&FetchQuality::P2160));
        assert_eq!(FetchQuality::P2160.label(), "4K");
        assert!(FetchQuality::P2160.selector().contains("height<=2160"));
        assert!(!FetchQuality::P2160.is_audio());
        // It is the second choice, right below "best": the list reads downwards
        // from most to least, and a resolution out of order would be a trap.
        assert_eq!(FetchQuality::ALL[1], FetchQuality::P2160);
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
