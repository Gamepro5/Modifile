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
        out.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(out)
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
}
