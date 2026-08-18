//! Turning what the user dropped, typed or pasted into a list of clips.
//! Ported from Get-VideoFilesInFolder / Split-PathLine / Add-ClipsToList.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use crate::ffmpeg::Tools;
use crate::format;
use crate::probe::{self, ClipInfo};

pub const VIDEO_EXTENSIONS: [&str; 17] = [
    "mp4", "mov", "mkv", "avi", "m4v", "webm", "wmv", "flv", "mpg", "mpeg", "ts", "m2ts", "mts",
    "3gp", "3g2", "ogv", "asf",
];

/// Sound with no picture. Nothing to merge, but converting one of these into
/// another audio format is half of what a converter is for, so they are allowed
/// into the list and the merge refuses them instead.
pub const AUDIO_EXTENSIONS: [&str; 12] = [
    "mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus", "wma", "aiff", "aif", "alac",
];

fn has_extension(path: &Path, list: &[&str]) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| list.contains(&e.as_str()))
}

pub fn is_video(path: &Path) -> bool {
    has_extension(path, &VIDEO_EXTENSIONS)
}

pub fn is_audio(path: &Path) -> bool {
    has_extension(path, &AUDIO_EXTENSIONS)
}

/// Anything this program will read: video or audio.
pub fn is_media(path: &Path) -> bool {
    is_video(path) || is_audio(path)
}

/// Our own previous output should not become an input.
fn looks_like_our_output(name: &str) -> bool {
    let lower = name.to_lowercase();
    let Some(stem) = lower.strip_suffix(".mp4") else {
        return false;
    };
    stem == "merged" || stem.strip_prefix("merged_").is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
}

/// Every video in a folder, in natural filename order.
pub fn video_files_in_folder(folder: &Path, exclude_leaf: Option<&str>) -> Vec<PathBuf> {
    files_in_folder(folder, exclude_leaf, is_video)
}

/// Every video *and* audio file in a folder. What dropping a folder in means:
/// the list holds whatever can be read, and the merge is the one that insists on
/// a picture.
pub fn media_files_in_folder(folder: &Path, exclude_leaf: Option<&str>) -> Vec<PathBuf> {
    files_in_folder(folder, exclude_leaf, is_media)
}

fn files_in_folder(
    folder: &Path,
    exclude_leaf: Option<&str>,
    accept: fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(folder) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && accept(p))
            .filter(|p| {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                !looks_like_our_output(&name)
                    && exclude_leaf.is_none_or(|leaf| !name.eq_ignore_ascii_case(leaf))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort_by_key(|p| {
        format::natural_key(&p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
    });
    files
}

/// merged.mp4, or merged_2.mp4 when that is taken, and so on.
pub fn default_output_path(folder: &Path) -> PathBuf {
    let mut candidate = folder.join("merged.mp4");
    let mut n = 2;
    while candidate.exists() {
        candidate = folder.join(format!("merged_{n}.mp4"));
        n += 1;
    }
    candidate
}

/// Turns one typed, pasted or dropped line into a list of path candidates.
///
/// Dropping several files at once gives one line of quoted paths:
///     "C:\a\first clip.mp4" "C:\a\second clip.mp4"
/// while dropping a single file often gives no quotes at all, and that lone
/// path may itself contain spaces. Quoted segments are therefore pulled out
/// first, and only a line with no quotes in it is treated as one whole path.
pub fn split_path_line(line: &str) -> Vec<String> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }

    let quoted = extract_quoted(line);
    if !quoted.is_empty() {
        return quoted;
    }

    // No quotes anywhere: the whole line is a single path, spaces and all.
    if Path::new(line).exists() {
        return vec![line.to_string()];
    }

    // Nothing matched a real path, so fall back to whitespace-separated
    // tokens - a list of short names, or a typo to report back.
    line.split_whitespace().map(String::from).collect()
}

fn extract_quoted(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<(char, String)> = None;
    for c in line.chars() {
        match &mut current {
            Some((quote, buffer)) => {
                if c == *quote {
                    let value = buffer.trim().to_string();
                    if !value.is_empty() {
                        out.push(value);
                    }
                    current = None;
                } else {
                    buffer.push(c);
                }
            }
            None if c == '"' || c == '\'' => current = Some((c, String::new())),
            None => {}
        }
    }
    out
}

/// Matches a shell-style pattern with * and ?, case-insensitively.
fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    // Classic two-index scan with a restart point for the last '*'.
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut restart) = (None, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            restart = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            restart += 1;
            ni = restart;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// What one candidate string stands for: a folder's worth of clips, a single
/// file, or a wildcard's matches.
pub fn expand(candidate: &str) -> Result<Vec<PathBuf>, String> {
    let path = Path::new(candidate);

    if path.is_dir() {
        let files = media_files_in_folder(path, None);
        if files.is_empty() {
            return Err(format!("No videos or audio in that folder: {candidate}"));
        }
        return Ok(files);
    }

    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }

    if candidate.contains('*') || candidate.contains('?') {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf);
        let parent = parent.unwrap_or_else(|| PathBuf::from("."));
        let pattern = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut hits: Vec<PathBuf> = std::fs::read_dir(&parent)
            .map_err(|_| format!("No such folder: {}", parent.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .is_some_and(|n| wildcard_matches(&pattern, &n))
            })
            .collect();
        if hits.is_empty() {
            return Err(format!("Nothing matched: {candidate}"));
        }
        hits.sort_by_key(|p| {
            format::natural_key(
                &p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            )
        });
        return Ok(hits);
    }

    Err(format!("Not found: {candidate}"))
}

/// Progress from the background probe, so the UI stays live while ffprobe
/// works through a long drop.
#[derive(Debug, Clone)]
pub enum AddEvent {
    Started(usize),
    Added(Box<ClipInfo>),
    Rejected { name: String, why: String },
    Finished { added: usize, rejected: usize },
}

/// Probes candidates on a worker thread, in the order they were given, and
/// reports each clip as it becomes known.
pub fn spawn_probe<T: Send + 'static>(
    tools: Arc<Tools>,
    candidates: Vec<String>,
    tx: Sender<T>,
    wrap: fn(AddEvent) -> T,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let send = |event: AddEvent| {
            let _ = tx.send(wrap(event));
        };

        // Expand first so the count in "reading N files" is the real one.
        let mut files: Vec<PathBuf> = Vec::new();
        let mut rejected = 0usize;
        let mut problems: Vec<(String, String)> = Vec::new();

        for candidate in &candidates {
            match expand(candidate) {
                Ok(found) => files.extend(found),
                Err(why) => {
                    rejected += 1;
                    problems.push((candidate.clone(), why));
                }
            }
        }

        send(AddEvent::Started(files.len()));
        for (name, why) in problems {
            send(AddEvent::Rejected { name, why });
        }

        let mut added = 0usize;
        for file in files {
            let name = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !is_media(&file) {
                rejected += 1;
                send(AddEvent::Rejected { name, why: "not a video or audio format".into() });
                continue;
            }
            match probe::clip_info(&tools.ffprobe, &file) {
                Some(info) => {
                    added += 1;
                    send(AddEvent::Added(Box::new(info)));
                }
                None => {
                    rejected += 1;
                    send(AddEvent::Rejected {
                        name,
                        why: "has neither video nor audio in it".into(),
                    });
                }
            }
        }

        send(AddEvent::Finished { added, rejected });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn several_dropped_files_split_on_quotes() {
        let line = r#""C:\a\first clip.mp4" "C:\a\second, clip.mp4""#;
        assert_eq!(
            split_path_line(line),
            vec![r"C:\a\first clip.mp4", r"C:\a\second, clip.mp4"]
        );
    }

    #[test]
    fn an_unquoted_missing_path_falls_back_to_tokens() {
        // Not a real path, so it degrades to tokens the caller can report on.
        assert_eq!(split_path_line("a.mp4 b.mp4"), vec!["a.mp4", "b.mp4"]);
    }

    #[test]
    fn apostrophes_inside_quotes_survive() {
        let line = r#""C:\a\it's here.mp4""#;
        assert_eq!(split_path_line(line), vec![r"C:\a\it's here.mp4"]);
    }

    #[test]
    fn our_own_output_is_not_an_input() {
        assert!(looks_like_our_output("merged.mp4"));
        assert!(looks_like_our_output("MERGED_12.mp4"));
        assert!(!looks_like_our_output("merged_final.mp4"));
        assert!(!looks_like_our_output("unmerged.mp4"));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_matches("*.mp4", "Clip.MP4"));
        assert!(wildcard_matches("clip?.mp4", "clip3.mp4"));
        assert!(!wildcard_matches("clip?.mp4", "clip33.mp4"));
        assert!(wildcard_matches("*cam*.mov", "front-cam-2.mov"));
    }
}
