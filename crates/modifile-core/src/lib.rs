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
pub mod launch;
pub mod loader;
pub mod modpack;
pub mod pack;
pub mod paths;
pub mod process;
pub mod profile;
pub mod roots;
pub mod selfupdate;
pub mod share;
pub mod source;
pub mod state;
pub mod steam;
pub mod storage;
pub mod text;
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
    (
        "wow-classic-era.toml",
        include_str!("../../../packs/wow-classic-era.toml"),
    ),
    (
        "wow-classic.toml",
        include_str!("../../../packs/wow-classic.toml"),
    ),
    ("valheim.toml", include_str!("../../../packs/valheim.toml")),
    ("repo.toml", include_str!("../../../packs/repo.toml")),
    ("minecraft.toml", include_str!("../../../packs/minecraft.toml")),
];

#[derive(Debug, Default, Clone)]
pub struct PackInstallReport {
    pub written: Vec<String>,
    pub updated: Vec<String>,
    /// Packs the user has edited. Left exactly as they are.
    pub kept: Vec<String>,
    /// Packs replaced by `--force`, with where the old copy was saved.
    pub replaced: Vec<(String, std::path::PathBuf)>,
}

/// What state one bundled pack is in on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackState {
    /// Identical to what this build ships.
    Current,
    /// Not installed yet.
    Missing,
    /// Differs from what this build ships, and we last wrote it — so the
    /// difference is our update and it will be applied automatically.
    Outdated,
    /// Differs, and we cannot prove we wrote the version on disk. Kept, because
    /// overwriting someone's edits is worse than being out of date.
    Edited,
}

/// The state of every bundled pack, for telling the user why a fix has not
/// reached them.
pub fn pack_status(paths: &Paths) -> Vec<(String, PackState)> {
    let ledger: std::collections::BTreeMap<String, String> =
        std::fs::read(paths.packs.join(".bundled.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();

    BUNDLED_PACKS
        .iter()
        .map(|(name, body)| {
            let path = paths.packs.join(name);
            let shipped = hash::sha256_bytes(body.as_bytes());
            let state = match hash::sha256_file(&path) {
                Err(_) => PackState::Missing,
                Ok(on_disk) if on_disk == shipped => PackState::Current,
                Ok(on_disk) if ledger.get(*name) == Some(&on_disk) => PackState::Outdated,
                Ok(_) => PackState::Edited,
            };
            (name.to_string(), state)
        })
        .collect()
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
///
/// `force` replaces even packs we cannot prove are untouched, saving the old
/// copy alongside as `<name>.toml.bak`. That escape hatch exists because the
/// conservative rule has a trap: a pack installed before the ledger existed can
/// never be proven untouched, so it would be kept forever and every later fix
/// would silently fail to reach the user.
pub fn install_bundled_packs_with(paths: &Paths, force: bool) -> Result<PackInstallReport> {
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
            // The user changed it, or it predates the ledger.
            _ if force => {
                // Keep their copy rather than destroying it.
                let backup = paths.packs.join(format!("{name}.bak"));
                std::fs::copy(&path, &backup).ok();
                paths::write_atomic(&path, body.as_bytes())?;
                ledger.insert(name.to_string(), shipped);
                report.replaced.push((name.to_string(), backup));
            }
            _ => report.kept.push(name.to_string()),
        }
    }

    let _ = paths::write_atomic(&ledger_path, &serde_json::to_vec_pretty(&ledger)?);
    Ok(report)
}

/// Install or refresh bundled packs, never overwriting anything unproven.
pub fn install_bundled_packs(paths: &Paths) -> Result<PackInstallReport> {
    install_bundled_packs_with(paths, false)
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
    fn a_pack_predating_the_ledger_can_be_refreshed() {
        // The trap this guards: packs written before the ledger existed can
        // never be proven untouched, so they were kept forever and every later
        // fix silently failed to reach the user.
        let paths = temp_paths("pre-ledger");
        let old = "schema = 1\n# shipped by an older build, no ledger entry\n";
        paths::write_atomic(&paths.packs.join("wow.toml"), old.as_bytes()).unwrap();

        // Without force it is kept, and reported as needing attention.
        let report = install_bundled_packs(&paths).unwrap();
        assert!(report.kept.contains(&"wow.toml".to_string()));
        assert_eq!(
            pack_status(&paths)
                .into_iter()
                .find(|(n, _)| n == "wow.toml")
                .map(|(_, s)| s),
            Some(PackState::Edited)
        );

        // With force it is replaced, and the old copy survives beside it.
        let forced = install_bundled_packs_with(&paths, true).unwrap();
        let (name, backup) = forced
            .replaced
            .iter()
            .find(|(n, _)| n == "wow.toml")
            .expect("should have been replaced");
        assert_eq!(name, "wow.toml");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), old);

        let now = std::fs::read_to_string(paths.packs.join("wow.toml")).unwrap();
        assert!(now.contains("curseforge_game_id"), "the fix should have landed");
        assert_eq!(
            pack_status(&paths)
                .into_iter()
                .find(|(n, _)| n == "wow.toml")
                .map(|(_, s)| s),
            Some(PackState::Current)
        );

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
