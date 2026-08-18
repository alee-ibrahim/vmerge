//! Reading a clip's format with ffprobe. Ported from Get-ClipInfo.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::proc;

#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub path: PathBuf,
    pub name: String,
    /// Whether there is a picture at all. False for an mp3 or an m4a, which are
    /// perfectly good inputs for a conversion and no use at all to a merge.
    pub has_video: bool,
    pub video_codec: String,
    pub width: u32,
    pub height: u32,
    pub pix_fmt: String,
    /// The rate to use for decisions and display.
    pub fps: f64,
    /// r_frame_rate verbatim, kept only for the strict "are these identical" check.
    pub frame_rate_raw: String,
    pub rotation: i32,
    pub has_audio: bool,
    pub audio_codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub duration: f64,
    pub size_bytes: u64,
}

impl ClipInfo {
    pub fn audio_label(&self) -> &str {
        if self.has_audio { &self.audio_codec } else { "silent" }
    }

    pub fn dimensions(&self) -> String {
        if !self.has_video {
            return crate::theme::glyph::NONE.to_string();
        }
        format!("{}{}{}", self.width, crate::theme::glyph::TIMES, self.height)
    }

    /// The framerate column. A file with no picture has no framerate, and a "0"
    /// there reads as a measurement that came out wrong.
    pub fn fps_label(&self) -> String {
        if !self.has_video {
            return crate::theme::glyph::NONE.to_string();
        }
        crate::format::fps(self.fps)
    }

    /// Both codecs in one column, e.g. `h264·aac`, `h264·—` when silent, and
    /// `—·mp3` for a file that is only sound.
    pub fn codec_label(&self) -> String {
        let video = if self.has_video { self.video_codec.as_str() } else { crate::theme::glyph::NONE };
        let audio = if self.has_audio { self.audio_codec.as_str() } else { crate::theme::glyph::NONE };
        format!("{video}{}{audio}", crate::theme::glyph::DOT)
    }
}

#[derive(Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    /// ffprobe reports this as a JSON string, not a number.
    sample_rate: Option<String>,
    channels: Option<u32>,
    #[serde(default)]
    side_data_list: Option<Vec<SideData>>,
    #[serde(default)]
    tags: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct SideData {
    rotation: Option<f64>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

/// "30000/1001" -> 29.97. Returns 0.0 for anything unparseable, including
/// ffprobe's "0/0" for streams it could not measure.
fn parse_rational(text: &str) -> f64 {
    let (num, den) = match text.split_once('/') {
        Some((a, b)) => (a, b),
        None => return text.trim().parse().unwrap_or(0.0),
    };
    let num: f64 = num.trim().parse().unwrap_or(0.0);
    let den: f64 = den.trim().parse().unwrap_or(0.0);
    if den == 0.0 { 0.0 } else { num / den }
}

/// Reads one file's format. Returns None for anything with neither a video nor
/// an audio stream in it, which is how callers decide to skip a file.
///
/// A file with sound and no picture comes back as a `ClipInfo` with `has_video`
/// false and its video fields left at zero. That is not a clip a merge can use,
/// and it is exactly what a conversion to another audio format needs - so the
/// two callers that care check the flag rather than this returning None.
pub fn clip_info(ffprobe: &Path, path: &Path) -> Option<ClipInfo> {
    let mut cmd = Command::new(ffprobe);
    cmd.args(["-v", "quiet", "-print_format", "json", "-show_format", "-show_streams", "--"])
        .arg(path);
    let out = proc::run_captured(cmd).ok()?;
    if !out.status.success() {
        return None;
    }

    let data: ProbeOutput = serde_json::from_slice(&out.stdout).ok()?;
    let video = data.streams.iter().find(|s| s.codec_type.as_deref() == Some("video"));
    let audio = data.streams.iter().find(|s| s.codec_type.as_deref() == Some("audio"));
    // Neither stream means this is not media at all - a text file that someone
    // renamed, or a download that stopped before anything arrived.
    if video.is_none() && audio.is_none() {
        return None;
    }

    let raw_fps = video.and_then(|v| v.r_frame_rate.as_deref()).map(parse_rational).unwrap_or(0.0);
    let avg_fps = video.and_then(|v| v.avg_frame_rate.as_deref()).map(parse_rational).unwrap_or(0.0);

    // r_frame_rate is the highest rate the stream could carry, not the rate it
    // runs at. A previously merged file often reports something silly like 375.
    // Trusting it would pick an absurd target framerate and multiply the work,
    // so the average rate is what gets used for decisions and display.
    let effective_fps = if avg_fps > 0.0 && avg_fps <= 240.0 {
        avg_fps
    } else if raw_fps > 0.0 && raw_fps <= 240.0 {
        raw_fps
    } else if avg_fps > 0.0 {
        60.0
    } else {
        30.0
    };

    // Phone footage stores orientation as metadata: a portrait clip can report
    // itself as 1920x1080 plus rotate:90. Track it so a rotated clip is never
    // treated as format-identical to an unrotated one.
    let mut rotation = 0i32;
    if let Some(list) = video.and_then(|v| v.side_data_list.as_ref()) {
        for sd in list {
            if let Some(r) = sd.rotation {
                rotation = r.abs().round() as i32;
            }
        }
    }
    if rotation == 0
        && let Some(tags) = video.and_then(|v| v.tags.as_ref())
    {
        for (k, v) in tags {
            if k.eq_ignore_ascii_case("rotate")
                && let Ok(r) = v.trim().parse::<f64>()
            {
                rotation = r.abs().round() as i32;
            }
        }
    }
    rotation = rotation.rem_euclid(360);

    let mut width = video.and_then(|v| v.width).unwrap_or(0);
    let mut height = video.and_then(|v| v.height).unwrap_or(0);
    if rotation == 90 || rotation == 270 {
        std::mem::swap(&mut width, &mut height);
    }

    let duration = data
        .format
        .and_then(|f| f.duration)
        .and_then(|d| d.trim().parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(0.0);

    let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    Some(ClipInfo {
        path: path.to_path_buf(),
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        has_video: video.is_some(),
        video_codec: video.and_then(|v| v.codec_name.clone()).unwrap_or_default(),
        width,
        height,
        pix_fmt: video.and_then(|v| v.pix_fmt.clone()).unwrap_or_default(),
        // A file with no picture has no framerate either, and the fallback of 30
        // above would put an invented one on screen.
        fps: if video.is_some() { (effective_fps * 1000.0).round() / 1000.0 } else { 0.0 },
        frame_rate_raw: video.and_then(|v| v.r_frame_rate.clone()).unwrap_or_default(),
        rotation,
        has_audio: audio.is_some(),
        audio_codec: audio
            .and_then(|a| a.codec_name.clone())
            .unwrap_or_else(|| "none".into()),
        sample_rate: audio
            .and_then(|a| a.sample_rate.as_deref())
            .and_then(|r| r.trim().parse().ok())
            .unwrap_or(0),
        channels: audio.and_then(|a| a.channels).unwrap_or(0),
        duration,
        size_bytes,
    })
}

/// Just the container's length, in seconds, or 0.0 if it cannot be read.
///
/// Separate from `clip_info` on purpose: that one answers "is this readable
/// video", and everything which calls it depends on a file with no video stream
/// coming back as None. An audio-only download is exactly that file, and still
/// has a length worth putting on the finished screen.
pub fn duration_of(ffprobe: &Path, path: &Path) -> f64 {
    let mut cmd = Command::new(ffprobe);
    cmd.args(["-v", "quiet", "-print_format", "json", "-show_format", "--"]).arg(path);
    let Ok(out) = proc::run_captured(cmd) else {
        return 0.0;
    };
    if !out.status.success() {
        return 0.0;
    }
    serde_json::from_slice::<ProbeOutput>(&out.stdout)
        .ok()
        .and_then(|data| data.format)
        .and_then(|f| f.duration)
        .and_then(|d| d.trim().parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rationals() {
        assert!((parse_rational("30000/1001") - 29.97002997).abs() < 1e-6);
        assert_eq!(parse_rational("0/0"), 0.0);
        assert_eq!(parse_rational("25"), 25.0);
        assert_eq!(parse_rational("nonsense"), 0.0);
    }
}
