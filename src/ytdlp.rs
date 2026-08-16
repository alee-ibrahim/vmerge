//! Finding yt-dlp, and installing it per-user if it is missing.
//!
//! The same shape as ffmpeg.rs - a GitHub release asset, verified, unpacked
//! beside the executable or into LOCALAPPDATA when that folder is read-only, and
//! no admin rights anywhere. Two things differ, and both are deliberate.
//!
//! * **It is fetched lazily.** Almost every run of this program joins clips that
//!   are already on disk and never looks at a link. Downloading 18 MB during
//!   first-time setup would charge that to people who will never use it, so
//!   `resolve` is called from the download worker rather than from startup.
//! * **It goes stale.** An ffmpeg from two years ago still joins clips; a yt-dlp
//!   from two years ago fails on YouTube, because the sites it reads change
//!   underneath it and upstream ships a fix most weeks. A copy this program
//!   installed therefore updates itself before it is used if it has not been
//!   checked in a fortnight. A copy found on PATH is left alone: it belongs to
//!   whoever put it there.
//!
//! What the digest is worth is worth being honest about. `SHA2-256SUMS` comes
//! from the same release as the executable, so it is no defence against a
//! compromised release - only against a truncated download, a mirror serving
//! something else, and the asset mixup that would otherwise install an ARM build
//! on an x86 machine. Those are the failures that actually happen.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::ffmpeg::{self, Reporter};
use crate::proc;

/// The repository releases are accepted from. Nothing else is.
const REPO: &str = "yt-dlp/yt-dlp";

const LATEST_RELEASE: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";

/// The digests published beside the executables.
const SUMS: &str = "SHA2-256SUMS";

/// GitHub answers 403 to a request with no User-Agent, so it is required rather
/// than polite.
const AGENT_NAME: &str = "video-merge-setup";

/// Which release asset this build needs. Picking the wrong one installs a binary
/// that cannot run at all, which is why it is decided at compile time.
#[cfg(all(windows, target_arch = "aarch64"))]
const ASSET: &str = "yt-dlp_arm64.exe";
#[cfg(all(windows, target_arch = "x86"))]
const ASSET: &str = "yt-dlp_x86.exe";
#[cfg(all(windows, not(any(target_arch = "aarch64", target_arch = "x86"))))]
const ASSET: &str = "yt-dlp.exe";
#[cfg(target_os = "macos")]
const ASSET: &str = "yt-dlp_macos";
#[cfg(all(unix, not(target_os = "macos")))]
const ASSET: &str = "yt-dlp_linux";
#[cfg(not(any(windows, unix)))]
const ASSET: &str = "yt-dlp";

#[cfg(windows)]
const LOCAL_NAME: &str = "yt-dlp.exe";
#[cfg(not(windows))]
const LOCAL_NAME: &str = "yt-dlp";

/// Where an installed copy goes, relative to the folder it is installed into.
const FOLDER: &str = "yt-dlp";

/// Written beside the installed copy after every update check, so a check that
/// fails is not repeated on every single download.
const STAMP: &str = ".checked";

/// How long an installed copy may go unchecked before it updates itself.
///
/// Upstream releases roughly weekly. A fortnight is long enough that this is not
/// a network round trip on every download, and short enough that a site change
/// is usually already fixed by the time someone hits it.
const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// How old a copy on PATH may be before this installs its own instead.
///
/// A copy someone else put there is normally left well alone - it is theirs, and
/// updating it behind their back would be rude. But yt-dlp names its versions
/// after the day they were released, so how stale one is can simply be read off
/// it, and past a certain age it does not work at all: YouTube answers
/// `403 Forbidden` and the user is left with an error about streaming protocols
/// they have no way to connect to a package they installed months ago. Three
/// months is well beyond the point where that starts happening.
const PATH_COPY_MAX_AGE_DAYS: i64 = 90;

/// yt-dlp, and whether it is ours to keep current.
pub struct Tool {
    pub path: PathBuf,
    /// Installed by this program, so updating it is our business. A copy found
    /// on PATH belongs to whoever put it there and is never touched.
    pub managed: bool,
}

/// The two fields the release API is asked for.
#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// What a release offers this machine, once it has been through the rules.
#[derive(Debug, PartialEq, Eq)]
struct Latest {
    version: String,
    url: String,
    sums_url: String,
}

/// Whether a URL leads to a release asset of the yt-dlp repository, over TLS.
///
/// The prefix has to match exactly, which is what keeps a look-alike host such
/// as `github.com.example.net` out: its slash never falls in the right place.
fn trusted(url: &str) -> bool {
    url.strip_prefix("https://github.com/")
        .is_some_and(|rest| rest.starts_with(&format!("{REPO}/releases/download/")))
}

/// Reads the API answer and applies every rule that does not need a network, so
/// all of them can be tested without one.
fn read_release(json: &str) -> Result<Latest> {
    let release: ApiRelease =
        serde_json::from_str(json).context("reading the yt-dlp release listing")?;
    let version = release.tag_name.trim().to_string();
    if version.is_empty() {
        bail!("the newest yt-dlp release has no tag");
    }

    let find = |wanted: &str| -> Option<String> {
        release
            .assets
            .iter()
            .find(|a| a.name == wanted)
            .map(|a| a.browser_download_url.clone())
            .filter(|url| trusted(url))
    };

    let Some(url) = find(ASSET) else {
        bail!("yt-dlp {version} publishes no {ASSET} we can use");
    };
    let Some(sums_url) = find(SUMS) else {
        bail!("yt-dlp {version} publishes no {SUMS}, so the download cannot be checked");
    };
    Ok(Latest { version, url, sums_url })
}

/// The digest for one file out of a `sha256sum`-style listing.
///
/// Every asset in the release is in there, so the name has to be matched rather
/// than the first line taken - which would install the ARM build's digest
/// against the x86 one's bytes and reject a perfectly good download.
fn digest_for(listing: &str, name: &str) -> Option<String> {
    for line in listing.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hex), Some(file)) = (parts.next(), parts.next()) else {
            continue;
        };
        // `sha256sum` marks a binary-mode file with a leading '*'.
        if file.trim_start_matches('*') != name {
            continue;
        }
        let hex = hex.to_ascii_lowercase();
        if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hex);
        }
    }
    None
}

fn fetch_text(agent: &ureq::Agent, url: &str) -> Result<String> {
    agent
        .get(url)
        .header("User-Agent", AGENT_NAME)
        .header("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("asking {url}"))?
        .into_body()
        .read_to_string()
        .context("reading the answer")
}

/// The usual places a copy sits. `roots` is searched in order, which is how a
/// build in target/release still finds the yt-dlp folder beside the project.
fn find_local(roots: &[PathBuf]) -> Option<PathBuf> {
    for root in roots {
        for relative in [PathBuf::from(FOLDER).join(LOCAL_NAME), PathBuf::from(LOCAL_NAME)] {
            let candidate = root.join(relative);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Makes a freshly written file runnable. On Windows the bit does not exist; the
/// SmartScreen mark does, and `proc::unblock` clears that instead.
#[cfg(unix)]
fn make_runnable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn make_runnable(_path: &Path) {}

/// Days since 1970-01-01 for a proleptic Gregorian date.
///
/// Howard Hinnant's `days_from_civil`. Exact, and short enough not to be worth a
/// calendar dependency for the one question asked of it here.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // March-based years, so the leap day lands at the end and needs no case.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// yt-dlp names its versions after the day they were released - `2026.07.04`,
/// with a fourth part for a same-day rebuild. Read as days since the epoch, so
/// it can be compared with the clock without a calendar.
fn release_day(text: &str) -> Option<i64> {
    let mut parts = text.trim().split('.');
    let year: i64 = parts.next()?.trim().parse().ok()?;
    let month: i64 = parts.next()?.trim().parse().ok()?;
    let day: i64 = parts.next()?.trim().parse().ok()?;
    let sane = (2000..=3000).contains(&year)
        && (1..=12).contains(&month)
        && (1..=31).contains(&day);
    sane.then(|| days_from_civil(year, month, day))
}

/// How many days old a copy says it is, by asking it.
///
/// None when it will not say - a wrapper script, a build with no version, a
/// binary that does not run at all. Nothing is concluded from silence: an
/// unreadable version must not be treated as an ancient one.
fn age_in_days(exe: &Path) -> Option<i64> {
    let mut command = proc::command(exe);
    command.arg("--version");
    let output = proc::run_captured(command).ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let released = release_day(text.lines().next()?)?;
    let today = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64 / 86_400;
    Some(today - released)
}

fn stamp_path(exe: &Path) -> PathBuf {
    exe.parent().unwrap_or(Path::new(".")).join(STAMP)
}

/// Records that an update check just happened, whether or not it achieved
/// anything. Without this a check that cannot reach the network would run again
/// on every single download.
fn touch_stamp(exe: &Path) {
    let _ = fs::write(stamp_path(exe), b"");
}

fn stamp_is_stale(exe: &Path) -> bool {
    fs::metadata(stamp_path(exe))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|when| when.elapsed().ok())
        .is_none_or(|age| age >= MAX_AGE)
}

fn install(root: &Path, reporter: &mut dyn Reporter) -> Result<PathBuf> {
    let agent = ffmpeg::setup_agent();
    let latest = read_release(&fetch_text(&agent, LATEST_RELEASE)?)?;

    // Everything here runs as the current user. If the tool itself sits
    // somewhere unwritable (read-only share, Program Files), fall back to the
    // per-user AppData folder instead of asking for elevation.
    let mut target = root.join(FOLDER);
    if !ffmpeg::is_writable(root) {
        target = ffmpeg::local_app_data()
            .map(|d| d.join(FOLDER))
            .ok_or_else(|| anyhow::anyhow!("no writable folder to install yt-dlp into"))?;
        reporter.log(&format!(
            "The program folder is read-only, installing to {} instead.",
            target.display()
        ));
    }
    fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;

    // Downloaded beside its destination rather than into the temp folder, so the
    // move at the end is a rename on one volume and cannot half-succeed.
    let staged = target.join(format!(".{ASSET}.part{}", std::process::id()));
    let result = (|| -> Result<()> {
        reporter.log(&format!("Downloading yt-dlp {} - this happens once", latest.version));
        ffmpeg::download(&agent, &latest.url, &staged, reporter)?;

        let published = digest_for(&fetch_text(&agent, &latest.sums_url)?, ASSET)
            .with_context(|| format!("{SUMS} in yt-dlp {} lists no {ASSET}", latest.version))?;
        let actual = ffmpeg::sha256_of(&staged)?;
        if actual != published {
            bail!("the download hashes to {actual}, and the release says {published}");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result?;

    let installed = target.join(LOCAL_NAME);
    fs::rename(&staged, &installed)
        .with_context(|| format!("installing {}", installed.display()))?;
    // A fresh download carries the mark that makes SmartScreen interrupt its
    // first run, and this one is about to be run without anyone watching.
    proc::unblock(&installed);
    make_runnable(&installed);
    // Just installed, so it is as current as it can be: start the clock now
    // rather than updating it again on the very first download.
    touch_stamp(&installed);

    reporter.log(&format!("yt-dlp ready: {}", installed.display()));
    Ok(installed)
}

/// Finds yt-dlp, installing it if needed.
///
/// `root` is where an installed copy goes; `search` also covers the parents of
/// the executable so a cargo-built binary finds a sibling yt-dlp folder.
pub fn resolve(
    root: &Path,
    search: &[PathBuf],
    allow_download: bool,
    reporter: &mut dyn Reporter,
) -> Result<Tool> {
    let mut roots: Vec<PathBuf> = search.to_vec();
    if let Some(appdata) = ffmpeg::local_app_data() {
        roots.push(appdata);
    }

    // A local copy wins over one on PATH, the other way round from ffmpeg: this
    // is the copy we keep current, and a pip install found first would go stale
    // without anybody being in a position to do something about it.
    if let Some(found) = find_local(&roots) {
        return Ok(Tool { path: found, managed: true });
    }

    let on_path = proc::find_on_path("yt-dlp");
    let stale = match &on_path {
        Some(found) => match age_in_days(found) {
            Some(age) if age > PATH_COPY_MAX_AGE_DAYS => Some(age),
            // Current enough, or it would not say - either way it is theirs and
            // it is used as it is.
            _ => return Ok(Tool { path: found.clone(), managed: false }),
        },
        None => None,
    };

    if !allow_download {
        // An old copy still beats no copy when fetching one is forbidden.
        if let Some(found) = on_path {
            return Ok(Tool { path: found, managed: false });
        }
        bail!("yt-dlp is missing and --skip-ytdlp-download was set.");
    }

    match stale {
        Some(age) => {
            reporter.log(&format!(
                "The yt-dlp on your PATH was released {age} days ago, which is old enough"
            ));
            reporter.log("to be refused by most video sites. Leaving it alone and installing");
            reporter.log("a copy this program can keep up to date instead.");
        }
        None => {
            reporter.log("yt-dlp (the downloader this needs) is not installed yet.");
            reporter.log("Setting it up automatically - no admin rights, nothing installed");
            reporter.log("system-wide. It just lands in a \"yt-dlp\" folder next to this program.");
        }
    }

    let path = install(root, reporter).map_err(|e| {
        anyhow::anyhow!(
            "Could not set up yt-dlp automatically ({e}).\n  \
             Manual option:\n  \
             1. Open https://github.com/{REPO}/releases/latest\n  \
             2. Download \"{ASSET}\"\n  \
             3. Save it as {}",
            root.join(FOLDER).join(LOCAL_NAME).display()
        )
    })?;
    Ok(Tool { path, managed: true })
}

/// Updates a copy this program installed, if it has not been checked lately.
///
/// Best effort throughout. yt-dlp failing to update itself is not a reason to
/// refuse a download - the copy on disk may well still work, and finding out
/// costs one attempt.
pub fn refresh_if_stale(tool: &Tool, reporter: &mut dyn Reporter) {
    if !tool.managed || !stamp_is_stale(&tool.path) {
        return;
    }
    reporter.log("Checking for a newer yt-dlp - sites change, and it keeps up with them.");
    // Recorded before the attempt, not after: a check that cannot reach the
    // network must still count as a check, or every download retries it.
    touch_stamp(&tool.path);

    let mut command = proc::command(&tool.path);
    command.arg("--update");
    match proc::run_captured(command) {
        Ok(output) if output.status.success() => {
            let said = String::from_utf8_lossy(&output.stdout);
            // "yt-dlp is up to date" is the usual answer and says nothing worth
            // a line; an actual update is worth one.
            if let Some(line) = said.lines().find(|l| l.contains("Updated yt-dlp")) {
                reporter.log(line.trim());
            }
        }
        Ok(output) => {
            reporter.log(&format!(
                "yt-dlp could not update itself ({}). Carrying on with the copy on disk.",
                proc::error_tail(&output.stderr, 1)
            ));
        }
        Err(e) => reporter.log(&format!("yt-dlp could not be run to update itself ({e}).")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(tag: &str, assets: &[(&str, &str)]) -> String {
        let listed: Vec<String> = assets
            .iter()
            .map(|(name, url)| format!(r#"{{"name":"{name}","browser_download_url":"{url}"}}"#))
            .collect();
        format!(r#"{{"tag_name":"{tag}","assets":[{}]}}"#, listed.join(","))
    }

    fn good(name: &str) -> String {
        format!("https://github.com/yt-dlp/yt-dlp/releases/download/2026.07.04/{name}")
    }

    #[test]
    fn a_release_offers_this_machine_its_own_asset() {
        let json = payload(
            "2026.07.04",
            &[
                ("yt-dlp.exe", &good("yt-dlp.exe")),
                ("yt-dlp_arm64.exe", &good("yt-dlp_arm64.exe")),
                ("yt-dlp_linux", &good("yt-dlp_linux")),
                ("yt-dlp_macos", &good("yt-dlp_macos")),
                (SUMS, &good(SUMS)),
            ],
        );
        let latest = read_release(&json).expect("a usable release");
        assert_eq!(latest.version, "2026.07.04");
        // Whichever this build is, it must be the one asked for by name.
        assert_eq!(latest.url, good(ASSET));
        assert_eq!(latest.sums_url, good(SUMS));
    }

    #[test]
    fn releases_that_cannot_be_trusted_are_refused() {
        // No digests published, so nothing could be checked.
        let no_sums = payload("2026.07.04", &[(ASSET, &good(ASSET))]);
        let error = read_release(&no_sums).unwrap_err().to_string();
        assert!(error.contains(SUMS), "got {error}");

        // Nothing for this machine.
        let wrong = payload("2026.07.04", &[("yt-dlp_somethingelse", &good("x")), (SUMS, &good(SUMS))]);
        let error = read_release(&wrong).unwrap_err().to_string();
        assert!(error.contains(ASSET), "got {error}");

        // An asset hosted somewhere else is dropped rather than fetched from
        // wherever it points, which leaves the release unusable.
        let off_site = payload(
            "2026.07.04",
            &[(ASSET, "https://example.net/yt-dlp.exe"), (SUMS, &good(SUMS))],
        );
        assert!(read_release(&off_site).is_err());
    }

    #[test]
    fn the_download_has_to_come_from_the_yt_dlp_repository() {
        assert!(trusted(&good(ASSET)));
        for bad in [
            "https://github.com.example.net/yt-dlp/yt-dlp/releases/download/v1/yt-dlp.exe",
            "https://example.net/yt-dlp/yt-dlp/releases/download/v1/yt-dlp.exe",
            "https://github.com/someone/yt-dlp/releases/download/v1/yt-dlp.exe",
            "https://github.com/yt-dlp/yt-dlp/raw/master/yt-dlp.exe",
            "http://github.com/yt-dlp/yt-dlp/releases/download/v1/yt-dlp.exe",
        ] {
            assert!(!trusted(bad), "{bad} must be refused");
        }
    }

    /// Every asset is listed in one file, so the wrong line would reject a
    /// perfectly good download - or, worse, accept the wrong build.
    #[test]
    fn the_digest_is_matched_by_name() {
        let listing = "\
1111111111111111111111111111111111111111111111111111111111111111  yt-dlp
2222222222222222222222222222222222222222222222222222222222222222  yt-dlp.exe
3333333333333333333333333333333333333333333333333333333333333333 *yt-dlp_arm64.exe
";
        assert_eq!(digest_for(listing, "yt-dlp.exe"), Some("2".repeat(64)));
        assert_eq!(digest_for(listing, "yt-dlp_arm64.exe"), Some("3".repeat(64)));
        assert_eq!(digest_for(listing, "yt-dlp_x86.exe"), None);

        // Nothing usable rather than something wrong.
        assert_eq!(digest_for("not a digest  yt-dlp.exe", "yt-dlp.exe"), None);
        assert_eq!(digest_for("", "yt-dlp.exe"), None);
    }

    #[test]
    fn dates_convert_to_days_the_way_the_calendar_says() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        // A leap year, and a century that is one despite the rule of thumb.
        assert_eq!(days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28), 2);
        assert_eq!(days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 28), 1);
        assert_eq!(days_from_civil(2026, 1, 1) - days_from_civil(2025, 1, 1), 365);
    }

    /// The whole point of reading the version is telling a fresh copy from one
    /// that will simply be refused by the sites it is pointed at.
    #[test]
    fn a_version_is_read_as_the_day_it_was_released() {
        let old = release_day("2025.12.08").expect("a version");
        let new = release_day("2026.07.04").expect("a version");
        assert_eq!(new - old, 208, "the gap has to be in real days");
        // A same-day rebuild carries a fourth part, and still means that day.
        assert_eq!(release_day("2026.07.04.232919"), Some(new));
        assert_eq!(release_day("  2026.07.04\n"), Some(new));

        // Nothing rather than a wrong answer: silence must not read as ancient,
        // because that would install over a copy that was fine.
        for bad in ["", "unknown", "2026.07", "nightly", "2026.13.01", "0.1.2"] {
            assert_eq!(release_day(bad), None, "{bad:?} is not a release date");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_missing_stamp_counts_as_stale_and_writing_one_clears_it() {
        let dir = std::env::temp_dir().join(format!("vmerge-ytdlp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(LOCAL_NAME);
        fs::write(&exe, b"x").unwrap();

        assert!(stamp_is_stale(&exe), "never checked, so it is due one");
        touch_stamp(&exe);
        assert!(!stamp_is_stale(&exe), "a check just happened");

        let _ = fs::remove_dir_all(&dir);
    }
}
