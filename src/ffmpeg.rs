//! Finding ffmpeg, and installing it per-user if it is missing.
//! Ported from Find-LocalFfmpeg / Install-Ffmpeg / Resolve-Tools.
//!
//! Nothing here needs admin rights: the download lands in an "ffmpeg" folder
//! next to the executable, or in LOCALAPPDATA when that folder is read-only.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
///
/// Only the Windows sources name `SevenZ` - gyan.dev is the one publisher here
/// shipping a 7z - so off Windows the variant is matched but never constructed,
/// which is dead code as far as the compiler is concerned. The unpacker stays
/// either way: it is what makes adding a 7z source elsewhere a one-line change,
/// and deleting it to satisfy a lint would be the tail wagging the dog.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Packing {
    Zip,
    /// LZMA. Three times smaller than the same content as a zip, which is the
    /// difference between a two-minute wait and a seven-minute one.
    SevenZ,
}

/// A binary an archive is expected to yield, and the SHA-256 it must have.
///
/// The hash is of the *extracted binary*, not of the archive around it, because
/// that is the form the two publishers differ on: gyan.dev ships one archive
/// holding both tools and osxexperts ships each tool in an archive of its own,
/// but both end in a file whose bytes are the thing that actually gets run.
/// Checking there rather than at the archive also means a re-zipped upload with
/// identical contents does not read as tampering.
struct Binary {
    stem: &'static str,
    sha256: Option<&'static str>,
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
    /// What this archive is expected to provide. Windows builds carry both
    /// tools; the macOS ones are published a tool at a time, so setup keeps
    /// taking sources until everything it needs has arrived.
    binaries: &'static [Binary],
}

/// The tools setup is not finished without.
const NEEDED: [&str; 2] = ["ffmpeg", "ffprobe"];

/// Where someone is pointed when automatic setup has failed and they have to do
/// it by hand. The publisher whose builds this platform installs, so that what
/// they download matches what setup would have.
#[cfg(windows)]
const MANUAL_SOURCE: &str = "https://www.gyan.dev/ffmpeg/builds/";
#[cfg(target_os = "macos")]
const MANUAL_SOURCE: &str = "https://www.osxexperts.net/";
#[cfg(not(any(windows, target_os = "macos")))]
const MANUAL_SOURCE: &str = "https://ffmpeg.org/download.html";

/// SHA-256 of `vendor/ffmpeg-release-essentials.7z`.
///
/// Refreshing the mirror means refreshing this in the same commit; a mismatch
/// makes setup reject the mirror and fall through to gyan.dev, which is the
/// safe direction to fail in.
#[cfg(windows)]
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
#[cfg(windows)]
const SOURCES: &[Source] = &[
    Source {
        name: "this project's mirror",
        url: "https://raw.githubusercontent.com/alee-ibrahim/vmerge/main/vendor/ffmpeg-release-essentials.7z",
        packing: Packing::SevenZ,
        sha256: Some(MIRROR_SHA256),
        binaries: &BOTH_UNPINNED,
    },
    Source {
        name: "gyan.dev",
        url: "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.7z",
        packing: Packing::SevenZ,
        sha256: None,
        binaries: &BOTH_UNPINNED,
    },
    Source {
        name: "gyan.dev (zip)",
        url: "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip",
        packing: Packing::Zip,
        sha256: None,
        binaries: &BOTH_UNPINNED,
    },
    Source {
        name: "BtbN/GitHub",
        url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip",
        packing: Packing::Zip,
        sha256: None,
        binaries: &BOTH_UNPINNED,
    },
];

/// One Windows archive carries both tools, and neither is pinned: gyan.dev's
/// contents change with every ffmpeg release, and the mirror is pinned at the
/// archive instead.
#[cfg(windows)]
const BOTH_UNPINNED: [Binary; 2] = [
    Binary { stem: "ffmpeg", sha256: None },
    Binary { stem: "ffprobe", sha256: None },
];

/// Where to get ffmpeg on macOS.
///
/// The choice is far narrower than on Windows. ffmpeg.org publishes no macOS
/// binaries; BtbN builds only Windows and Linux; evermeet.cx states outright
/// that it will not build for Apple Silicon. osxexperts.net is the one publisher
/// of static arm64 builds, and it ships ffmpeg and ffprobe as separate archives
/// — which is why a source declares what it provides and setup keeps going until
/// it has everything.
///
/// These URLs carry the major version in the name, so unlike gyan.dev their
/// contents do *not* drift, and the published SHA-256 of each binary is pinned
/// below. The cost is that the next major release lands at a new URL: when
/// osxexperts moves to ffmpeg 10, both the URL and the hash need updating
/// together, exactly as refreshing the Windows mirror does.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const SOURCES: &[Source] = &[
    Source {
        name: "osxexperts.net (ffmpeg, Apple Silicon)",
        url: "https://www.osxexperts.net/ffmpeg9arm.zip",
        packing: Packing::Zip,
        sha256: None,
        binaries: &[Binary {
            stem: "ffmpeg",
            sha256: Some("591260c945d0eef150e3bf82b0ef988bd36a9cecc18ff05d6679617159f0a95e"),
        }],
    },
    Source {
        name: "osxexperts.net (ffprobe, Apple Silicon)",
        url: "https://www.osxexperts.net/ffprobe9arm.zip",
        packing: Packing::Zip,
        sha256: None,
        binaries: &[Binary {
            stem: "ffprobe",
            sha256: Some("e11c17e8200b3ee4c4c186d245e2b4053f01d56957336c1817fca0b997469106"),
        }],
    },
];

/// Intel Macs. osxexperts' Intel builds sit a major version behind its arm64
/// ones (8.0 against 9.0), which is no obstacle here: nothing this program asks
/// of ffmpeg is newer than that.
#[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
const SOURCES: &[Source] = &[
    Source {
        name: "osxexperts.net (ffmpeg, Intel)",
        url: "https://www.osxexperts.net/ffmpeg80intel.zip",
        packing: Packing::Zip,
        sha256: None,
        binaries: &[Binary {
            stem: "ffmpeg",
            sha256: Some("df3f1e3facdc1ae0ad0bd898cdfb072fbc9641bf47b11f172844525a05db8d11"),
        }],
    },
    Source {
        name: "osxexperts.net (ffprobe, Intel)",
        url: "https://www.osxexperts.net/ffprobe80intel.zip",
        packing: Packing::Zip,
        sha256: None,
        binaries: &[Binary {
            stem: "ffprobe",
            sha256: Some("5228e651e2bd67bb55819b27f6138351587b16d2b87446007bf35b7cf930d891"),
        }],
    },
];

/// Everywhere else. There is no published build this could reach for that would
/// be right more often than it was wrong, so setup does not guess: ffmpeg is
/// found on PATH or the user is told how to install it.
#[cfg(not(any(windows, target_os = "macos")))]
const SOURCES: &[Source] = &[];

/// Distinctive enough that sweeping up leftovers cannot touch anyone else's
/// files.
const TEMP_PREFIX: &str = "video-merge-ffmpeg-";

use crate::proc::{EXE_SUFFIX, exe_name, find_on_path};

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

/// The per-user folder tools are installed into when the program's own folder
/// cannot be written to.
///
/// Every platform has one and every platform spells it differently. Returning
/// `None` is not a detail: it is what makes the read-only fallback fail with
/// "no writable folder to install ffmpeg into", so a platform without a branch
/// here has no fallback at all.
#[cfg(windows)]
pub(crate) fn local_app_data() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("video-merge"))
}

/// `~/Library/Application Support` is where a macOS program keeps files the
/// user did not create and would not go looking for, which is exactly this.
#[cfg(target_os = "macos")]
pub(crate) fn local_app_data() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/video-merge"))
}

/// The XDG default, and the fallback its own specification prescribes when
/// `XDG_DATA_HOME` is unset.
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn local_app_data() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|d| d.join("video-merge"))
}

pub fn is_writable(dir: &Path) -> bool {
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

/// How long a transfer may take before it is treated as a stall.
///
/// Generous on purpose: the biggest source here is 106 MB and the slowest
/// measured link served it at 268 KB/s, which is close to seven minutes. The
/// point is only that an unreachable host cannot hang the program for ever,
/// which with no timeout at all it can.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(900);

/// The agent setup downloads use, ffmpeg's and yt-dlp's alike. The self-updater
/// builds its own with tighter limits, because that one runs before the user has
/// asked for anything.
pub(crate) fn setup_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_global(Some(TRANSFER_TIMEOUT))
            .build(),
    )
}

/// Streams a download straight to disk rather than buffering it in memory:
/// the archive is over 100 MB and nothing needs it all at once.
pub fn download(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    reporter: &mut dyn Reporter,
) -> Result<()> {
    let response = agent
        .get(url)
        .header("User-Agent", "video-merge-setup")
        .call()
        .with_context(|| format!("requesting {url}"))?;
    stream_to_file(response, dest, reporter)
}

/// Writes a response body to disk, reporting progress as it goes.
///
/// Split out from `download` so that a caller which has to send its own headers
/// gets the same progress bar and the same short-read check: the self-updater
/// asks GitHub's API for an asset's *bytes*, which takes a specific Accept
/// header, and without one the answer is the asset's metadata instead.
pub fn stream_to_file(
    response: ureq::http::Response<ureq::Body>,
    dest: &Path,
    reporter: &mut dyn Reporter,
) -> Result<()> {
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
pub fn sha256_of(path: &Path) -> Result<String> {
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
    wanted: &[&Binary],
    reporter: &mut dyn Reporter,
) -> Result<Vec<String>> {
    match packing {
        Packing::Zip => unpack_zip(archive_path, stage, reporter)?,
        Packing::SevenZ => unpack_7z(archive_path, stage, reporter)?,
    }
    collect_binaries(stage, target_bin, wanted)
}

/// The only entries worth putting on disk.
///
/// The archive unpacks to about 417 MB, of which we keep 205: ffplay alone is
/// 104 MB and is never invoked, and the rest is documentation and presets. A
/// solid 7z block still has to be *decompressed* in order to reach later
/// entries, but it does not have to be *written*, and skipping the writes is
/// most of the wait on a slow disk.
fn is_wanted(name: &Path) -> bool {
    let Some(leaf) = name.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    [exe_name("ffmpeg"), exe_name("ffprobe")]
        .iter()
        .any(|wanted| leaf.eq_ignore_ascii_case(wanted))
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

    // Unwanted and unsafe entries are both drained rather than skipped: in a
    // solid archive the bytes have to be read to reach whatever comes next, and
    // the progress bar counts them either way.
    if !safe || !is_wanted(name) {
        return std::io::copy(data, &mut std::io::sink()).context("reading the archive");
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
            continue;
        }
        written += write_entry(stage, &relative, &mut entry)?;
        reporter.progress(written.min(total), Some(total));
    }
    reporter.finished();
    Ok(())
}

/// Moves the executables an archive yielded out of the unpacked tree into their
/// final home, and reports which ones made it.
///
/// A source that provides only some of what is needed is not a failure — the
/// macOS archives hold one tool each — so this returns what it installed and
/// lets the caller decide whether setup is finished. Failing outright is
/// reserved for an archive that yielded none of what it promised.
///
/// ffplay is never copied: another 104 MB on disk, and nothing here invokes it.
/// The PowerShell this was ported from took all three.
fn collect_binaries(stage: &Path, target_bin: &Path, wanted: &[&Binary]) -> Result<Vec<String>> {
    fs::create_dir_all(target_bin)
        .with_context(|| format!("creating {}", target_bin.display()))?;

    let mut installed = Vec::new();
    for binary in wanted {
        let name = exe_name(binary.stem);
        let Some(from) = find_extracted(stage, &name) else {
            continue;
        };

        // Checked here rather than at the archive because this is the file that
        // will be executed, and because a publisher who re-zips identical
        // contents has not tampered with anything.
        if let Some(expected) = binary.sha256 {
            let actual = sha256_of(&from).with_context(|| format!("checking {name}"))?;
            if !actual.eq_ignore_ascii_case(expected) {
                bail!(
                    "{name} is not the expected build (sha256 {}, expected {})",
                    &actual[..16.min(actual.len())],
                    &expected[..16.min(expected.len())]
                );
            }
        }

        let to = target_bin.join(&name);
        fs::copy(&from, &to).with_context(|| format!("copying {name}"))?;
        // Extraction wrote it 0644 and macOS has marked it as downloaded, so
        // without this the file that just arrived cannot be run.
        proc::make_installed_runnable(&to);
        installed.push(binary.stem.to_string());
    }

    if installed.is_empty() {
        let names: Vec<String> = wanted.iter().map(|b| exe_name(b.stem)).collect();
        bail!("the downloaded archive did not contain {}", names.join(" or "));
    }
    Ok(installed)
}

/// Walks an extracted tree for a file of this exact name.
///
/// Depth-first from the root, because the layouts differ: a gyan.dev zip buries
/// the binaries under a versioned folder and a `bin/`, while the macOS archives
/// hold a single file at the top with no folder at all.
fn find_extracted(root: &Path, name: &str) -> Option<PathBuf> {
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
            } else if path.file_name().is_some_and(|n| n == name) {
                return Some(path);
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

    let target_bin = target.join("bin");

    // A source is only really good once its archive has unpacked, so download
    // and unpack are attempted together: a 7z that will not decode should fall
    // back to the zip rather than failing setup outright.
    //
    // Sources are taken until everything needed has arrived rather than until
    // one of them succeeds, because a macOS source supplies a single tool. A
    // source offering only what is already installed is skipped, so the Windows
    // path still stops at the first archive that works.
    let mut outstanding: Vec<&str> = NEEDED.to_vec();
    let agent = setup_agent();
    for source in SOURCES {
        if outstanding.is_empty() {
            break;
        }
        let wanted: Vec<&Binary> = source
            .binaries
            .iter()
            .filter(|binary| outstanding.contains(&binary.stem))
            .collect();
        if wanted.is_empty() {
            continue;
        }

        let name = source.name;
        // Named rather than always saying "ffmpeg", because a source may be
        // being fetched for ffprobe alone and watching it claim otherwise is
        // how a correct download looks like a stuck one.
        let fetching: Vec<&str> = wanted.iter().map(|binary| binary.stem).collect();
        // No size in the message: the bar reports whatever the server declares.
        // The figure inherited from the PowerShell said 40 MB; the zip is 106.
        reporter.log(&format!(
            "Downloading {} from {name} - this happens once",
            fetching.join(" and ")
        ));
        if let Err(e) = download(&agent, source.url, &zip_path, reporter) {
            reporter.log(&format!("Could not download from {name}: {e}"));
            continue;
        }

        // Only our own mirror is pinned at the archive, and a mismatch means
        // falling through to upstream rather than unpacking something
        // unexpected. Sources that pin their binaries instead are checked after
        // extraction, in collect_binaries.
        if let Some(expected) = source.sha256 {
            match sha256_of(&zip_path) {
                Ok(actual) if actual.eq_ignore_ascii_case(expected) => {}
                Ok(actual) => {
                    reporter.log(&format!(
                        "The archive from {name} is not the expected one \
                         (sha256 {}, expected {}); trying the next source.",
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
        let _ = fs::remove_dir_all(&stage);
        match extract_binaries(&zip_path, source.packing, &stage, &target_bin, &wanted, reporter) {
            Ok(installed) => outstanding.retain(|stem| !installed.iter().any(|got| got == stem)),
            Err(e) => reporter.log(&format!("Could not unpack the archive from {name}: {e}")),
        }
        let _ = fs::remove_dir_all(&stage);
    }

    let _ = fs::remove_file(&zip_path);
    let _ = fs::remove_dir_all(&stage);

    if !outstanding.is_empty() {
        bail!("could not install {} from any download source", outstanding.join(" or "));
    }

    reporter.log(&format!("ffmpeg ready: {}", target_bin.display()));
    Ok(target_bin.join(exe_name("ffmpeg")))
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
            // Nowhere to download from is a different failure from a download
            // that went wrong, and telling someone their transfer failed when
            // none was ever attempted sends them looking in the wrong place.
            if SOURCES.is_empty() {
                bail!(
                    "ffmpeg is not installed, and this platform has no download source.\n  \
                     Install it with your package manager - for example \
                     \"sudo apt install ffmpeg\" - and run this again."
                );
            }
            reporter.log("First-time setup");
            reporter.log("ffmpeg (the free video engine this tool needs) is not installed yet.");
            reporter.log("Setting it up automatically - no admin rights, nothing installed");
            reporter.log("system-wide. It just lands in an \"ffmpeg\" folder next to this program.");
            install(root, reporter).map_err(|e| {
                anyhow::anyhow!(
                    "Could not set up ffmpeg automatically ({e}).\n  \
                     Manual option:\n  \
                     1. Open {MANUAL_SOURCE}\n  \
                     2. Download the ffmpeg{EXE_SUFFIX} and ffprobe{EXE_SUFFIX} builds\n  \
                     3. Unpack them so that {} exists",
                    root.join("ffmpeg")
                        .join("bin")
                        .join(exe_name("ffmpeg"))
                        .display()
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

    /// What a Windows archive is asked for: both tools, neither pinned.
    fn both() -> [Binary; 2] {
        [
            Binary { stem: "ffmpeg", sha256: None },
            Binary { stem: "ffprobe", sha256: None },
        ]
    }

    /// `extract_binaries` takes the wanted list by reference, the way `install`
    /// builds it when filtering out what is already on disk.
    fn want(binaries: &[Binary]) -> Vec<&Binary> {
        binaries.iter().collect()
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vmerge-ffmpeg-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    #[cfg_attr(miri, ignore)]
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
        let installed = extract_binaries(
            &archive,
            Packing::Zip,
            &dir.join("stage"),
            &target_bin,
            &want(&both()),
            &mut recorder,
        )
        .unwrap();

        assert_eq!(installed, ["ffmpeg", "ffprobe"]);
        let installed_ffmpeg = target_bin.join(exe_name("ffmpeg"));
        assert!(target_bin.join(exe_name("ffprobe")).is_file(), "ffprobe comes along too");
        assert!(
            !target_bin.join(exe_name("ffplay")).is_file(),
            "ffplay is 104 MB and never invoked, so it must not be copied"
        );
        // Nor should anything unwanted have been written on the way through.
        assert!(
            !dir.join("stage").join("ffmpeg-7.1-essentials").join("README.txt").is_file(),
            "entries we do not need must be drained, not written to disk"
        );
        assert!(
            !dir.join("stage").join("ffmpeg-7.1-essentials").join("bin").join(exe_name("ffplay")).is_file(),
            "ffplay must not be written even to the staging folder"
        );
        assert_eq!(fs::metadata(&installed_ffmpeg).unwrap().len(), 2048, "copied whole");

        // The bar needs a total, and it has to arrive at it.
        let (received, total) = recorder.last.expect("progress was reported");
        assert_eq!(total, Some(40 + 2048 + 1024 + 512), "the bar counts every entry unpacked");
        assert_eq!(Some(received), total, "progress must reach the total");
        assert!(recorder.calls > 1);
        assert_eq!(recorder.finishes, 1, "the line gets closed exactly once");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn an_archive_without_ffmpeg_is_rejected() {
        let dir = scratch("empty");
        let archive = dir.join("ffmpeg.zip");
        build_archive(&archive, &[("notes.txt", 10)]);

        let error = extract_binaries(
            &archive,
            Packing::Zip,
            &dir.join("stage"),
            &dir.join("out").join("bin"),
            &want(&both()),
            &mut Recorder::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("did not contain"), "got {error}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A downloaded archive is untrusted input: an entry named ..\..\evil.exe
    /// must not be able to write outside the staging folder.
    #[test]
    #[cfg_attr(miri, ignore)]
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
            &want(&both()),
            &mut Recorder::default(),
        )
        .unwrap();

        assert!(!dir.join("escaped.txt").exists(), "the traversal entry was written outside");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The macOS shape: one archive, one binary, sitting at the top with no
    /// folder around it. The Windows builds always nest theirs under a
    /// versioned folder and a `bin/`, so this layout only appears off Windows
    /// and would otherwise go untested everywhere.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_bare_binary_at_the_archive_root_is_found() {
        let dir = scratch("bare");
        let archive = dir.join("ffmpeg.zip");
        build_archive(&archive, &[(&exe_name("ffmpeg"), 64)]);

        let target_bin = dir.join("out").join("bin");
        let installed = extract_binaries(
            &archive,
            Packing::Zip,
            &dir.join("stage"),
            &target_bin,
            &want(&[Binary { stem: "ffmpeg", sha256: None }]),
            &mut Recorder::default(),
        )
        .unwrap();

        assert_eq!(installed, ["ffmpeg"], "one archive may supply one tool");
        assert!(target_bin.join(exe_name("ffmpeg")).is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    /// An archive supplying only some of what was asked for is a success, not a
    /// failure: setup goes on to the next source for the rest. Without this,
    /// the macOS ffmpeg archive would be rejected for lacking ffprobe.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_partial_archive_reports_only_what_it_supplied() {
        let dir = scratch("partial");
        let archive = dir.join("ffmpeg.zip");
        build_archive(&archive, &[(&exe_name("ffmpeg"), 32)]);

        let installed = extract_binaries(
            &archive,
            Packing::Zip,
            &dir.join("stage"),
            &dir.join("out").join("bin"),
            &want(&both()),
            &mut Recorder::default(),
        )
        .unwrap();

        assert_eq!(installed, ["ffmpeg"], "ffprobe was absent and is still outstanding");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The pinned macOS builds are checked at the binary rather than at the
    /// archive, so a build that is not the expected one has to be refused here.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_binary_that_fails_its_hash_is_refused() {
        let dir = scratch("hash");
        let archive = dir.join("ffmpeg.zip");
        build_archive(&archive, &[(&exe_name("ffmpeg"), 64)]);

        let target_bin = dir.join("out").join("bin");
        let error = extract_binaries(
            &archive,
            Packing::Zip,
            &dir.join("stage"),
            &target_bin,
            &want(&[Binary {
                stem: "ffmpeg",
                sha256: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            }]),
            &mut Recorder::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("not the expected build"), "got {error}");
        assert!(
            !target_bin.join(exe_name("ffmpeg")).is_file(),
            "a binary that failed its hash must not be left installed"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
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
