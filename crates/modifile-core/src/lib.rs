//! modifile core.
//!
//! Design constraints, in priority order:
//!
//! 1. **Nothing runs while you play.** Mods are linked into the game directory
//!    and stay there. Launch from Steam, a shortcut, or anywhere else.
//! 2. **Profiles do not duplicate the game.** One content-addressed store,
//!    hard links into the game directory, zero copied bytes per profile.
//! 3. **Game support is data, not code.** A game pack is a TOML file that can
//!    express paths and globs and nothing else — it cannot execute anything.
//! 4. **Only auditable mods.** GitHub releases, public source, with a trust
//!    ladder that refuses to pretend a compiled binary is readable source.

pub mod deploy;
pub mod engine;
pub mod error;
pub mod hash;
pub mod http;
pub mod pack;
pub mod paths;
pub mod process;
pub mod profile;
pub mod roots;
pub mod source;
pub mod state;
pub mod steam;
pub mod store;
pub mod trust;

pub use engine::{format_bytes, Engine, Event};
pub use error::{Error, Result};
pub use pack::{CompiledPack, Pack, Target, TargetKind};
pub use paths::Paths;
pub use profile::{Lock, ModEntry, Profile};
pub use source::ModId;
pub use trust::{TrustLevel, TrustPolicy, TrustReport};

/// Packs shipped in the binary so a fresh install is useful immediately.
/// They are written to the packs directory on first run and are plain files
/// afterwards — editable, replaceable, no different from a community pack.
pub const BUNDLED_PACKS: &[(&str, &str)] = &[
    ("wow.toml", include_str!("../../../packs/wow.toml")),
    ("valheim.toml", include_str!("../../../packs/valheim.toml")),
    ("minecraft.toml", include_str!("../../../packs/minecraft.toml")),
];

#[derive(Debug, Default, Clone)]
pub struct PackInstallReport {
    pub written: Vec<String>,
    pub updated: Vec<String>,
    /// Packs the user has edited. Left exactly as they are.
    pub kept: Vec<String>,
}

/// Install or refresh the bundled packs.
///
/// A pack is a bug-fixable artifact — getting `processes` or a `mutable` config
/// rule wrong means broken behaviour for everyone shipping that pack. So a
/// bundled pack the user has *not* touched is updated in place. One they have
/// edited is never overwritten; it is reported instead, so their work is safe
/// and they can see that a newer version exists.
///
/// "Untouched" is decided by comparing the file against the hash of whatever we
/// last wrote, recorded in `packs/.bundled.json`.
pub fn install_bundled_packs(paths: &Paths) -> Result<PackInstallReport> {
    let ledger_path = paths.packs.join(".bundled.json");
    let mut ledger: std::collections::BTreeMap<String, String> = std::fs::read(&ledger_path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default();

    let mut report = PackInstallReport::default();

    for (name, body) in BUNDLED_PACKS {
        let path = paths.packs.join(name);
        let shipped = hash::sha256_bytes(body.as_bytes());

        if !path.exists() {
            paths::write_atomic(&path, body.as_bytes())?;
            ledger.insert(name.to_string(), shipped);
            report.written.push(name.to_string());
            continue;
        }

        let on_disk = hash::sha256_file(&path)?;
        if on_disk == shipped {
            // Already current.
            ledger.insert(name.to_string(), shipped);
            continue;
        }

        match ledger.get(*name) {
            // Matches what we last wrote, so the difference is our update.
            Some(previous) if previous == &on_disk => {
                paths::write_atomic(&path, body.as_bytes())?;
                ledger.insert(name.to_string(), shipped);
                report.updated.push(name.to_string());
            }
            // The user changed it, or it predates the ledger. Hands off.
            _ => report.kept.push(name.to_string()),
        }
    }

    let _ = paths::write_atomic(&ledger_path, &serde_json::to_vec_pretty(&ledger)?);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> Paths {
        let home = std::env::temp_dir().join(format!(
            "modifile-packs-{tag}-{}",
            paths::now_millis()
        ));
        let paths = Paths::rooted(home);
        paths.ensure().unwrap();
        paths
    }

    #[test]
    fn first_run_writes_every_pack() {
        let paths = temp_paths("first");
        let report = install_bundled_packs(&paths).unwrap();

        assert_eq!(report.written.len(), BUNDLED_PACKS.len());
        assert!(report.updated.is_empty() && report.kept.is_empty());
        assert!(paths.packs.join("wow.toml").exists());

        // Running again changes nothing.
        let second = install_bundled_packs(&paths).unwrap();
        assert!(second.written.is_empty() && second.updated.is_empty());
        assert!(second.kept.is_empty(), "an untouched pack must not be flagged");

        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn an_untouched_pack_is_upgraded_in_place() {
        // Stand in for "shipped with an older build": write an old body and
        // record it as ours, exactly as a previous version would have.
        let paths = temp_paths("upgrade");
        let old_body = "schema = 1\n# an older bundled pack\n";
        paths::write_atomic(&paths.packs.join("wow.toml"), old_body.as_bytes()).unwrap();

        let mut ledger = std::collections::BTreeMap::new();
        ledger.insert("wow.toml".to_string(), hash::sha256_bytes(old_body.as_bytes()));
        paths::write_atomic(
            &paths.packs.join(".bundled.json"),
            &serde_json::to_vec(&ledger).unwrap(),
        )
        .unwrap();

        let report = install_bundled_packs(&paths).unwrap();
        assert!(
            report.updated.contains(&"wow.toml".to_string()),
            "a pack the user never touched should be refreshed: {report:?}"
        );

        let now = std::fs::read_to_string(paths.packs.join("wow.toml")).unwrap();
        assert!(now.contains("processes"), "the fix should have landed");

        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn an_edited_pack_is_never_overwritten() {
        let paths = temp_paths("edited");
        install_bundled_packs(&paths).unwrap();

        let edited = "schema = 1\n# my own careful changes\n";
        paths::write_atomic(&paths.packs.join("wow.toml"), edited.as_bytes()).unwrap();

        let report = install_bundled_packs(&paths).unwrap();
        assert!(report.kept.contains(&"wow.toml".to_string()));
        assert_eq!(
            std::fs::read_to_string(paths.packs.join("wow.toml")).unwrap(),
            edited,
            "the user's edits must survive"
        );

        std::fs::remove_dir_all(&paths.home).ok();
    }
}
