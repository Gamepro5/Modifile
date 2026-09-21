//! The content-addressed mod store.
//!
//! Every downloaded artifact lands in `store/<ab>/<sha256>/`, unpacked if the
//! pack says it is an archive. Profiles never own files — they own references
//! into this store. Two profiles using the same version of a mod share one copy
//! on disk, and switching between them moves zero bytes.

use std::path::{Path, PathBuf};

use crate::error::{Context, Error, Result};

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// One file inside a stored entry.
#[derive(Debug, Clone)]
pub struct StoredFile {
    /// Path relative to the entry root, always `/`-separated.
    pub rel: String,
    pub abs: PathBuf,
    pub size: u64,
}

/// The per-file checksums of one entry, written when it is stored.
///
/// The directory name is the hash of the *archive*, which says nothing about
/// the unpacked files inside it once they exist on disk. Deployment hard-links
/// those files into the game folder, and a hard link is not a copy: anything
/// that writes to the game's copy in place writes straight through to the
/// store. Without this record that damage is undetectable and permanent —
/// `contains` is true, so nothing ever re-downloads.
const ENTRY_FILE: &str = ".modifile-entry.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntryRecord {
    pub files: Vec<EntryFile>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntryFile {
    pub rel: String,
    pub size: u64,
    pub sha256: String,
}

/// What a checksum pass found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryHealth {
    Good,
    /// Files whose bytes no longer match what was stored, or that are gone.
    Damaged { files: Vec<String> },
    /// Stored before checksums existed, so there is nothing to compare
    /// against. Not evidence of damage, and must not be reported as such.
    Unrecorded,
    Missing,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn entry_path(&self, sha256: &str) -> PathBuf {
        self.root.join(&sha256[..2]).join(sha256)
    }

    pub fn contains(&self, sha256: &str) -> bool {
        self.entry_path(sha256).is_dir()
    }

    /// Put an artifact in the store. Idempotent: an existing entry is left
    /// alone, which makes re-running an install free.
    pub fn insert(
        &self,
        sha256: &str,
        artifact: &Path,
        asset_name: &str,
        unpack: bool,
    ) -> Result<PathBuf> {
        let dest = self.entry_path(sha256);
        if dest.is_dir() {
            return Ok(dest);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
        }

        // Build in a sibling temp dir and rename, so a crash mid-extract can
        // never leave a half-populated entry that looks complete.
        let staging = dest.with_file_name(format!(
            "{sha256}.incoming.{}",
            crate::paths::now_millis()
        ));
        if staging.exists() {
            std::fs::remove_dir_all(&staging).ok();
        }
        std::fs::create_dir_all(&staging).ctx(format!("creating {}", staging.display()))?;

        let result = if unpack {
            unpack_zip(artifact, &staging)
        } else {
            let name = sanitize_file_name(asset_name);
            std::fs::copy(artifact, staging.join(&name))
                .map(|_| ())
                .ctx(format!("storing {name}"))
        };

        if let Err(e) = result {
            std::fs::remove_dir_all(&staging).ok();
            return Err(e);
        }

        // Checksum every unpacked file, then take write permission away. The
        // record is what lets damage be found; the read-only bit is what stops
        // most of it happening, by turning a silent write-through into a
        // failed write the tool doing it has to report.
        if let Err(e) = seal(&staging) {
            std::fs::remove_dir_all(&staging).ok();
            return Err(e);
        }

        match std::fs::rename(&staging, &dest) {
            Ok(()) => Ok(dest),
            Err(_) if dest.is_dir() => {
                // Another process won the race; its entry is byte-identical
                // because the directory name is the content hash.
                std::fs::remove_dir_all(&staging).ok();
                Ok(dest)
            }
            Err(e) => {
                std::fs::remove_dir_all(&staging).ok();
                Err(Error::Io(e)).ctx(format!("publishing store entry {sha256}"))
            }
        }
    }

    /// Every file in an entry, sorted for stable deploy plans.
    pub fn files(&self, sha256: &str) -> Result<Vec<StoredFile>> {
        let root = self.entry_path(sha256);
        if !root.is_dir() {
            return Err(Error::NotFound(format!("store entry {sha256}")));
        }
        let mut out = Vec::new();
        walk(&root, &root, &mut out)?;
        // Our own bookkeeping is not part of the mod.
        out.retain(|f| f.rel != ENTRY_FILE);
        out.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(out)
    }

    /// Re-checksum an entry against what was recorded when it was stored.
    ///
    /// Reads every byte, so it is for repair and explicit verification, not
    /// for something that runs on every deploy.
    pub fn check(&self, sha256: &str) -> EntryHealth {
        let root = self.entry_path(sha256);
        if !root.is_dir() {
            return EntryHealth::Missing;
        }
        let Some(record) = self.record(sha256) else {
            return EntryHealth::Unrecorded;
        };

        let mut damaged = Vec::new();
        for file in &record.files {
            let path = root.join(&file.rel);
            match std::fs::metadata(&path) {
                Ok(meta) if meta.len() == file.size => {
                    match crate::hash::sha256_file(&path) {
                        Ok(sha) if sha == file.sha256 => {}
                        _ => damaged.push(file.rel.clone()),
                    }
                }
                // A size change is proof enough; no need to read the bytes.
                _ => damaged.push(file.rel.clone()),
            }
        }

        if damaged.is_empty() {
            EntryHealth::Good
        } else {
            EntryHealth::Damaged { files: damaged }
        }
    }

    fn record(&self, sha256: &str) -> Option<EntryRecord> {
        let raw = std::fs::read(self.entry_path(sha256).join(ENTRY_FILE)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    /// Throw an entry away so the next sync fetches it again. Used when a
    /// checksum pass finds the stored copy is no longer what was downloaded.
    pub fn discard(&self, sha256: &str) -> Result<()> {
        let path = self.entry_path(sha256);
        if !path.is_dir() {
            return Ok(());
        }
        unseal(&path);
        std::fs::remove_dir_all(&path).ctx(format!("discarding store entry {sha256}"))?;
        Ok(())
    }

    /// Every stored entry with its size on disk.
    pub fn entries(&self) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        let Ok(shards) = std::fs::read_dir(&self.root) else {
            return Ok(out);
        };
        for shard in shards.flatten() {
            if !shard.path().is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())?.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.contains(".incoming.") {
                    continue;
                }
                out.push((name, dir_size(&entry.path())));
            }
        }
        Ok(out)
    }

    /// Bytes one entry occupies, or 0 if it is not stored.
    pub fn size_of(&self, sha256: &str) -> u64 {
        let path = self.entry_path(sha256);
        if path.is_dir() {
            dir_size(&path)
        } else {
            0
        }
    }

    /// Remove one entry by hash.
    pub fn remove(&self, sha256: &str) -> Result<u64> {
        let path = self.entry_path(sha256);
        if !path.is_dir() {
            return Ok(0);
        }
        let size = dir_size(&path);
        unseal(&path);
        std::fs::remove_dir_all(&path).ctx(format!("removing store entry {sha256}"))?;
        Ok(size)
    }

    /// Delete entries no lockfile references. Returns (entries, bytes) freed.
    pub fn gc(&self, keep: &std::collections::HashSet<String>) -> Result<(usize, u64)> {
        let mut entries = 0;
        let mut bytes = 0;
        let Ok(shards) = std::fs::read_dir(&self.root) else {
            return Ok((0, 0));
        };
        for shard in shards.flatten() {
            if !shard.path().is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())?.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                // Leave abandoned `.incoming.` dirs to the next insert.
                if keep.contains(&name) || name.contains(".incoming.") {
                    continue;
                }
                let size = dir_size(&entry.path());
                unseal(&entry.path());
                std::fs::remove_dir_all(entry.path())
                    .ctx(format!("removing store entry {name}"))?;
                entries += 1;
                bytes += size;
            }
        }
        Ok((entries, bytes))
    }

    pub fn size_bytes(&self) -> u64 {
        dir_size(&self.root)
    }
}

/// Checksum an entry's files, write the record, and drop write permission.
///
/// Run on the staging directory before it is published, so a published entry
/// is always sealed and there is no window where it is not.
fn seal(staging: &Path) -> Result<()> {
    let mut files = Vec::new();
    walk(staging, staging, &mut files)?;
    files.sort_by(|a, b| a.rel.cmp(&b.rel));

    let record = EntryRecord {
        files: files
            .iter()
            .map(|file| {
                Ok(EntryFile {
                    rel: file.rel.clone(),
                    size: file.size,
                    sha256: crate::hash::sha256_file(&file.abs)?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    std::fs::write(
        staging.join(ENTRY_FILE),
        serde_json::to_vec_pretty(&record)?,
    )
    .ctx("writing store entry checksums")?;

    // After the record, so the record itself is sealed too.
    for file in files {
        set_readonly(&file.abs, true);
    }
    set_readonly(&staging.join(ENTRY_FILE), true);
    Ok(())
}

/// Give write permission back to everything under `dir`, so it can be deleted.
/// Windows refuses to remove a read-only file, and Unix needs the write bit on
/// the file to unlink it from a directory we own.
fn unseal(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => unseal(&path),
            Ok(_) => set_readonly(&path, false),
            Err(_) => {}
        }
    }
}

/// Add or remove write permission, without touching anything else.
///
/// `Permissions::set_readonly(false)` is not usable here: on Unix it sets the
/// file world-writable regardless of umask, which on a shared box hosting a
/// dedicated game server is a real problem and not a hypothetical one. So on
/// Unix we flip the write bits by hand and leave the rest of the mode alone.
pub(crate) fn set_readonly(path: &Path, readonly: bool) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let mut perms = meta.permissions();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = perms.mode();
        perms.set_mode(if readonly {
            mode & !0o222
        } else {
            // Owner only. Whoever else could write it before still can.
            mode | 0o200
        });
    }
    #[cfg(not(unix))]
    perms.set_readonly(readonly);

    let _ = std::fs::set_permissions(path, perms);
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<StoredFile>) -> Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            walk(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| Error::other("store walk escaped its root"))?
                .to_string_lossy()
                .replace('\\', "/");
            out.push(StoredFile {
                rel,
                abs: path,
                size: meta.len(),
            });
        }
    }
    Ok(())
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(m) if m.is_dir() => total += dir_size(&entry.path()),
            Ok(m) => total += m.len(),
            Err(_) => {}
        }
    }
    total
}

/// Archive junk that should never reach a game directory.
fn is_junk(path: &str) -> bool {
    let lowered = path.to_ascii_lowercase();
    lowered.starts_with("__macosx/")
        || lowered.contains("/__macosx/")
        || lowered.ends_with(".ds_store")
        || lowered.ends_with("thumbs.db")
}

/// Reject anything that would write outside the destination: absolute paths,
/// `..`, drive letters, UNC prefixes. A malicious pack cannot reach this code,
/// but a malicious *archive* can, and zip-slip is the classic way in.
fn safe_relative(name: &str) -> Option<PathBuf> {
    let normalized = name.replace('\\', "/");
    if normalized.is_empty() || is_junk(&normalized) {
        return None;
    }
    let mut out = PathBuf::new();
    for component in normalized.split('/') {
        match component {
            "" | "." => continue,
            ".." => return None,
            c if c.contains(':') => return None,
            c => out.push(c),
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    if cleaned.trim().is_empty() {
        "artifact.bin".to_string()
    } else {
        cleaned
    }
}

/// Extract an archive over a directory, leaving named files alone if they are
/// already there.
///
/// Used to lay a mod loader over a game install: BepInEx's archive carries a
/// default `BepInEx/config/BepInEx.cfg`, and reinstalling must not throw away
/// the one the user has been editing.
pub fn extract_over(archive: &Path, dest: &Path, preserve: &[PathBuf]) -> Result<usize> {
    let file = std::fs::File::open(archive).ctx(format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))?;
    let mut written = 0;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let Some(rel) = safe_relative(entry.name()) else {
            continue;
        };
        let out_path = dest.join(&rel);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
            continue;
        }
        // Anything already present inside a preserved directory stays.
        if out_path.exists()
            && preserve
                .iter()
                .any(|keep| out_path.starts_with(dest.join(keep)))
        {
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::io::BufWriter::new(
            std::fs::File::create(&out_path)
                .ctx(format!("writing {}", out_path.display()))?,
        );
        std::io::copy(&mut entry, &mut out)?;
        written += 1;
    }
    Ok(written)
}

fn unpack_zip(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive).ctx(format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))?;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let raw_name = entry.name().to_string();
        let Some(rel) = safe_relative(&raw_name) else {
            continue;
        };
        let out_path = dest.join(&rel);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::io::BufWriter::new(
            std::fs::File::create(&out_path)
                .ctx(format!("extracting {}", out_path.display()))?,
        );
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_zip_slip() {
        assert!(safe_relative("../../etc/passwd").is_none());
        assert!(safe_relative("a/../../b").is_none());
        assert!(safe_relative("C:/Windows/system32/evil.dll").is_none());
        assert!(safe_relative("..\\..\\windows\\evil.dll").is_none());
    }

    #[test]
    fn keeps_ordinary_paths() {
        assert_eq!(
            safe_relative("WeakAuras/Core.lua").unwrap(),
            PathBuf::from("WeakAuras").join("Core.lua")
        );
        assert_eq!(
            safe_relative("./plugins/Mod.dll").unwrap(),
            PathBuf::from("plugins").join("Mod.dll")
        );
    }

    #[test]
    fn drops_archive_junk() {
        assert!(safe_relative("__MACOSX/._Core.lua").is_none());
        assert!(safe_relative("WeakAuras/.DS_Store").is_none());
    }

    #[test]
    fn sharded_layout_is_stable() {
        let store = Store::new("/tmp/store");
        let sha = "abcdef0123456789";
        assert_eq!(
            store.entry_path(sha),
            PathBuf::from("/tmp/store").join("ab").join(sha)
        );
    }

    fn scratch(tag: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "modifile-store-{tag}-{}",
            crate::paths::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (Store::new(dir.join("store")), dir)
    }

    /// Stores a loose (unpacked) artifact and returns its entry path.
    fn insert_one(store: &Store, dir: &Path, sha: &str, body: &[u8]) -> PathBuf {
        let artifact = dir.join("artifact.dll");
        std::fs::write(&artifact, body).unwrap();
        store.insert(sha, &artifact, "artifact.dll", false).unwrap()
    }

    #[test]
    fn a_stored_entry_checksums_clean() {
        let (store, dir) = scratch("clean");
        let sha = "aa00000000000000000000000000000000000000000000000000000000000000";
        insert_one(&store, &dir, sha, b"the real mod");

        assert_eq!(store.check(sha), EntryHealth::Good);
        // The bookkeeping file is not part of the mod.
        let files = store.files(sha).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].rel, "artifact.dll");

        unseal(&store.entry_path(sha));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The case that has no other defence: a deployed hard link written
    /// through into the store. The bytes are already gone; all that is left is
    /// noticing, so that it can be downloaded again.
    #[test]
    fn tampering_with_a_stored_file_is_caught() {
        let (store, dir) = scratch("tamper");
        let sha = "bb00000000000000000000000000000000000000000000000000000000000000";
        let entry = insert_one(&store, &dir, sha, b"the real mod");

        let victim = entry.join("artifact.dll");
        set_readonly(&victim, false);
        std::fs::write(&victim, b"something else!").unwrap();

        match store.check(sha) {
            EntryHealth::Damaged { files } => assert_eq!(files, vec!["artifact.dll".to_string()]),
            other => panic!("expected damage, got {other:?}"),
        }

        unseal(&entry);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stored_file_cannot_be_written_in_place() {
        let (store, dir) = scratch("readonly");
        let sha = "cc00000000000000000000000000000000000000000000000000000000000000";
        let entry = insert_one(&store, &dir, sha, b"the real mod");

        let err = std::fs::OpenOptions::new()
            .write(true)
            .open(entry.join("artifact.dll"));
        assert!(err.is_err(), "a sealed store file accepted a write");

        unseal(&entry);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Read-only files are exactly the ones Windows refuses to delete, so the
    /// cleanup paths have to undo the seal first.
    #[test]
    fn sealed_entries_can_still_be_collected() {
        let (store, dir) = scratch("gc");
        let sha = "dd00000000000000000000000000000000000000000000000000000000000000";
        insert_one(&store, &dir, sha, b"the real mod");

        let (entries, _) = store.gc(&std::collections::HashSet::new()).unwrap();
        assert_eq!(entries, 1);
        assert!(!store.contains(sha));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_entry_without_a_record_is_unknown_not_damaged() {
        let (store, dir) = scratch("legacy");
        let sha = "ee00000000000000000000000000000000000000000000000000000000000000";
        let entry = insert_one(&store, &dir, sha, b"the real mod");

        // An entry stored by an older build, which kept no checksums.
        set_readonly(&entry.join(ENTRY_FILE), false);
        std::fs::remove_file(entry.join(ENTRY_FILE)).unwrap();

        assert_eq!(store.check(sha), EntryHealth::Unrecorded);

        unseal(&entry);
        std::fs::remove_dir_all(&dir).ok();
    }
}
