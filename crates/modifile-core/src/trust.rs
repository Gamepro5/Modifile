//! The trust ladder.
//!
//! "It has a public repo" is not the same as "you can audit what runs". WoW
//! addons ship Lua, so the artifact *is* the source. Valheim plugins and
//! Minecraft mods ship compiled binaries, and nothing about a public repo
//! proves the binary came from it. This module keeps that distinction visible
//! instead of flattening it into a green checkmark.

use serde::{Deserialize, Serialize};

use crate::pack::CompiledPack;
use crate::source::RepoInfo;
use crate::store::StoredFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    /// No public source or no license — refused by default.
    Blocked,
    /// Public repo, compiled artifact, no provenance. You are trusting the author.
    Claimed,
    /// The artifact is source. You can read exactly what will run.
    Readable,
    /// Build provenance ties this exact artifact to a commit in this repo.
    Verified,
}

impl TrustLevel {
    pub fn label(self) -> &'static str {
        match self {
            TrustLevel::Verified => "verified",
            TrustLevel::Readable => "readable",
            TrustLevel::Claimed => "claimed",
            TrustLevel::Blocked => "blocked",
        }
    }

    pub fn explain(self) -> &'static str {
        match self {
            TrustLevel::Verified => "build provenance proves this artifact was built from this repo",
            TrustLevel::Readable => "ships as source you can read",
            TrustLevel::Claimed => "compiled artifact; public repo but no proof the binary matches it",
            TrustLevel::Blocked => "no detected open-source license",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustReport {
    pub level: TrustLevel,
    #[serde(default)]
    pub license: Option<String>,
    pub attested: bool,
    /// Files you can open and read.
    pub readable_files: usize,
    /// Opaque but inert: textures, fonts, sounds. These do not affect the rung.
    #[serde(default)]
    pub data_files: usize,
    /// Files that can execute code you cannot read. These do.
    pub executable_files: usize,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct TrustPolicy {
    /// Refuse mods with no detected SPDX license.
    pub require_license: bool,
    /// Refuse anything below this rung.
    pub minimum: TrustLevel,
}

impl Default for TrustPolicy {
    fn default() -> Self {
        Self {
            // Plenty of legitimate addons are unlicensed by neglect rather than
            // by intent, so this warns by default instead of blocking.
            require_license: false,
            minimum: TrustLevel::Claimed,
        }
    }
}

impl TrustPolicy {
    pub fn permits(&self, report: &TrustReport) -> bool {
        if self.require_license && report.license.is_none() {
            return false;
        }
        report.level >= self.minimum
    }
}

pub fn assess(
    pack: &CompiledPack,
    files: &[StoredFile],
    repo: Option<&RepoInfo>,
    attested: bool,
) -> TrustReport {
    let license = repo.and_then(|r| r.license.clone());
    let mut notes = Vec::new();

    let mut readable_files = 0usize;
    let mut data_files = 0usize;
    let mut executable_files = 0usize;
    for file in files {
        if pack.is_executable(&file.rel) {
            executable_files += 1;
        } else if pack.is_readable(&file.rel) {
            readable_files += 1;
        } else {
            data_files += 1;
        }
    }

    if let Some(repo) = repo {
        if repo.archived {
            notes.push("repository is archived; it will not receive fixes".to_string());
        }
    }
    if license.is_none() {
        notes.push("no detected license — source is visible but the terms are not".to_string());
    }

    let level = if attested {
        TrustLevel::Verified
    } else if executable_files == 0 && readable_files > 0 {
        // Nothing here can run code you cannot read. Textures and fonts do not
        // change that.
        TrustLevel::Readable
    } else if executable_files > 0 && license.is_none() {
        TrustLevel::Blocked
    } else if executable_files > 0 {
        notes.push(format!(
            "{executable_files} compiled file(s) you cannot audit; \
             ask the author to enable build attestations"
        ));
        TrustLevel::Claimed
    } else {
        // No source, no executables — an archive of pure data.
        TrustLevel::Claimed
    };

    TrustReport {
        level,
        license,
        attested,
        readable_files,
        data_files,
        executable_files,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::{CompiledPack, GameMeta, Pack, Target, TargetKind};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn test_pack() -> CompiledPack {
        let mut paths = BTreeMap::new();
        paths.insert("addons".to_string(), "Interface/AddOns".to_string());
        CompiledPack::new(Pack {
            schema: 1,
            game: GameMeta {
                id: "test".into(),
                name: "Test".into(),
                description: String::new(),
                maintainers: vec![],
            },
            targets: vec![Target {
                id: "client".into(),
                name: "Client".into(),
                kind: TargetKind::Client,
                flavor: None,
                asset_reject: vec![],
                processes: vec![],
                markers: vec!["game.exe".into()],
                paths: BTreeMap::new(),
                steam: None,
                candidates: vec![],
            }],
            paths,
            assets: Default::default(),
            state: Default::default(),
            install: vec![],
            readable_extensions: vec!["lua".into(), "toc".into()],
            executable_extensions: vec!["dll".into(), "jar".into()],
        })
        .unwrap()
    }

    fn file(rel: &str) -> StoredFile {
        StoredFile {
            rel: rel.to_string(),
            abs: PathBuf::from(rel),
            size: 1,
        }
    }

    #[test]
    fn all_source_is_readable_even_without_a_license() {
        let files = vec![file("A/Core.lua"), file("A/A.toc")];
        let report = assess(&test_pack(), &files, None, false);
        assert_eq!(report.level, TrustLevel::Readable);
    }

    #[test]
    fn textures_and_fonts_do_not_demote_a_source_addon() {
        // WeakAuras ships hundreds of .tga/.ttf files alongside its Lua. They
        // are opaque but inert, and must not be counted as unauditable code.
        let files = vec![
            file("WeakAuras/Core.lua"),
            file("WeakAuras/Media/icon.tga"),
            file("WeakAuras/Media/font.ttf"),
        ];
        let report = assess(&test_pack(), &files, None, false);
        assert_eq!(report.level, TrustLevel::Readable);
        assert_eq!(report.readable_files, 1);
        assert_eq!(report.data_files, 2);
        assert_eq!(report.executable_files, 0);
    }

    #[test]
    fn unlicensed_binary_is_blocked() {
        let files = vec![file("plugins/Mod.dll")];
        let report = assess(&test_pack(), &files, None, false);
        assert_eq!(report.level, TrustLevel::Blocked);
    }

    #[test]
    fn licensed_binary_is_merely_claimed() {
        let files = vec![file("plugins/Mod.dll")];
        let repo = RepoInfo {
            license: Some("MIT".into()),
            ..Default::default()
        };
        let report = assess(&test_pack(), &files, Some(&repo), false);
        assert_eq!(report.level, TrustLevel::Claimed);
    }

    #[test]
    fn attestation_beats_everything() {
        let files = vec![file("plugins/Mod.dll")];
        let report = assess(&test_pack(), &files, None, true);
        assert_eq!(report.level, TrustLevel::Verified);
    }

    #[test]
    fn policy_can_demand_a_license() {
        let files = vec![file("A/Core.lua")];
        let report = assess(&test_pack(), &files, None, false);
        let strict = TrustPolicy {
            require_license: true,
            minimum: TrustLevel::Claimed,
        };
        assert!(!strict.permits(&report));
        assert!(TrustPolicy::default().permits(&report));
    }
}
