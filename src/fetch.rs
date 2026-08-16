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

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use clap::ValueEnum;

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
    Downloading,
    /// Every byte is in; ffmpeg is putting the streams together.
    Finishing,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Setup => "getting ready",
            Stage::Downloading => "downloading",
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

    emit(FetchEvent::Stage(Stage::Downloading));
    emit(FetchEvent::Progress { done: 0, total: None, rate: 0.0, eta: None });

    let mut result = download(&tool.path, job, &temp_dir, cancel, emit);
    let mut attempt = 1;
    while attempt < ATTEMPTS
        && !cancel.load(Ordering::Relaxed)
        && result.as_ref().err().is_some_and(|e| worth_retrying(e))
    {
        attempt += 1;
        emit(FetchEvent::Note(format!(
            "The site refused that one. Asking again for fresh links ({attempt} of {ATTEMPTS})."
        )));
        emit(FetchEvent::Stage(Stage::Downloading));
        result = download(&tool.path, job, &temp_dir, cancel, emit);
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

/// The arguments, kept apart from the running so they can be checked without a
/// network.
fn args(job: &Job, temp_dir: &Path, ffmpeg_dir: Option<&Path>) -> Vec<String> {
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
        "-f".into(),
        job.quality.selector().into(),
    ];

    if job.quality.is_audio() {
        args.extend(["-x".to_string(), "--audio-format".into(), "m4a".into()]);
    } else {
        args.extend(["--merge-output-format".to_string(), "mp4".into()]);
        // Resolution first, or this would happily prefer a 360p H.264 stream
        // over a 1080p VP9 one. Within a resolution, H.264 in an mp4 is what
        // the merge side of this program joins without re-encoding anything.
        args.extend(["-S".to_string(), "res,vcodec:h264,acodec:aac,ext:mp4:m4a".into()]);
    }

    if let Some(dir) = ffmpeg_dir {
        // The copy this program already installed, rather than whatever may or
        // may not be on PATH.
        args.extend(["--ffmpeg-location".to_string(), dir.display().to_string()]);
    }

    // YouTube hands out video links behind a JavaScript challenge, and yt-dlp
    // has to run an engine to answer it. It enables deno by itself and nothing
    // else, so a machine with Node installed - which is most of them - is
    // treated as having no engine at all, and the download dies with
    // `HTTP Error 403: Forbidden`. Naming what is here costs one flag.
    for runtime in js_runtimes() {
        args.extend(["--js-runtimes".to_string(), runtime.to_string()]);
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

    let mut title: Option<String> = None;
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
