//! Finding ffmpeg, and installing it per-user if it is missing.
//! Ported from Find-LocalFfmpeg / Install-Ffmpeg / Resolve-Tools.
//!
//! Nothing here needs admin rights: the download lands in an "ffmpeg" folder
//! next to the executable, or in LOCALAPPDATA when that folder is read-only.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::proc;

pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

const SOURCES: [(&str, &str); 2] = [
    (
        "gyan.dev",
        "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip",
    ),
    (
        "BtbN/GitHub",
        "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip",
    ),
];

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
/// the archive is around 40 MB and nothing needs it all at once.
fn download(url: &str, dest: &Path) -> Result<()> {
    let response = ureq::get(url)
        .header("User-Agent", "video-merge-setup")
        .call()
        .with_context(|| format!("requesting {url}"))?;
    let mut reader = response.into_body().into_reader();
    let mut file = fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    io::copy(&mut reader, &mut file).context("writing the download to disk")?;
    Ok(())
}

fn extract_binaries(zip_path: &Path, stage: &Path, target_bin: &Path) -> Result<PathBuf> {
    let file = fs::File::open(zip_path).context("opening the downloaded archive")?;
    let mut archive = zip::ZipArchive::new(file).context("reading the downloaded archive")?;
    archive.extract(stage).context("extracting the archive")?;

    let source_bin = find_extracted_bin(stage)
        .ok_or_else(|| anyhow::anyhow!("the downloaded archive did not contain ffmpeg{EXE_SUFFIX}"))?;

    fs::create_dir_all(target_bin)
        .with_context(|| format!("creating {}", target_bin.display()))?;

    for stem in ["ffmpeg", "ffprobe", "ffplay"] {
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

fn install(root: &Path, log: &mut dyn FnMut(&str)) -> Result<PathBuf> {
    let temp = std::env::temp_dir();
    let stamp = std::process::id();
    let zip_path = temp.join(format!("ffmpeg-{stamp}.zip"));
    let stage = temp.join(format!("ffmpeg-x-{stamp}"));

    // Everything here runs as the current user. If the tool itself sits
    // somewhere unwritable (read-only share, Program Files), fall back to the
    // per-user AppData folder instead of asking for elevation.
    let mut target = root.join("ffmpeg");
    if !is_writable(root) {
        target = local_app_data()
            .map(|d| d.join("ffmpeg"))
            .ok_or_else(|| anyhow::anyhow!("no writable folder to install ffmpeg into"))?;
        log(&format!(
            "The program folder is read-only, installing to {} instead.",
            target.display()
        ));
    }

    let _ = fs::remove_dir_all(&stage);

    let mut downloaded = false;
    for (name, url) in SOURCES {
        log(&format!("Downloading ffmpeg from {name} (about 40 MB, one time only)..."));
        match download(url, &zip_path) {
            Ok(()) => {
                downloaded = true;
                break;
            }
            Err(e) => log(&format!("Could not download from {name}: {e}")),
        }
    }

    let result = if downloaded {
        log("Extracting...");
        extract_binaries(&zip_path, &stage, &target.join("bin"))
    } else {
        Err(anyhow::anyhow!("no download source worked"))
    };

    let _ = fs::remove_file(&zip_path);
    let _ = fs::remove_dir_all(&stage);

    if result.is_ok() {
        log(&format!("ffmpeg ready: {}", target.join("bin").display()));
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
    log: &mut dyn FnMut(&str),
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
            log("First-time setup");
            log("ffmpeg (the free video engine this tool needs) is not installed yet.");
            log("Setting it up automatically - no admin rights, nothing installed");
            log("system-wide. It just lands in an \"ffmpeg\" folder next to this program.");
            install(root, log).map_err(|e| {
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
