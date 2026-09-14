//! The screen someone gets when they start the program without telling it what
//! to do.
//!
//! Every job this program does has a flag, and a flag is only obvious to
//! someone who already knows it exists. The person this is built for opened a
//! terminal because a set of instructions told them to; asking them to also
//! know that joining clips is the default and fetching a link is `--download`
//! is asking them to read the README first. So when no argument says otherwise,
//! it asks.
//!
//! It only ever fills in the arguments that were not given. Everything after
//! this point runs exactly as it would have had the flags been typed, which is
//! what keeps one path through the program rather than two.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

/// Whether to ask at all.
///
/// Anything that already says what to do means the answer is known, and a
/// prompt would be in the way. The terminal checks matter more than they look:
/// without them a scripted run with no arguments - piping output to a file,
/// a scheduled job - would stop dead waiting for an answer nobody is there to
/// give.
pub fn wanted(
    files_empty: bool,
    folder: bool,
    download: bool,
    file_list: bool,
    convert_to: bool,
    no_tui: bool,
) -> bool {
    files_empty
        && !folder
        && !download
        && !file_list
        && !convert_to
        && !no_tui
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
}

/// What the person chose, as the arguments they would otherwise have typed.
pub enum Choice {
    /// Join the clips in this folder.
    Merge { folder: PathBuf },
    /// Fetch this link, saving into this folder.
    Download { url: String, folder: PathBuf },
    /// They changed their mind. Not a failure.
    Quit,
}

/// Where a finished download goes, and where clips are looked for first.
///
/// Downloads is where a browser puts things and where people look for what they
/// just got, which makes it a better guess than the folder the program happens
/// to be sitting in - that is wherever the installer chose, and nobody keeps
/// videos there.
pub fn downloads_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");

    let candidate = PathBuf::from(home?).join("Downloads");
    candidate.is_dir().then_some(candidate)
}

pub fn ask() -> Choice {
    println!();
    println!("  What would you like to do?");
    println!();
    println!("    1   Join video clips into one file");
    println!("    2   Download a video from a link");
    println!();
    println!("    q   Quit");
    println!();

    loop {
        match prompt("  Choose 1, 2 or q: ") {
            None => return Choice::Quit,
            Some(answer) => match answer.trim().to_lowercase().as_str() {
                "1" | "m" | "merge" => return ask_merge(),
                "2" | "d" | "download" => return ask_download(),
                "q" | "quit" | "exit" => return Choice::Quit,
                "" => {}
                other => println!("  \"{other}\" is not one of the choices."),
            },
        }
    }
}

fn ask_merge() -> Choice {
    let default = downloads_dir().or_else(|| std::env::current_dir().ok());

    println!();
    println!("  Which folder holds the clips?");
    // Dragging the folder onto the window is the gesture most people reach for
    // before they think to type a path, and both terminals that ship with the
    // two platforms this builds for paste one when you do.
    println!("  You can drag the folder onto this window to fill in its path.");
    if let Some(dir) = &default {
        println!("  Press Enter on its own for {}", dir.display());
    }
    println!();

    loop {
        let Some(answer) = prompt("  Folder: ") else {
            return Choice::Quit;
        };
        let answer = answer.trim();

        let folder = if answer.is_empty() {
            match &default {
                Some(dir) => dir.clone(),
                None => {
                    println!("  Please type a folder.");
                    continue;
                }
            }
        } else {
            PathBuf::from(unescape_path(answer))
        };

        if folder.is_dir() {
            return Choice::Merge { folder };
        }
        println!("  There is no folder at {}.", folder.display());
    }
}

fn ask_download() -> Choice {
    let folder = downloads_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    println!();
    println!("  Paste the link to the video.");
    println!("  It will be saved to {}", folder.display());
    println!();

    loop {
        let Some(answer) = prompt("  Link: ") else {
            return Choice::Quit;
        };
        let url = answer.trim();

        if url.is_empty() {
            return Choice::Quit;
        }
        // Only the shape is checked here. Whether the link leads anywhere is
        // yt-dlp's business, and it gives a far better answer than anything
        // this could guess at.
        if url.starts_with("http://") || url.starts_with("https://") {
            return Choice::Download { url: url.to_string(), folder };
        }
        println!("  That does not look like a link - it should start with http.");
    }
}

/// Reads one line. `None` means end of input, which is ctrl-D or a closed pipe:
/// there is nobody left to ask, so the caller stops rather than looping.
fn prompt(label: &str) -> Option<String> {
    print!("{label}");
    let _ = io::stdout().flush();

    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

/// Turns a path as a terminal hands it over into the path it means.
///
/// Dragging a folder in pastes it shell-escaped - `/Users/me/My\ Clips` - and
/// pasting one copied from elsewhere often brings quotes along. Taken
/// literally, either becomes a folder that does not exist, and the person is
/// told their folder is missing while looking straight at it.
fn unescape_path(text: &str) -> String {
    let text = text.trim();
    let text = match (text.starts_with('\''), text.ends_with('\'')) {
        (true, true) if text.len() >= 2 => &text[1..text.len() - 1],
        _ => text,
    };
    let text = match (text.starts_with('"'), text.ends_with('"')) {
        (true, true) if text.len() >= 2 => &text[1..text.len() - 1],
        _ => text,
    };

    // A backslash before anything means "this character, literally". On Windows
    // it is a path separator instead, so nothing is unescaped there.
    if cfg!(windows) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_already_described_is_not_asked_about() {
        // Nothing given: worth asking, terminal permitting.
        let blank = (true, false, false, false, false, false);
        // The terminal checks cannot be met under `cargo test`, so the flag
        // logic is what is checked here; the whole function is exercised by
        // running the program.
        assert!(!wanted(blank.0, true, false, false, false, false), "--folder says merge");
        assert!(!wanted(blank.0, false, true, false, false, false), "--download says fetch");
        assert!(!wanted(blank.0, false, false, true, false, false), "--file-list says merge");
        assert!(!wanted(blank.0, false, false, false, true, false), "--convert-to says convert");
        assert!(!wanted(blank.0, false, false, false, false, true), "--no-tui means no prompts");
        assert!(!wanted(false, false, false, false, false, false), "files were given");
    }

    #[test]
    fn a_dragged_path_becomes_the_path_it_means() {
        assert_eq!(unescape_path("  /Users/me/Clips  "), "/Users/me/Clips");
        assert_eq!(unescape_path("\"/Users/me/My Clips\""), "/Users/me/My Clips");
        assert_eq!(unescape_path("'/Users/me/My Clips'"), "/Users/me/My Clips");
    }

    #[cfg(not(windows))]
    #[test]
    fn terminals_escape_spaces_when_a_folder_is_dragged_in() {
        assert_eq!(unescape_path(r"/Users/me/My\ Clips"), "/Users/me/My Clips");
        assert_eq!(unescape_path(r"/Users/me/Tom\'s\ Clips"), "/Users/me/Tom's Clips");
        // A trailing backslash is a typo, not an escape of the end of the line.
        assert_eq!(unescape_path(r"/Users/me/Clips\"), "/Users/me/Clips");
    }

    #[cfg(windows)]
    #[test]
    fn windows_separators_are_not_escapes() {
        assert_eq!(unescape_path(r"C:\Users\me\Clips"), r"C:\Users\me\Clips");
        assert_eq!(unescape_path("\"C:\\Users\\me\\My Clips\""), r"C:\Users\me\My Clips");
    }
}
