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
///
/// `width` and `height` are the frame as *stored*, and `sar` is the shape of one
/// pixel in it - so the picture on screen is `width * sar` by `height`. Two clips
/// can only be joined if they agree about the shape of a pixel as well as the
/// number of them, and there are two ways to reach that agreement: everything
/// square, or everything already the same non-square shape. The second is worth
/// having, because for anamorphic footage it is the difference between encoding a
/// 350-wide frame and upscaling it to 764 for no new detail at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub width: u32,
    pub height: u32,
    /// The pixel shape, as ffmpeg's `setsar` wants it. `(1, 1)` for square.
    pub sar: (u32, u32),
    pub fps: f64,
    /// The rate as ffmpeg should be told it, e.g. "30000/1001".
    pub fps_expr: String,
    pub video_codec: String,
    pub pix_fmt: String,
    pub sample_rate: u32,
    pub channels: u32,
}

impl Target {
    /// How wide one stored pixel is compared to its height.
    pub fn pixel_aspect(&self) -> f64 {
        let (num, den) = self.sar;
        if num == 0 || den == 0 { 1.0 } else { num as f64 / den as f64 }
    }

    /// The size the finished file will be *displayed* at, which is the only size
    /// worth putting in front of anyone: it is what a player will report and what
    /// the picture will look like.
    pub fn display_size(&self) -> (u32, u32) {
        let width = (self.width as f64 * self.pixel_aspect()).round().max(2.0) as u32;
        (width + width % 2, self.height)
    }

    pub fn label(&self) -> String {
        let (width, height) = self.display_size();
        format!("{width}{}{height} @ {} fps", crate::theme::glyph::TIMES, format::fps(self.fps))
    }

    pub fn channel_layout(&self) -> &'static str {
        if self.channels == 1 { "mono" } else { "stereo" }
    }

    /// The filter chain that lands a clip exactly on this target.
    ///
    /// Fit inside the target box and pad the rest black, so nothing is cropped
    /// and clips of a different shape still line up frame-for-frame.
    ///
    /// The fit is worked out from `dar` - the ratio the clip is *displayed* at -
    /// and then divided back through the target's own pixel shape, because for
    /// anamorphic footage the stored frame and the picture are not the same
    /// rectangle. A 350x572 stream with 24:11 pixels is a 764x572 picture, so
    /// fitting it by its stored numbers squeezes it into a third of its width.
    /// `force_original_aspect_ratio=decrease` did exactly that - it measures the
    /// stored frame - and the `setsar=1` that followed threw away the stretch that
    /// would have put it right again.
    pub fn video_filter(&self) -> String {
        // Commas inside min() are escaped, or the filter graph reads them as the
        // end of the scale filter. Even numbers, because H.264 requires them.
        let (sar_num, sar_den) = self.sar;
        format!(
            "scale=w=trunc(min({w}\\,{h}*dar*{sar_den}/{sar_num})/2)*2:\
             h=trunc(min({h}\\,{w}*{sar_num}/{sar_den}/dar)/2)*2,\
             setsar={sar_num}/{sar_den},pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,\
             fps={fps},format={pix}",
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
            // Same number of pixels is not the same picture unless the pixels are
            // the same shape: two 350x572 streams, one anamorphic and one not, are
            // a 764x572 picture and a 350x572 one.
            && c.sample_aspect_raw == first.sample_aspect_raw
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

/// The pixel shape every clip already agrees on, or `(1, 1)` when they do not.
///
/// Read from the verbatim ffprobe field rather than from the parsed ratio: "24:11"
/// and "48:22" are the same shape, and treating them as different would only cost
/// a needless normalisation, but comparing floats for equality to decide it would
/// be worse.
fn shared_pixel_shape(clips: &[ClipInfo]) -> (u32, u32) {
    let Some(first) = clips.first() else {
        return (1, 1);
    };
    if !clips.iter().all(|c| c.sample_aspect_raw == first.sample_aspect_raw) {
        return (1, 1);
    }
    // Rotated footage has its pixel shape turned with it, and the raw field no
    // longer describes what is on screen. Rare enough, and confusing enough, to
    // leave to the square-pixel path.
    if clips.iter().any(|c| c.rotation != 0) {
        return (1, 1);
    }
    first.pixel_shape()
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

    // Everything anamorphic in the same way can stay that way: the frames are
    // already a common shape, so they need no stretching to line up, and encoding
    // a 350-wide frame beats upscaling it to 764 to invent nothing. Anything mixed
    // has to be brought to square pixels, because that is the only shape a
    // stretched clip and an unstretched one can both be made into.
    //
    // A size typed in by hand is a size on screen, so it means square pixels too.
    let sar = if over.is_some() { (1, 1) } else { shared_pixel_shape(clips) };
    let anamorphic = sar != (1, 1);

    if let Some(o) = over.filter(|o| o.width > 0 && o.height > 0) {
        width = o.width;
        height = o.height;
        fps = o.fps;
    } else {
        let mut size_weight: BTreeMap<(u32, u32), f64> = BTreeMap::new();
        let mut fps_weight: BTreeMap<String, f64> = BTreeMap::new();
        for c in clips {
            let seconds = c.duration.max(0.1);
            // Weighted by the size on screen rather than the size in the file,
            // because for anamorphic footage those differ and what matters is how
            // big the picture is. Where every clip shares one pixel shape the
            // stored sizes are already comparable, and using them keeps the target
            // at a size the footage actually has.
            let size = if anamorphic { (c.width, c.height) } else { c.display_size() };
            *size_weight.entry(size).or_insert(0.0) += seconds;
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
        sar,
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
/// be, and whatever shape its pixels are. Nothing is re-encoded on this path, so
/// an anamorphic set stays anamorphic and keeps displaying correctly; the size
/// reported is the one it displays at.
pub fn pass_through_target(clip: &ClipInfo) -> Target {
    Target {
        width: clip.width,
        height: clip.height,
        sar: clip.pixel_shape(),
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
        // Same number of pixels is only the same picture if they are the same
        // shape. Where the target is square this excludes every anamorphic clip,
        // and where the target is anamorphic it is what lets one straight through
        // untouched.
        && clip.pixel_shape() == target.sar
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
            sample_aspect_raw: "1:1".into(),
            pixel_aspect: 1.0,
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

    /// 350x572 stored with 24:11 pixels is a 764x572 picture. Broadcast footage
    /// like this is what "the resolution goes cramped after a merge" was: the
    /// target was taken from the stored numbers and `setsar=1` then threw away the
    /// stretch that made them a picture, squeezing it into a third of its width.
    fn anamorphic(w: u32, h: u32, secs: f64) -> ClipInfo {
        ClipInfo {
            sample_aspect_raw: "24:11".into(),
            pixel_aspect: 24.0 / 11.0,
            ..clip(w, h, 25.0, secs)
        }
    }

    #[test]
    fn footage_that_is_all_anamorphic_the_same_way_stays_that_way() {
        let clips = vec![anamorphic(350, 574, 90.0), anamorphic(350, 572, 80.0)];
        let t = target_format(&clips, None);

        // Encoded at the size it is stored at, not upscaled to the size it is
        // shown at: 764 wide would be a third more pixels per row and not one more
        // pixel of detail.
        assert_eq!((t.width, t.height), (350, 574));
        assert_eq!(t.sar, (24, 11));
        // What it will be displayed at, which is what the plan line says.
        assert_eq!(t.display_size(), (764, 574));
        assert_eq!(t.label(), "764×574 @ 25 fps");

        // The clip that is already exactly this goes through untouched; the other
        // one is two rows short, so it converts.
        assert!(clip_matches_target(&clips[0], &t));
        assert!(!clip_matches_target(&clips[1], &t));

        let filter = t.video_filter();
        assert!(filter.contains("setsar=24/11"), "the shape is kept: {filter}");
        assert!(filter.contains("dar"), "and the fit reads it: {filter}");
        assert!(!filter.contains("force_original_aspect_ratio"), "{filter}");
    }

    /// One stretched clip and one square one cannot both keep their shape, so
    /// everything is brought to square pixels at the size it displays at.
    #[test]
    fn mixing_pixel_shapes_normalises_to_square_ones() {
        let clips = vec![anamorphic(350, 574, 90.0), clip(1280, 720, 25.0, 10.0)];
        let t = target_format(&clips, None);

        assert_eq!(t.sar, (1, 1));
        assert_eq!((t.width, t.height), (764, 574), "the heaviest picture, in square pixels");
        // And the anamorphic clip must not be copied into it: it has to be
        // stretched first, or that half of the merge plays squeezed.
        assert!(!clip_matches_target(&clips[0], &t));
        assert!(t.video_filter().contains("setsar=1/1"), "{}", t.video_filter());
    }

    /// A size typed in by hand is a size on screen, so it means square pixels.
    #[test]
    fn a_size_asked_for_by_hand_is_a_displayed_size() {
        let clips = vec![anamorphic(350, 574, 90.0)];
        let t = target_format(&clips, Some(TargetOverride { width: 1280, height: 720, fps: 25.0 }));
        assert_eq!((t.width, t.height), (1280, 720));
        assert_eq!(t.sar, (1, 1));
        assert_eq!(t.display_size(), (1280, 720));
    }

    /// Two clips anamorphic in the same way are identical, so the fast join still
    /// applies and no pixel is touched at all. Two that differ only in pixel shape
    /// are not identical, however alike their numbers look.
    #[test]
    fn the_fast_path_reads_the_pixel_shape_too() {
        let mut clips = vec![anamorphic(350, 572, 5.0), anamorphic(350, 572, 5.0)];
        assert!(can_stream_copy(&clips));

        // Nothing is re-encoded on that path, so the target is the clip as it is -
        // reported at the size it plays at.
        let target = pass_through_target(&clips[0]);
        assert_eq!((target.width, target.height), (350, 572));
        assert_eq!(target.sar, (24, 11));
        assert_eq!(target.display_size(), (764, 572));

        clips[1].sample_aspect_raw = "1:1".into();
        clips[1].pixel_aspect = 1.0;
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
