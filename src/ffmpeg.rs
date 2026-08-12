//! Finding ffmpeg, and installing it per-user if it is missing.
//! Ported from Find-LocalFfmpeg / Install-Ffmpeg / Resolve-Tools.
//!
//! Nothing here needs admin rights: the download lands in an "ffmpeg" folder
//! next to the executable, or in LOCALAPPDATA when that folder is read-only.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::proc;

pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// Where setup messages go. A plain log line is not enough for a download of
/// this size - over 100 MB - because without a byte count it is
/// indistinguishable from a hang.
pub trait Reporter {
    fn log(&mut self, line: &str);

    /// Bytes so far, and the total if the server declared one.
    fn progress(&mut self, received: u64, total: Option<u64>);

    /// The transfer ended, one way or the other. Lets a console reporter
    /// finish off the line it has been rewriting in place.
    fn finished(&mut self);
}

/// How a downloaded archive is packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Packing {
    Zip,
    /// LZMA. Three times smaller than the same content as a zip, which is the
    /// difference between a two-minute wait and a seven-minute one.
    SevenZ,
}

/// One place ffmpeg can be fetched from.
struct Source {
    name: &'static str,
    url: &'static str,
    packing: Packing,
    /// Set only for archives whose exact contents we control. Upstream changes
    /// with every ffmpeg release, so pinning a hash there would break setup the
    /// day a new version ships.
    sha256: Option<&'static str>,
}

/// SHA-256 of `vendor/ffmpeg-release-essentials.7z`.
///
/// Refreshing the mirror means refreshing this in the same commit; a mismatch
/// makes setup reject the mirror and fall through to gyan.dev, which is the
/// safe direction to fail in.
const MIRROR_SHA256: &str = "49a73bdf0850092a252ac4641d922f3048d63ed113e196cc65ce1e4f7fb33e85";

/// Where to get ffmpeg, best first.
///
/// Our own mirror leads for two measured reasons. GitHub's *release asset* host
/// is unreachable from some networks - it returns nothing at all - but its code
/// hosts are not, and are far faster there than gyan.dev: 9 MB/s against
/// 210 KB/s on the connection this was measured on. That is why the archive sits
/// in the repository tree and is fetched over raw.githubusercontent.com.
///
/// Upstream stays as the fallback so the tool keeps working if this repository
/// is renamed, made private, or simply unreachable. The 7z is preferred over the
/// zip because it is the same build in a third of the bytes: 32.8 MB against
/// 106.1 MB.
const SOURCES: [Source; 4] = [
    Source {
        name: "this project's mirror",
        url: "https://raw.githubusercontent.com/alee-ibrahim/vmerge/main/vendor/ffmpeg-release-essentials.7z",
        packing: Packing::SevenZ,
        sha256: Some(MIRROR_SHA256),
    },
    Source {
        name: "gyan.dev",
        url: "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.7z",
        packing: Packing::SevenZ,
        sha256: None,
    },
    Source {
        name: "gyan.dev (zip)",
        url: "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip",
        packing: Packing::Zip,
        sha256: None,
    },
    Source {
        name: "BtbN/GitHub",
        url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip",
        packing: Packing::Zip,
        sha256: None,
    },
];

/// Distinctive enough that sweeping up leftovers cannot touch anyone else's
/// files.
const TEMP_PREFIX: &str = "video-merge-ffmpeg-";

#[cfg(windows)]
const EXE_SUFFIX: &str = ".exe";
#[cfg(not(windows))]
const EXE_SUFFIX: &str = "";

fn exe_name(stem: &str) -> String {
    format!("{stem}{EXE_SUFFIX}")
}

fn find_on_path(stem: &str) -> Option<PathBuf> {
    let name = exe_name(stem);
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(&name);
            candidate.is_file().then_some(candidate)
        })
    })
}

/// The usual places a copy sits next to the tool. `roots` is searched in order,
/// which is how a build in target/release still finds the ffmpeg folder that
/// lives beside the project.
fn find_local(roots: &[PathBuf]) -> Option<PathBuf> {
    let name = exe_name("ffmpeg");
    for root in roots {
        for relative in [
            PathBuf::from("ffmpeg").join("bin").join(&name),
            PathBuf::from("ffmpeg").join(&name),
            PathBuf::from(&name),
        ] {
            let candidate = root.join(relative);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        // A zip extracted as-is leaves a versioned folder in the middle, so
        // look one level deeper before giving up on this root.
        if let Ok(entries) = fs::read_dir(root.join("ffmpeg")) {
            for entry in entries.flatten() {
                for relative in [PathBuf::from("bin").join(&name), PathBuf::from(&name)] {
                    let candidate = entry.path().join(relative);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
}

fn local_app_data() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("video-merge"))
}

fn is_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".write-test-{}", std::process::id()));
    match fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Streams a download straight to disk rather than buffering it in memory:
/// the archive is over 100 MB and nothing needs it all at once.
fn download(url: &str, dest: &Path, reporter: &mut dyn Reporter) -> Result<()> {
    let response = ureq::get(url)
        .header("User-Agent", "video-merge-setup")
        .call()
        .with_context(|| format!("requesting {url}"))?;

    // Not every mirror declares a length, and a redirect chain can lose it, so
    // the reporter has to cope with not knowing the total.
    let total = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|bytes| *bytes > 0);

    let mut reader = response.into_body().into_reader();
    let mut file = fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;

    // Copied by hand rather than with io::copy, which cannot report progress.
    let mut buffer = vec![0u8; 64 * 1024];
    let mut received = 0u64;
    reporter.progress(0, total);
    loop {
        let read = reader.read(&mut buffer).context("reading the download")?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read]).context("writing the download to disk")?;
        received += read as u64;
        reporter.progress(received, total);
    }
    reporter.finished();

    // A truncated download extracts to nothing useful, so catch it here where
    // the reason is still obvious.
    if let Some(total) = total
        && received < total
    {
        bail!("the download stopped early ({received} of {total} bytes)");
    }
    Ok(())
}

/// SHA-256 of a file, read in chunks so a 100 MB archive is not held in memory.
fn sha256_of(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).context("reading the archive")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Removes leftovers from earlier runs, leaving this run's own paths alone.
fn sweep_stale_temp_files(temp: &Path, keep_zip: &Path, keep_stage: &Path) {
    let Ok(entries) = fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep_zip || path == keep_stage {
            continue;
        }
        let is_ours = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with(TEMP_PREFIX));
        if !is_ours {
            continue;
        }
        // Best effort: another copy of the program may still be using it, in
        // which case the delete fails and that is fine.
        if path.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
}

fn extract_binaries(
    archive_path: &Path,
    packing: Packing,
    stage: &Path,
    target_bin: &Path,
    reporter: &mut dyn Reporter,
) -> Result<PathBuf> {
    match packing {
        Packing::Zip => unpack_zip(archive_path, stage, reporter)?,
        Packing::SevenZ => unpack_7z(archive_path, stage, reporter)?,
    }
    collect_binaries(stage, target_bin)
}

/// Writes one entry, refusing any path that would climb out of `stage`.
///
/// A downloaded archive is untrusted input: an entry named `..\..\evil.exe`
/// must land nowhere. `zip` checks this itself via `enclosed_name`; 7z has no
/// equivalent, so both go through here.
fn write_entry(stage: &Path, name: &Path, data: &mut dyn Read) -> Result<u64> {
    let safe = name.components().all(|part| {
        matches!(part, std::path::Component::Normal(_) | std::path::Component::CurDir)
    });
    if !safe {
        return Ok(0);
    }
    let out = stage.join(name);
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = fs::File::create(&out).with_context(|| format!("writing {}", out.display()))?;
    let written = std::io::copy(data, &mut file).context("unpacking the archive")?;
    Ok(written)
}

fn unpack_7z(archive_path: &Path, stage: &Path, reporter: &mut dyn Reporter) -> Result<()> {
    // Read the header first, only for the total: the bar needs to know how far
    // it has to go before the first byte comes out.
    let listing = sevenz_rust2::Archive::open(archive_path)
        .map_err(|e| anyhow::anyhow!("reading the downloaded archive: {e}"))?;
    let total: u64 = listing.files.iter().filter(|f| !f.is_directory).map(|f| f.size).sum();

    let mut reader = sevenz_rust2::ArchiveReader::open(archive_path, Default::default())
        .map_err(|e| anyhow::anyhow!("opening the downloaded archive: {e}"))?;

    fs::create_dir_all(stage).with_context(|| format!("creating {}", stage.display()))?;
    let mut written = 0u64;
    let mut failure: Option<anyhow::Error> = None;
    reporter.progress(0, Some(total));

    reader
        .for_each_entries(|entry, data| {
            let path = PathBuf::from(entry.name.replace('\\', "/"));
            if entry.is_directory {
                let _ = fs::create_dir_all(stage.join(&path));
                return Ok(true);
            }
            match write_entry(stage, &path, data) {
                Ok(bytes) => {
                    written += bytes;
                    reporter.progress(written.min(total), Some(total));
                    Ok(true)
                }
                Err(e) => {
                    failure = Some(e);
                    Ok(false)
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("unpacking the downloaded archive: {e}"))?;

    reporter.finished();
    if let Some(e) = failure {
        return Err(e);
    }
    Ok(())
}

fn unpack_zip(archive_path: &Path, stage: &Path, reporter: &mut dyn Reporter) -> Result<()> {
    let file = fs::File::open(archive_path).context("opening the downloaded archive")?;
    let mut archive = zip::ZipArchive::new(file).context("reading the downloaded archive")?;

    // Unpacked, ffmpeg and ffprobe are around 90 MB each: enough that a silent
    // pause here looks like the same hang the download used to. Measured in
    // bytes rather than files, so the bar advances smoothly across two big
    // entries and a hundred tiny ones.
    let count = archive.len();
    let mut total = 0u64;
    for index in 0..count {
        if let Ok(entry) = archive.by_index(index) {
            total += entry.size();
        }
    }

    let mut written = 0u64;
    reporter.progress(0, Some(total));
    for index in 0..count {
        let mut entry = archive.by_index(index).context("reading an archive entry")?;
        let Some(relative) = entry.enclosed_name() else {
            continue;
        };
        if entry.is_dir() {
            let out = stage.join(&relative);
            fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
            continue;
        }
        written += write_entry(stage, &relative, &mut entry)?;
        reporter.progress(written.min(total), Some(total));
    }
    reporter.finished();
    Ok(())
}

/// Pulls the three executables out of an unpacked tree into their final home.
fn collect_binaries(stage: &Path, target_bin: &Path) -> Result<PathBuf> {
    let source_bin = find_extracted_bin(stage)
        .ok_or_else(|| anyhow::anyhow!("the downloaded archive did not contain ffmpeg{EXE_SUFFIX}"))?;

    fs::create_dir_all(target_bin)
        .with_context(|| format!("creating {}", target_bin.display()))?;

    // ffplay is not copied: it is another 104 MB on disk and nothing here ever
    // invokes it. The PowerShell took all three.
    for stem in ["ffmpeg", "ffprobe"] {
        let name = exe_name(stem);
        let from = source_bin.join(&name);
        if from.is_file() {
            let to = target_bin.join(&name);
            fs::copy(&from, &to).with_context(|| format!("copying {name}"))?;
            proc::unblock(&to);
        }
    }

    let installed = target_bin.join(exe_name("ffmpeg"));
    if !installed.is_file() {
        bail!("ffmpeg{EXE_SUFFIX} did not end up in {}", target_bin.display());
    }
    Ok(installed)
}

/// Walks the extracted tree for the folder holding ffmpeg itself.
fn find_extracted_bin(root: &Path) -> Option<PathBuf> {
    let name = exe_name("ffmpeg");
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == name.as_str()) {
                return path.parent().map(Path::to_path_buf);
            }
        }
    }
    None
}

fn install(root: &Path, reporter: &mut dyn Reporter) -> Result<PathBuf> {
    let temp = std::env::temp_dir();
    let stamp = std::process::id();
    let zip_path = temp.join(format!("{TEMP_PREFIX}{stamp}.zip"));
    let stage = temp.join(format!("{TEMP_PREFIX}{stamp}"));

    // A run killed mid-download leaves a hundred-odd MB behind. Sweep up what a
    // previous run of ours left, which is why the prefix is distinctive.
    sweep_stale_temp_files(&temp, &zip_path, &stage);

    // Everything here runs as the current user. If the tool itself sits
    // somewhere unwritable (read-only share, Program Files), fall back to the
    // per-user AppData folder instead of asking for elevation.
    let mut target = root.join("ffmpeg");
    if !is_writable(root) {
        target = local_app_data()
            .map(|d| d.join("ffmpeg"))
            .ok_or_else(|| anyhow::anyhow!("no writable folder to install ffmpeg into"))?;
        reporter.log(&format!(
            "The program folder is read-only, installing to {} instead.",
            target.display()
        ));
    }

    let _ = fs::remove_dir_all(&stage);

    // A source is only really good once its archive has unpacked, so download
    // and unpack are attempted together: a 7z that will not decode should fall
    // back to the zip rather than failing setup outright.
    let mut result = Err(anyhow::anyhow!("no download source worked"));
    for source in SOURCES {
        let name = source.name;
        // No size in the message: the bar reports whatever the server declares.
        // The figure inherited from the PowerShell said 40 MB; the zip is 106.
        reporter.log(&format!("Downloading ffmpeg from {name} - this happens once"));
        if let Err(e) = download(source.url, &zip_path, reporter) {
            reporter.log(&format!("Could not download from {name}: {e}"));
            continue;
        }

        // Only our own mirror is pinned, and a mismatch means falling through to
        // upstream rather than unpacking something unexpected.
        if let Some(expected) = source.sha256 {
            match sha256_of(&zip_path) {
                Ok(actual) if actual.eq_ignore_ascii_case(expected) => {}
                Ok(actual) => {
                    reporter.log(&format!(
                        "The archive from {name} is not the expected one                          (sha256 {}, expected {}); trying the next source.",
                        &actual[..16.min(actual.len())],
                        &expected[..16.min(expected.len())]
                    ));
                    continue;
                }
                Err(e) => {
                    reporter.log(&format!("Could not check the archive from {name}: {e}"));
                    continue;
                }
            }
        }

        reporter.log("Unpacking...");
        match extract_binaries(&zip_path, source.packing, &stage, &target.join("bin"), reporter) {
            Ok(installed) => {
                result = Ok(installed);
                break;
            }
            Err(e) => {
                reporter.log(&format!("Could not unpack the archive from {name}: {e}"));
                let _ = fs::remove_dir_all(&stage);
            }
        }
    }

    let _ = fs::remove_file(&zip_path);
    let _ = fs::remove_dir_all(&stage);

    if result.is_ok() {
        reporter.log(&format!("ffmpeg ready: {}", target.join("bin").display()));
    }
    result
}

/// Finds ffmpeg and ffprobe, installing them if needed.
///
/// `root` is where an installed copy is put; `search` also covers the parents
/// of the executable so a cargo-built binary finds a sibling ffmpeg folder.
pub fn resolve(
    root: &Path,
    search: &[PathBuf],
    allow_download: bool,
    reporter: &mut dyn Reporter,
) -> Result<Tools> {
    let mut roots: Vec<PathBuf> = search.to_vec();
    if let Some(appdata) = local_app_data() {
        roots.push(appdata);
    }

    let ffmpeg = match find_on_path("ffmpeg").or_else(|| find_local(&roots)) {
        Some(found) => found,
        None => {
            // Announce the download only when there is going to be one.
            if !allow_download {
                bail!("ffmpeg is missing and --skip-ffmpeg-download was set.");
            }
            reporter.log("First-time setup");
            reporter.log("ffmpeg (the free video engine this tool needs) is not installed yet.");
            reporter.log("Setting it up automatically - no admin rights, nothing installed");
            reporter.log("system-wide. It just lands in an \"ffmpeg\" folder next to this program.");
            install(root, reporter).map_err(|e| {
                anyhow::anyhow!(
                    "Could not set up ffmpeg automatically ({e}).\n  \
                     Manual option:\n  \
                     1. Open https://www.gyan.dev/ffmpeg/builds/\n  \
                     2. Download \"ffmpeg-release-essentials.zip\"\n  \
                     3. Unzip it so that {}\\ffmpeg\\bin\\ffmpeg{EXE_SUFFIX} exists",
                    root.display()
                )
            })?
        }
    };

    let beside = ffmpeg
        .parent()
        .map(|d| d.join(exe_name("ffprobe")))
        .filter(|p| p.is_file());
    let ffprobe = match beside.or_else(|| find_on_path("ffprobe")) {
        Some(p) => p,
        None => bail!("Found ffmpeg but not ffprobe next to it ({}).", ffmpeg.display()),
    };

    Ok(Tools { ffmpeg, ffprobe })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what a reporter was told, so the progress contract can be checked.
    #[derive(Default)]
    struct Recorder {
        logs: Vec<String>,
        last: Option<(u64, Option<u64>)>,
        calls: usize,
        finishes: usize,
    }

    impl Reporter for Recorder {
        fn log(&mut self, line: &str) {
            self.logs.push(line.to_string());
        }
        fn progress(&mut self, received: u64, total: Option<u64>) {
            self.last = Some((received, total));
            self.calls += 1;
        }
        fn finished(&mut self) {
            self.finishes += 1;
        }
    }

    /// An archive shaped like the real ones: a versioned folder, then bin/.
    fn build_archive(path: &Path, entries: &[(&str, usize)]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, size) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(&vec![b'x'; *size]).unwrap();
        }
        zip.finish().unwrap();
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vmerge-ffmpeg-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unpacking_finds_the_binaries_and_reports_bytes() {
        let dir = scratch("unpack");
        let archive = dir.join("ffmpeg.zip");
        build_archive(
            &archive,
            &[
                ("ffmpeg-7.1-essentials/README.txt", 40),
                (&format!("ffmpeg-7.1-essentials/bin/{}", exe_name("ffmpeg")), 2048),
                (&format!("ffmpeg-7.1-essentials/bin/{}", exe_name("ffprobe")), 1024),
                (&format!("ffmpeg-7.1-essentials/bin/{}", exe_name("ffplay")), 512),
            ],
        );

        let mut recorder = Recorder::default();
        let target_bin = dir.join("out").join("bin");
        let installed =
            extract_binaries(&archive, Packing::Zip, &dir.join("stage"), &target_bin, &mut recorder)
                .unwrap();

        assert_eq!(installed, target_bin.join(exe_name("ffmpeg")));
        assert!(target_bin.join(exe_name("ffprobe")).is_file(), "ffprobe comes along too");
        assert!(
            !target_bin.join(exe_name("ffplay")).is_file(),
            "ffplay is 104 MB and never invoked, so it must not be copied"
        );
        assert_eq!(fs::metadata(&installed).unwrap().len(), 2048, "copied whole");

        // The bar needs a total, and it has to arrive at it.
        let (received, total) = recorder.last.expect("progress was reported");
        assert_eq!(total, Some(40 + 2048 + 1024 + 512), "the bar counts every entry unpacked");
        assert_eq!(Some(received), total, "progress must reach the total");
        assert!(recorder.calls > 1);
        assert_eq!(recorder.finishes, 1, "the line gets closed exactly once");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_archive_without_ffmpeg_is_rejected() {
        let dir = scratch("empty");
        let archive = dir.join("ffmpeg.zip");
        build_archive(&archive, &[("notes.txt", 10)]);

        let error = extract_binaries(
            &archive,
            Packing::Zip,
            &dir.join("stage"),
            &dir.join("out").join("bin"),
            &mut Recorder::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("did not contain"), "got {error}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A downloaded archive is untrusted input: an entry named ..\..\evil.exe
    /// must not be able to write outside the staging folder.
    #[test]
    fn entries_cannot_escape_the_staging_folder() {
        let dir = scratch("slip");
        let archive = dir.join("ffmpeg.zip");
        build_archive(
            &archive,
            &[
                ("../escaped.txt", 8),
                (&format!("bin/{}", exe_name("ffmpeg")), 16),
            ],
        );

        let stage = dir.join("stage");
        extract_binaries(
            &archive,
            Packing::Zip,
            &stage,
            &dir.join("out").join("bin"),
            &mut Recorder::default(),
        )
        .unwrap();

        assert!(!dir.join("escaped.txt").exists(), "the traversal entry was written outside");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_leftovers_are_swept_but_other_files_are_left_alone() {
        let dir = scratch("sweep");
        let stale_zip = dir.join(format!("{TEMP_PREFIX}9999.zip"));
        let stale_dir = dir.join(format!("{TEMP_PREFIX}9999"));
        let keep_zip = dir.join(format!("{TEMP_PREFIX}1.zip"));
        let innocent = dir.join("someone-elses-file.zip");
        fs::write(&stale_zip, b"old").unwrap();
        fs::create_dir_all(&stale_dir).unwrap();
        fs::write(&keep_zip, b"mine").unwrap();
        fs::write(&innocent, b"not ours").unwrap();

        sweep_stale_temp_files(&dir, &keep_zip, &dir.join(format!("{TEMP_PREFIX}1")));

        assert!(!stale_zip.exists(), "a leftover download should be removed");
        assert!(!stale_dir.exists(), "a leftover staging folder should be removed");
        assert!(keep_zip.exists(), "this run's own file must survive");
        assert!(innocent.exists(), "files that are not ours must be left alone");

        let _ = fs::remove_dir_all(&dir);
    }
}
