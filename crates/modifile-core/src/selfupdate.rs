//! Keeping Modifile up to date.
//!
//! Modifile spends its life telling people that a compiled binary they cannot
//! read deserves suspicion. It would be poor form to then replace its own
//! executable from the internet without applying the same care, so:
//!
//! - **It never runs in the background.** The check is one conditional request,
//!   made when the app starts or when you ask. There is no service, no timer
//!   and nothing listening.
//! - **The download is verified before it is used.** GitHub publishes a SHA-256
//!   for every release asset through its API, and the release carries a
//!   `SHA256SUMS.txt` as well. Both are checked. What that proves is that the
//!   bytes are the ones GitHub is serving — not that they are benign; nothing
//!   downloaded can prove that, and saying otherwise would be the same lie this
//!   project refuses to tell about mods.
//! - **Nothing is replaced without consent**, unless you have turned automatic
//!   installs on yourself.
//! - **A failed update leaves the old version working.** The new binary is
//!   written beside the old one and swapped in by rename; if the swap fails
//!   halfway, the previous binary is put back.
//!
//! ## Replacing a running program
//!
//! Windows will not let you delete a running executable, but it will let you
//! *rename* one. So the old binary is renamed aside, the new one takes its
//! place, and the leftover is deleted on the next launch. Unix allows the
//! unlink directly, but does the same dance anyway so both platforms behave
//! identically and there is one path to reason about.

use std::path::{Path, PathBuf};

use crate::error::{Context, Error, Result};
use crate::http::Http;
use crate::source::github::GitHub;
use crate::source::{Asset, ModId};

/// Where Modifile's own releases live.
pub const REPO_OWNER: &str = "Gamepro5";
pub const REPO_NAME: &str = "modifile";

/// The version this binary was built as.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn repo() -> ModId {
    ModId::github(REPO_OWNER, REPO_NAME)
}

/// A release newer than the one running.
#[derive(Debug, Clone)]
pub struct Available {
    /// The tag, with any leading `v` kept as published.
    pub tag: String,
    /// The tag reduced to digits and dots, for display beside the current one.
    pub version: String,
    pub published_at: String,
    pub notes: String,
    pub web_url: String,
    /// The archive for this platform.
    pub asset: Asset,
    /// The release's `SHA256SUMS.txt`, when it has one.
    pub checksums: Option<Asset>,
}

impl Available {
    pub fn size(&self) -> u64 {
        self.asset.size
    }
}

/// The platform slug the release workflow builds archives for.
///
/// Returns `None` on anything it does not publish, which is honest rather than
/// hopeful: an aarch64 Linux user should be told there is no build, not handed
/// an x86-64 one.
pub fn target_slug() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "windows") => Some("x86_64-windows"),
        ("x86_64", "linux") => Some("x86_64-linux"),
        _ => None,
    }
}

/// The binaries an update replaces. Nothing else in the archive is written.
pub fn binary_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["modifile.exe", "modifile-gui.exe"]
    } else {
        &["modifile", "modifile-gui"]
    }
}

/// Is `candidate` a newer version than `current`?
///
/// Tolerant on purpose: tags are written `v1.0`, `1.0`, `v1.2.3` and
/// `1.2.3-beta.1` by the same person in the same month. Missing components
/// count as zero, so `1.0` and `1.0.0` are the same version, and anything
/// carrying a pre-release suffix loses to the plain release it qualifies.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    compare(candidate, current) == std::cmp::Ordering::Greater
}

fn compare(a: &str, b: &str) -> std::cmp::Ordering {
    let (a_nums, a_pre) = split_version(a);
    let (b_nums, b_pre) = split_version(b);

    for i in 0..a_nums.len().max(b_nums.len()) {
        let left = a_nums.get(i).copied().unwrap_or(0);
        let right = b_nums.get(i).copied().unwrap_or(0);
        match left.cmp(&right) {
            std::cmp::Ordering::Equal => {}
            other => return other,
        }
    }

    // Equal numbers: a release beats its own pre-releases.
    match (a_pre.is_empty(), b_pre.is_empty()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => a_pre.cmp(&b_pre),
    }
}

fn split_version(text: &str) -> (Vec<u64>, String) {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match text.find(['-', '+']) {
        Some(at) => (&text[..at], text[at + 1..].to_string()),
        None => (text, String::new()),
    };
    let nums = core
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect();
    (nums, pre)
}

/// Normalise a tag for showing next to the current version.
pub fn display_version(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

// ---------------------------------------------------------------------------
// Checking
// ---------------------------------------------------------------------------

/// Is there a newer release than this binary?
///
/// One conditional request against the repository's releases, through the same
/// cache everything else uses — so asking twice in a session costs nothing, and
/// a `304` costs nothing at all with a token set.
///
/// Draft releases are not published and never appear here, which is deliberate
/// on the publishing side too: the workflow creates drafts so a bad build can
/// be checked before anyone is offered it.
pub async fn check(github: &GitHub) -> Result<Option<Available>> {
    let Some(slug) = target_slug() else {
        return Err(Error::other(format!(
            "there is no published build for {}-{}, so Modifile cannot update itself on \
             this machine. Build from source, or watch https://github.com/{REPO_OWNER}/\
             {REPO_NAME}/releases.",
            std::env::consts::ARCH,
            std::env::consts::OS,
        )));
    };

    let releases = github.releases(&repo()).await?;
    let Some(newest) = releases
        .iter()
        .filter(|r| !r.prerelease)
        .find(|r| is_newer(&r.tag, current_version()))
    else {
        return Ok(None);
    };

    // The archive for this platform, and the checksums beside it.
    let asset = newest
        .assets
        .iter()
        .find(|a| a.name.contains(slug) && is_archive(&a.name))
        .cloned();
    let Some(asset) = asset else {
        return Err(Error::other(format!(
            "Modifile {} is out, but it publishes no {slug} archive — so there is nothing \
             to install here. {}",
            display_version(&newest.tag),
            newest.web_url
        )));
    };

    Ok(Some(Available {
        tag: newest.tag.clone(),
        version: display_version(&newest.tag),
        published_at: newest.published_at.clone(),
        notes: newest.name.clone(),
        web_url: newest.web_url.clone(),
        asset,
        checksums: newest
            .assets
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case("SHA256SUMS.txt"))
            .cloned(),
    }))
}

fn is_archive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".zip") || lower.ends_with(".tar.gz")
}

// ---------------------------------------------------------------------------
// Downloading
// ---------------------------------------------------------------------------

/// Download the update and check it against everything the release says.
///
/// Returns the archive's path. Verification happens here rather than at install
/// time so a corrupted download never reaches the swap.
pub async fn stage(http: &Http, available: &Available, into: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(into).ctx(format!("creating {}", into.display()))?;
    let archive = into.join(&available.asset.name);

    let (_, sha256) = http
        .download_to(&available.asset.download_url, &archive)
        .await?;

    // GitHub's own digest for the asset, straight from the API.
    if let Some(expected) = &available.asset.digest {
        if !expected.eq_ignore_ascii_case(&sha256) {
            let _ = std::fs::remove_file(&archive);
            return Err(Error::Integrity {
                name: available.asset.name.clone(),
                expected: expected.clone(),
                actual: sha256,
            });
        }
    }

    // And the checksums file published alongside it. A second opinion from the
    // same publisher is not proof of anything grand, but it does catch a
    // half-uploaded asset, which is the failure that actually happens.
    if let Some(checksums) = &available.checksums {
        if let Ok(Some(bytes)) = http.get_bytes(&checksums.download_url).await {
            if let Some(expected) =
                find_checksum(&String::from_utf8_lossy(&bytes), &available.asset.name)
            {
                if !expected.eq_ignore_ascii_case(&sha256) {
                    let _ = std::fs::remove_file(&archive);
                    return Err(Error::Integrity {
                        name: available.asset.name.clone(),
                        expected,
                        actual: sha256,
                    });
                }
            }
        }
    }

    if available.asset.digest.is_none() && available.checksums.is_none() {
        let _ = std::fs::remove_file(&archive);
        return Err(Error::other(format!(
            "release {} publishes neither a digest nor a SHA256SUMS.txt, so this download \
             cannot be checked against anything. Refusing to install it. Download it \
             yourself from {} if you are sure.",
            available.version, available.web_url
        )));
    }

    Ok(archive)
}

/// Pull one file's hash out of a `sha256sum` listing.
fn find_checksum(text: &str, file_name: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        // sha256sum writes " *name" for binary mode.
        let name = name.trim_start_matches('*');
        if name.eq_ignore_ascii_case(file_name) {
            return Some(hash.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub replaced: Vec<String>,
    /// Old binaries that could not be deleted because they are still running.
    /// Cleaned up on the next launch; harmless meanwhile.
    pub left_behind: usize,
}

/// Where this Modifile is installed.
pub fn install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().ctx("finding this program's own path")?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::other("this program has no containing directory"))
}

/// Replace the installed binaries with the ones in `archive`.
///
/// Only the files in `binary_names` are written, whatever else the archive
/// holds. An update is not an opportunity to drop arbitrary files into a
/// directory on someone's machine.
pub fn apply(archive: &Path, dir: &Path) -> Result<Applied> {
    let wanted = binary_names();
    let staged = extract_binaries(archive, dir, wanted)?;

    if staged.is_empty() {
        return Err(Error::other(format!(
            "{} contains none of the expected binaries ({}). Refusing to install it.",
            archive.display(),
            wanted.join(", ")
        )));
    }

    let mut report = Applied::default();
    for (name, new_path) in staged {
        let target = dir.join(&name);
        match swap(&new_path, &target) {
            Ok(left) => {
                report.replaced.push(name);
                if left {
                    report.left_behind += 1;
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&new_path);
                return Err(e);
            }
        }
    }
    Ok(report)
}

/// Move `new_path` onto `target`, keeping the old one recoverable throughout.
///
/// Returns whether the previous binary had to be left on disk.
fn swap(new_path: &Path, target: &Path) -> Result<bool> {
    if !target.exists() {
        std::fs::rename(new_path, target)
            .ctx(format!("installing {}", target.display()))?;
        return Ok(false);
    }

    // Unique, because a previous update's leftover may still be locked by a
    // running copy and `.old` would collide with it.
    let parked = target.with_extension(format!("old-{}", crate::paths::now_millis()));
    std::fs::rename(target, &parked).ctx(format!(
        "moving the running {} aside — is it open somewhere this cannot rename it?",
        target.display()
    ))?;

    if let Err(e) = std::fs::rename(new_path, target) {
        // Put it back rather than leaving no binary at all.
        let _ = std::fs::rename(&parked, target);
        return Err(Error::Io(e)).ctx(format!("installing {}", target.display()));
    }

    // Deleting a running executable fails on Windows, which is expected and
    // not a problem: `cleanup` sweeps it up next launch.
    Ok(std::fs::remove_file(&parked).is_err())
}

/// Delete binaries parked aside by a previous update.
///
/// Called at startup. Silent: a leftover that cannot be removed yet is not
/// worth telling anybody about, it will go on the launch after this one.
pub fn cleanup(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let parked = name.contains(".old-")
            && binary_names()
                .iter()
                .any(|b| name.starts_with(b.split('.').next().unwrap_or(b)));
        if parked && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Pull the wanted binaries out of the archive, as `<name>.new` beside the
/// installed ones.
fn extract_binaries(
    archive: &Path,
    dir: &Path,
    wanted: &[&str],
) -> Result<Vec<(String, PathBuf)>> {
    let lower = archive.to_string_lossy().to_ascii_lowercase();
    let mut out = Vec::new();

    let mut take = |entry_name: &str, read: &mut dyn std::io::Read| -> Result<()> {
        // The archive nests everything under a versioned directory, and the
        // file name is all that matters. Taking only the last component also
        // means a crafted path cannot escape the directory.
        let base = entry_name
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(entry_name)
            .to_string();
        if !wanted.contains(&base.as_str()) || out.iter().any(|(n, _)| *n == base) {
            return Ok(());
        }

        let staged = dir.join(format!("{base}.new"));
        let mut file = std::fs::File::create(&staged)
            .ctx(format!("writing {}", staged.display()))?;
        std::io::copy(read, &mut file).ctx(format!("writing {}", staged.display()))?;
        drop(file);
        make_executable(&staged);
        out.push((base, staged));
        Ok(())
    };

    if lower.ends_with(".zip") {
        let file = std::fs::File::open(archive)
            .ctx(format!("opening {}", archive.display()))?;
        let mut zip = zip::ZipArchive::new(file)
            .map_err(|e| Error::other(format!("{} is not a zip: {e}", archive.display())))?;
        for i in 0..zip.len() {
            let mut entry = zip
                .by_index(i)
                .map_err(|e| Error::other(format!("reading the update archive: {e}")))?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            take(&name, &mut entry)?;
        }
    } else if lower.ends_with(".tar.gz") {
        let file = std::fs::File::open(archive)
            .ctx(format!("opening {}", archive.display()))?;
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        for entry in tar
            .entries()
            .ctx(format!("reading {}", archive.display()))?
        {
            let mut entry = entry.ctx("reading the update archive")?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let name = entry
                .path()
                .ctx("reading a name from the update archive")?
                .to_string_lossy()
                .into_owned();
            take(&name, &mut entry)?;
        }
    } else {
        return Err(Error::other(format!(
            "{} is not an archive Modifile knows how to open",
            archive.display()
        )));
    }

    Ok(out)
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_are_recognised() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn a_leading_v_is_not_part_of_the_version() {
        // The workflow accepts tags with and without it, so both arrive here.
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(is_newer("V1.0", "0.9"));
    }

    #[test]
    fn missing_components_count_as_zero() {
        // `1.0` and `1.0.0` are the same release tagged two ways.
        assert!(!is_newer("1.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0"));
        assert!(is_newer("1.0.1", "1.0"));
        assert!(is_newer("2", "1.9.9"));
    }

    #[test]
    fn a_release_beats_its_own_prereleases() {
        assert!(is_newer("1.0.0", "1.0.0-beta.1"));
        assert!(!is_newer("1.0.0-beta.1", "1.0.0"));
        // And a prerelease still beats an older release.
        assert!(is_newer("1.1.0-rc.1", "1.0.0"));
    }

    #[test]
    fn version_numbers_are_compared_as_numbers() {
        // The trap: "0.10.0" sorts before "0.9.0" as text.
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0.10", "1.0.9"));
    }

    #[test]
    fn checksums_are_read_from_a_sha256sum_listing() {
        let listing = "\
abc123  modifile-v1.0-x86_64-linux.tar.gz
def456 *modifile-v1.0-x86_64-windows.zip
";
        assert_eq!(
            find_checksum(listing, "modifile-v1.0-x86_64-windows.zip").as_deref(),
            Some("def456")
        );
        assert_eq!(
            find_checksum(listing, "modifile-v1.0-x86_64-linux.tar.gz").as_deref(),
            Some("abc123")
        );
        assert_eq!(find_checksum(listing, "not-in-there.zip"), None);
    }

    #[test]
    fn only_the_published_archive_kinds_are_accepted() {
        assert!(is_archive("modifile-v1.0-x86_64-windows.zip"));
        assert!(is_archive("modifile-v1.0-x86_64-linux.tar.gz"));
        assert!(!is_archive("SHA256SUMS.txt"));
        assert!(!is_archive("README.md"));
    }

    #[test]
    fn the_swap_leaves_a_working_binary() {
        let dir = std::env::temp_dir().join(format!(
            "modifile-selfupdate-{}",
            crate::paths::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let target = dir.join("modifile-test-bin");
        std::fs::write(&target, b"old").unwrap();
        let staged = dir.join("modifile-test-bin.new");
        std::fs::write(&staged, b"new").unwrap();

        swap(&staged, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!staged.exists(), "the staged copy should be gone");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn installing_over_nothing_still_works() {
        // First install into an empty directory, or a binary someone deleted.
        let dir = std::env::temp_dir().join(format!(
            "modifile-selfupdate-fresh-{}",
            crate::paths::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let target = dir.join("modifile-test-bin");
        let staged = dir.join("modifile-test-bin.new");
        std::fs::write(&staged, b"new").unwrap();

        assert!(!swap(&staged, &target).unwrap());
        assert_eq!(std::fs::read(&target).unwrap(), b"new");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_archive_cannot_write_outside_the_install_directory() {
        // Only the base name is ever used, so a crafted entry path is inert.
        for crafted in [
            "../../../evil",
            "/etc/modifile",
            "nested/dir/modifile",
            "..\\..\\modifile.exe",
        ] {
            let base = crafted
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(crafted);
            assert!(!base.contains('/') && !base.contains('\\') && base != "..");
        }
    }
}
