//! Running child processes, and the two Windows filesystem pokes the
//! PowerShell version got for free from Test-Path / Unblock-File.
//!
//! Every ffmpeg/ffprobe call must have all three standard streams redirected.
//! ffmpeg writes progress to stderr, and a single stray byte reaching the real
//! console would tear a hole in the Ratatui frame. stdin is closed for the same
//! reason in reverse: ffmpeg reads keyboard commands from stdin by default and
//! would eat the keystrokes meant for the UI.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
pub const EXE_SUFFIX: &str = ".exe";
#[cfg(not(windows))]
pub const EXE_SUFFIX: &str = "";

pub fn exe_name(stem: &str) -> String {
    format!("{stem}{EXE_SUFFIX}")
}

/// Where an executable of this name sits on PATH, if anywhere.
///
/// Every tool this program shells out to is looked up the same way, so the
/// `.exe` suffix and the PATH walk live here rather than once per tool.
pub fn find_on_path(stem: &str) -> Option<PathBuf> {
    let name = exe_name(stem);
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(&name);
            candidate.is_file().then_some(candidate)
        })
    })
}

/// Detach the child from our console so it cannot draw on it.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A child process with stdin closed and both outputs captured.
pub fn command(exe: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Runs a prepared command to completion, capturing both outputs.
pub fn run_captured(mut cmd: Command) -> io::Result<Output> {
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.output()
}

/// Keeps the tail of a long stderr dump: the last line ffmpeg wrote before
/// giving up is the one worth showing, and the rest is noise.
pub fn error_tail(stderr: &[u8], max_lines: usize) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("; ")
}

/// Marks a directory hidden, so the working folder does not clutter the user's
/// view mid-merge. Best effort: a failure here changes nothing that matters.
#[cfg(windows)]
pub fn set_hidden(path: &Path) {
    use std::os::windows::ffi::OsStrExt;

    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32;
    }

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let attrs = if path.is_dir() {
        FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_HIDDEN
    };
    unsafe {
        SetFileAttributesW(wide.as_ptr(), attrs);
    }
}

#[cfg(not(windows))]
pub fn set_hidden(_path: &Path) {}

/// Clears the "downloaded from the internet" mark so Windows does not raise a
/// SmartScreen prompt the first time the freshly installed ffmpeg.exe runs.
/// The mark is an alternate data stream, and deleting the stream is exactly
/// what Unblock-File does.
#[cfg(windows)]
pub fn unblock(path: &Path) {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    let _ = std::fs::remove_file(Path::new(&stream));
}

#[cfg(not(windows))]
pub fn unblock(_path: &Path) {}
