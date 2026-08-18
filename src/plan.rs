//! Deciding what format everything gets joined in, and which clips can skip
//! the encoder. Ported from Test-CanStreamCopy / Get-TargetFormat /
//! Get-PassThroughTarget / Test-ClipMatchesTarget / Get-TargetFilter.

use std::collections::BTreeMap;

use crate::format;
use crate::probe::ClipInfo;

/// A size and framerate the user picked by hand, overriding the automatic choice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TargetOverride {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// The one format every segment is made to share before joining.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// The rate as ffmpeg should be told it, e.g. "30000/1001".
    pub fps_expr: String,
    pub video_codec: String,
    pub pix_fmt: String,
    pub sample_rate: u32,
    pub channels: u32,
}

impl Target {
    pub fn label(&self) -> String {
        format!(
            "{}{}{} @ {} fps",
            self.width,
            crate::theme::glyph::TIMES,
            self.height,
            format::fps(self.fps)
        )
    }

    pub fn channel_layout(&self) -> &'static str {
        if self.channels == 1 { "mono" } else { "stereo" }
    }

    /// The filter chain that lands a clip exactly on this target.
    ///
    /// Fit inside the target box and pad the rest black, so nothing is cropped
    /// and clips of a different shape still line up frame-for-frame.
    pub fn video_filter(&self) -> String {
        format!(
            "scale=w={w}:h={h}:force_original_aspect_ratio=decrease,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,\
             setsar=1,fps={fps},format={pix}",
            w = self.width,
            h = self.height,
            fps = self.fps_expr,
            pix = self.pix_fmt
        )
    }
}

/// Identical enough to stitch together without touching the pixels?
pub fn can_stream_copy(clips: &[ClipInfo]) -> bool {
    if clips.len() < 2 {
        return true;
    }
    let first = &clips[0];
    if first.video_codec != "h264" && first.video_codec != "hevc" {
        return false;
    }
    if first.has_audio && first.audio_codec != "aac" {
        return false;
    }
    clips.iter().all(|c| {
        c.video_codec == first.video_codec
            && c.width == first.width
            && c.height == first.height
            && c.pix_fmt == first.pix_fmt
            && c.frame_rate_raw == first.frame_rate_raw
            && c.rotation == first.rotation
            && c.has_audio == first.has_audio
            && c.audio_codec == first.audio_codec
            && c.sample_rate == first.sample_rate
            && c.channels == first.channels
    })
}

/// ffmpeg wants a rate, and the exact rational matters: a clip written as
/// 30000/1001 and one written as 29.97 are not seen as the same format later,
/// which would cost a needless re-encode.
pub fn fps_expr(fps: f64) -> String {
    const NTSC: [(&str, f64); 5] = [
        ("24000/1001", 23.976),
        ("30000/1001", 29.97),
        ("48000/1001", 47.952),
        ("60000/1001", 59.94),
        ("120000/1001", 119.88),
    ];
    for (expr, value) in NTSC {
        if (fps - value).abs() < 0.02 {
            return expr.to_string();
        }
    }
    if (fps - fps.round()).abs() < 0.01 {
        return format!("{}", fps.round() as i64);
    }
    format!("{fps:.3}")
}

/// Picks whichever value carries the most footage. Ties go to the caller's
/// preference (larger frame, higher rate), and then to the key itself so the
/// result never depends on map iteration order.
fn heaviest<K: Ord + Clone>(weights: &BTreeMap<K, f64>, prefer: impl Fn(&K) -> f64) -> Option<K> {
    weights
        .iter()
        .max_by(|(ka, va), (kb, vb)| {
            va.total_cmp(vb)
                .then_with(|| prefer(ka).total_cmp(&prefer(kb)))
                .then_with(|| ka.cmp(kb))
        })
        .map(|(k, _)| k.clone())
}

/// Works out the one shape and framerate everything gets converted to.
///
/// Weighted by DURATION, not by file count: whichever format most of the actual
/// footage is already in wins. Picking "biggest frame, highest framerate"
/// instead lets a six-second phone clip decide the format for an hour of camera
/// footage - every other clip then gets upscaled and frame-doubled, which
/// multiplies the encoding time and leaves most of each frame black.
pub fn target_format(clips: &[ClipInfo], over: Option<TargetOverride>) -> Target {
    let (mut width, mut height, mut fps);

    if let Some(o) = over.filter(|o| o.width > 0 && o.height > 0) {
        width = o.width;
        height = o.height;
        fps = o.fps;
    } else {
        let mut size_weight: BTreeMap<(u32, u32), f64> = BTreeMap::new();
        let mut fps_weight: BTreeMap<String, f64> = BTreeMap::new();
        for c in clips {
            let seconds = c.duration.max(0.1);
            *size_weight.entry((c.width, c.height)).or_insert(0.0) += seconds;
            *fps_weight.entry(format!("{:.3}", c.fps)).or_insert(0.0) += seconds;
        }

        let best_size = heaviest(&size_weight, |(w, h)| (*w as f64) * (*h as f64));
        let best_fps = heaviest(&fps_weight, |k| k.parse::<f64>().unwrap_or(0.0));

        let (w, h) = best_size.unwrap_or((1920, 1080));
        width = w;
        height = h;
        fps = best_fps.and_then(|k| k.parse().ok()).unwrap_or(30.0);

        // A framerate-only override still applies on top of the automatic size.
        if let Some(o) = over.filter(|o| o.fps > 0.0) {
            fps = o.fps;
        }
    }

    if width == 0 || height == 0 {
        width = 1920;
        height = 1080;
    }
    // H.264 needs even dimensions.
    width += width % 2;
    height += height % 2;
    if fps <= 0.0 {
        fps = 30.0;
    }
    if fps > 120.0 {
        fps = 120.0;
    }

    // Audio target, same duration-weighted idea. Matching the majority's sample
    // rate is what lets those clips be copied instead of re-encoded further on.
    let mut rate_weight: BTreeMap<u32, f64> = BTreeMap::new();
    let mut chan_weight: BTreeMap<u32, f64> = BTreeMap::new();
    for c in clips {
        if !c.has_audio || c.sample_rate == 0 {
            continue;
        }
        let seconds = c.duration.max(0.1);
        *rate_weight.entry(c.sample_rate).or_insert(0.0) += seconds;
        *chan_weight.entry(c.channels).or_insert(0.0) += seconds;
    }
    let sample_rate = heaviest(&rate_weight, |r| *r as f64).unwrap_or(48_000);
    let mut channels = heaviest(&chan_weight, |c| *c as f64).unwrap_or(2);
    if !(1..=2).contains(&channels) {
        channels = 2;
    }

    Target {
        width,
        height,
        fps: (fps * 1000.0).round() / 1000.0,
        fps_expr: fps_expr(fps),
        // Everything is normalised to H.264 + AAC, the pair that plays
        // everywhere. The exception is a set of clips already identical to each
        // other, which pass_through_target handles instead.
        video_codec: "h264".into(),
        pix_fmt: "yuv420p".into(),
        sample_rate,
        channels,
    }
}

/// When every clip is already the same format as every other, that format is
/// the target and no pixels get touched at all - whatever the codec happens to
/// be.
pub fn pass_through_target(clip: &ClipInfo) -> Target {
    Target {
        width: clip.width,
        height: clip.height,
        fps: clip.fps,
        fps_expr: fps_expr(clip.fps),
        video_codec: clip.video_codec.clone(),
        pix_fmt: clip.pix_fmt.clone(),
        sample_rate: clip.sample_rate,
        channels: clip.channels,
    }
}

/// Is this clip already exactly what the target asks for, video and audio? If
/// so it can be copied into the join untouched - no quality loss, no encoding
/// time.
pub fn clip_matches_target(clip: &ClipInfo, target: &Target) -> bool {
    clip.video_codec == target.video_codec
        && clip.width == target.width
        && clip.height == target.height
        && clip.pix_fmt == target.pix_fmt
        && clip.rotation == 0
        && (clip.fps - target.fps).abs() < 0.01
        && clip.has_audio
        && clip.audio_codec == "aac"
        && clip.sample_rate == target.sample_rate
        && clip.channels == target.channels
}

/// How many clips actually have to be re-encoded to reach the target?
pub fn convert_count(clips: &[ClipInfo], target: &Target) -> usize {
    clips.iter().filter(|c| !clip_matches_target(c, target)).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn clip(w: u32, h: u32, fps: f64, secs: f64) -> ClipInfo {
        ClipInfo {
            path: PathBuf::from("x.mp4"),
            name: "x.mp4".into(),
            has_video: true,
            video_codec: "h264".into(),
            width: w,
            height: h,
            pix_fmt: "yuv420p".into(),
            fps,
            frame_rate_raw: format!("{}/1", fps as u32),
            rotation: 0,
            has_audio: true,
            audio_codec: "aac".into(),
            sample_rate: 48_000,
            channels: 2,
            duration: secs,
            size_bytes: 1,
        }
    }

    #[test]
    fn most_footage_wins_not_biggest_clip() {
        // One short 4K clip must not drag an hour of 1080p up with it.
        let clips = vec![clip(1920, 1080, 30.0, 3600.0), clip(3840, 2160, 60.0, 6.0)];
        let t = target_format(&clips, None);
        assert_eq!((t.width, t.height), (1920, 1080));
        assert_eq!(t.fps, 30.0);
    }

    #[test]
    fn ties_go_to_the_larger_frame() {
        let clips = vec![clip(1280, 720, 30.0, 10.0), clip(1920, 1080, 30.0, 10.0)];
        let t = target_format(&clips, None);
        assert_eq!((t.width, t.height), (1920, 1080));
    }

    #[test]
    fn override_wins_and_odd_sizes_are_evened() {
        let clips = vec![clip(1920, 1080, 30.0, 10.0)];
        let t = target_format(
            &clips,
            Some(TargetOverride { width: 1081, height: 607, fps: 25.0 }),
        );
        assert_eq!((t.width, t.height), (1082, 608));
        assert_eq!(t.fps, 25.0);
    }

    #[test]
    fn ntsc_rates_keep_their_rational() {
        assert_eq!(fps_expr(29.97), "30000/1001");
        assert_eq!(fps_expr(29.970029), "30000/1001");
        assert_eq!(fps_expr(30.0), "30");
        assert_eq!(fps_expr(23.976), "24000/1001");
    }

    #[test]
    fn rotation_blocks_the_fast_path() {
        let mut clips = vec![clip(1920, 1080, 30.0, 5.0), clip(1920, 1080, 30.0, 5.0)];
        assert!(can_stream_copy(&clips));
        clips[1].rotation = 90;
        assert!(!can_stream_copy(&clips));
    }

    #[test]
    fn mixed_audio_rate_blocks_the_fast_path() {
        let mut clips = vec![clip(1920, 1080, 30.0, 5.0), clip(1920, 1080, 30.0, 5.0)];
        clips[1].sample_rate = 44_100;
        assert!(!can_stream_copy(&clips));
    }

    #[test]
    fn silent_clip_never_matches_a_target() {
        let mut c = clip(1920, 1080, 30.0, 5.0);
        let t = target_format(std::slice::from_ref(&c), None);
        assert!(clip_matches_target(&c, &t));
        c.has_audio = false;
        assert!(!clip_matches_target(&c, &t));
    }
}
