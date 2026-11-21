//! Horizon: a second engine behind the same window.
//!
//! Horizon is a Rust/wgpu reimplementation of Minecraft Java Edition. Their
//! launcher gates play behind an account; their game binaries never have. This
//! module runs *their* executables and owns none of their code — no fork, no
//! patched launcher, no bundled build.
//!
//! What lives here is the part that does not need a GPU: find their releases,
//! verify and unpack a build, run `horizon-prepare`, and know where our own
//! instances keep their worlds. The window in `bin/trenchlight.rs` is one
//! consumer; a native UI would be another.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::download::Downloader;
use crate::paths::Paths;

/// Upstream repository. Releases and the plain release feed are public; the API
/// needs no token but is rate-limited per address, and this PC exits through a
/// shared WARP address, so the API is allowed to fail.
pub const REPO: &str = "horizon-rust/horizon";
/// The game archive, the only asset a front end needs.
pub const ZIP: &str = "horizon-windows-x64.zip";
/// Checksums for every asset in the release.
pub const SUMS: &str = "SHA256SUMS";
/// The game itself, used to tell "installed" from "listed".
pub const CLIENT: &str = "horizon-client.exe";
/// Their preparer: turns a Minecraft install into the resources the game reads.
pub const PREPARE: &str = "horizon-prepare.exe";
/// Their control tool: launches, quits, lists worlds.
pub const CONTROL: &str = "horizon-ctl.exe";

const API: &str = "https://api.github.com/repos/horizon-rust/horizon/releases?per_page=30";
const FEED: &str = "https://github.com/horizon-rust/horizon/releases.atom";
const EXPANDED: &str = "https://github.com/horizon-rust/horizon/releases/expanded_assets";

/// One installable game build.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Build {
    /// Release tag, e.g. `nightly` (rolling) or `nightly-2026-11-18`.
    pub tag: String,
    /// Our folder name on disk, e.g. `nightly-2026-11-19-f00125f`.
    pub folder: String,
    /// `yyyy-mm-dd` — the release's own date, never today's.
    pub date: String,
    /// Short commit from the release title, when it says one.
    #[serde(default)]
    pub sha: Option<String>,
    /// Asset size in bytes, from the API or the release's asset list. The asset
    /// list prints one decimal ("73.8 MB"), so a value from there is a display
    /// estimate — verification is SHA-256, never this number.
    #[serde(default)]
    pub size: Option<u64>,
    pub zip_url: String,
    pub sums_url: String,
}

impl Build {
    /// `nightly-2026-11-19` style label for the screen.
    pub fn label(&self) -> String {
        match &self.sha {
            Some(sha) => format!("{} · {sha}", self.date),
            None => self.date.clone(),
        }
    }
}

/// Where the Horizon side of the launcher keeps its state.
///
/// `root` is the launcher root (`Paths::root`), so everything sits together:
///
/// ```text
/// <root>/horizon/
///   builds/<folder>/        unpacked archive, one folder per build
///   resources/<folder>/     horizon-prepare output
///   instances/<name>/       game dir: saves/, options.txt, mods/
///   instances/<name>/home/  HORIZON_HOME: logs and crash reports
///   builds.json             last good list, so the screen opens offline
/// ```
#[derive(Clone, Debug)]
pub struct HorizonPaths {
    pub root: PathBuf,
}

impl HorizonPaths {
    pub fn at(paths: &Paths) -> Self {
        HorizonPaths {
            root: paths.root.join("horizon"),
        }
    }

    pub fn builds(&self) -> PathBuf {
        self.root.join("builds")
    }

    pub fn build_dir(&self, folder: &str) -> PathBuf {
        self.builds().join(folder)
    }

    pub fn resources(&self, folder: &str) -> PathBuf {
        self.root.join("resources").join(folder)
    }

    /// The game executable inside an unpacked build, if that build is real.
    pub fn client(&self, folder: &str) -> Option<PathBuf> {
        let exe = self.build_dir(folder).join(CLIENT);
        exe.is_file().then_some(exe)
    }

    pub fn control(&self, folder: &str) -> Option<PathBuf> {
        let exe = self.build_dir(folder).join("horizon-ctl.exe");
        exe.is_file().then_some(exe)
    }

    pub fn instance(&self, name: &str) -> PathBuf {
        self.root.join("instances").join(name)
    }

    pub fn home(&self, name: &str) -> PathBuf {
        self.instance(name).join("home")
    }

    pub fn cache_file(&self) -> PathBuf {
        self.root.join("builds.json")
    }

    /// Our copy of the pinned server jar, kept across builds of the same version.
    pub fn server_jar(&self, version: &str) -> PathBuf {
        self.root.join("import").join(format!("server-{version}.jar"))
    }

    /// Folders under `builds/` that actually hold a game executable.
    ///
    /// A killed install leaves `<folder>.tmp-install` behind; it is not a build
    /// and must never be listed as one.
    pub fn installed(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.builds()) else {
            return Vec::new();
        };
        let mut out: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().join(CLIENT).is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|name| !name.ends_with(".tmp-install"))
            .collect();
        out.sort();
        out.reverse();
        out
    }
}

/// A release list plus where it came from, so the screen can be honest about a
/// fallback instead of silently showing a shorter list.
#[derive(Clone, Debug, Serialize)]
pub struct ReleaseList {
    pub builds: Vec<Build>,
    /// `api` or `feed`.
    pub source: String,
    /// Why the API was not used, when it was not.
    #[serde(default)]
    pub warning: Option<String>,
}

/// List the game builds.
///
/// The API is tried first because it carries sizes and exact publish dates. On
/// any failure — a 403 from the shared-address rate limit is the normal case —
/// the public Atom feed is parsed instead: it still names every release tag and
/// its update time, which is enough for a real list. Only when both fail does
/// this return an error, and the caller keeps the list it already had.
pub fn fetch_builds(dl: &Downloader) -> Result<ReleaseList> {
    // A diagnostic switch, not a feature: it points both sources at a socket
    // that is closed by construction, so the offline path can be exercised on a
    // machine that is online.
    let offline = std::env::var_os("MCL_HORIZON_OFFLINE").is_some();
    let (api, feed) = if offline {
        ("http://127.0.0.1:1/releases", "http://127.0.0.1:1/releases.atom")
    } else {
        (API, FEED)
    };
    fetch_from(dl, api, feed)
}

/// [`fetch_builds`] with the two sources named, so a test can point them at a
/// closed port and prove the failure is reported instead of swallowed.
pub fn fetch_from(dl: &Downloader, api: &str, feed: &str) -> Result<ReleaseList> {
    let api_error = match dl.get_text(api) {
        Ok(text) => match api_builds(&text) {
            Ok(builds) if !builds.is_empty() => {
                return Ok(ReleaseList {
                    builds,
                    source: "api".into(),
                    warning: None,
                })
            }
            Ok(_) => anyhow!("the API listed no nightly build"),
            Err(error) => error,
        },
        Err(error) => error,
    };

    let text = dl
        .get_text(feed)
        .context("the API failed and so did the release feed")?;
    let builds = feed_builds(&text, dl, feed);
    if builds.is_empty() {
        bail!("no nightly build in the release feed either ({api_error:#})");
    }
    Ok(ReleaseList {
        builds,
        source: "feed".into(),
        warning: Some(format!("GitHub API unavailable ({api_error}); using the release feed")),
    })
}

/// Read the last good list from disk. The screen opens with this before any
/// network call, so an offline launch is not an empty screen.
pub fn cached(paths: &HorizonPaths) -> Option<ReleaseList> {
    let text = std::fs::read_to_string(paths.cache_file()).ok()?;
    let builds: Vec<Build> = serde_json::from_str(&text).ok()?;
    (!builds.is_empty()).then(|| ReleaseList {
        builds,
        source: "cache".into(),
        warning: None,
    })
}

pub fn cache_builds(paths: &HorizonPaths, builds: &[Build]) -> Result<()> {
    std::fs::create_dir_all(&paths.root)?;
    let text = serde_json::to_string_pretty(builds)?;
    std::fs::write(paths.cache_file(), text).context("writing builds.json")?;
    Ok(())
}

fn api_builds(text: &str) -> Result<Vec<Build>> {
    let value: serde_json::Value = serde_json::from_str(text)
        .with_context(|| format!("GitHub API reply is not JSON: {}", first_line(text)))?;
    let releases = value
        .as_array()
        .ok_or_else(|| anyhow!("GitHub API reply is an object, not a list"))?;
    let mut out = Vec::new();
    for release in releases {
        let tag = release["tag_name"].as_str().unwrap_or_default();
        if !is_game_tag(tag) {
            continue;
        }
        let published = release["published_at"].as_str().unwrap_or_default();
        // A dated tag names its own build day; a release published after
        // midnight would otherwise be labelled with the next day. Only the
        // rolling `nightly` tag needs the publish date.
        let date = {
            let from_tag = tag_date(tag);
            if from_tag.is_empty() {
                iso_date(published).unwrap_or_default()
            } else {
                from_tag
            }
        };
        let assets = release["assets"].as_array().cloned().unwrap_or_default();
        let zip = assets
            .iter()
            .find(|a| a["name"].as_str() == Some(ZIP))
            .cloned();
        let sums = assets
            .iter()
            .find(|a| a["name"].as_str() == Some(SUMS))
            .cloned();
        let size = zip.as_ref().and_then(|z| z["size"].as_u64());
        let zip_url = zip
            .as_ref()
            .and_then(|z| z["browser_download_url"].as_str().map(str::to_string))
            .unwrap_or_else(|| asset_url(tag, ZIP));
        let sums_url = sums
            .as_ref()
            .and_then(|s| s["browser_download_url"].as_str().map(str::to_string))
            .unwrap_or_else(|| asset_url(tag, SUMS));
        let sha = short_sha(release["name"].as_str().unwrap_or_default())
            .or_else(|| short_sha(release["body"].as_str().unwrap_or_default()));
        out.push(Build {
            folder: folder_for(tag, &date, sha.as_deref()),
            tag: tag.to_string(),
            date,
            sha,
            size,
            zip_url,
            sums_url,
        });
    }
    out.sort_by(|a, b| b.date.cmp(&a.date).then(a.folder.cmp(&b.folder)));
    out.dedup_by(|a, b| a.folder == b.folder);
    Ok(out)
}

/// One `<entry>` of the releases feed, reduced to what a list needs.
struct Entry {
    tag: String,
    updated: String,
    title: String,
}

fn feed_builds(text: &str, dl: &Downloader, feed: &str) -> Vec<Build> {
    let base = feed
        .strip_suffix(".atom")
        .unwrap_or(feed)
        .trim_end_matches('/');
    let mut out = Vec::new();
    for entry in parse_feed(text) {
        if !is_game_tag(&entry.tag) {
            continue;
        }
        let date = {
            let from_tag = tag_date(&entry.tag);
            if from_tag.is_empty() {
                iso_date(&entry.updated).unwrap_or_default()
            } else {
                from_tag
            }
        };
        let sha = short_sha(&entry.title);
        let mut build = Build {
            folder: folder_for(&entry.tag, &date, sha.as_deref()),
            tag: entry.tag.clone(),
            date,
            sha,
            size: None,
            zip_url: asset_url(&entry.tag, ZIP),
            sums_url: asset_url(&entry.tag, SUMS),
        };
        // The feed has no sizes. The release's own asset list does, and it is
        // plain HTML with no rate limit. One request for the newest few builds
        // only — the rest are history nobody scrolls to.
        if out.len() < 3 {
            let url = format!("{base}/{}/{}", EXPANDED.rsplit('/').next().unwrap_or("expanded_assets"), encode_tag(&entry.tag));
            if let Ok(html) = dl.get_text(&url) {
                build.size = size_in_assets(&html, ZIP);
            }
        }
        out.push(build);
    }
    out.sort_by(|a, b| b.date.cmp(&a.date).then(a.folder.cmp(&b.folder)));
    out.dedup_by(|a, b| a.folder == b.folder);
    out
}

fn parse_feed(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<entry>") {
        let after = &rest[start + "<entry>".len()..];
        let Some(end) = after.find("</entry>") else {
            break;
        };
        let block = &after[..end];
        // The tag is in the alternate link; titles and ids are prose.
        let tag = block
            .split("releases/tag/")
            .nth(1)
            .and_then(|tail| tail.split('"').next())
            .map(decode_tag)
            .unwrap_or_default();
        let updated = tag_text(block, "updated").unwrap_or_default();
        let title = tag_text(block, "title").unwrap_or_default();
        if !tag.is_empty() {
            out.push(Entry {
                tag,
                updated,
                title,
            });
        }
        rest = &after[end + "</entry>".len()..];
    }
    out
}

fn tag_text(block: &str, name: &str) -> Option<String> {
    let open = format!("<{name}");
    let start = block.find(&open)?;
    let start = block[start..].find('>')? + start + 1;
    let end = block[start..].find("</")? + start;
    Some(block[start..end].trim().to_string())
}

/// `2026-11-19T15:33:58Z` -> `2026-11-19`. Anything else is no date at all.
fn iso_date(stamp: &str) -> Option<String> {
    let date = stamp.get(0..10)?;
    let looks_like_date = date.len() == 10
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[7] == b'-'
        && date.chars().enumerate().all(|(i, c)| match i {
            4 | 7 => c == '-',
            _ => c.is_ascii_digit(),
        });
    looks_like_date.then(|| date.to_string())
}

/// `nightly-2026-11-18` -> `2026-11-18`.
fn tag_date(tag: &str) -> String {
    let rest = tag.trim_start_matches("nightly").trim_start_matches('-');
    iso_date(rest).unwrap_or_default()
}

/// The short commit a release title carries, e.g. `Nightly 2026-11-19 · f00125f`.
fn short_sha(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|word| {
            word.len() >= 7
                && word.len() <= 12
                && word.chars().any(|c| c.is_ascii_digit())
                && word.chars().any(|c| c.is_ascii_alphabetic())
                // A date is hex-ish too; keep the first word that is not one.
                && !(word.len() == 8 && word.chars().all(|c| c.is_ascii_digit()))
        })
        .map(|word| word.to_ascii_lowercase())
}

/// A game build is any `nightly*` tag except the source mirror.
pub fn is_game_tag(tag: &str) -> bool {
    tag.starts_with("nightly") && !tag.starts_with("nightly-source")
}

/// `nightly-<date>[-<sha>]`, matching the folders Horizon's own launcher writes.
fn folder_for(tag: &str, date: &str, sha: Option<&str>) -> String {
    if tag != "nightly" {
        return tag.to_string();
    }
    let mut folder = format!("nightly-{date}");
    if let Some(sha) = sha {
        if !sha.is_empty() {
            folder.push('-');
            folder.push_str(sha);
        }
    }
    folder
}

pub fn asset_url(tag: &str, asset: &str) -> String {
    format!(
        "https://github.com/{REPO}/releases/download/{}/{asset}",
        encode_tag(tag)
    )
}

/// Tags can hold `@` and `/`; the download path wants them percent-encoded.
fn encode_tag(tag: &str) -> String {
    tag.replace('@', "%40").replace('/', "%2F")
}

fn decode_tag(tag: &str) -> String {
    tag.replace("%40", "@").replace("%2F", "/")
}

/// Pair every asset link in a release's asset list with its printed size.
///
/// The page is GitHub's own `expanded_assets` fragment: one `<li>` per asset,
/// an `<a href=".../download/<tag>/<name>">` and a size span. Sized in
/// `[\d.]+ (KB|MB|GB|Bytes)`.
fn size_in_assets(html: &str, asset: &str) -> Option<u64> {
    for block in html.split("<li").skip(1) {
        let named = block
            .split("releases/download/")
            .nth(1)
            .and_then(|tail| tail.split('"').next())
            .and_then(|path| path.rsplit('/').next())
            == Some(asset);
        if !named {
            continue;
        }
        if let Some(bytes) = block.split("text-right").nth(1).and_then(parse_size) {
            return Some(bytes);
        }
    }
    None
}

/// Read `61.3 MB` (or `630 Bytes`) out of a fragment.
///
/// The first digits in the span are not the size — `flex-shrink-0">` carries a
/// zero — so the number is taken from immediately **before** the unit, not from
/// the start of the string.
fn parse_size(fragment: &str) -> Option<u64> {
    let window: String = fragment.chars().take(200).collect();
    for (unit, factor) in [
        ("Bytes", 1.0),
        ("Byte", 1.0),
        ("KB", 1024.0),
        ("MB", 1024.0 * 1024.0),
        ("GB", 1024.0 * 1024.0 * 1024.0),
    ] {
        let Some(at) = window.find(unit) else {
            continue;
        };
        let head = window[..at].trim_end();
        let number: String = head
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if number.is_empty() {
            continue;
        }
        let value: f64 = number.parse().ok()?;
        return Some((value * factor).round() as u64);
    }
    None
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    line.chars().take(140).collect()
}

/* ── install ────────────────────────────────────────────────────────────── */

/// What an install is doing right now. The window turns `Bytes` into a bar and
/// `Stage` into the sentence above it.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum InstallEvent {
    Stage { stage: &'static str, note: String },
    Bytes { done: u64, total: Option<u64> },
}

/// Download, verify and unpack one build.
///
/// Order matters: the archive is hashed **as it streams** and the result is
/// compared with the release's `SHA256SUMS` before anything is unpacked. A
/// tampered or truncated archive never reaches the build folder — a corrupt
/// download that unpacked first would leave a half-run game that looks fine.
pub fn install(
    paths: &HorizonPaths,
    build: &Build,
    sums_url: &str,
    cancel: &std::sync::atomic::AtomicBool,
    on: &mut dyn FnMut(InstallEvent),
) -> Result<PathBuf> {
    use std::sync::atomic::Ordering;

    let final_dir = paths.build_dir(&build.folder);
    if final_dir.join(CLIENT).is_file() {
        on(InstallEvent::Stage {
            stage: "done",
            note: format!("{} is already installed", build.folder),
        });
        return Ok(final_dir);
    }

    std::fs::create_dir_all(paths.builds())?;
    let archive = paths.root.join("downloads").join(format!("{}.zip", build.folder));
    if let Some(parent) = archive.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // The checksum is the gate, so it is fetched before the 74 MB archive.
    on(InstallEvent::Stage {
        stage: "checksum",
        note: "Fetching SHA256SUMS".into(),
    });
    let expected = fetch_checksum(sums_url, ZIP)?;

    on(InstallEvent::Stage {
        stage: "download",
        note: format!("Downloading {}", build.folder),
    });
    download_archive(&build.zip_url, &archive, &expected, cancel, on)?;

    on(InstallEvent::Stage {
        stage: "unpack",
        note: format!("Unpacking {}", build.folder),
    });
    unpack_verified(&archive, &expected, &final_dir, cancel)?;
    let _ = std::fs::remove_file(&archive);
    if cancel.load(Ordering::SeqCst) {
        bail!("cancelled");
    }

    on(InstallEvent::Stage {
        stage: "done",
        note: format!("{} installed", build.folder),
    });
    Ok(final_dir)
}

/// The SHA-256 a release publishes for `asset`.
pub fn fetch_checksum(sums_url: &str, asset: &str) -> Result<String> {
    let dl = Downloader::new();
    let text = dl
        .get_text(sums_url)
        .with_context(|| format!("reading {sums_url}"))?;
    checksum_for(&text, asset)
        .ok_or_else(|| anyhow!("{sums_url} has no line for {asset}; refusing to install unverified"))
}

/// Find `asset` in a `sha256sum` file (`<64 hex>  <name>`).
pub fn checksum_for(sums: &str, asset: &str) -> Option<String> {
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        // Some tools prefix the name with `*` for binary mode.
        let name = name.trim_start_matches('*');
        if name == asset && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn download_archive(
    url: &str,
    dest: &std::path::Path,
    expected: &str,
    cancel: &std::sync::atomic::AtomicBool,
    on: &mut dyn FnMut(InstallEvent),
) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(20))
        .timeout_read(std::time::Duration::from_secs(120))
        .user_agent(&format!(
            "{}/{}",
            crate::paths::LAUNCHER_NAME,
            crate::paths::LAUNCHER_VERSION
        ))
        .build();

    let part = dest.with_extension("zip.part");
    let response = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let total = response
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok());
    let mut body = response.into_reader();

    let mut file = std::fs::File::create(&part)
        .with_context(|| format!("create {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut last_report = 0u64;
    loop {
        if cancel.load(Ordering::SeqCst) {
            drop(file);
            let _ = std::fs::remove_file(&part);
            bail!("cancelled");
        }
        let read = body
            .read(&mut buffer)
            .with_context(|| format!("reading {url}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        copied += read as u64;
        // Four updates a second is plenty for a bar and keeps the IPC quiet.
        if copied - last_report >= 1_500_000 {
            last_report = copied;
            on(InstallEvent::Bytes {
                done: copied,
                total,
            });
        }
    }
    file.flush()?;
    drop(file);

    let actual = crate::util::to_hex(&hasher.finalize());
    if actual != expected {
        let _ = std::fs::remove_file(&part);
        bail!(
            "the archive did not match its published SHA-256 (expected {expected}, got {actual}); \
             nothing was installed"
        );
    }
    on(InstallEvent::Bytes {
        done: copied,
        total: total.or(Some(copied)),
    });
    std::fs::rename(&part, dest).with_context(|| format!("rename to {}", dest.display()))?;
    Ok(())
}

/// Verify an archive on disk and unpack it into `dest`.
///
/// Split from the download so the corrupt-archive refusal is testable without a
/// network, and so a rebuild from a file is possible.
pub fn unpack_verified(
    archive: &std::path::Path,
    expected: &str,
    dest: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::sync::atomic::Ordering;

    let actual = sha256_file(archive)?;
    if actual != expected {
        bail!(
            "{} is not the published build (expected {expected}, got {actual}); \
             nothing was unpacked",
            archive.display()
        );
    }

    let temp = dest.with_extension("tmp-install");
    if temp.exists() {
        std::fs::remove_dir_all(&temp)?;
    }
    std::fs::create_dir_all(&temp)?;
    let outcome = extract_zip(archive, &temp, cancel);
    if let Err(error) = outcome {
        let _ = std::fs::remove_dir_all(&temp);
        return Err(error);
    }
    if dest.exists() {
        std::fs::remove_dir_all(dest).ok();
    }
    std::fs::rename(&temp, dest)
        .with_context(|| format!("finishing the install into {}", dest.display()))?;
    if cancel.load(Ordering::SeqCst) {
        bail!("cancelled");
    }
    Ok(())
}

fn extract_zip(
    archive: &std::path::Path,
    dest: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::io::Read;
    use std::sync::atomic::Ordering;

    let file = std::fs::File::open(archive)
        .with_context(|| format!("open {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("{} is not a zip archive", archive.display()))?;

    // `enclosed_name` is the path-traversal guard: an entry named
    // `../../Windows/System32/...` gets no name and is skipped entirely. It is
    // also the only thing read from each entry here, so the archive is walked
    // once for names and once for bytes.
    let mut names: Vec<Option<(std::path::PathBuf, bool)>> = Vec::with_capacity(zip.len());
    for index in 0..zip.len() {
        let entry = zip.by_index(index)?;
        let is_dir = entry.is_dir();
        names.push(
            entry
                .enclosed_name()
                .map(|p| (p.to_path_buf(), is_dir)),
        );
    }
    // Horizon packs the build inside one folder (`horizon-windows-x64/`). Strip
    // it when every entry shares it, so `builds/<folder>/horizon-client.exe` is
    // where the rest of this module looks. A flat archive keeps its layout.
    let wrapper = common_root(&names);

    for (index, name) in names.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        let Some((name, _)) = name else {
            continue;
        };
        let relative = match &wrapper {
            Some(wrapper) => match name.strip_prefix(wrapper) {
                Ok(inner) => inner.to_path_buf(),
                Err(_) => continue,
            },
            None => name.clone(),
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let mut entry = zip.by_index(index)?;
        let out = dest.join(&relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut buffer = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buffer)?;
        std::fs::write(&out, &buffer)
            .with_context(|| format!("write {}", out.display()))?;
    }
    Ok(())
}

/// The one folder every entry sits inside, if there is exactly one.
///
/// `None` when the archive is flat, when entries disagree on the first
/// component, or when a file sits at the top level next to the folder.
///
/// A directory entry for the wrapper itself (`horizon-windows-x64/`) is a
/// single component and must not count as "a file at the top level" — real
/// archives list their folders, and counting them would disable the strip.
fn common_root(names: &[Option<(std::path::PathBuf, bool)>]) -> Option<std::path::PathBuf> {
    let mut root: Option<std::path::PathBuf> = None;
    for (name, is_dir) in names.iter().flatten() {
        let mut parts = name.components();
        let first = parts.next()?;
        // A top-level **file** (`PLAY.txt` at the root) means nothing can be
        // stripped: the folder is part of the archive's meaning. A top-level
        // folder is the wrapper itself.
        if parts.next().is_none() && !is_dir {
            return None;
        }
        let first = std::path::PathBuf::from(first.as_os_str());
        match &root {
            Some(known) if *known != first => return None,
            Some(_) => {}
            None => root = Some(first),
        }
    }
    root
}

/// The official launcher's game folder: the copy `horizon-prepare` imports
/// from. `%APPDATA%\.minecraft` on Windows.
pub fn official_minecraft_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join(".minecraft"))
}

/* ── prepare ─────────────────────────────────────────────────────────────── */

/// What `horizon-prepare --progress json` reports. One object a line:
/// `{"message": …}` for each log line, `{"fraction": F}` as work advances, and
/// `{"error": …}` when it fails.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PrepareEvent {
    Message { text: String },
    Fraction { value: f64 },
    Error { text: String },
    /// Bytes of the pinned server jar. Kept apart from `Fraction` because it is
    /// a download with a known size, not a conversion with a guessed one.
    Bytes {
        done: u64,
        total: Option<u64>,
        text: String,
    },
}

/// The game version a build is pinned to, from its own `research-lock.json`.
///
/// The preparer refuses to guess: it wants exactly the pinned server jar, and
/// this is where the pin is written down.
#[derive(Clone, Debug, PartialEq)]
pub struct PinnedGame {
    pub version: String,
    pub server_sha1: String,
}

/// Read `<build>/research-lock.json`.
pub fn pin(build_dir: &std::path::Path) -> Result<PinnedGame> {
    let path = build_dir.join("research-lock.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not JSON", path.display()))?;
    let minecraft = value
        .get("minecraft")
        .ok_or_else(|| anyhow!("{} has no minecraft pin", path.display()))?;
    let version = minecraft
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{} has no pinned version", path.display()))?;
    let server_sha1 = minecraft
        .get("server_sha1")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{} has no pinned server sha1", path.display()))?;
    Ok(PinnedGame {
        version: version.to_string(),
        server_sha1: server_sha1.to_ascii_lowercase(),
    })
}

/// Mojang's content-addressed store: the URL is the SHA-1 itself, so the file
/// that arrives can be checked against the name it came from.
pub fn server_url(sha1: &str) -> String {
    format!("https://piston-data.mojang.com/v1/objects/{sha1}/server.jar")
}

/// Make sure the pinned server jar is on disk, fetching it from Mojang if not.
///
/// Horizon will not download the game; it asks for this one file. Fetching it
/// from Mojang's own content store is the same file the preparer's error message
/// tells the user to download by hand, and it is verified against the SHA-1 the
/// build pins before anything reads it.
pub fn ensure_server_jar(
    paths: &HorizonPaths,
    folder: &str,
    cancel: &std::sync::atomic::AtomicBool,
    on: &mut dyn FnMut(PrepareEvent),
) -> Result<PathBuf> {
    use sha1::{Digest, Sha1};
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;

    let pin = pin(&paths.build_dir(folder))?;
    let target = paths.server_jar(&pin.version);
    if target.is_file() {
        if crate::util::sha1_file(&target)? == pin.server_sha1 {
            on(PrepareEvent::Message {
                text: format!("Using the pinned {} server jar already imported", pin.version),
            });
            return Ok(target);
        }
        // A file that does not match the pin is worse than no file.
        let _ = std::fs::remove_file(&target);
    }

    let url = server_url(&pin.server_sha1);
    on(PrepareEvent::Message {
        text: format!("Fetching the pinned {} server jar from Mojang", pin.version),
    });
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let part = target.with_extension("jar.part");

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(20))
        .timeout_read(std::time::Duration::from_secs(120))
        .user_agent(&format!(
            "{}/{}",
            crate::paths::LAUNCHER_NAME,
            crate::paths::LAUNCHER_VERSION
        ))
        .build();
    let response = agent.get(&url).call().with_context(|| format!("GET {url}"))?;
    let total = response
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok());
    let mut body = response.into_reader();
    let mut file = std::fs::File::create(&part)
        .with_context(|| format!("create {}", part.display()))?;
    let mut hasher = Sha1::new();
    let mut copied = 0u64;
    let mut last = 0u64;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        if cancel.load(Ordering::SeqCst) {
            drop(file);
            let _ = std::fs::remove_file(&part);
            bail!("cancelled");
        }
        let read = body
            .read(&mut buffer)
            .with_context(|| format!("reading {url}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        copied += read as u64;
        if copied - last >= 2_000_000 {
            last = copied;
            on(PrepareEvent::Bytes {
                done: copied,
                total,
                text: format!("{} server jar", pin.version),
            });
        }
    }
    file.flush()?;
    drop(file);

    let actual = crate::util::to_hex(&hasher.finalize());
    if actual != pin.server_sha1 {
        let _ = std::fs::remove_file(&part);
        bail!(
            "the {} server jar did not match the pinned SHA-1 (expected {}, got {actual}); \
             nothing was kept",
            pin.version,
            pin.server_sha1
        );
    }
    on(PrepareEvent::Bytes {
        done: copied,
        total: total.or(Some(copied)),
        text: format!("{} server jar", pin.version),
    });
    std::fs::rename(&part, &target)
        .with_context(|| format!("rename to {}", target.display()))?;
    Ok(target)
}

/// Read one line of the preparer's JSON progress.
///
/// Anything that is not one of the three known objects is passed through as a
/// message: a preparer that prints a plain line should still reach the screen
/// instead of vanishing.
pub fn parse_progress(line: &str) -> Option<PrepareEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
        if let Some(text) = value.get("error").and_then(|v| v.as_str()) {
            return Some(PrepareEvent::Error {
                text: text.to_string(),
            });
        }
        if let Some(value) = value.get("fraction").and_then(|v| v.as_f64()) {
            return Some(PrepareEvent::Fraction { value });
        }
        if let Some(text) = value.get("message").and_then(|v| v.as_str()) {
            return Some(PrepareEvent::Message {
                text: text.to_string(),
            });
        }
        // A JSON line with none of the three keys is still worth showing.
    }
    Some(PrepareEvent::Message {
        text: line.to_string(),
    })
}

/// `horizon-prepare --check`: is this build's resource set complete?
///
/// `Ok(None)` means ready. `Ok(Some(gap))` is the preparer's own sentence about
/// what is missing. An `Err` means the question could not be asked at all.
pub fn prepare_check(paths: &HorizonPaths, folder: &str) -> Result<Option<String>> {
    let root = paths.build_dir(folder);
    let prepare = root.join(PREPARE);
    if !prepare.is_file() {
        bail!("{folder} has no {PREPARE}; install the build first");
    }
    let output = paths.resources(folder);
    let mut command = std::process::Command::new(&prepare);
    command
        .arg("--check")
        .arg("--root")
        .arg(&root)
        .arg("--output")
        .arg(&output);
    no_window(&mut command);
    let done = command
        .output()
        .with_context(|| format!("running {}", prepare.display()))?;
    let stdout = String::from_utf8_lossy(&done.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&done.stderr).trim().to_string();
    if done.status.success() {
        return Ok(None);
    }
    if stdout.is_empty() && stderr.is_empty() {
        bail!("{PREPARE} --check exited with {}", done.status);
    }
    Ok(Some(if stdout.is_empty() { stderr } else { stdout }))
}

/// Run `horizon-prepare --prepare` and stream what it says.
///
/// This is the slow step: it reads the pinned Minecraft version, converts the
/// assets and writes roughly a gigabyte of resources. The first
/// run took about three minutes, which is why the fraction matters — a frozen
/// bar and a working bar look identical otherwise.
pub fn prepare(
    paths: &HorizonPaths,
    folder: &str,
    minecraft_dir: &std::path::Path,
    server_jar: Option<&std::path::Path>,
    cancel: &std::sync::atomic::AtomicBool,
    on: &mut dyn FnMut(PrepareEvent),
) -> Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::atomic::Ordering;

    let root = paths.build_dir(folder);
    let prepare = root.join(PREPARE);
    if !prepare.is_file() {
        bail!("{folder} has no {PREPARE}; install the build first");
    }
    if !minecraft_dir.is_dir() {
        bail!(
            "{} is not a Minecraft folder; point Settings at your official .minecraft",
            minecraft_dir.display()
        );
    }
    let output = paths.resources(folder);
    // A prepare that was killed mid-way leaves an output the preparer will
    // refuse ("the sounds missing: horizon-prepare sounds"). It cannot resume,
    // so the partial folder is thrown away rather than blamed on the user.
    if output.exists() {
        let partial = match prepare_check(paths, folder) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(_) => true,
        };
        if partial {
            on(PrepareEvent::Message {
                text: "The last prepare did not finish; starting over".into(),
            });
            std::fs::remove_dir_all(&output)
                .with_context(|| format!("clear {}", output.display()))?;
        }
    }
    std::fs::create_dir_all(&output)?;

    let mut command = std::process::Command::new(&prepare);
    command
        .arg("--prepare")
        // `--prepare` alone leaves the sounds out, and `--check` then reports
        // "the sounds missing": a build we cannot honestly call Ready. One
        // command that also copies them is worth the extra 380 MB.
        .arg("--complete")
        .arg("--minecraft-dir")
        .arg(minecraft_dir)
        .arg("--root")
        .arg(&root)
        .arg("--output")
        .arg(&output);
    // The preparer will not download the game; it needs this one file, and the
    // pin it checks it against is the build's own.
    if let Some(jar) = server_jar {
        command.arg("--server-jar").arg(jar);
    }
    command
        .args(["--progress", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(&mut command);

    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", prepare.display()))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    // stderr is drained on its own thread: a piped stream nobody reads fills up
    // and stops the child, which would look exactly like a hang.
    let stderr = child.stderr.take().expect("stderr is piped");
    let drain = std::thread::spawn(move || {
        let mut text = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            text.push_str(&line);
            text.push('\n');
        }
        text
    });

    let mut failure: Option<String> = None;
    for line in BufReader::new(stdout).lines() {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("cancelled");
        }
        let Ok(line) = line else { break };
        match parse_progress(&line) {
            Some(PrepareEvent::Error { text }) => {
                failure = Some(text.clone());
                on(PrepareEvent::Error { text });
            }
            Some(event) => on(event),
            None => {}
        }
    }
    let status = child.wait().with_context(|| "waiting for horizon-prepare")?;
    let noise = drain.join().unwrap_or_default();

    if let Some(error) = failure {
        bail!("horizon-prepare failed: {error}");
    }
    if !status.success() {
        let tail: String = noise
            .lines()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" / ");
        if tail.is_empty() {
            bail!("horizon-prepare exited with {status}");
        }
        bail!("horizon-prepare exited with {status}: {tail}");
    }
    Ok(())
}

/// One Horizon dive, resolved and ready to start.
///
/// Resolving first is what makes the negative control honest: a build whose
/// resources are missing is refused before anything is spawned, with the
/// preparer's own reason on screen.
pub struct LaunchPlan {
    pub ctl: PathBuf,
    pub game: PathBuf,
    pub resources: PathBuf,
    pub game_dir: PathBuf,
    pub home: PathBuf,
    pub world: String,
    /// The offline name the game should use. Empty means "let the game pick",
    /// which is what it did before the launcher had a name to give it.
    pub name: String,
}

/// Where a build's saves live when the user never named an instance.
///
/// One game folder per build would hide yesterday's world behind today's
/// nightly, so the folder is the *engine*, not the build: `horizon`. The world
/// name inside it is what changes.
pub const SHARED_INSTANCE: &str = "horizon";

pub fn plan(
    paths: &HorizonPaths,
    folder: &str,
    instance: &str,
    world: &str,
    name: &str,
) -> Result<LaunchPlan> {
    let ctl = paths
        .control(folder)
        .ok_or_else(|| anyhow!("{folder} has no horizon-ctl.exe; install it again"))?;
    let game = paths
        .client(folder)
        .ok_or_else(|| anyhow!("{folder} has no horizon-client.exe; install it again"))?;
    // Refuse an unprepared build here, not after the window opens on nothing.
    if let Some(reason) = prepare_check(paths, folder)? {
        bail!("{folder} is not prepared: {reason}");
    }
    let resources = paths.resources(folder);
    let game_dir = paths.instance(instance);
    let home = paths.home(instance);
    std::fs::create_dir_all(&game_dir)
        .with_context(|| format!("create {}", game_dir.display()))?;
    std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
    Ok(LaunchPlan {
        ctl,
        game,
        resources,
        game_dir,
        home,
        world: world.to_string(),
        name: name.trim().to_string(),
    })
}

impl LaunchPlan {
    /// `horizon-ctl launch --wait …`, with the game's own folders spelled out.
    ///
    /// `--wait` means the player spawned and the chunks are loaded, so a reply
    /// from this command is the honest "the game is running" signal.
    pub fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.ctl);
        command
            .arg("launch")
            .arg("--wait")
            .arg("--world")
            .arg(&self.world)
            .arg("--game")
            .arg(&self.game)
            // Everything below is a game argument: ctl forwards what it does
            // not own, so these reach horizon-client as they are.
            .arg("--resources")
            .arg(&self.resources)
            .arg("--game-dir")
            .arg(&self.game_dir)
            .arg("--home")
            .arg(&self.home);
        // The name the player typed in Settings. Without one the game keeps
        // generating its own and storing it in horizon-player.txt.
        if !self.name.is_empty() {
            command.arg("--name").arg(&self.name);
        }
        command
            .env("HORIZON_HOME", &self.home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        no_window(&mut command);
        command
    }
}

/// Start the game through `horizon-ctl` and return the game's pid.
///
/// ctl is only a starter: once it has answered, it exits and the game keeps
/// running on its own. Its output is returned as the tail, so a refusal by the
/// game reaches the failure card with the game's own words.
pub fn launch(plan: &LaunchPlan, cancel: &std::sync::atomic::AtomicBool) -> Result<(u32, String)> {
    use std::io::Read;
    use std::sync::atomic::Ordering;

    let mut child = plan
        .command()
        .spawn()
        .with_context(|| format!("starting {}", plan.ctl.display()))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    // Both streams are drained on their own threads: a piped stream nobody
    // reads fills up and stops the child, which would look exactly like a hang.
    let out_thread = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err_thread = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::BufReader::new(stderr).read_to_string(&mut text);
        text
    });

    // `--wait` answers when the player has spawned, which took about twenty
    // seconds here. A game that never answers is not "starting", it is stuck:
    // ten minutes is long enough for this disk and short enough to say so.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    let status = loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("cancelled");
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Horizon did not answer in ten minutes; the game is probably stuck");
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(150)),
            Err(error) => return Err(error).context("waiting for horizon-ctl"),
        }
    };
    let text = out_thread.join().unwrap_or_default();
    let noise = err_thread.join().unwrap_or_default();
    let tail = [text.trim(), noise.trim()]
        .iter()
        .filter(|part| !part.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");

    if !status.success() {
        let last: String = tail
            .lines()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" / ");
        if last.is_empty() {
            bail!("horizon-ctl launch exited with {status}");
        }
        bail!("horizon-ctl launch exited with {status}: {last}");
    }

    let pid = launched_pid(&text)
        .or_else(|| game_pid(&plan.game))
        .ok_or_else(|| {
        anyhow!(
            "horizon-ctl says the game is up, but no {} process is running",
            plan.game
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "horizon-client".into())
        )
    })?;
    Ok((pid, String::new()))
}

/// The game's pid out of ctl's final state line.
///
/// `horizon-ctl launch --wait` answers with one line of state JSON that carries
/// `"launch":{"pid":N,…}` — the authoritative pid, so the process list is only
/// a fallback. The JSON is deliberately not returned to the page: it is tens of
/// kilobytes and means nothing on a failure card.
pub fn launched_pid(state_line: &str) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_str(state_line.trim()).ok()?;
    value
        .get("launch")?
        .get("pid")?
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
}

/// One world, as `horizon-ctl worlds` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldRow {
    /// The folder under `saves/`; every ctl command takes this, not the name.
    pub folder: String,
    pub name: String,
    pub mode: String,
    pub version: String,
    /// Milliseconds since the epoch, the game's own stamp.
    pub last_played: u64,
    pub corrupted: bool,
}

impl WorldRow {
    /// The stamp in the launcher's format, or `never`.
    pub fn played(&self) -> String {
        if self.last_played == 0 {
            return "never".into();
        }
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.last_played);
        crate::util::iso_from_system_time(when)
    }
}

/// Run `horizon-ctl` and return its stdout, refusing to wait for ever.
///
/// Every command here talks to the running game over its control socket, so a
/// half-dead game can leave ctl waiting on a socket that will never answer. The
/// caller gets an error at the deadline instead of a worker stuck all session.
pub fn ctl_reply(
    ctl: &std::path::Path,
    args: &[&str],
    timeout: std::time::Duration,
) -> Result<String> {
    let mut command = std::process::Command::new(ctl);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    no_window(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("running {}", ctl.display()))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} {} did not answer in {} s",
                ctl.display(),
                args.first().copied().unwrap_or(""),
                timeout.as_secs()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let done = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&done.stdout).trim().to_string();
    if stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&done.stderr).trim().to_string();
        if !stderr.is_empty() {
            bail!("{stderr}");
        }
    }
    Ok(stdout)
}

/// The worlds of the running game, newest first.
///
/// This build of `horizon-ctl` reads the list from the game over its socket, so
/// it needs a game that is running (the title screen is enough). Without one the
/// reply is a connection error, which is passed on as-is: the page turns it into
/// "start a dive first" rather than an empty list.
pub fn worlds(ctl: &std::path::Path) -> Result<Vec<WorldRow>> {
    let reply = ctl_reply(ctl, &["worlds"], std::time::Duration::from_secs(60))?;
    parse_worlds(&reply)
}

pub fn parse_worlds(reply: &str) -> Result<Vec<WorldRow>> {
    let text = reply.trim();
    let value: serde_json::Value = serde_json::from_str(text).with_context(|| {
        let short: String = text.chars().take(200).collect();
        format!("horizon-ctl worlds said {short}")
    })?;
    if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
        bail!("{error}");
    }
    let rows = value
        .get("worlds")
        .and_then(|w| w.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(WorldRow {
                folder: row.get("folder")?.as_str()?.to_string(),
                name: row
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                mode: row
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                version: row
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                last_played: row
                    .get("last_played")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                corrupted: row
                    .get("corrupted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect())
}

/// A zip in `instances/<name>/backups/`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BackupRow {
    pub file: String,
    pub bytes: u64,
    /// The world folder the zip was taken from, read back from its entries.
    pub folder: String,
}

pub fn backups_dir(instance: &std::path::Path) -> PathBuf {
    instance.join("backups")
}

/// Every backup on disk, newest first. Names begin with the timestamp.
pub fn list_backups(instance: &std::path::Path) -> Vec<BackupRow> {
    let Ok(entries) = std::fs::read_dir(backups_dir(instance)) else {
        return Vec::new();
    };
    let mut rows: Vec<BackupRow> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let is_zip = path
                .extension()
                .map(|e| e.to_string_lossy().eq_ignore_ascii_case("zip"))
                .unwrap_or(false);
            if !is_zip {
                return None;
            }
            let file = path.file_name()?.to_string_lossy().to_string();
            let bytes = entry.metadata().ok()?.len();
            let folder = zip_top_folder(&path).unwrap_or_else(|| file.clone());
            Some(BackupRow {
                file,
                bytes,
                folder,
            })
        })
        .collect();
    rows.sort_by(|a, b| b.file.cmp(&a.file));
    rows
}

fn zip_top_folder(path: &std::path::Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let first = archive.by_index(0).ok()?;
    let name = first.name().split('/').next()?.to_string();
    (!name.is_empty()).then_some(name)
}

/// A world folder this build's ctl can pass over the socket.
///
/// `horizon-ctl` sends the command as one line and the game splits it on
/// spaces, so `world-backup "New World"` arrives as folder `New` and fails with
/// "backup of New failed". Folders with a space are zipped by the launcher
/// instead — same zip, same size, and restore was always ours.
pub fn ctl_can_take(folder: &str) -> bool {
    !folder.is_empty() && !folder.chars().any(char::is_whitespace)
}

pub struct BackupMade {
    pub file: String,
    pub bytes: u64,
    /// True when Horizon's own ctl wrote the zip, false when we did.
    pub by_ctl: bool,
}

/// Zip one world into `instances/<name>/backups/`.
pub fn backup(
    ctl: Option<&std::path::Path>,
    instance: &std::path::Path,
    folder: &str,
) -> Result<BackupMade> {
    let saves = instance.join("saves").join(folder);
    if !saves.is_dir() {
        bail!("there is no world called {folder}");
    }
    std::fs::create_dir_all(backups_dir(instance))?;
    if let Some(ctl) = ctl {
        if ctl_can_take(folder) {
            let reply =
                ctl_reply(ctl, &["world-backup", folder], std::time::Duration::from_secs(300))?;
            let value: serde_json::Value = serde_json::from_str(reply.trim())
                .with_context(|| format!("horizon-ctl world-backup said {reply}"))?;
            if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
                bail!("{error}");
            }
            // ctl names the zip itself; take the newest one for this folder.
            let suffix = format!("_{folder}.zip");
            let made = list_backups(instance)
                .into_iter()
                .find(|row| row.file.ends_with(&suffix));
            let bytes = value.get("bytes").and_then(|b| b.as_u64()).unwrap_or(0);
            if let Some(row) = made {
                return Ok(BackupMade {
                    file: row.file,
                    bytes: if bytes > 0 { bytes } else { row.bytes },
                    by_ctl: true,
                });
            }
        }
    }
    let stamp = crate::util::now_iso().replace(' ', "_").replace(':', "-");
    let file = format!("{stamp}_{folder}.zip");
    let path = backups_dir(instance).join(&file);
    zip_world(&saves, folder, &path)?;
    Ok(BackupMade {
        file,
        bytes: std::fs::metadata(&path)?.len(),
        by_ctl: false,
    })
}

/// A world folder as a zip whose entries all sit under `<folder>/`, which is
/// the layout `horizon-ctl world-backup` writes and `restore` expects.
fn zip_world(saves: &std::path::Path, folder: &str, out: &std::path::Path) -> Result<()> {
    let file = std::fs::File::create(out).with_context(|| format!("create {}", out.display()))?;
    let mut writer = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut stack = vec![saves.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path
                .strip_prefix(saves)?
                .to_string_lossy()
                .replace('\\', "/");
            let name = format!("{folder}/{rel}");
            if path.is_dir() {
                writer.add_directory(format!("{name}/"), options)?;
                stack.push(path);
            } else {
                writer.start_file(name, options)?;
                let mut from = std::fs::File::open(&path)?;
                std::io::copy(&mut from, &mut writer)?;
            }
        }
    }
    writer.finish()?;
    Ok(())
}

/// Put a backup back under `saves/`, refusing to overwrite a world.
pub fn restore(instance: &std::path::Path, file: &str) -> Result<String> {
    let path = backups_dir(instance).join(file);
    let handle = std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(handle)?;
    let mut names: Vec<String> = Vec::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        names.push(entry.name().to_string());
    }
    // Every entry sits under one folder, which is the world folder's name.
    let folder = names
        .first()
        .and_then(|n| n.split('/').next())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("{file} has no world folder inside it"))?
        .to_string();
    let target = instance.join("saves").join(&folder);
    if target.exists() {
        bail!("{folder} is already there; delete it first, then restore");
    }
    std::fs::create_dir_all(&target)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        let Some(rel) = name.strip_prefix(&format!("{folder}/")) else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        let out = target.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut sink =
            std::fs::File::create(&out).with_context(|| format!("write {}", out.display()))?;
        std::io::copy(&mut entry, &mut sink)?;
    }
    Ok(folder)
}

/// Delete a world. Through ctl when the folder has no space and the game is
/// running; by hand otherwise.
pub fn delete_world(
    ctl: Option<&std::path::Path>,
    instance: &std::path::Path,
    folder: &str,
) -> Result<()> {
    let dir = instance.join("saves").join(folder);
    if let Some(ctl) = ctl {
        if ctl_can_take(folder) {
            // ctl refuses without --confirm ("ask the player, then add --confirm").
            // The player was asked: the button turns into "Delete for good" and
            // needs a second press, so this call is the confirmation.
            let reply = ctl_reply(
                ctl,
                &["world-delete", folder, "--confirm"],
                std::time::Duration::from_secs(120),
            );
            match reply {
                Ok(reply) => {
                    let value: serde_json::Value = serde_json::from_str(reply.trim())
                        .with_context(|| format!("horizon-ctl world-delete said {reply}"))?;
                    if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
                        bail!("{error}");
                    }
                    return Ok(());
                }
                // No game to ask: ctl speaks to a running one over a socket. The
                // page has to work with no dive open, so the folder goes directly.
                // A game that is running holds session.lock and the delete then
                // fails loudly instead of pulling a live world out from under it.
                Err(error) => {
                    if !dir.is_dir() {
                        return Err(error);
                    }
                }
            }
        }
    }
    if !dir.is_dir() {
        bail!("there is no world called {folder}");
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("delete {}", dir.display()))?;
    Ok(())
}

/// Ask the running game for the next rendered frame.
pub fn screenshot(ctl: &std::path::Path, out: &std::path::Path) -> Result<(u64, u64)> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let reply = ctl_reply(
        ctl,
        &["screenshot", &out.to_string_lossy()],
        std::time::Duration::from_secs(30),
    )?;
    let value: serde_json::Value = serde_json::from_str(reply.trim())
        .with_context(|| format!("horizon-ctl screenshot said {reply}"))?;
    if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
        bail!("{error}");
    }
    let width = value.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
    let height = value.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
    Ok((width, height))
}

/// The newest crash report written after `since`, if the game left one.
///
/// A game we do not own has no exit code to read, so the crash report is the
/// honest signal: Horizon writes one into its home when it dies on a panic, and
/// a clean quit leaves none.
pub fn crash_report(home: &std::path::Path, since: u64) -> Option<PathBuf> {
    let dir = home.join("crash-reports");
    let mut newest: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let stamp = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // A report from an earlier dive is not this dive's crash.
        if stamp + 5 < since {
            continue;
        }
        if newest.as_ref().map_or(true, |(old, _)| stamp >= *old) {
            newest = Some((stamp, path));
        }
    }
    newest.map(|(_, path)| path)
}

/// The pid of the running `horizon-client.exe`, by exact executable path.
///
/// A second game started by hand would otherwise be mistaken for ours, so the
/// image name is compared as well as the name: the path is the only thing that
/// tells one build's client from another's.
pub fn game_pid(game: &std::path::Path) -> Option<u32> {
    let want = game
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| CLIENT.to_lowercase());
    let mut command = std::process::Command::new("tasklist");
    command.args(["/FI", &format!("IMAGENAME eq {want}"), "/FO", "CSV", "/NH"]);
    no_window(&mut command);
    let done = command.output().ok()?;
    let text = String::from_utf8_lossy(&done.stdout);
    let mut best: Option<u32> = None;
    for line in text.lines() {
        // "horizon-client.exe","12345","Console","1","211.000 K"
        let mut fields = line.split("\",\"");
        let image = fields.next().unwrap_or("").trim_matches('"').to_lowercase();
        if image != want {
            continue;
        }
        if let Some(pid) = fields.next().and_then(|p| p.trim().parse::<u32>().ok()) {
            // Newest wins: the game we just started has the higher pid.
            best = Some(best.map_or(pid, |old| old.max(pid)));
        }
    }
    best
}

/// What the launcher can honestly say about a game it did not parent.
pub struct ExitVerdict {
    pub crash: bool,
    /// The last lines of the game's own log, for the failure card.
    pub tail: String,
}

/// Decide whether a Horizon dive that ended on its own actually crashed.
///
/// There is no exit code to read — ctl is not the parent — so two signals are
/// used, in order of certainty: a crash report written during this dive, and the
/// game's own log. A clean exit saves the world and says so; a process that
/// disappeared without that line did not shut itself down.
pub fn exit_verdict(home: &std::path::Path, since: u64) -> ExitVerdict {
    if let Some(report) = crash_report(home, since) {
        let name = report
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        return ExitVerdict {
            crash: true,
            tail: format!("Horizon wrote a crash report: {name}"),
        };
    }
    let log = home.join("logs").join("latest.log");
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail: String = lines
        .iter()
        .rev()
        .take(12)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    // The last thing a clean Horizon exit does is save. An autosave a minute
    // before the kill is not an exit, so only the last few lines count.
    let saved = lines
        .iter()
        .rev()
        .take(3)
        .any(|line| line.contains("Saving") || line.contains("Stopping") || line.contains("shut"));
    ExitVerdict {
        crash: !saved,
        tail,
    }
}

/// Keep a console window from flashing when a tool is started from the GUI.
fn no_window(command: &mut std::process::Command) {    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

pub fn sha256_file(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let file = std::fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(crate::util::to_hex(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed real reply: one nightly, one launcher, one sdk.
    const API_REPLY: &str = r#"[
      {"tag_name":"launcher@0.2.85","published_at":"2026-11-20T00:10:00Z","name":"Horizon launcher 0.2.85",
       "assets":[{"name":"horizon-windows-x64.zip","size":999,"browser_download_url":"nope"}]},
      {"tag_name":"nightly","published_at":"2026-11-19T15:33:58Z","name":"Nightly 2026-11-19 · f00125f",
       "assets":[
         {"name":"horizon-windows-x64.zip","size":77389414,"browser_download_url":"https://example/zip"},
         {"name":"SHA256SUMS","size":630,"browser_download_url":"https://example/sums"}
       ]}
    ]"#;

    const FEED: &str = r#"<entry>
      <id>tag:github.com,2008:Repository/1/sdk-f001</id>
      <updated>2026-11-19T15:32:06Z</updated>
      <link rel="alternate" type="text/html" href="https://github.com/horizon-rust/horizon/releases/tag/sdk-f001"/>
      <title>Mod SDK f001</title>
    </entry>
    <entry>
      <id>tag:github.com,2008:Repository/1/nightly</id>
      <updated>2026-11-19T15:33:58Z</updated>
      <link rel="alternate" type="text/html" href="https://github.com/horizon-rust/horizon/releases/tag/nightly"/>
      <title>Nightly 2026-11-19 · f00125f</title>
    </entry>
    <entry>
      <id>tag:github.com,2008:Repository/1/launcher@0.2.85</id>
      <updated>2026-11-20T00:10:00Z</updated>
      <link rel="alternate" type="text/html" href="https://github.com/horizon-rust/horizon/releases/tag/launcher%400.2.85"/>
      <title>Horizon launcher 0.2.85</title>
    </entry>"#;

    const ASSETS: &str = r#"<ul>
      <li class="x"><a href="/horizon-rust/horizon/releases/download/nightly/horizon-linux-x86_64.tar.gz">
        <span>Linux</span></a>
        <span class="Truncate-text">sha256:aa</span>
        <span class="color-fg-muted text-right flex-shrink-0">71.8 MB</span></li>
      <li class="x"><a href="/horizon-rust/horizon/releases/download/nightly/horizon-windows-x64.zip">
        <span>Windows</span></a>
        <span class="Truncate-text">sha256:bb</span>
        <span class="color-fg-muted text-right flex-shrink-0">73.8 MB</span></li>
      <li class="x"><a href="/horizon-rust/horizon/releases/download/nightly/SHA256SUMS">sums</a>
        <span class="color-fg-muted text-right flex-shrink-0">630 Bytes</span></li>
    </ul>"#;

    #[test]
    fn the_api_list_keeps_nightly_builds_and_their_sizes() {
        let builds = api_builds(API_REPLY).unwrap();
        assert_eq!(builds.len(), 1, "launcher releases must be ignored");
        let build = &builds[0];
        assert_eq!(build.tag, "nightly");
        assert_eq!(build.date, "2026-11-19");
        assert_eq!(build.sha.as_deref(), Some("f00125f"));
        assert_eq!(build.folder, "nightly-2026-11-19-f00125f");
        assert_eq!(build.size, Some(77_389_414));
        assert_eq!(build.zip_url, "https://example/zip");
        assert_eq!(build.sums_url, "https://example/sums");
    }

    #[test]
    fn the_feed_yields_the_same_build_without_the_api() {
        let entries = parse_feed(FEED);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].tag, "nightly");
        let date = iso_date(&entries[1].updated).unwrap();
        assert_eq!(date, "2026-11-19");
        let sha = short_sha(&entries[1].title).unwrap();
        assert_eq!(folder_for(&entries[1].tag, &date, Some(&sha)), "nightly-2026-11-19-f00125f");
        // A launcher tag is not a game build.
        assert!(!is_game_tag(&entries[2].tag));
        // The `@` in a launcher tag is percent-encoded in the feed and must
        // decode before it is used in a URL, or the link 404s.
        assert_eq!(decode_tag(&entries[2].tag), "launcher@0.2.85");
    }

    #[test]
    fn dates_come_from_the_release_not_from_today() {
        assert_eq!(iso_date("2026-11-19T15:33:58Z").as_deref(), Some("2026-11-19"));
        assert_eq!(iso_date("yesterday"), None);
        assert_eq!(tag_date("nightly-2026-11-18"), "2026-11-18");
        assert_eq!(tag_date("nightly"), "");
    }

    /// `nightly-2026-11-18` was published at 01:37 the next day. The row must
    /// say 18, because that is the build the tag and the release title name.
    #[test]
    fn a_dated_tag_keeps_its_own_day_across_midnight() {
        let reply = r#"[{"tag_name":"nightly-2026-11-18","published_at":"2026-11-19T01:37:24Z",
            "name":"Nightly 2026-11-18 · dd67b76",
            "assets":[{"name":"horizon-windows-x64.zip","size":76412911,"browser_download_url":"https://example/z"}]}]"#;
        let builds = api_builds(reply).unwrap();
        assert_eq!(builds[0].date, "2026-11-18");
        assert_eq!(builds[0].folder, "nightly-2026-11-18");
        assert_eq!(builds[0].sha.as_deref(), Some("dd67b76"));
    }

    #[test]
    fn the_rolling_tag_gets_a_dated_folder_but_a_dated_tag_keeps_its_name() {
        assert_eq!(folder_for("nightly", "2026-11-19", Some("f00125f")), "nightly-2026-11-19-f00125f");
        assert_eq!(folder_for("nightly", "2026-11-19", None), "nightly-2026-11-19");
        assert_eq!(folder_for("nightly-2026-11-18", "2026-11-18", None), "nightly-2026-11-18");
    }

    #[test]
    fn asset_sizes_are_read_from_the_release_page() {
        assert_eq!(size_in_assets(ASSETS, "horizon-windows-x64.zip"), Some(77_384_909));
        assert_eq!(size_in_assets(ASSETS, "SHA256SUMS"), Some(630));
        assert_eq!(size_in_assets(ASSETS, "nope.zip"), None);
        assert_eq!(size_in_assets("", ZIP), None);
    }

    #[test]
    fn a_commit_is_not_mistaken_for_a_date() {
        assert_eq!(short_sha("Nightly 2026-11-19 · f00125f").as_deref(), Some("f00125f"));
        assert_eq!(short_sha("Nightly 2026-11-19"), None);
        assert_eq!(short_sha("nothing here"), None);
    }

    /// The offline gate: both sources down must be an error that names what
    /// failed, never an empty list pretending to be a list.
    #[test]
    fn with_no_network_the_fetch_fails_loudly() {
        let dl = Downloader::new();
        let error = fetch_from(
            &dl,
            "http://127.0.0.1:1/releases",
            "http://127.0.0.1:1/releases.atom",
        )
        .expect_err("a closed port must not look like an empty release list");
        let text = format!("{error:#}");
        assert!(
            text.contains("release feed"),
            "the error must say the feed failed too, got: {text}"
        );
    }

    #[test]
    fn a_checksum_line_is_read_for_the_right_asset_only() {
        let sums = "d28ed26468fadad288b99a138fc4f94620a1db49ef23315c96839190bebf4b1c  horizon-windows-x64.zip\n\
                    aa  horizon-linux-x86_64.tar.gz\n\
                    bbbb  not-a-hash.txt\n";
        assert_eq!(
            checksum_for(sums, "horizon-windows-x64.zip").as_deref(),
            Some("d28ed26468fadad288b99a138fc4f94620a1db49ef23315c96839190bebf4b1c")
        );
        // A short or non-hex hash is not a checksum.
        assert_eq!(checksum_for(sums, "horizon-linux-x86_64.tar.gz"), None);
        assert_eq!(checksum_for(sums, "not-a-hash.txt"), None);
        assert_eq!(checksum_for(sums, "missing.zip"), None);
        // `sha256sum -b` marks binary mode with a star.
        assert_eq!(
            checksum_for(
                "d28ed26468fadad288b99a138fc4f94620a1db49ef23315c96839190bebf4b1c *horizon-windows-x64.zip",
                "horizon-windows-x64.zip"
            )
            .is_some(),
            true
        );
    }

    /// Write a real zip so the refusal path is exercised on a real archive
    /// rather than a byte string that only looks like one.
    fn tiny_zip(dir: &std::path::Path, prefix: &str) -> std::path::PathBuf {
        use std::io::Write;
        let path = dir.join(format!("build-{}.zip", prefix.trim_end_matches('/').replace('/', "-")));
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer
            .start_file(format!("{prefix}horizon-client.exe"), options)
            .unwrap();
        writer.write_all(b"not really a game").unwrap();
        writer
            .start_file(format!("{prefix}assets/readme.txt"), options)
            .unwrap();
        writer.write_all(b"hello").unwrap();
        writer.finish().unwrap();
        path
    }

    /// The build pins its own game version and server jar. Reading the pin wrong
    /// would fetch a jar the preparer then refuses, so it is tested directly.
    #[test]
    fn the_pin_names_the_version_and_the_server_jar() {
        let dir = std::env::temp_dir().join(format!("mcl-horizon-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("research-lock.json"),
            r#"{"minecraft":{"version":"26.3",
                 "server_sha1":"33680F5F2AC32864D6D7CF5E56A705FDB3E05F4C"}}"#,
        )
        .unwrap();
        let pinned = pin(&dir).unwrap();
        assert_eq!(pinned.version, "26.3");
        // Lowercased: the URL is the hash, and Mojang's store is lowercase.
        assert_eq!(pinned.server_sha1, "33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c");
        assert_eq!(
            server_url(&pinned.server_sha1),
            "https://piston-data.mojang.com/v1/objects/\
             33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c/server.jar"
        );

        // A build with no lock file is an error, not a guess.
        std::fs::remove_file(dir.join("research-lock.json")).unwrap();
        assert!(pin(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A jar already on disk with the right hash is used without a network call.
    #[test]
    fn a_matching_server_jar_is_reused_and_a_wrong_one_is_dropped() {
        let dir = std::env::temp_dir().join(format!("mcl-horizon-jar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = HorizonPaths { root: dir.clone() };
        let build = paths.build_dir("b");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(
            build.join("research-lock.json"),
            r#"{"minecraft":{"version":"26.3","server_sha1":"0000000000000000000000000000000000000000"}}"#,
        )
        .unwrap();

        // Right version, wrong bytes: the file must be dropped, and the fetch
        // that follows fails on a closed network rather than being skipped.
        let target = paths.server_jar("26.3");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"not the pinned jar").unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut events = Vec::new();
        let outcome = ensure_server_jar(&paths, "b", &cancel, &mut |e| events.push(e));
        assert!(outcome.is_err(), "a wrong jar must not be accepted");
        assert!(!target.exists(), "the wrong file must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_preparers_progress_lines_are_understood() {
        assert_eq!(
            parse_progress(r#"{"message":"importing assets"}"#),
            Some(PrepareEvent::Message {
                text: "importing assets".into()
            })
        );
        assert_eq!(
            parse_progress(r#"{"fraction": 0.375}"#),
            Some(PrepareEvent::Fraction { value: 0.375 })
        );
        assert_eq!(
            parse_progress(r#"{"error":"no pinned version"}"#),
            Some(PrepareEvent::Error {
                text: "no pinned version".into()
            })
        );
        // A plain line still reaches the screen; a blank line does not.
        assert_eq!(
            parse_progress("warning: something"),
            Some(PrepareEvent::Message {
                text: "warning: something".into()
            })
        );
        assert_eq!(parse_progress("   "), None);
        // `error` outranks `fraction`: a failing step reports both.
        assert_eq!(
            parse_progress(r#"{"fraction":0.9,"error":"boom"}"#),
            Some(PrepareEvent::Error { text: "boom".into() })
        );
    }

    /// `--check` says nothing and exits 0 when the resources are there; the
    /// screen must read that as ready, not as a silent failure.
    #[test]
    fn a_ready_check_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("mcl-horizon-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = HorizonPaths { root: dir.clone() };
        std::fs::create_dir_all(paths.build_dir("nothing")).unwrap();
        // No preparer in the folder: that is an error, and it names the folder.
        let error = prepare_check(&paths, "nothing").expect_err("no preparer must fail");
        assert!(format!("{error:#}").contains("install the build first"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_single_wrapper_folder_is_stripped_but_a_flat_archive_is_not() {
        let wrapped = vec![
            Some((PathBuf::from("horizon-windows-x64"), true)),
            Some((PathBuf::from("horizon-windows-x64/horizon-client.exe"), false)),
            Some((PathBuf::from("horizon-windows-x64/LICENSE"), false)),
            Some((PathBuf::from("horizon-windows-x64/mods"), true)),
        ];
        assert_eq!(
            common_root(&wrapped),
            Some(PathBuf::from("horizon-windows-x64"))
        );
        // Flat: nothing to strip.
        assert_eq!(
            common_root(&[
                Some((PathBuf::from("horizon-client.exe"), false)),
                Some((PathBuf::from("LICENSE"), false)),
            ]),
            None
        );
        // A top-level file next to a folder means the folder is meaningful.
        assert_eq!(
            common_root(&[
                Some((PathBuf::from("PLAY.txt"), false)),
                Some((PathBuf::from("bin/horizon-client.exe"), false)),
            ]),
            None
        );
        // Two different roots: no single wrapper.
        assert_eq!(
            common_root(&[
                Some((PathBuf::from("a/x"), false)),
                Some((PathBuf::from("b/y"), false)),
            ]),
            None
        );
    }

    /// The negative control for stage 2: an archive whose bytes do not match the
    /// published checksum must be refused **before** anything is unpacked, and
    /// it must leave no build folder behind.
    #[test]
    fn a_corrupt_archive_is_refused_and_installs_nothing() {
        let dir = std::env::temp_dir().join(format!("mcl-horizon-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = tiny_zip(&dir, "");
        let dest = dir.join("builds").join("nightly-test");
        let cancel = std::sync::atomic::AtomicBool::new(false);

        let wrong = "0".repeat(64);
        let error = unpack_verified(&archive, &wrong, &dest, &cancel)
            .expect_err("a mismatched hash must not install");
        let text = format!("{error:#}");
        assert!(text.contains("not the published build"), "got: {text}");
        assert!(!dest.exists(), "a refused archive must leave no build folder");
        assert!(
            !dest.with_extension("tmp-install").exists(),
            "no half-unpacked folder may survive a refusal"
        );

        // And the same archive with its real hash does install, so the refusal
        // above was the hash and not a broken unpacker.
        let good = sha256_file(&archive).unwrap();
        unpack_verified(&archive, &good, &dest, &cancel).unwrap();
        assert!(dest.join("horizon-client.exe").is_file());
        assert!(dest.join("assets/readme.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real archive puts everything in `horizon-windows-x64/`. After the
    /// strip, the game executable must sit directly in the build folder — the
    /// path `client()` and `installed()` already use.
    #[test]
    fn a_wrapped_archive_lands_flat_in_the_build_folder() {
        let dir = std::env::temp_dir().join(format!("mcl-horizon-wrap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = tiny_zip(&dir, "horizon-windows-x64/");
        let dest = dir.join("builds").join("nightly-wrapped");
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let good = sha256_file(&archive).unwrap();
        unpack_verified(&archive, &good, &dest, &cancel).unwrap();
        assert!(
            dest.join("horizon-client.exe").is_file(),
            "the wrapper folder must not survive the unpack"
        );
        assert!(!dest.join("horizon-windows-x64").exists());
        assert!(dest.join("assets/readme.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The launch command must name every folder explicitly: a game that falls
    /// back to the working directory would scatter saves into the build folder.
    #[test]
    fn the_launch_command_names_every_folder() {
        let plan = LaunchPlan {
            ctl: PathBuf::from(r"C:\h\build\horizon-ctl.exe"),
            game: PathBuf::from(r"C:\h\build\horizon-client.exe"),
            resources: PathBuf::from(r"C:\h\resources\nightly"),
            game_dir: PathBuf::from(r"C:\h\instances\horizon"),
            home: PathBuf::from(r"C:\h\instances\horizon\home"),
            world: "New World".into(),
            name: "TestPlayer".into(),
        };
        let command = plan.command();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let at = |flag: &str| -> String {
            let index = args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("{flag} missing from {args:?}"));
            args[index + 1].clone()
        };
        assert_eq!(args[0], "launch");
        assert!(args.contains(&"--wait".to_string()), "--wait is the running signal");
        assert_eq!(at("--world"), "New World");
        assert_eq!(at("--name"), "TestPlayer");
        assert_eq!(at("--game"), r"C:\h\build\horizon-client.exe");
        assert_eq!(at("--resources"), r"C:\h\resources\nightly");
        assert_eq!(at("--game-dir"), r"C:\h\instances\horizon");
        assert_eq!(at("--home"), r"C:\h\instances\horizon\home");
        // The game reads HORIZON_HOME; passing --home alone is not enough for
        // the crash reports the failure card looks for.
        let env: Vec<String> = command
            .get_envs()
            .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.unwrap_or_default().to_string_lossy()))
            .collect();
        assert!(
            env.iter().any(|e| e == r"HORIZON_HOME=C:\h\instances\horizon\home"),
            "HORIZON_HOME must point at the instance home, got {env:?}"
        );
    }

    /// The pid comes from ctl's own state line, not from guessing by name.
    #[test]
    fn the_state_line_names_the_game_pid() {
        let line = r#"{"screen":"playing","launch":{"game":"C:\\h\\horizon-client.exe","home":null,"log":"C:\\Temp\\horizon-control.log","ok":true,"pid":8044,"resources":null,"socket":"C:\\Temp\\horizon-control.json","source":"--game","version":null}}"#;
        assert_eq!(launched_pid(line), Some(8044));
        assert_eq!(launched_pid("not json"), None);
        assert_eq!(launched_pid(r#"{"launch":{"ok":false}}"#), None);
    }

    /// A game that disappears without saving did not shut itself down.
    #[test]
    fn a_log_without_a_save_is_a_crash() {
        let dir = std::env::temp_dir().join(format!("mcl-horizon-verdict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let log = logs.join("latest.log");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        std::fs::write(&log, "[1] [main/INFO]: starting\n[2] [main/INFO]: loaded\n").unwrap();
        let cut = exit_verdict(&dir, now - 60);
        assert!(cut.crash, "a log that stops mid-run is a crash");
        assert!(cut.tail.contains("loaded"), "the card needs the last lines");

        std::fs::write(
            &log,
            "[1] [main/INFO]: starting\n[2] [Server thread/INFO]: Saving and pausing game...\n",
        )
        .unwrap();
        assert!(!exit_verdict(&dir, now - 60).crash, "a save is a clean exit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A crash report is the only signal a game we do not own left one: the exit
    /// code is not ours to read. A report from an earlier dive must not count.
    #[test]
    fn only_a_fresh_crash_report_counts() {
        let dir = std::env::temp_dir().join(format!("mcl-horizon-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reports = dir.join("crash-reports");
        std::fs::create_dir_all(&reports).unwrap();
        assert_eq!(crash_report(&dir, 0), None, "no report, no crash");

        std::fs::write(reports.join("crash-2026-11-20.txt"), "boom").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            crash_report(&dir, now - 60).is_some(),
            "a report written during this dive is this dive's crash"
        );
        assert_eq!(
            crash_report(&dir, now + 120),
            None,
            "a report that predates the dive belongs to the last one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real `worlds` reply, trimmed to the fields the page uses.
    const WORLDS: &str = r#"{"worlds":[
      {"commands":false,"corrupted":false,"folder":"New World","last_played":1795201725121,"mode":"survival","name":"New World","version":"26.3"},
      {"commands":true,"corrupted":true,"folder":"Probe","last_played":0,"mode":"creative","name":"Probe","version":"26.2"}
    ]}"#;

    #[test]
    fn worlds_are_read_with_the_folder_and_the_stamp() {
        let rows = parse_worlds(WORLDS).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].folder, "New World", "the folder, not the name, is the key");
        assert_eq!(rows[0].name, "New World");
        assert_eq!(rows[0].mode, "survival");
        assert_eq!(rows[0].version, "26.3");
        assert!(!rows[0].corrupted);
        assert_eq!(rows[0].played().len(), 19, "a stamp, not a raw number");
        assert!(rows[1].corrupted);
        assert_eq!(rows[1].played(), "never");
    }

    /// The page must not turn a refused command into an empty list.
    #[test]
    fn a_refused_worlds_reply_is_an_error() {
        let refused = r#"{"error":"cannot connect to C:\\Temp\\horizon-control.json; is the game running with --control?","kind":"connection"}"#;
        let error = parse_worlds(refused).unwrap_err().to_string();
        assert!(error.contains("is the game running"), "got {error}");
        assert!(parse_worlds("not json").is_err());
    }

    /// This build's ctl splits its command line on spaces, so a folder with one
    /// must never be handed to it: the launcher zips that world itself.
    #[test]
    fn a_folder_with_a_space_is_not_for_ctl() {
        assert!(ctl_can_take("Probe"));
        assert!(ctl_can_take("new-world_2"));
        assert!(!ctl_can_take("New World"));
        assert!(!ctl_can_take(""));
    }

    /// A backup has to come back as the same world, not as a pile of files in
    /// the wrong place: the zip's top folder is the world folder.
    #[test]
    fn a_world_survives_a_backup_and_a_restore() {
        let dir = std::env::temp_dir().join(format!("mcl-horizon-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let instance = dir.join("instances").join("horizon");
        let saves = instance.join("saves").join("New World");
        std::fs::create_dir_all(saves.join("data").join("minecraft")).unwrap();
        std::fs::create_dir_all(saves.join("region")).unwrap();
        std::fs::write(saves.join("level.dat"), b"level").unwrap();
        std::fs::write(saves.join("data/minecraft/weather.dat"), b"rain").unwrap();
        std::fs::write(saves.join("region/r.0.0.mca"), vec![7u8; 4096]).unwrap();

        // No ctl: this is the path a folder with a space takes.
        let made = backup(None, &instance, "New World").unwrap();
        assert!(!made.by_ctl);
        assert!(made.file.ends_with("_New World.zip"));
        assert_eq!(made.bytes, std::fs::metadata(backups_dir(&instance).join(&made.file)).unwrap().len());
        // Deflate shrinks a run of zeros, so the size proves nothing; the entry does.
        let handle = std::fs::File::open(backups_dir(&instance).join(&made.file)).unwrap();
        let mut archive = zip::ZipArchive::new(handle).unwrap();
        assert!(archive.by_name("New World/region/r.0.0.mca").is_ok(), "the zip holds the world under its own folder");

        let listed = list_backups(&instance);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].folder, "New World", "read back from the zip's entries");
        assert_eq!(listed[0].bytes, made.bytes);

        // The world is gone, then the zip puts it back byte for byte.
        std::fs::remove_dir_all(&saves).unwrap();
        let restored = restore(&instance, &made.file).unwrap();
        assert_eq!(restored, "New World");
        assert_eq!(std::fs::read(saves.join("level.dat")).unwrap(), b"level");
        assert_eq!(
            std::fs::read(saves.join("data/minecraft/weather.dat")).unwrap(),
            b"rain"
        );
        assert_eq!(std::fs::read(saves.join("region/r.0.0.mca")).unwrap().len(), 4096);

        // Restoring over a world that is there would silently undo it.
        let again = restore(&instance, &made.file).unwrap_err().to_string();
        assert!(again.contains("already there"), "got {again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deleting is the launcher's job when the folder has a space, and it must
    /// refuse a folder that is not there rather than report success.
    #[test]
    fn a_world_with_a_space_is_deleted_by_the_launcher() {
        let dir = std::env::temp_dir().join(format!("mcl-horizon-delete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let instance = dir.join("instances").join("horizon");
        let saves = instance.join("saves").join("New World");
        std::fs::create_dir_all(&saves).unwrap();
        std::fs::write(saves.join("level.dat"), b"level").unwrap();

        delete_world(None, &instance, "New World").unwrap();
        assert!(!saves.exists());
        assert!(delete_world(None, &instance, "New World").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no game to answer, ctl cannot be asked, and the delete still has to
    /// happen: the page is usable with no dive open.
    #[test]
    fn a_world_is_deleted_by_hand_when_no_game_answers() {
        let dir = std::env::temp_dir().join(format!("mcl-horizon-hand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let instance = dir.join("instances").join("horizon");
        let saves = instance.join("saves").join("Probe");
        std::fs::create_dir_all(&saves).unwrap();
        std::fs::write(saves.join("level.dat"), b"level").unwrap();

        // A ctl that cannot run stands in for "no game is listening".
        let missing = dir.join("no-such-ctl.exe");
        delete_world(Some(&missing), &instance, "Probe").unwrap();
        assert!(!saves.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty name means the game picks one, exactly as before.
    #[test]
    fn an_empty_name_is_not_passed_to_the_game() {
        let plan = LaunchPlan {
            ctl: PathBuf::from(r"C:\h\ctl.exe"),
            game: PathBuf::from(r"C:\h\client.exe"),
            resources: PathBuf::from(r"C:\h\res"),
            game_dir: PathBuf::from(r"C:\h\game"),
            home: PathBuf::from(r"C:\h\home"),
            world: "New World".into(),
            name: String::new(),
        };
        let args: Vec<String> = plan
            .command()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(!args.contains(&"--name".to_string()), "got {args:?}");
    }
}
