//! Self-update: looks for a newer release on every start, and replaces this
//! executable with it.
//!
//! The people this is built for double-click an exe someone sent them. They do
//! not watch a repository, and will never hear that a fix exists — so the fix
//! has to come to them.
//!
//! Four things make doing that without asking defensible:
//!
//! * **The source is pinned.** Only a release asset of this repository, over
//!   TLS. The API answer arrives over TLS from GitHub already, but the download
//!   URL is the one field that decides what gets executed, so it is checked
//!   against the repository it has to belong to.
//! * **Forwards only.** The remote version has to parse *and* be strictly
//!   greater than this build. A tag nobody can read is refused rather than
//!   guessed at, so a renamed or malformed release cannot install an older
//!   binary over a newer one.
//! * **Verified before it is swapped in.** The declared length, a Windows
//!   executable header, and a SHA-256 whenever the release publishes one beside
//!   the exe as `MERGE-VIDEOS.exe.sha256`. Publishing that file is worth the
//!   half-second it takes: it is the only check here that would survive someone
//!   with write access to the release but not to the tag.
//! * **Failing changes nothing.** No network, a rate-limited API, an asset host
//!   that stalls — documented as intermittent for GitHub releases, which is why
//!   ffmpeg is mirrored elsewhere in this crate — or a folder that cannot be
//!   written all leave the running executable exactly as it was. An update that
//!   does not happen must never be the reason a merge does not happen.
//!
//! Where the bytes come from is not a detail. `github.com/…/releases/download/…`
//! redirects to `objects.githubusercontent.com`, which on the connection this
//! was measured on refuses every connection in about 300 ms — five attempts out
//! of five, with curl as well as from here. It is the same unreliability that
//! made this project mirror ffmpeg in its own repository rather than attach it to
//! a release. GitHub's *API* asset route serves the identical bytes through
//! `release-assets.githubusercontent.com` and worked first time, so that is
//! tried first and the browser URL is the fallback — the same "best source
//! first, and one only counts once it has arrived" shape as ffmpeg setup.
//!
//! The swap is the Windows rename trick. A running image cannot be overwritten,
//! but it *can* be renamed out of the way, which frees its name for the new
//! file. Both moves are on one volume, so the name is never pointing at nothing.
//! The old image is hidden and swept up on a later start, because nothing can
//! delete it while it is still the process doing the deleting.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::ffmpeg::{self, Reporter};
use crate::proc;

/// The repository releases are accepted from. Nothing else is.
const REPO: &str = "alee-ibrahim/vmerge";

const LATEST_RELEASE: &str = "https://api.github.com/repos/alee-ibrahim/vmerge/releases/latest";

/// The asset to install, and the name its digest would be published under.
const ASSET: &str = "MERGE-VIDEOS.exe";

/// Set on the executable we hand over to, so the new version does not open by
/// checking for an update all over again. A loop of relaunches would be the one
/// failure here a user could not get out of.
const HANDED_OVER: &str = "VMERGE_UPDATED";

/// Where the running image is moved to so its name can be reused.
const PREVIOUS: &str = ".previous";

/// Prefix for the part-downloaded file, distinctive enough that sweeping up
/// leftovers cannot touch anything else.
const STAGING: &str = ".vmerge-update-";

/// A version, as its three numbers.
///
/// Compared field by field rather than as text, because `0.1.10` has to come
/// after `0.1.9` and as strings it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

impl Version {
    /// Reads `0.1.2`, or `v0.1.2` as a tag is written.
    ///
    /// Anything else — a missing part, a suffix, a fourth number — is refused
    /// rather than interpreted. This is the gate that decides whether a remote
    /// build counts as newer, so it has to be exact or not answer at all.
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.');
        let major = parts.next()?.parse::<u32>().ok()?;
        let minor = parts.next()?.parse::<u32>().ok()?;
        let patch = parts.next()?.parse::<u32>().ok()?;
        parts.next().is_none().then_some(Self { major, minor, patch })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// This build's own version, taken from Cargo.toml so that the crate, the tag
/// and the installed binary cannot disagree about what is running.
pub fn current() -> Version {
    // A version this program cannot read would make every release look newer, so
    // it fails the other way instead: nothing is ever newer than this. The test
    // below keeps the real value honest.
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version {
        major: u32::MAX,
        minor: 0,
        patch: 0,
    })
}

/// What the newest release says about itself, once it has been through the rules.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Latest {
    version: Version,
    /// Where the executable can be fetched, best first.
    urls: Vec<String>,
    /// The length GitHub declares for the asset. A download that does not match
    /// it is not the asset.
    size: u64,
    /// Where the published digest is, best first. Empty when the release does
    /// not publish one.
    digest_urls: Vec<String>,
}

/// The handful of fields the release API is asked for.
#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    id: u64,
}

impl ApiAsset {
    /// Every route to this asset, best first. See the note at the top of the
    /// file: the browser URL's host is the one that fails.
    fn routes(&self) -> Vec<String> {
        let mut routes = Vec::with_capacity(2);
        // Zero means the answer carried no id, and asset 0 does not exist.
        if self.id != 0 {
            routes.push(format!("https://api.github.com/repos/{REPO}/releases/assets/{}", self.id));
        }
        routes.push(self.browser_download_url.clone());
        routes.retain(|url| trusted(url));
        routes
    }
}

/// Whether a URL leads to a release asset of this repository, over TLS.
///
/// Both prefixes have to match exactly, which is what keeps a look-alike host
/// such as `github.com.example.net` out: its slash never falls in the right
/// place.
fn trusted(url: &str) -> bool {
    let api = format!("https://api.github.com/repos/{REPO}/releases/assets/");
    if let Some(id) = url.strip_prefix(&api) {
        // Nothing but an asset number, so no path can be appended to it.
        return !id.is_empty() && id.chars().all(|c| c.is_ascii_digit());
    }
    url.strip_prefix("https://github.com/")
        .is_some_and(|rest| rest.starts_with(&format!("{REPO}/releases/download/")))
}

/// Reads the API answer and applies every rule that does not need a network, so
/// all of them can be tested without one.
fn read_release(json: &str) -> Result<Latest> {
    let release: ApiRelease = serde_json::from_str(json).context("reading the release listing")?;
    if release.draft || release.prerelease {
        bail!("the newest release is not a finished one");
    }
    let Some(version) = Version::parse(&release.tag_name) else {
        bail!("the tag {} is not a version this build can compare", release.tag_name);
    };

    let Some(asset) = release.assets.iter().find(|a| a.name == ASSET) else {
        bail!("release {version} has no {ASSET}");
    };
    if asset.size == 0 {
        bail!("{ASSET} in release {version} declares no length");
    }
    let urls = asset.routes();
    if urls.is_empty() {
        bail!("{} is not a release asset of {REPO}", asset.browser_download_url);
    }

    let digest_name = format!("{ASSET}.sha256");
    let digest_urls = release
        .assets
        .iter()
        .find(|a| a.name == digest_name)
        .map(ApiAsset::routes)
        .unwrap_or_default();

    Ok(Latest { version, urls, size: asset.size, digest_urls })
}

/// The hex digest out of a `sha256sum`-style file, which may or may not have a
/// filename after it.
fn read_digest(text: &str) -> Option<String> {
    let word = text.split_whitespace().next()?.to_ascii_lowercase();
    let usable = word.len() == 64 && word.chars().all(|c| c.is_ascii_hexdigit());
    usable.then_some(word)
}

/// A downloaded file that is not a Windows executable is not the one we asked
/// for, whatever its length says.
#[cfg(windows)]
fn looks_executable(path: &Path) -> bool {
    use std::io::Read;

    let mut header = [0u8; 2];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok_and(|()| &header == b"MZ")
}

#[cfg(not(windows))]
fn looks_executable(_path: &Path) -> bool {
    true
}

fn previous_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(PREVIOUS);
    PathBuf::from(name)
}

/// Where the running image can be moved to, best first.
///
/// The plain `.previous` is the name to use, and it is also the name that gets
/// stuck: another copy of the program started before the last update is still
/// running from that image, so it can be neither deleted nor written over, and
/// every update from then on fails at the same step - which is exactly what
/// happened here, silently, for two versions in a row.
///
/// One live process can hold one name. The fallback carries this process's id, so
/// there is always a free name to move aside to, and the sweep clears both.
fn aside_paths(exe: &Path) -> Vec<PathBuf> {
    let mut name = exe.as_os_str().to_owned();
    name.push(format!("{PREVIOUS}-{}", std::process::id()));
    vec![previous_path(exe), PathBuf::from(name)]
}

/// Clears up after an earlier update, whatever this run is going to do.
///
/// Deliberately not part of `run`: leaving a hidden copy of a previous version
/// lying about for ever is not something turning update checks off should ask
/// for, and it is the only housekeeping here that nothing else will get to.
pub fn tidy() {
    if let Ok(exe) = std::env::current_exe()
        && let (Some(dir), Some(name)) = (exe.parent(), exe.file_name())
    {
        sweep(dir, name);
    }
}

/// Clears out what an earlier update could not: the image it was running at the
/// time, and any download that never finished.
fn sweep(dir: &Path, exe_name: &OsStr) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let stale_image = {
        let mut name = exe_name.to_owned();
        name.push(PREVIOUS);
        name
    };
    // Starts with rather than equals: an update that could not have the plain
    // `.previous` moved the image aside under a name carrying its process id, and
    // that copy needs sweeping just as much.
    let stale_prefix = stale_image.to_str().map(str::to_owned);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let ours = name == stale_image
            || name.to_str().is_some_and(|text| {
                text.starts_with(STAGING)
                    || stale_prefix.as_deref().is_some_and(|stale| text.starts_with(stale))
            });
        // Best effort throughout: another copy of the program may still be
        // running from one of these, and then the delete simply fails.
        if ours {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The check has to be quick, because it is on the path of every launch.
///
/// Measured here: the round trip settles at about 280 ms, and a network that is
/// simply absent refuses in about 300 ms. Neither of those needs a long fuse -
/// what the fuse is for is the network that accepts a connection and then says
/// nothing, and six seconds of that is already more than anyone double-clicking
/// an icon should be asked to wait for a check they did not ask for.
fn check_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            .timeout_global(Some(Duration::from_secs(6)))
            .build(),
    )
}

/// Bounded, because the release-asset host is known to stall on some networks
/// and a launch that hangs would be far worse than an update that is skipped.
/// Four megabytes in two minutes is slower than any connection this has been
/// measured on.
fn download_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(120)))
            .build(),
    )
}

/// GitHub answers 403 to a request with no User-Agent, so it is required rather
/// than polite.
const AGENT_NAME: &str = "video-merge-updater";

/// What the API returns when asked about a release.
const JSON: &str = "application/vnd.github+json";

/// What it returns when asked for an asset's contents. Without this the asset
/// route answers with the asset's metadata instead of the file.
const BYTES: &str = "application/octet-stream";

fn get(
    agent: &ureq::Agent,
    url: &str,
    accept: &str,
) -> Result<ureq::http::Response<ureq::Body>> {
    agent
        .get(url)
        .header("User-Agent", AGENT_NAME)
        .header("Accept", accept)
        .call()
        .with_context(|| format!("asking {url}"))
}

fn fetch_text(agent: &ureq::Agent, url: &str, accept: &str) -> Result<String> {
    get(agent, url, accept)?.into_body().read_to_string().context("reading the answer")
}

/// What happened, from the caller's point of view.
pub enum Outcome {
    /// Nothing to install, or nothing could be found out. Either way, carry on
    /// with the version that is running.
    UpToDate,
    /// A newer build is now under this executable's name. This process is still
    /// the old image, so the caller hands over to it.
    Replaced(Version),
    /// A newer version exists and could not be installed. Carried out of here
    /// rather than only printed, because on the interactive path the console line
    /// saying so is wiped by the full-screen UI a moment later - which is how two
    /// versions' worth of failed updates went unnoticed.
    Failed(Version),
}

/// Looks for a newer release and installs it.
///
/// Never returns an error: an update is an improvement, not a requirement, so
/// every way it can fail ends with the program carrying on as it was. Only a
/// failure that happens *after* the user has been told an update is coming is
/// worth a line on screen; a check that could not reach the network is silent,
/// because saying so on every offline start would train people to ignore it.
pub fn run(reporter: &mut dyn Reporter) -> Outcome {
    let Ok(exe) = std::env::current_exe() else {
        return Outcome::UpToDate;
    };
    // The version we just handed over to must not turn round and do this again.
    if std::env::var_os(HANDED_OVER).is_some() {
        return Outcome::UpToDate;
    }

    let here = current();
    let Ok(latest) = fetch_text(&check_agent(), LATEST_RELEASE, JSON)
        .and_then(|json| read_release(&json))
    else {
        return Outcome::UpToDate;
    };
    if latest.version <= here {
        return Outcome::UpToDate;
    }

    reporter.log(&format!("Version {} is out — this is {here}. Updating.", latest.version));
    match install(&exe, &latest, reporter) {
        Ok(()) => Outcome::Replaced(latest.version),
        Err(error) => {
            reporter.finished();
            reporter.log(&format!("The update did not finish ({error:#})."));
            reporter.log(&format!("Carrying on with {here}. Nothing was changed."));
            Outcome::Failed(latest.version)
        }
    }
}

/// Downloads the new build, checks it, and puts it where the old one was.
fn install(exe: &Path, latest: &Latest, reporter: &mut dyn Reporter) -> Result<()> {
    let dir = exe.parent().unwrap_or(Path::new("."));
    if !ffmpeg::is_writable(dir) {
        bail!("{} cannot be written to", dir.display());
    }

    let staged = dir.join(format!("{STAGING}{}.exe", std::process::id()));
    let agent = download_agent();
    let result = verified_download(&agent, latest, &staged, reporter);
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result?;

    // A fresh download carries the mark that makes SmartScreen interrupt the
    // first run, and this one is about to become the program itself.
    proc::unblock(&staged);
    swap_in(exe, &staged)
}

fn verified_download(
    agent: &ureq::Agent,
    latest: &Latest,
    staged: &Path,
    reporter: &mut dyn Reporter,
) -> Result<()> {
    fetch_asset(agent, &latest.urls, staged, reporter)?;

    let written = fs::metadata(staged).map(|m| m.len()).unwrap_or(0);
    if written != latest.size {
        bail!("the download is {written} bytes, and the release says {}", latest.size);
    }
    if !looks_executable(staged) {
        bail!("the download is not a Windows executable");
    }

    // Only when the release publishes one: a release without a digest could
    // otherwise never be installed at all. But a digest that is published and
    // then cannot be read, or does not match, stops the update dead - it is 65
    // bytes, so failing to fetch it says something is wrong.
    if !latest.digest_urls.is_empty() {
        let published = latest
            .digest_urls
            .iter()
            .find_map(|url| fetch_text(agent, url, BYTES).ok().as_deref().and_then(read_digest))
            .context("reading the published sha256")?;
        let actual = ffmpeg::sha256_of(staged)?;
        if actual != published {
            bail!("the download hashes to {actual}, and the release says {published}");
        }
    }
    Ok(())
}

/// Fetches the asset, trying each route until one delivers it.
fn fetch_asset(
    agent: &ureq::Agent,
    urls: &[String],
    staged: &Path,
    reporter: &mut dyn Reporter,
) -> Result<()> {
    let mut last = None;
    for url in urls {
        match get(agent, url, BYTES).and_then(|r| ffmpeg::stream_to_file(r, staged, reporter)) {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no download route was usable")))
}

/// Puts `staged` under the running executable's name.
fn swap_in(exe: &Path, staged: &Path) -> Result<()> {
    let mut last = None;
    for previous in aside_paths(exe) {
        // Left over from an update that has since been restarted, in which case
        // nothing is running from it any more - or still in use by a copy that has
        // not been closed, in which case this fails and so does the rename, and the
        // next name along is tried instead.
        let _ = fs::remove_file(&previous);
        if let Err(error) = fs::rename(exe, &previous) {
            last = Some(error);
            continue;
        }
        proc::set_hidden(&previous);

        if let Err(error) = fs::rename(staged, exe) {
            // Put the working copy back rather than leave the folder with no
            // executable in it at all.
            let _ = fs::rename(&previous, exe);
            return Err(error).with_context(|| format!("installing the new {}", exe.display()));
        }
        return Ok(());
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no name to move the old version to")))
        .with_context(|| format!("moving {} out of the way", exe.display()))
}

/// Runs the newly installed executable with the arguments this one was given,
/// and reports the code it exits with.
///
/// Windows has no exec, so this process stays alive as a launcher. It reads no
/// input and draws nothing while it waits, so the new version has the console to
/// itself, and the exit code is passed straight through: whatever started this
/// one sees the answer it would have got anyway.
pub fn relaunch() -> Result<i32> {
    let exe = std::env::current_exe().context("finding the new executable")?;
    let status = Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .env(HANDED_OVER, current().to_string())
        .status()
        .with_context(|| format!("starting {}", exe.display()))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(major: u32, minor: u32, patch: u32) -> Version {
        Version { major, minor, patch }
    }

    #[test]
    fn versions_are_read_from_tags() {
        assert_eq!(Version::parse("v0.1.2"), Some(version(0, 1, 2)));
        assert_eq!(Version::parse("0.1.2"), Some(version(0, 1, 2)));
        assert_eq!(Version::parse(" 1.20.300 "), Some(version(1, 20, 300)));

        // Refused rather than guessed at: anything unreadable must not end up
        // looking newer than what is installed.
        for bad in ["", "v", "0.1", "1.2.3.4", "0.1.x", "0.1.2-rc1", "latest", "-1.0.0"] {
            assert_eq!(Version::parse(bad), None, "{bad:?} is not a version");
        }
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        // The case a string comparison gets wrong, and the whole reason this is
        // three integers.
        assert!(version(0, 1, 10) > version(0, 1, 9));
        assert!(version(0, 2, 0) > version(0, 1, 99));
        assert!(version(1, 0, 0) > version(0, 99, 99));
        assert_eq!(version(0, 1, 1), version(0, 1, 1));
    }

    #[test]
    fn the_crate_version_parses() {
        // If it ever stops parsing, `current` fails closed and no update would
        // ever be offered - silently, which is why this is checked here.
        assert_eq!(Version::parse(env!("CARGO_PKG_VERSION")), Some(current()));
    }

    fn payload(tag: &str, assets: &str, extra: &str) -> String {
        format!(r#"{{"tag_name":"{tag}",{extra}"assets":[{assets}]}}"#)
    }

    fn asset(name: &str, url: &str, size: u64) -> String {
        asset_with_id(name, url, size, 42)
    }

    fn asset_with_id(name: &str, url: &str, size: u64, id: u64) -> String {
        format!(
            r#"{{"name":"{name}","browser_download_url":"{url}","size":{size},"id":{id}}}"#
        )
    }

    const GOOD_URL: &str =
        "https://github.com/alee-ibrahim/vmerge/releases/download/v0.2.0/MERGE-VIDEOS.exe";

    #[test]
    fn a_release_is_read_from_the_api_answer() {
        let json = payload("v0.2.0", &asset_with_id("MERGE-VIDEOS.exe", GOOD_URL, 4_254_720, 7), "");
        let latest = read_release(&json).expect("a usable release");
        assert_eq!(latest.version, version(0, 2, 0));
        assert_eq!(latest.size, 4_254_720);
        assert_eq!(latest.digest_urls, Vec::<String>::new(), "none was published");

        // The API route leads, because the browser URL's host is the one
        // measured to refuse connections outright.
        assert_eq!(
            latest.urls,
            vec![
                "https://api.github.com/repos/alee-ibrahim/vmerge/releases/assets/7".to_string(),
                GOOD_URL.to_string(),
            ]
        );
    }

    #[test]
    fn a_published_digest_is_picked_up() {
        let json = payload(
            "v0.2.0",
            &format!(
                "{},{}",
                asset_with_id("MERGE-VIDEOS.exe", GOOD_URL, 10, 7),
                asset_with_id("MERGE-VIDEOS.exe.sha256", &format!("{GOOD_URL}.sha256"), 65, 8)
            ),
            "",
        );
        let digests = read_release(&json).unwrap().digest_urls;
        assert_eq!(digests.len(), 2, "the same two routes: {digests:?}");
        assert!(digests[0].ends_with("/assets/8"), "the API route leads: {digests:?}");
        assert!(digests[1].ends_with(".sha256"));
    }

    #[test]
    fn releases_that_cannot_be_trusted_are_refused() {
        let cases = [
            // Not finished, so not for anyone yet.
            (payload("v0.2.0", &asset(ASSET, GOOD_URL, 10), r#""draft":true,"#), "finished"),
            (payload("v0.2.0", &asset(ASSET, GOOD_URL, 10), r#""prerelease":true,"#), "finished"),
            // A tag this build cannot compare is not evidence of anything.
            (payload("nightly", &asset(ASSET, GOOD_URL, 10), ""), "not a version"),
            // Nothing to install.
            (payload("v0.2.0", &asset("source.zip", GOOD_URL, 10), ""), "no MERGE-VIDEOS.exe"),
            // A length of nothing cannot be checked against.
            (payload("v0.2.0", &asset(ASSET, GOOD_URL, 0), ""), "declares no length"),
            // No route left: an off-repository URL and no asset id to fall back
            // on. Refused rather than fetched from wherever it points.
            (
                payload("v0.2.0", &asset_with_id(ASSET, "https://example.net/x.exe", 10, 0), ""),
                "not a release asset",
            ),
        ];
        for (json, expected) in cases {
            let error = read_release(&json).expect_err(&format!("{json} must be refused"));
            let text = format!("{error:#}");
            assert!(text.contains(expected), "got {text:?}, wanted {expected:?}");
        }
    }

    #[test]
    fn the_download_has_to_come_from_this_repository() {
        for good in [
            GOOD_URL,
            "https://github.com/alee-ibrahim/vmerge/releases/download/v9.9.9/MERGE-VIDEOS.exe",
            "https://api.github.com/repos/alee-ibrahim/vmerge/releases/assets/511965178",
        ] {
            assert!(trusted(good), "{good} is one of ours");
        }
        for bad in [
            // The look-alike hosts, which are the ones worth being sure about.
            "https://github.com.example.net/alee-ibrahim/vmerge/releases/download/v1/x.exe",
            "https://api.github.com.example.net/repos/alee-ibrahim/vmerge/releases/assets/1",
            "https://example.net/alee-ibrahim/vmerge/releases/download/v1/x.exe",
            // Someone else's repository, and someone else's fork.
            "https://github.com/someone/vmerge/releases/download/v1/x.exe",
            "https://github.com/alee-ibrahim/other/releases/download/v1/x.exe",
            "https://api.github.com/repos/someone/vmerge/releases/assets/1",
            // Plain HTTP, and paths that are not a release asset.
            "http://github.com/alee-ibrahim/vmerge/releases/download/v1/x.exe",
            "https://github.com/alee-ibrahim/vmerge/raw/main/x.exe",
            // An asset number is all that may follow, so nothing can be hung
            // off the end of one.
            "https://api.github.com/repos/alee-ibrahim/vmerge/releases/assets/1/../../evil",
            "https://api.github.com/repos/alee-ibrahim/vmerge/releases/assets/",
        ] {
            assert!(!trusted(bad), "{bad} must be refused");
        }

        // And the rule is enforced where it matters, not just available: an
        // off-repository browser URL leaves the API route as the only one, so
        // the asset id is what has to be missing for a release to be unusable.
        let json = payload(
            "v0.2.0",
            &asset_with_id(
                ASSET,
                "https://example.net/alee-ibrahim/vmerge/releases/download/v1/x.exe",
                10,
                7,
            ),
            "",
        );
        assert_eq!(
            read_release(&json).unwrap().urls,
            vec!["https://api.github.com/repos/alee-ibrahim/vmerge/releases/assets/7".to_string()],
            "the untrusted route is dropped, not followed"
        );
    }

    #[test]
    fn digests_are_read_in_either_shape() {
        let hex = "49a73bdf0850092a252ac4641d922f3048d63ed113e196cc65ce1e4f7fb33e85";
        assert_eq!(read_digest(hex), Some(hex.to_string()));
        assert_eq!(read_digest(&format!("{hex}  MERGE-VIDEOS.exe\n")), Some(hex.to_string()));
        assert_eq!(read_digest(&hex.to_uppercase()), Some(hex.to_string()));

        // Too short, not hex, empty: no digest rather than a wrong one, and a
        // release claiming one has to produce it or the update stops.
        for bad in ["", "not-a-digest", &hex[..63], &format!("{hex}ff")] {
            assert_eq!(read_digest(bad), None, "{bad:?} is not a digest");
        }
    }

    /// The failure that went unnoticed for two versions: another copy of the
    /// program, started before the last update, is still running from
    /// `MERGE-VIDEOS.exe.previous`. That name can then be neither deleted nor
    /// written over, and every update from then on dies at the same step. There has
    /// to be a second name to move aside to.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_stuck_previous_image_does_not_block_the_update() {
        let dir = std::env::temp_dir().join(format!("vmerge-swap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("MERGE-VIDEOS.exe");
        let staged = dir.join(format!("{STAGING}test.exe"));
        fs::write(&exe, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();

        // A held handle is what makes the name unusable on Windows. Nothing here
        // can hold one portably, so the same corner is reached by making the plain
        // name impossible to take: a directory cannot be deleted as a file, and
        // cannot be renamed over either.
        fs::create_dir(previous_path(&exe)).unwrap();

        swap_in(&exe, &staged).expect("the update must not be stopped by a stuck name");

        assert_eq!(fs::read(&exe).unwrap(), b"new", "the new version is installed");
        assert!(!staged.exists(), "and the staged copy is gone, not left lying about");
        let aside = aside_paths(&exe)[1].clone();
        assert_eq!(fs::read(&aside).unwrap(), b"old", "the old one is beside it, out of the way");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_image_moved_aside_keeps_the_name_it_can_be_swept_by() {
        let previous = previous_path(Path::new("C:/tools/MERGE-VIDEOS.exe"));
        assert_eq!(previous.file_name().unwrap(), "MERGE-VIDEOS.exe.previous");

        // Which is what the sweep looks for, along with a part-finished
        // download. Everything else in the folder is left alone.
        let dir = std::env::temp_dir().join(format!("vmerge-sweep-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let files = [
            "MERGE-VIDEOS.exe.previous",
            "MERGE-VIDEOS.exe.previous-4242",
            &format!("{STAGING}1234.exe"),
            "MERGE-VIDEOS.exe",
            "clip.mp4",
        ];
        for name in files {
            fs::write(dir.join(name), b"x").unwrap();
        }

        sweep(&dir, OsStr::new("MERGE-VIDEOS.exe"));

        assert!(!dir.join("MERGE-VIDEOS.exe.previous").exists(), "the old image goes");
        assert!(
            !dir.join("MERGE-VIDEOS.exe.previous-4242").exists(),
            "and so does one moved aside under a name carrying a process id"
        );
        assert!(!dir.join(format!("{STAGING}1234.exe")).exists(), "so does a stale download");
        assert!(dir.join("MERGE-VIDEOS.exe").exists(), "the program itself stays");
        assert!(dir.join("clip.mp4").exists(), "and so does everything else");
        let _ = fs::remove_dir_all(&dir);
    }
}
