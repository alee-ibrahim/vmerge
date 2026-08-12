//! Formatting helpers, ported from the console helpers in merge-videos.ps1.

/// Human-readable byte count. Mirrors Format-Size.
pub fn size(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.0} KB", b / KB)
    }
}

/// HH:MM:SS, or "unknown" for a duration ffprobe could not report.
pub fn duration(seconds: f64) -> String {
    if seconds <= 0.0 || !seconds.is_finite() {
        return "unknown".into();
    }
    let total = seconds.round() as u64;
    format!("{:02}:{:02}:{:02}", total / 3600, (total / 60) % 60, total % 60)
}

/// mm:ss, for the short elapsed/remaining readouts on the merge screen.
pub fn short_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "--:--".into();
    }
    let total = seconds.round() as u64;
    if total >= 3600 {
        format!("{}:{:02}:{:02}", total / 3600, (total / 60) % 60, total % 60)
    } else {
        format!("{}:{:02}", total / 60, total % 60)
    }
}

/// 30 rather than 30.00, but 29.97 keeps its decimals.
///
/// Format-Fps used a 0.05 tolerance here, which rounded 29.97 to a displayed
/// "30". That makes the clip table contradict itself: a 29.97 clip and a 30 fps
/// clip look identical in the list while the plan line says one of them needs
/// converting. The tolerance is tight enough to keep NTSC rates distinct.
pub fn fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.01 {
        format!("{}", fps.round() as i64)
    } else {
        format!("{fps:.2}")
    }
}

/// Sorts clip2 before clip10, which plain alphabetical would not.
/// Digit runs are zero-padded so they compare by value; everything else is
/// lowercased. Mirrors Get-NaturalKey.
pub fn natural_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut run = String::from(c);
            while let Some(n) = chars.peek() {
                if n.is_ascii_digit() {
                    run.push(*n);
                    chars.next();
                } else {
                    break;
                }
            }
            // A 20-wide field covers any real filename; longer runs are kept
            // whole rather than truncated, which would collide.
            let width = run.len().max(20);
            for _ in run.len()..width {
                out.push('0');
            }
            out.push_str(&run);
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Clips to `width` with an ellipsis, counting characters rather than bytes.
pub fn ellipsize(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width <= 3 {
        return text.chars().take(width).collect();
    }
    let mut s: String = text.chars().take(width - 3).collect();
    s.push_str("...");
    s
}

/// Pads to `width` after clipping, for the plain-text one-shot output.
pub fn pad(text: &str, width: usize) -> String {
    let s = ellipsize(text, width);
    let len = s.chars().count();
    let mut out = s;
    for _ in len..width {
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_is_numeric() {
        let mut names = vec!["clip10.mp4", "clip2.mp4", "Clip1.mp4"];
        names.sort_by_key(|n| natural_key(n));
        assert_eq!(names, vec!["Clip1.mp4", "clip2.mp4", "clip10.mp4"]);
    }

    #[test]
    fn fps_drops_pointless_decimals() {
        assert_eq!(fps(30.0), "30");
        assert_eq!(fps(29.97), "29.97");
        assert_eq!(fps(59.94005994), "59.94");
    }

    #[test]
    fn durations_and_sizes() {
        assert_eq!(duration(0.0), "unknown");
        assert_eq!(duration(3725.0), "01:02:05");
        assert_eq!(size(1536), "2 KB");
        assert_eq!(size(5 * 1024 * 1024), "5.0 MB");
    }
}
