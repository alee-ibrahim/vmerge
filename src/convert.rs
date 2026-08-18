//! Converting files from one format into another.
//!
//! A separate engine from `merge`, because the two jobs are different shapes: a
//! merge is many inputs and one output, a conversion is one output per input.
//! What the two share is the vocabulary - `MergeEvent`, `Step`, `Outcome` - so
//! the progress screen, the plain-text one-shot path and the final report all
//! work for both without a second copy of each.
//!
//! Two rules decide what happens to a file:
//!
//! * **The container changes without the streams being touched whenever that is
//!   possible.** An H.264 + AAC mp4 becoming an mkv is a remux: seconds, and not
//!   a pixel altered. That is the whole reason "convert to mkv" is worth having
//!   as something other than a re-encode.
//! * **Nothing is resized or re-timed.** Only the format changes, so the picture
//!   comes out the size it went in and every frame is still there. GIF is the one
//!   exception - it is a still-image format pressed into service as video, and 12
//!   fps at 640 wide is the difference between a file that can be posted and one
//!   that cannot - and it says so in the picker.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use clap::ValueEnum;

use crate::encoder::{self, EncoderChoice, EncoderPref, Quality};
use crate::ffmpeg::Tools;
use crate::merge::{MergeEvent, Outcome, Step, run_ffmpeg};
use crate::probe::{self, ClipInfo};

/// GIF is a still-image format, so a full-size 30 fps one is enormous. These are
/// the numbers that keep the file postable.
const GIF_FPS: u32 = 12;
const GIF_WIDTH: u32 = 640;

/// What a file can be turned into.
///
/// One entry per format rather than a free-form "codec plus container" pair: the
/// combinations that actually work are a short list, and picking "MKV" is a
/// decision a person can make. Picking "matroska + libx264 + libopus" is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Target {
    Mp4,
    Mkv,
    Mov,
    Webm,
    Avi,
    Ts,
    Gif,
    Mp3,
    M4a,
    Wav,
    Flac,
    Opus,
}

impl Target {
    pub const ALL: [Target; 12] = [
        Target::Mp4,
        Target::Mkv,
        Target::Mov,
        Target::Webm,
        Target::Avi,
        Target::Ts,
        Target::Gif,
        Target::Mp3,
        Target::M4a,
        Target::Wav,
        Target::Flac,
        Target::Opus,
    ];

    /// The extension, which is also the name on the command line.
    pub fn ext(self) -> &'static str {
        match self {
            Target::Mp4 => "mp4",
            Target::Mkv => "mkv",
            Target::Mov => "mov",
            Target::Webm => "webm",
            Target::Avi => "avi",
            Target::Ts => "ts",
            Target::Gif => "gif",
            Target::Mp3 => "mp3",
            Target::M4a => "m4a",
            Target::Wav => "wav",
            Target::Flac => "flac",
            Target::Opus => "opus",
        }
    }

    /// What it is, for the picker: the name first, then what is inside it.
    pub fn label(self) -> &'static str {
        match self {
            Target::Mp4 => "MP4    H.264 + AAC",
            Target::Mkv => "MKV    Matroska, takes any codec",
            Target::Mov => "MOV    QuickTime",
            Target::Webm => "WEBM   VP9 + Opus",
            Target::Avi => "AVI    MPEG-4 + MP3",
            Target::Ts => "TS     MPEG transport stream",
            Target::Gif => "GIF    silent animation",
            Target::Mp3 => "MP3    audio only",
            Target::M4a => "M4A    AAC audio only",
            Target::Wav => "WAV    uncompressed audio",
            Target::Flac => "FLAC   lossless audio",
            Target::Opus => "OPUS   audio only",
        }
    }

    /// What choosing it costs or buys.
    pub fn note(self) -> &'static str {
        match self {
            Target::Mp4 => "plays anywhere",
            Target::Mkv => "usually a remux, not a re-encode",
            Target::Mov => "for Premiere and Final Cut",
            Target::Webm => "small, for the web - slow to encode",
            Target::Avi => "only for software old enough to need it",
            Target::Ts => "broadcast; what a merge uses inside",
            Target::Gif => "12 fps, 640 wide, no sound",
            Target::Mp3 => "plays on anything",
            Target::M4a => "smaller than MP3 at the same quality",
            Target::Wav => "big, lossless, for editing",
            Target::Flac => "lossless, about half the size of WAV",
            Target::Opus => "the smallest of these at one quality",
        }
    }

    /// Whether the format carries a picture at all. The audio-only ones drop it.
    pub fn keeps_video(self) -> bool {
        !matches!(self, Target::Mp3 | Target::M4a | Target::Wav | Target::Flac | Target::Opus)
    }

    /// GIF is the odd one out: it has a picture and no sound.
    pub fn keeps_audio(self) -> bool {
        self != Target::Gif
    }

    /// Video codecs this container will carry as they are.
    ///
    /// Deliberately not "everything ffmpeg will let you mux". AVI will hold
    /// H.264 and the result plays in VLC, but the only reason to ask for an AVI
    /// is software that cannot read anything newer - so H.264 into AVI is a
    /// re-encode here, not a copy.
    fn copyable_video(self) -> &'static [&'static str] {
        match self {
            // Matroska is the container that takes whatever it is given.
            Target::Mkv => &["*"],
            Target::Mp4 => &["h264", "hevc", "av1", "vp9", "mpeg4"],
            Target::Mov => &["h264", "hevc", "prores", "mpeg4"],
            Target::Webm => &["vp8", "vp9", "av1"],
            Target::Avi => &["mpeg4", "msmpeg4v3", "mjpeg"],
            Target::Ts => &["h264", "hevc", "mpeg2video"],
            _ => &[],
        }
    }

    /// Audio codecs this container will carry as they are.
    fn copyable_audio(self) -> &'static [&'static str] {
        match self {
            Target::Mkv => &["*"],
            Target::Mp4 | Target::M4a => &["aac", "mp3", "ac3", "eac3", "alac"],
            Target::Mov => &["aac", "mp3", "alac", "pcm_s16le"],
            Target::Webm | Target::Opus => &["opus"],
            Target::Avi | Target::Mp3 => &["mp3"],
            Target::Ts => &["aac", "mp3", "ac3"],
            Target::Wav => &["pcm_s16le"],
            Target::Flac => &["flac"],
            Target::Gif => &[],
        }
    }

    /// The video encoder used when a re-encode is needed. `None` where there is
    /// no picture to encode.
    fn video_encoder(self, h264: &EncoderChoice) -> Option<String> {
        match self {
            Target::Mp4 | Target::Mkv | Target::Mov | Target::Ts => Some(h264.name.clone()),
            Target::Webm => Some("libvpx-vp9".into()),
            Target::Avi => Some("mpeg4".into()),
            Target::Gif => Some("gif".into()),
            _ => None,
        }
    }

    /// Whether a re-encode into this format would go through the H.264 encoder,
    /// which is the only one a GPU is ever asked about.
    fn uses_h264(self) -> bool {
        matches!(self, Target::Mp4 | Target::Mkv | Target::Mov | Target::Ts)
    }

    fn audio_encoder(self) -> Option<&'static str> {
        match self {
            Target::Mp4 | Target::Mkv | Target::Mov | Target::Ts | Target::M4a => Some("aac"),
            Target::Webm | Target::Opus => Some("libopus"),
            Target::Avi | Target::Mp3 => Some("libmp3lame"),
            Target::Wav => Some("pcm_s16le"),
            Target::Flac => Some("flac"),
            Target::Gif => None,
        }
    }

    /// Muxer flags that belong to the container rather than to a codec.
    fn container_args(self) -> Vec<String> {
        match self {
            // Puts the index at the front, so the file starts playing before it
            // has all arrived - which is what a browser needs.
            Target::Mp4 | Target::Mov | Target::M4a => {
                vec!["-movflags".into(), "+faststart".into()]
            }
            // Without this a GIF plays once and stops.
            Target::Gif => vec!["-loop".into(), "0".into()],
            _ => Vec::new(),
        }
    }
}

/// Whether a codec is on a container's list. A `*` means the container takes
/// anything, which is Matroska and only Matroska.
fn accepts(list: &[&str], codec: &str) -> bool {
    list.contains(&"*") || list.contains(&codec)
}

/// What is going to happen to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Move {
    /// The streams already suit the container: rewrite the wrapper and nothing
    /// else. Seconds, and no quality loss at all.
    Copy,
    Encode,
    /// Nothing sensible to do. The reason is reported rather than the file being
    /// silently left out.
    Skip(&'static str),
}

/// Decides what one file needs, before anything runs - which is what lets the
/// screen say "3 of 5 need re-encoding" up front instead of finding out on the
/// way through.
pub fn decide(clip: &ClipInfo, target: Target, force_reencode: bool) -> Move {
    if target.keeps_video() && !clip.has_video {
        return Move::Skip("has no picture - convert it to an audio format instead");
    }
    if !target.keeps_video() && !clip.has_audio {
        return Move::Skip("has no sound, so there is nothing to write");
    }

    if force_reencode {
        return Move::Encode;
    }

    if target.keeps_video() {
        let video_fits = accepts(target.copyable_video(), &clip.video_codec);
        // A silent clip has no audio stream to be a problem, and a GIF discards
        // the one it has.
        let audio_fits = !clip.has_audio
            || !target.keeps_audio()
            || accepts(target.copyable_audio(), &clip.audio_codec);
        if video_fits && audio_fits { Move::Copy } else { Move::Encode }
    } else if accepts(target.copyable_audio(), &clip.audio_codec) {
        Move::Copy
    } else {
        Move::Encode
    }
}

/// Where one converted file goes: beside the original, same name, new extension.
///
/// Never over the original, and never over an earlier conversion - converting a
/// folder twice should leave both results rather than quietly replacing the
/// first. It is also what stops mp4 -> mp4, a deliberate re-encode, from handing
/// ffmpeg the file it is reading.
pub fn output_for(source: &Path, target: Target) -> PathBuf {
    let folder = source.parent().unwrap_or(Path::new("."));
    let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut candidate = folder.join(format!("{stem}.{}", target.ext()));
    let mut n = 2;
    while candidate.exists() {
        candidate = folder.join(format!("{stem}_{n}.{}", target.ext()));
        n += 1;
    }
    candidate
}

pub struct Job {
    pub tools: Arc<Tools>,
    pub clips: Vec<ClipInfo>,
    pub target: Target,
    pub quality: Quality,
    pub encoder: EncoderPref,
    pub force_reencode: bool,
}

/// Runs the conversion on a worker thread, reporting through the merge screen's
/// event type so there is one progress screen rather than two.
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

/// Converts each file in turn, keeping whatever finishes.
///
/// One bad file does not end the batch: a folder of forty clips with one
/// truncated download in it should come out as thirty-nine conversions and a
/// note, not as nothing.
pub fn run(job: &Job, cancel: &AtomicBool, emit: &mut dyn FnMut(MergeEvent)) -> Outcome {
    let started = Instant::now();
    let folder = job
        .clips
        .first()
        .and_then(|c| c.path.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut outcome = Outcome {
        ok: false,
        output: folder,
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
        outcome.error = Some("There are no files to convert.".into());
        return outcome;
    }

    let moves: Vec<Move> =
        job.clips.iter().map(|c| decide(c, job.target, job.force_reencode)).collect();
    let copies = moves.iter().filter(|m| **m == Move::Copy).count();
    let encodes = moves.iter().filter(|m| **m == Move::Encode).count();
    let skips = moves.len() - copies - encodes;

    // Probing a GPU encoder means test-encoding with each candidate, which takes
    // a moment. Say so, or the screen looks stalled. Only H.264 targets ask: VP9,
    // MPEG-4, GIF and every audio format have one encoder each.
    let h264 = if encodes > 0 && job.target.uses_h264() {
        if job.encoder == EncoderPref::Auto {
            emit(MergeEvent::Plan("Checking which encoder works here...".into()));
        }
        encoder::select(&job.tools.ffmpeg, job.encoder)
    } else {
        EncoderChoice { name: "libx264".into(), label: "CPU (libx264)".into() }
    };

    for line in plan_lines(job, copies, encodes, skips, &h264) {
        emit(MergeEvent::Plan(line));
    }
    emit(MergeEvent::Pass { total: job.clips.len(), attempt: 1 });

    let mut written: Vec<PathBuf> = Vec::new();
    let mut stopped_after = 0usize;

    for (index, (clip, planned)) in job.clips.iter().zip(moves.iter()).enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        stopped_after = index;

        if let Move::Skip(why) = planned {
            emit(MergeEvent::SegmentStart {
                index,
                name: clip.name.clone(),
                step: Step::Convert,
                duration: clip.duration,
            });
            emit(MergeEvent::SegmentEnd { index, step: Step::Convert, ok: false, elapsed: 0.0 });
            note(format!("Left out {} - it {why}.", clip.name), emit, &mut outcome.warnings);
            continue;
        }

        let copying = *planned == Move::Copy;
        let step = if copying { Step::Copy } else { Step::Convert };
        emit(MergeEvent::SegmentStart {
            index,
            name: clip.name.clone(),
            step,
            duration: clip.duration,
        });

        let output = output_for(&clip.path, job.target);
        let clip_started = Instant::now();
        let mut report = |done: f64| emit(MergeEvent::SegmentProgress { index, done });

        let encode = || encode_args(clip, job.target, job.quality, &h264, &output);
        let mut result = if copying {
            run_ffmpeg(&job.tools.ffmpeg, &copy_args(clip, job.target, &output), cancel, &mut report)
        } else {
            run_ffmpeg(&job.tools.ffmpeg, &encode(), cancel, &mut report)
        };

        // A remux that will not go through is not a dead end: the streams can be
        // re-encoded into something the container definitely accepts.
        let mut fell_back = false;
        if result.is_err() && copying && !cancel.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&output);
            fell_back = true;
            result = run_ffmpeg(&job.tools.ffmpeg, &encode(), cancel, &mut report);
        }

        let wrote_something = result.is_ok() && output.is_file();
        emit(MergeEvent::SegmentEnd {
            index,
            step: if fell_back { Step::Convert } else { step },
            ok: wrote_something,
            elapsed: clip_started.elapsed().as_secs_f64(),
        });

        if cancel.load(Ordering::Relaxed) {
            // A file cut off part way through is worse than no file: it looks
            // playable and is not.
            let _ = fs::remove_file(&output);
            break;
        }

        match result {
            Ok(()) if wrote_something => {
                if fell_back {
                    note(
                        format!("{} could not be remuxed, so it was re-encoded.", clip.name),
                        emit,
                        &mut outcome.warnings,
                    );
                }
                outcome.size += fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
                written.push(output);
                stopped_after = index + 1;
            }
            Ok(()) => {
                let _ = fs::remove_file(&output);
                note(format!("{} produced nothing usable.", clip.name), emit, &mut outcome.warnings);
            }
            Err(e) => {
                let _ = fs::remove_file(&output);
                note(
                    format!("Could not convert {} ({e}).", clip.name),
                    emit,
                    &mut outcome.warnings,
                );
            }
        }
    }

    outcome.elapsed = started.elapsed().as_secs_f64();
    outcome.cancelled = cancel.load(Ordering::Relaxed);

    if outcome.cancelled && stopped_after < job.clips.len() {
        note(
            format!("Stopped after {stopped_after} of {} files.", job.clips.len()),
            emit,
            &mut outcome.warnings,
        );
    }

    // Files that finished are files the user has, whether or not the rest ran.
    // Reporting a stopped batch as a loss would be a lie about what is on disk.
    outcome.ok = !written.is_empty();
    if !outcome.ok {
        outcome.error = Some(if outcome.cancelled {
            "Stopped before anything was written.".into()
        } else {
            "None of the files could be converted.".into()
        });
        return outcome;
    }

    outcome.output = written[0].clone();
    if written.len() == 1 {
        // One file has a shape and a length worth reporting, the same as a merge.
        match probe::clip_info(&job.tools.ffprobe, &written[0]) {
            Some(info) => {
                outcome.out_duration = info.duration;
                if info.has_video {
                    outcome.out_format = Some((info.width, info.height, info.fps));
                }
            }
            None => outcome.out_duration = probe::duration_of(&job.tools.ffprobe, &written[0]),
        }
    } else {
        outcome.out_duration =
            written.iter().map(|p| probe::duration_of(&job.tools.ffprobe, p)).sum();
    }
    outcome.outputs = written;
    outcome
}

/// Records a warning and shows it straight away, rather than saving every
/// problem for a summary the user only reaches minutes later.
fn note(text: String, emit: &mut dyn FnMut(MergeEvent), warnings: &mut Vec<String>) {
    emit(MergeEvent::Warning(text.clone()));
    warnings.push(text);
}

/// The two lines that say what is about to happen. Kept pure, so the wording can
/// be checked without running ffmpeg.
pub fn plan_lines(
    job: &Job,
    copies: usize,
    encodes: usize,
    skips: usize,
    h264: &EncoderChoice,
) -> Vec<String> {
    let total = job.clips.len();
    let doing = total - skips;
    let ext = job.target.ext().to_uppercase();
    let mut lines = vec![if skips == 0 {
        format!(
            "{total} file{} to {ext}, {}written beside the original.",
            if total == 1 { "" } else { "s" },
            if total == 1 { "" } else { "each " }
        )
    } else {
        let other = if skips == 1 { "one".to_string() } else { skips.to_string() };
        format!("{doing} of {total} files to {ext} - the other {other} cannot be.")
    }];

    let with = match job.target {
        Target::Webm => "VP9 + Opus".to_string(),
        Target::Avi => "MPEG-4 + MP3".to_string(),
        Target::Gif => "the GIF encoder".to_string(),
        Target::Mp3 => "MP3".to_string(),
        Target::M4a => "AAC".to_string(),
        Target::Wav => "16-bit PCM".to_string(),
        Target::Flac => "FLAC".to_string(),
        Target::Opus => "Opus".to_string(),
        _ => format!("{} + AAC", h264.label),
    };

    if doing == 0 {
        lines.push(format!("Not one of them can become {ext}."));
    } else if encodes == 0 {
        lines.push(format!("Nothing needs re-encoding - the streams already fit {ext}."));
    } else if copies > 0 {
        lines.push(format!(
            "{copies} {} remuxed as {}; {encodes} {} re-encoded with {with}, quality {}.",
            if copies == 1 { "is" } else { "are" },
            if copies == 1 { "it is" } else { "they are" },
            if encodes == 1 { "is" } else { "are" },
            job.quality.label()
        ));
    } else {
        lines.push(format!("Re-encoding with {with}, quality {}.", job.quality.label()));
    }
    lines
}

/// The container swap: read the streams, write them out untouched.
fn copy_args(clip: &ClipInfo, target: Target, output: &Path) -> Vec<String> {
    let mut args = vec!["-i".to_string(), clip.path.display().to_string()];
    if target.keeps_video() {
        args.extend(["-map".to_string(), "0:v:0".into()]);
        if clip.has_audio && target.keeps_audio() {
            args.extend(["-map".to_string(), "0:a:0".into()]);
        }
        args.extend(["-c".to_string(), "copy".into()]);
    } else {
        // The cover image inside an mp3 is a video stream, and copying it into a
        // wav is an error rather than a nicety.
        args.extend(["-map".to_string(), "0:a:0".into(), "-vn".into()]);
        args.extend(["-c:a".to_string(), "copy".into()]);
    }
    args.extend(target.container_args());
    args.extend(["--".to_string(), output.display().to_string()]);
    args
}

/// The re-encode: whatever the source is, out it comes as the target format.
fn encode_args(
    clip: &ClipInfo,
    target: Target,
    quality: Quality,
    h264: &EncoderChoice,
    output: &Path,
) -> Vec<String> {
    let mut args = vec!["-i".to_string(), clip.path.display().to_string()];

    if target == Target::Gif {
        // One pass would mean 256 colours chosen without ever looking at the
        // clip; two passes over the same decode - a palette built from these
        // frames, then applied to them - is the difference between a gradient and
        // a poster.
        args.extend([
            "-filter_complex".to_string(),
            format!(
                "fps={GIF_FPS},scale=w='min({GIF_WIDTH}\\,iw)':h=-1:flags=lanczos,\
                 split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer"
            ),
            "-an".into(),
        ]);
        args.extend(target.container_args());
        args.extend(["--".to_string(), output.display().to_string()]);
        return args;
    }

    if target.keeps_video() {
        args.extend(["-map".to_string(), "0:v:0".into()]);
        if clip.has_audio {
            args.extend(["-map".to_string(), "0:a:0".into()]);
        }
        // Odd dimensions are legal in a source and rejected by every encoder
        // here, and a 10-bit source encoded as H.264 High 10 plays on very
        // little. Neither is a resize: the picture keeps its size and its shape.
        args.extend([
            "-vf".to_string(),
            "scale=trunc(iw/2)*2:trunc(ih/2)*2".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
        ]);
        let name = target.video_encoder(h264).unwrap_or_else(|| h264.name.clone());
        args.extend(["-c:v".to_string(), name.clone()]);
        args.extend(video_quality_args(target, &name, quality));
    } else {
        args.extend(["-map".to_string(), "0:a:0".into(), "-vn".into()]);
    }

    if clip.has_audio && target.keeps_audio() {
        args.extend(audio_args(target, quality));
    } else if target.keeps_video() {
        args.push("-an".to_string());
    }

    args.extend(target.container_args());
    args.extend(["--".to_string(), output.display().to_string()]);
    args
}

/// Quality flags per encoder. The scales have nothing to do with one another:
/// H.264's CRF runs 0-51, VP9's 0-63, and MPEG-4 wants a 1-31 quantiser.
fn video_quality_args(target: Target, encoder_name: &str, quality: Quality) -> Vec<String> {
    match target {
        Target::Webm => {
            let crf = match quality {
                Quality::VisuallyLossless => 24,
                Quality::High => 31,
                Quality::Medium => 36,
                Quality::Small => 42,
            };
            // -b:v 0 is what makes -crf a quality target rather than a ceiling on
            // a bitrate nobody set. row-mt and cpu-used are the difference
            // between VP9 being slow and being unusable.
            vec![
                "-b:v".into(),
                "0".into(),
                "-crf".into(),
                crf.to_string(),
                "-row-mt".into(),
                "1".into(),
                "-deadline".into(),
                "good".into(),
                "-cpu-used".into(),
                "4".into(),
            ]
        }
        Target::Avi => {
            let q = match quality {
                Quality::VisuallyLossless => 2,
                Quality::High => 4,
                Quality::Medium => 7,
                Quality::Small => 12,
            };
            vec!["-qscale:v".into(), q.to_string()]
        }
        _ => encoder::quality_args(encoder_name, quality),
    }
}

/// Audio flags per format. WAV and FLAC ignore quality by their nature: one is
/// uncompressed and the other is lossless, so there is nothing to trade.
fn audio_args(target: Target, quality: Quality) -> Vec<String> {
    let encoder = target.audio_encoder().unwrap_or("aac");
    let mut args = vec!["-c:a".to_string(), encoder.to_string()];
    match encoder {
        "aac" => {
            let rate = match quality {
                Quality::VisuallyLossless => "256k",
                Quality::High => "192k",
                Quality::Medium => "160k",
                Quality::Small => "128k",
            };
            args.extend(["-b:a".to_string(), rate.into()]);
        }
        "libmp3lame" => {
            // LAME's own VBR scale, where 0 is best and 9 is smallest.
            let q = match quality {
                Quality::VisuallyLossless => 0,
                Quality::High => 2,
                Quality::Medium => 4,
                Quality::Small => 6,
            };
            args.extend(["-q:a".to_string(), q.to_string()]);
        }
        "libopus" => {
            let rate = match quality {
                Quality::VisuallyLossless => "160k",
                Quality::High => "128k",
                Quality::Medium => "96k",
                Quality::Small => "64k",
            };
            args.extend(["-b:a".to_string(), rate.into()]);
        }
        "flac" => args.extend(["-compression_level".to_string(), "5".into()]),
        _ => {}
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(name: &str, video: &str, audio: Option<&str>) -> ClipInfo {
        ClipInfo {
            path: PathBuf::from(format!("C:/clips/{name}")),
            name: name.into(),
            has_video: !video.is_empty(),
            video_codec: video.into(),
            width: 1920,
            height: 1080,
            pix_fmt: "yuv420p".into(),
            fps: 30.0,
            frame_rate_raw: "30/1".into(),
            rotation: 0,
            has_audio: audio.is_some(),
            audio_codec: audio.unwrap_or("none").into(),
            sample_rate: 48_000,
            channels: 2,
            duration: 10.0,
            size_bytes: 1024,
        }
    }

    fn cpu() -> EncoderChoice {
        EncoderChoice { name: "libx264".into(), label: "CPU (libx264)".into() }
    }

    #[test]
    fn a_container_swap_is_a_copy_and_not_a_re_encode() {
        let mp4 = clip("a.mp4", "h264", Some("aac"));
        assert_eq!(decide(&mp4, Target::Mkv, false), Move::Copy);
        assert_eq!(decide(&mp4, Target::Mov, false), Move::Copy);
        assert_eq!(decide(&mp4, Target::Ts, false), Move::Copy);

        let args = copy_args(&mp4, Target::Mkv, Path::new("C:/clips/a.mkv"));
        assert!(args.windows(2).any(|w| w == ["-c", "copy"]), "{args:?}");
    }

    #[test]
    fn a_container_that_will_not_take_the_codec_re_encodes() {
        let mp4 = clip("a.mp4", "h264", Some("aac"));
        // WebM carries VP8, VP9 and AV1 with Opus or Vorbis - none of which this
        // is.
        assert_eq!(decide(&mp4, Target::Webm, false), Move::Encode);
        // AVI would hold H.264, but then it would not be doing the job AVI gets
        // asked for.
        assert_eq!(decide(&mp4, Target::Avi, false), Move::Encode);
        // A GIF is always built, never copied.
        assert_eq!(decide(&mp4, Target::Gif, false), Move::Encode);
        // A VP9 clip is the other way round.
        let webm = clip("b.webm", "vp9", Some("opus"));
        assert_eq!(decide(&webm, Target::Webm, false), Move::Copy);
        assert_eq!(decide(&webm, Target::Mov, false), Move::Encode);
    }

    #[test]
    fn matching_audio_comes_out_of_a_video_untouched() {
        let mp4 = clip("a.mp4", "h264", Some("aac"));
        // Already AAC, so pulling it into an .m4a touches no samples.
        assert_eq!(decide(&mp4, Target::M4a, false), Move::Copy);
        assert_eq!(decide(&mp4, Target::Mp3, false), Move::Encode);
        assert_eq!(decide(&mp4, Target::Wav, false), Move::Encode);

        let args = copy_args(&mp4, Target::M4a, Path::new("C:/clips/a.m4a"));
        assert!(args.contains(&"-vn".to_string()), "no picture in an m4a: {args:?}");
    }

    #[test]
    fn forcing_a_re_encode_overrides_a_perfectly_good_copy() {
        let mp4 = clip("a.mp4", "h264", Some("aac"));
        assert_eq!(decide(&mp4, Target::Mkv, true), Move::Encode);
    }

    #[test]
    fn a_file_without_the_stream_the_format_needs_is_left_out() {
        let music = clip("song.mp3", "", Some("mp3"));
        assert!(matches!(decide(&music, Target::Mp4, false), Move::Skip(_)));
        // Audio to audio is exactly what it is for.
        assert_eq!(decide(&music, Target::Mp3, false), Move::Copy);
        assert_eq!(decide(&music, Target::Flac, false), Move::Encode);

        let silent = clip("timelapse.mp4", "h264", None);
        assert!(matches!(decide(&silent, Target::Mp3, false), Move::Skip(_)));
        // A silent video is still a video: it converts, and stays silent.
        assert_eq!(decide(&silent, Target::Mkv, false), Move::Copy);
    }

    #[test]
    fn a_silent_clip_is_never_given_an_audio_encoder() {
        let silent = clip("timelapse.mp4", "h264", None);
        let args = encode_args(&silent, Target::Mp4, Quality::High, &cpu(), Path::new("out.mp4"));
        assert!(args.contains(&"-an".to_string()), "{args:?}");
        assert!(!args.contains(&"aac".to_string()), "{args:?}");
    }

    #[test]
    fn the_gif_pass_builds_its_own_palette() {
        let mp4 = clip("a.mp4", "h264", Some("aac"));
        let args = encode_args(&mp4, Target::Gif, Quality::High, &cpu(), Path::new("a.gif"));
        let filter = args.iter().find(|a| a.contains("palettegen")).expect("a palette pass");
        assert!(filter.contains("paletteuse"), "{filter}");
        assert!(filter.contains(&format!("fps={GIF_FPS}")), "{filter}");
        // The comma inside min() has to be escaped, or ffmpeg reads it as the end
        // of the scale filter and the chain falls apart.
        assert!(filter.contains("min(640\\,iw)"), "{filter}");
        assert!(args.contains(&"-an".to_string()), "a GIF has no sound: {args:?}");
        assert!(args.windows(2).any(|w| w == ["-loop", "0"]), "and it loops: {args:?}");
    }

    #[test]
    fn each_format_gets_the_encoder_that_belongs_to_it() {
        let mp4 = clip("a.mp4", "h264", Some("aac"));
        let gpu = EncoderChoice { name: "h264_nvenc".into(), label: "GPU (h264_nvenc)".into() };

        let webm = encode_args(&mp4, Target::Webm, Quality::High, &gpu, Path::new("a.webm"));
        assert!(webm.contains(&"libvpx-vp9".to_string()), "{webm:?}");
        assert!(webm.contains(&"libopus".to_string()), "{webm:?}");
        // The GPU's H.264 encoder has nothing to do with a VP9 target.
        assert!(!webm.contains(&"h264_nvenc".to_string()), "{webm:?}");

        let mp4_args = encode_args(&mp4, Target::Mp4, Quality::High, &gpu, Path::new("b.mp4"));
        assert!(mp4_args.contains(&"h264_nvenc".to_string()), "{mp4_args:?}");
        assert!(mp4_args.contains(&"+faststart".to_string()), "{mp4_args:?}");

        let wav = encode_args(&mp4, Target::Wav, Quality::High, &gpu, Path::new("c.wav"));
        assert!(wav.contains(&"pcm_s16le".to_string()), "{wav:?}");
        assert!(wav.contains(&"-vn".to_string()), "{wav:?}");
        assert!(!wav.contains(&"-c:v".to_string()), "{wav:?}");
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_conversion_never_writes_over_its_own_source() {
        let dir = std::env::temp_dir().join(format!("vmerge-convert-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("clip.mp4");
        fs::write(&source, b"x").unwrap();

        // mp4 -> mp4 is a legitimate re-encode, and the obvious name is taken by
        // the file being read.
        assert_eq!(output_for(&source, Target::Mp4), dir.join("clip_2.mp4"));
        assert_eq!(output_for(&source, Target::Mkv), dir.join("clip.mkv"));

        fs::write(dir.join("clip.mkv"), b"x").unwrap();
        assert_eq!(output_for(&source, Target::Mkv), dir.join("clip_2.mkv"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_plan_says_what_will_be_copied_and_what_will_not() {
        let tools = Arc::new(Tools {
            ffmpeg: PathBuf::from("ffmpeg.exe"),
            ffprobe: PathBuf::from("ffprobe.exe"),
        });
        let job = Job {
            tools,
            clips: vec![clip("a.mp4", "h264", Some("aac")), clip("b.webm", "vp9", Some("opus"))],
            target: Target::Mkv,
            quality: Quality::High,
            encoder: EncoderPref::Cpu,
            force_reencode: false,
        };

        let all_copied = plan_lines(&job, 2, 0, 0, &cpu()).join(" ");
        assert!(all_copied.contains("2 files to MKV"), "{all_copied}");
        assert!(all_copied.contains("Nothing needs re-encoding"), "{all_copied}");

        let mixed = plan_lines(&job, 1, 1, 0, &cpu()).join(" ");
        assert!(mixed.contains("1 is remuxed"), "{mixed}");
        assert!(mixed.contains("CPU (libx264)"), "{mixed}");
        assert!(mixed.contains("quality high"), "{mixed}");

        // A file that cannot become this format at all is counted out of the
        // total up front, rather than the plan promising two and writing one.
        let with_a_skip = plan_lines(&job, 1, 0, 1, &cpu()).join(" ");
        assert!(with_a_skip.contains("1 of 2 files to MKV"), "{with_a_skip}");
        assert!(with_a_skip.contains("the other one cannot be"), "{with_a_skip}");

        // One file is one file, not "each" of one.
        let single = Job { clips: vec![clip("a.mp4", "h264", Some("aac"))], ..job };
        let one = plan_lines(&single, 1, 0, 0, &cpu()).join(" ");
        assert!(one.contains("1 file to MKV, written beside"), "{one}");
    }
}
