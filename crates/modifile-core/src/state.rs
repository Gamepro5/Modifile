//! Profile-owned state: config files and anything else the game rewrites.
//!
//! The store holds immutable content and is shared by every profile, so
//! anything the game or the user edits must live somewhere else. Each profile
//! gets its own copy, captured when you switch away and restored when you
//! switch back. These files are small — copying is the right call, and it
//! avoids every aliasing problem a hard link would create here.

use std::path::{Path, PathBuf};

use crate::error::{Context, Result};

/// Where one profile keeps its state for one target.
///
/// Under the profile's own game directory, so two games can both have a
/// profile called `main` without their saved settings colliding.
pub fn profile_state_dir(
    profiles_dir: &Path,
    id: &crate::profile::ProfileId,
    target: &str,
) -> PathBuf {
    profiles_dir
        .join(crate::engine::sanitize_name(&id.game))
        .join(format!("{}.state", id.name))
        .join(target)
}

#[derive(Debug, Clone, Default)]
pub struct StateReport {
    pub files: usize,
    pub bytes: u64,
}

/// Copy a directory tree. Missing sources are not an error — a profile that has
/// never been deployed simply has nothing to restore.
fn copy_tree(from: &Path, to: &Path, report: &mut StateReport) -> Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to).ctx(format!("creating {}", to.display()))?;

    for entry in std::fs::read_dir(from)?.flatten() {
        let source = entry.path();
        let dest = to.join(entry.file_name());
        let meta = entry.metadata()?;
        if meta.is_dir() {
            copy_tree(&source, &dest, report)?;
        } else if meta.is_file() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Remove first: if the destination is a hard link into the store,
            // writing through it would mutate shared content.
            std::fs::remove_file(&dest).ok();
            std::fs::copy(&source, &dest)
                .ctx(format!("copying {}", source.display()))?;
            report.files += 1;
            report.bytes += meta.len();
        }
    }
    Ok(())
}

/// Take the live contents of a game's state directory into a profile's keeping.
pub fn capture(live: &Path, profile_state: &Path) -> Result<StateReport> {
    let mut report = StateReport::default();
    copy_tree(live, profile_state, &mut report)?;
    Ok(report)
}

/// Put a profile's saved state back into the game directory.
///
/// Files present in the game directory but not in the profile's copy are left
/// alone — a mod that generates a config we have never seen keeps it.
pub fn restore(profile_state: &Path, live: &Path) -> Result<StateReport> {
    let mut report = StateReport::default();
    copy_tree(profile_state, live, &mut report)?;
    Ok(report)
}

/// Every file under a state directory, as paths relative to it.
pub fn list_files(dir: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.metadata() {
                Ok(m) if m.is_dir() => walk(root, &path, out),
                Ok(m) if m.is_file() => {
                    if let Ok(rel) = path.strip_prefix(root) {
                        out.push(rel.to_path_buf());
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Delete the live contents of a state directory. Only ever called after a
/// successful capture, when switching to a different profile.
pub fn clear(live: &Path) -> Result<()> {
    if !live.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(live)?.flatten() {
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            std::fs::remove_dir_all(&path).ok();
        } else {
            std::fs::remove_file(&path).ok();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "modifile-state-{tag}-{}",
            crate::paths::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn capture_then_restore_round_trips_edits() {
        let root = temp("round");
        let live = root.join("live");
        let saved = root.join("saved");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("valheim_plus.cfg"), b"maxPlayers=20").unwrap();

        let captured = capture(&live, &saved).unwrap();
        assert_eq!(captured.files, 1);

        // The user switches profiles; the live directory is wiped.
        clear(&live).unwrap();
        assert!(!live.join("valheim_plus.cfg").exists());

        // Switching back brings the edit with it.
        restore(&saved, &live).unwrap();
        assert_eq!(
            std::fs::read_to_string(live.join("valheim_plus.cfg")).unwrap(),
            "maxPlayers=20"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn two_profiles_keep_separate_values() {
        let root = temp("two");
        let live = root.join("live");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&live).unwrap();

        std::fs::write(live.join("mod.cfg"), b"difficulty=easy").unwrap();
        capture(&live, &a).unwrap();
        clear(&live).unwrap();

        std::fs::write(live.join("mod.cfg"), b"difficulty=hard").unwrap();
        capture(&live, &b).unwrap();
        clear(&live).unwrap();

        restore(&a, &live).unwrap();
        assert_eq!(
            std::fs::read_to_string(live.join("mod.cfg")).unwrap(),
            "difficulty=easy"
        );
        restore(&b, &live).unwrap();
        assert_eq!(
            std::fs::read_to_string(live.join("mod.cfg")).unwrap(),
            "difficulty=hard"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn restore_never_breaks_a_hardlink_into_the_store() {
        // Copying onto a hard link must replace it, not write through it.
        let root = temp("link");
        let store = root.join("store");
        let live = root.join("live");
        let saved = root.join("saved");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&saved).unwrap();

        let shared = store.join("default.cfg");
        std::fs::write(&shared, b"SHARED DEFAULT").unwrap();
        std::fs::hard_link(&shared, live.join("default.cfg")).unwrap();
        std::fs::write(saved.join("default.cfg"), b"my edit").unwrap();

        restore(&saved, &live).unwrap();

        assert_eq!(
            std::fs::read_to_string(live.join("default.cfg")).unwrap(),
            "my edit"
        );
        // The store copy is untouched — this is the whole point.
        assert_eq!(
            std::fs::read_to_string(&shared).unwrap(),
            "SHARED DEFAULT"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nested_directories_survive() {
        let root = temp("nested");
        let live = root.join("live");
        let saved = root.join("saved");
        std::fs::create_dir_all(live.join("sub/deeper")).unwrap();
        std::fs::write(live.join("sub/deeper/a.cfg"), b"x").unwrap();

        capture(&live, &saved).unwrap();
        clear(&live).unwrap();
        restore(&saved, &live).unwrap();

        assert_eq!(
            std::fs::read_to_string(live.join("sub/deeper/a.cfg")).unwrap(),
            "x"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_sources_are_not_errors() {
        let root = temp("missing");
        assert_eq!(
            capture(&root.join("nope"), &root.join("out")).unwrap().files,
            0
        );
        assert_eq!(
            restore(&root.join("nope"), &root.join("out")).unwrap().files,
            0
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
