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
use std::sync::atomic::{AtomicBool, Ordering};

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

/// macOS marks anything downloaded with a `com.apple.quarantine` extended
/// attribute, and Gatekeeper refuses to execute a file carrying it: the same job
/// Zone.Identifier does on Windows, done with an xattr instead of a stream.
/// Removing it is what `xattr -d com.apple.quarantine` does, and removexattr is
/// called directly so this does not depend on /usr/bin/xattr existing.
#[cfg(target_os = "macos")]
pub fn unblock(path: &Path) {
    use std::ffi::{CString, c_char};
    use std::os::unix::ffi::OsStrExt;

    // No #[link]: this lives in libSystem, which std has already linked, so
    // naming a library here could only get it wrong.
    unsafe extern "C" {
        fn removexattr(path: *const c_char, name: *const c_char, options: i32) -> i32;
    }

    let (Ok(path), Ok(name)) = (
        CString::new(path.as_os_str().as_bytes()),
        CString::new("com.apple.quarantine"),
    ) else {
        return;
    };
    // Best effort, exactly like the Windows side: a file that never carried the
    // attribute fails with ENOATTR, which is the state we wanted anyway.
    unsafe {
        removexattr(path.as_ptr(), name.as_ptr(), 0);
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn unblock(_path: &Path) {}

/// Makes a freshly written file runnable.
///
/// Extraction writes entries with `File::create`, which is 0644 — so a binary
/// that was executable inside the archive arrives without the bit and cannot be
/// run. On Windows the bit does not exist and there is nothing to do.
#[cfg(unix)]
pub fn make_runnable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
pub fn make_runnable(_path: &Path) {}

/// Gives a binary an ad-hoc signature, which on Apple Silicon is what makes it
/// runnable at all.
///
/// arm64 macOS refuses to execute an unsigned binary outright — the kernel kills
/// it before `main`, and the error says nothing useful about why. A downloaded
/// ffmpeg therefore has to be signed locally, and `codesign -s -` is the
/// signature that needs no certificate and no developer account. osxexperts,
/// who publish the builds this installs, document exactly this step.
///
/// Best effort: where the binary is already signed, or the command line tools
/// are absent, this changes nothing and the caller finds out the usual way —
/// by running it.
#[cfg(target_os = "macos")]
pub fn ad_hoc_sign(path: &Path) {
    let _ = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(target_os = "macos"))]
pub fn ad_hoc_sign(_path: &Path) {}

/// Everything a freshly installed binary needs before it can be executed.
///
/// The three steps are one idea — "this arrived from the internet, make it
/// runnable" — and every install path wants all three, so they travel together
/// rather than being remembered separately at each call site.
pub fn make_installed_runnable(path: &Path) {
    unblock(path);
    make_runnable(path);
    ad_hoc_sign(path);
}

/// A child process and everything it goes on to start, killable as one thing.
///
/// `Child::kill` is `TerminateProcess`, which ends exactly one process and
/// leaves its children running - and both children this program starts have one
/// of their own. yt-dlp runs ffmpeg to join the video and audio streams; the
/// live recorder *is* ffmpeg, reached through a pipe. Stopping yt-dlp mid-merge
/// therefore used to leave an ffmpeg behind, still writing and still holding the
/// working folder open, so clearing that folder failed and a download the user
/// had stopped carried on to the end.
///
/// A Windows job object is how the operating system expresses "these processes
/// belong together". The child joins one just after it is spawned, and the whole
/// group can then be ended in a single call. The handle is held open for as long
/// as the child is being waited on, and `KILL_ON_JOB_CLOSE` means even a panic
/// on the way out takes the tree with it rather than orphaning it.
///
/// There is a gap between spawning the child and putting it in the job, and a
/// grandchild started inside that gap would escape. It is microseconds against
/// the seconds yt-dlp spends extracting before it runs anything, so the race is
/// real and has never been reachable. Closing it properly needs
/// `CREATE_SUSPENDED` and a `ResumeThread`, which is a great deal of machinery
/// for a window that narrow.
pub struct Group {
    #[cfg(windows)]
    job: Option<isize>,
}

#[cfg(windows)]
mod job {
    use std::ffi::c_void;

    pub const KILL_ON_CLOSE: u32 = 0x2000;
    /// `JobObjectExtendedLimitInformation`.
    pub const EXTENDED_LIMITS: u32 = 9;

    /// Laid out for the operating system rather than for Rust. Written with
    /// Rust's own widths - `usize` where Windows says `SIZE_T`/`ULONG_PTR` -
    /// so the 32-bit and ARM64 builds get the layout right without a second
    /// definition of each field.
    #[repr(C)]
    #[derive(Default)]
    pub struct BasicLimits {
        pub per_process_user_time: i64,
        pub per_job_user_time: i64,
        pub limit_flags: u32,
        pub minimum_working_set: usize,
        pub maximum_working_set: usize,
        pub active_process_limit: u32,
        pub affinity: usize,
        pub priority_class: u32,
        pub scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct IoCounters {
        pub read_operations: u64,
        pub write_operations: u64,
        pub other_operations: u64,
        pub read_transferred: u64,
        pub write_transferred: u64,
        pub other_transferred: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct ExtendedLimits {
        pub basic: BasicLimits,
        pub io: IoCounters,
        pub process_memory_limit: usize,
        pub job_memory_limit: usize,
        pub peak_process_memory: usize,
        pub peak_job_memory: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> isize;
        pub fn SetInformationJobObject(
            job: isize,
            class: u32,
            info: *const c_void,
            length: u32,
        ) -> i32;
        pub fn AssignProcessToJobObject(job: isize, process: isize) -> i32;
        pub fn TerminateJobObject(job: isize, exit_code: u32) -> i32;
        pub fn CloseHandle(handle: isize) -> i32;
    }
}

impl Group {
    /// Puts a freshly spawned child, and anything it starts, in one group.
    ///
    /// Best effort. If the job cannot be made - an old Windows, a policy that
    /// forbids it - `kill` falls back to ending just the child, which is
    /// exactly what happened before any of this existed.
    #[cfg(windows)]
    pub fn around(child: &std::process::Child) -> Group {
        use std::os::windows::io::AsRawHandle;

        let handle = unsafe { job::CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if handle == 0 {
            return Group { job: None };
        }

        let limits = job::ExtendedLimits {
            basic: job::BasicLimits {
                limit_flags: job::KILL_ON_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        let set = unsafe {
            job::SetInformationJobObject(
                handle,
                job::EXTENDED_LIMITS,
                (&raw const limits).cast(),
                size_of::<job::ExtendedLimits>() as u32,
            )
        };
        let assigned =
            unsafe { job::AssignProcessToJobObject(handle, child.as_raw_handle() as isize) };
        if set == 0 || assigned == 0 {
            // Closing a job nothing was assigned to kills nothing, so this is
            // safe to do while the child runs on outside it.
            unsafe { job::CloseHandle(handle) };
            return Group { job: None };
        }
        Group { job: Some(handle) }
    }

    #[cfg(not(windows))]
    pub fn around(_child: &std::process::Child) -> Group {
        Group {}
    }

    /// Ends the child and its descendants.
    ///
    /// The child is passed in rather than held, so that the caller can keep
    /// reading its output - which is the whole reason this is not simply a
    /// `Drop`.
    #[cfg(windows)]
    pub fn kill(&self, child: &mut std::process::Child) {
        match self.job {
            Some(handle) => {
                unsafe { job::TerminateJobObject(handle, 1) };
            }
            None => {
                let _ = child.kill();
            }
        }
    }

    #[cfg(not(windows))]
    pub fn kill(&self, child: &mut std::process::Child) {
        let _ = child.kill();
    }

}

#[cfg(windows)]
impl Drop for Group {
    fn drop(&mut self) {
        if let Some(handle) = self.job.take() {
            unsafe { job::CloseHandle(handle) };
        }
    }
}

/// Set by ctrl-c once `stop_on_interrupt` has been called.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// The handler itself. Returning non-zero claims the event, which is what stops
/// Windows from ending the process where it stands.
#[cfg(windows)]
unsafe extern "system" fn on_interrupt(kind: u32) -> i32 {
    const CTRL_C: u32 = 0;
    const CTRL_BREAK: u32 = 1;
    if kind != CTRL_C && kind != CTRL_BREAK {
        // Closing the window or logging off is not a request, and there is no
        // useful work to be done in the seconds Windows allows before it stops
        // asking.
        return 0;
    }
    // The first press is a request. Anyone who presses it twice has stopped
    // asking, so the second is left to the default handler, which kills us -
    // and the job object takes ffmpeg down with it rather than orphaning it.
    if INTERRUPTED.swap(true, Ordering::SeqCst) { 0 } else { 1 }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

/// Turns ctrl-c into a request to stop rather than a killed process, and hands
/// back the flag it sets.
///
/// This exists for one case: recording a live broadcast from the command line.
/// The file being written is only finished when ffmpeg is told to wrap up, so a
/// ctrl-c that killed the program outright would throw away the ending of every
/// recording made this way - which, for a three-hour sitting, is the whole point
/// of having recorded it. For an ordinary download it changes nothing anyone can
/// see: the request is noticed within a fraction of a second and the program
/// stops just as it did before.
///
/// Off Windows the flag is returned without a handler behind it, so ctrl-c keeps
/// its usual meaning there. That is a gap rather than a decision, and the
/// program this belongs to is a Windows one.
pub fn stop_on_interrupt() -> &'static AtomicBool {
    INTERRUPTED.store(false, Ordering::SeqCst);
    #[cfg(windows)]
    unsafe {
        SetConsoleCtrlHandler(Some(on_interrupt), 1)
    };
    &INTERRUPTED
}
