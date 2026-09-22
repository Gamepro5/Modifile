//! What a shared setup consists of.
//!
//! A [`Bundle`] is the mod list, the exact versions, the loader and game
//! version, and (optionally) the tuned config files. It is the *contents* of a
//! shared pack; [`crate::mfpack`] is the file those contents travel in.
//!
//! The two are separate because the contents outlived a format once already.
//! Bundles used to be written as a single JSON document, which this build no
//! longer reads or writes — see `mfpack` for why a zip replaced it.
//!
//! What a bundle deliberately does **not** hold is the mods themselves, or
//! their hashes presented as trustworthy. Your friend's copy resolves every
//! mod from its own source on their machine, verifies the download against
//! that source's published digest, and runs the trust ladder locally. So a
//! pack from a stranger can waste your time, but it cannot hand you a binary
//! that nobody else can see.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Context, Error, Result};
use crate::paths::write_atomic;
use crate::profile::{Lock, ModEntry, Profile};
use crate::source::ModId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    /// Format version, so a future field cannot silently corrupt an old
    /// reader. Named for the format it appears in: this is what sits at the
    /// top of a `.mfpack`'s index.
    pub mfpack: u32,
    pub name: String,
    pub game: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub targets: Vec<String>,
    /// What the mods were built against. Minecraft mods are published per game
    /// version and per loader, so a pack that does not carry these resolves to
    /// whatever happens to be newest and hands the importer a set of mods that
    /// will not load together.
    ///
    /// `serde(default)` because bundles written before this existed have
    /// neither, and must still open.
    #[serde(default)]
    pub game_version: Option<String>,
    #[serde(default)]
    pub loader: Option<String>,
    pub mods: Vec<BundleMod>,
    /// target id -> relative path -> file contents.
    #[serde(default)]
    pub configs: BTreeMap<String, BTreeMap<String, ConfigFile>>,
    #[serde(default)]
    pub exported_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleMod {
    pub id: ModId,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub targets: Option<Vec<String>>,
    /// The release the exporter was actually running. Applied as a pin on
    /// import unless the importer asks for latest.
    #[serde(default)]
    pub version: Option<String>,
    /// The source's exact file handle, when the sender had one. A profile that
    /// came from a modpack pins CurseForge file ids, and dropping them here
    /// would quietly hand the recipient a different set of mods than the one
    /// that was tested.
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub prerelease: bool,
}

fn yes() -> bool {
    true
}

/// Config files are text far more often than not, so the common case stays
/// readable and diffable. Anything else falls back to base64.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigFile {
    Text { text: String },
    Binary { base64: String },
}

impl ConfigFile {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).ctx(format!("reading {}", path.display()))?;
        Ok(match String::from_utf8(bytes) {
            Ok(text) => ConfigFile::Text { text },
            Err(e) => ConfigFile::Binary {
                base64: base64_encode(e.as_bytes()),
            },
        })
    }

    /// Classify bytes that came from somewhere other than a file on disk —
    /// an entry inside a `.mfpack`, for instance.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        match String::from_utf8(bytes) {
            Ok(text) => ConfigFile::Text { text },
            Err(e) => ConfigFile::Binary {
                base64: base64_encode(e.as_bytes()),
            },
        }
    }

    pub fn bytes(&self) -> Result<Vec<u8>> {
        Ok(match self {
            ConfigFile::Text { text } => text.as_bytes().to_vec(),
            ConfigFile::Binary { base64 } => base64_decode(base64)?,
        })
    }
}

// A tiny base64 so the whole feature does not need a dependency.
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    let mut lookup = [255u8; 256];
    for (i, c) in B64.iter().enumerate() {
        lookup[*c as usize] = i as u8;
    }
    let cleaned: Vec<u8> = text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();

    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        let mut n = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            let value = lookup[*byte as usize];
            if value == 255 {
                return Err(Error::other("bundle contains invalid base64"));
            }
            n |= (value as u32) << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

impl Bundle {
    /// Build a bundle from a profile, its lock, and its saved configs.
    pub fn build(
        profile: &Profile,
        lock: &Lock,
        configs: BTreeMap<String, BTreeMap<String, ConfigFile>>,
        description: String,
    ) -> Self {
        Self {
            mfpack: crate::mfpack::MFPACK_VERSION,
            name: profile.name.clone(),
            game: profile.game.clone(),
            description,
            targets: profile.targets.clone(),
            game_version: profile.game_version.clone(),
            loader: profile.loader.clone(),
            mods: profile
                .mods
                .iter()
                .map(|entry| BundleMod {
                    id: entry.id.clone(),
                    enabled: entry.enabled,
                    targets: entry.targets.clone(),
                    // An explicit pin wins; otherwise share what we are running.
                    version: entry
                        .pin
                        .clone()
                        .or_else(|| lock.get(&entry.id).map(|l| l.version.clone())),
                    file: entry.file.clone(),
                    prerelease: entry.prerelease,
                })
                .collect(),
            configs,
            exported_by: format!("modifile {}", env!("CARGO_PKG_VERSION")),
        }
    }

    /// Turn a bundle back into a profile.
    ///
    /// `pin_versions` keeps the exporter's exact releases — the usual choice,
    /// because a shared setup is one that was known to work together.
    pub fn to_profile(&self, name: &str, pin_versions: bool) -> Profile {
        let mut profile = Profile::new(name, &self.game);
        profile.targets = self.targets.clone();
        // Carried through rather than left to resolve: these decide which
        // build of each mod is correct, and guessing produces a profile whose
        // mods refuse to load together.
        profile.game_version = self.game_version.clone();
        profile.loader = self.loader.clone();
        profile.mods = self
            .mods
            .iter()
            .map(|m| ModEntry {
                id: m.id.clone(),
                enabled: m.enabled,
                targets: m.targets.clone(),
                pin: if pin_versions { m.version.clone() } else { None },
                // The exact file goes with the pin: asking for the newest
                // release means asking to be let off the sender's exact one.
                file: if pin_versions { m.file.clone() } else { None },
                prerelease: m.prerelease,
                // A bundle carries no files, so a mod the exporter supplied by
                // hand cannot be reconstructed from it — the importer has to
                // provide their own copy.
                manual: false,
            })
            .collect();
        profile
    }

    /// Write the bundle's configs into a profile's state directories.
    pub fn write_configs(
        &self,
        profiles_dir: &Path,
        id: &crate::profile::ProfileId,
    ) -> Result<usize> {
        let mut written = 0;
        for (target, files) in &self.configs {
            let base = crate::state::profile_state_dir(profiles_dir, id, target);
            for (rel, file) in files {
                let Some(safe) = safe_relative(rel) else {
                    continue;
                };
                let dest = base.join(safe);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                write_atomic(&dest, &file.bytes()?)?;
                written += 1;
            }
        }
        Ok(written)
    }

    pub fn mod_count(&self) -> usize {
        self.mods.iter().filter(|m| m.enabled).count()
    }

    pub fn config_count(&self) -> usize {
        self.configs.values().map(|files| files.len()).sum()
    }
}

/// A bundle is a file from someone else, so its paths get the same treatment
/// as an archive's: no absolute paths, no `..`, no drive letters.
pub(crate) fn safe_relative(name: &str) -> Option<std::path::PathBuf> {
    let normalized = name.replace('\\', "/");
    let mut out = std::path::PathBuf::new();
    for component in normalized.split('/') {
        match component {
            "" | "." => continue,
            ".." => return None,
            c if c.contains(':') => return None,
            c => out.push(c),
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        let cases: [&[u8]; 6] = [
            b"".as_slice(),
            b"a",
            b"ab",
            b"abc",
            b"abcd",
            &[0u8, 255, 128, 1, 2, 3],
        ];
        for case in cases {
            let encoded = base64_encode(case);
            assert_eq!(base64_decode(&encoded).unwrap(), case, "case {case:?}");
        }
    }

    #[test]
    fn bundle_paths_cannot_escape() {
        assert!(safe_relative("../../etc/passwd").is_none());
        assert!(safe_relative("C:/Windows/evil.dll").is_none());
        assert!(safe_relative("config/valheim_plus.cfg").is_some());
    }

    /// The point of carrying configs at all: a tuned profile has to arrive
    /// tuned. Text stays text so a bundle is still readable and diffable, and
    /// a config that is not valid UTF-8 survives rather than being mangled
    /// into replacement characters.
    #[test]
    fn configs_survive_a_round_trip() {
        let dir = std::env::temp_dir().join(format!("modifile-share-{}", crate::paths::now_millis()));
        let state = dir.join("state");
        std::fs::create_dir_all(&state).expect("scratch");

        let text = state.join("valheim_plus.cfg");
        std::fs::write(&text, "[Server]\nenabled = true\n").expect("write text");
        let binary = state.join("cache.dat");
        std::fs::write(&binary, [0xff, 0xfe, 0x00, 0x80]).expect("write binary");

        let mut files = BTreeMap::new();
        files.insert(
            "config/valheim_plus.cfg".to_string(),
            ConfigFile::read(&text).expect("read text"),
        );
        files.insert(
            "config/cache.dat".to_string(),
            ConfigFile::read(&binary).expect("read binary"),
        );
        assert!(matches!(
            files["config/valheim_plus.cfg"],
            ConfigFile::Text { .. }
        ));
        assert!(matches!(
            files["config/cache.dat"],
            ConfigFile::Binary { .. }
        ));

        let mut configs = BTreeMap::new();
        configs.insert("client".to_string(), files);
        let bundle = Bundle {
            mfpack: crate::mfpack::MFPACK_VERSION,
            name: "raiding".into(),
            game: "valheim".into(),
            description: String::new(),
            targets: vec!["client".into()],
            game_version: None,
            loader: None,
            mods: Vec::new(),
            configs,
            exported_by: String::new(),
        };
        assert_eq!(bundle.config_count(), 2);

        // Through the file, as it would actually travel.
        let path = dir.join("raiding.mfpack");
        crate::mfpack::write(&path, &bundle).expect("write pack");
        let reopened = crate::mfpack::read(&path).expect("read pack");

        let profiles = dir.join("profiles");
        let id = crate::profile::ProfileId::new("valheim", "theirs");
        assert_eq!(reopened.write_configs(&profiles, &id).expect("write"), 2);

        let landed = crate::state::profile_state_dir(&profiles, &id, "client");
        assert_eq!(
            std::fs::read_to_string(landed.join("config/valheim_plus.cfg")).expect("text back"),
            "[Server]\nenabled = true\n"
        );
        assert_eq!(
            std::fs::read(landed.join("config/cache.dat")).expect("binary back"),
            vec![0xff, 0xfe, 0x00, 0x80]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn importing_pins_the_exporters_versions() {
        let bundle = Bundle {
            mfpack: crate::mfpack::MFPACK_VERSION,
            name: "raiding".into(),
            game: "valheim".into(),
            description: String::new(),
            targets: vec!["server".into()],
            game_version: Some("0.217.46".into()),
            loader: None,
            mods: vec![BundleMod {
                id: ModId::github("Grantapher", "ValheimPlus"),
                enabled: true,
                targets: None,
                version: Some("0.9.9.15".into()),
                file: None,
                prerelease: false,
            }],
            configs: BTreeMap::new(),
            exported_by: String::new(),
        };

        let pinned = bundle.to_profile("mine", true);
        assert_eq!(pinned.mods[0].pin.as_deref(), Some("0.9.9.15"));
        assert_eq!(pinned.game, "valheim");
        assert_eq!(pinned.name, "mine");

        let latest = bundle.to_profile("mine", false);
        assert!(latest.mods[0].pin.is_none());
    }

}
