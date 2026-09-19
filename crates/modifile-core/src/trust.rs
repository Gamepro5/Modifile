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
    /// A compiled file. The source is public, but nothing proves the file you
    /// are about to run was built from it.
    #[serde(alias = "claimed")]
    Unchecked,
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
            TrustLevel::Unchecked => "unchecked",
            TrustLevel::Blocked => "blocked",
        }
    }

    /// One plain sentence. This is what people actually read, so it says what
    /// it means for them rather than naming a mechanism.
    pub fn explain(self) -> &'static str {
        match self {
            TrustLevel::Verified => {
                "Safe to check: GitHub proves this exact file was built from the \
                 public source code, by the project's own build."
            }
            TrustLevel::Readable => {
                "You can read it: this mod ships as source code, so every file it \
                 installs can be opened and read before you run it."
            }
            TrustLevel::Unchecked => {
                "You are trusting the author: this mod installs a compiled file. \
                 The source code is public, but nothing proves the file matches \
                 it — the author built it on their own machine and uploaded it."
            }
            TrustLevel::Blocked => {
                "Nothing to check: this mod installs a compiled file, publishes no \
                 source code anywhere, and declares no licence. Refused unless you \
                 allow it in Settings."
            }
        }
    }

    /// Badge text. Two words that stand on their own, because most people will
    /// never hover for the long version.
    pub fn short(self) -> &'static str {
        match self {
            TrustLevel::Verified => "verified build",
            TrustLevel::Readable => "readable source",
            TrustLevel::Unchecked => "unverified binary",
            TrustLevel::Blocked => "no source at all",
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
            minimum: TrustLevel::Unchecked,
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
    let source_url = repo.and_then(|r| r.source_url.clone());
    if license.is_none() && source_url.is_some() {
        notes.push("no declared licence — the code is public but the terms are not".to_string());
    }

    let level = if attested {
        TrustLevel::Verified
    } else if executable_files == 0 && readable_files > 0 {
        // Nothing here can run code you cannot read. Textures and fonts do not
        // change that.
        TrustLevel::Readable
    } else if executable_files > 0 && source_url.is_none() && license.is_none() {
        // A compiled file with no code published anywhere and no licence. There
        // is nothing to check it against, and nothing honest to say about it.
        notes.push(
            "no public source code is linked, so there is no way to check what this does"
                .to_string(),
        );
        TrustLevel::Blocked
    } else if executable_files > 0 {
        let mut note = format!("{executable_files} compiled file(s) you cannot audit");
        match &source_url {
            Some(url) => note.push_str(&format!("; source published at {url}")),
            None => note.push_str("; no source link, only a licence"),
        }
        notes.push(note);
        TrustLevel::Unchecked
    } else {
        // No source, no executables — an archive of pure data.
        TrustLevel::Unchecked
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
            versions: Default::default(),
            loaders: Vec::new(),
            search: Default::default(),
            running: Default::default(),
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
    fn licensed_binary_is_unchecked() {
        let files = vec![file("plugins/Mod.dll")];
        let repo = RepoInfo {
            license: Some("MIT".into()),
            ..Default::default()
        };
        let report = assess(&test_pack(), &files, Some(&repo), false);
        assert_eq!(report.level, TrustLevel::Unchecked);
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
            minimum: TrustLevel::Unchecked,
        };
        assert!(!strict.permits(&report));
        assert!(TrustPolicy::default().permits(&report));
    }
}
