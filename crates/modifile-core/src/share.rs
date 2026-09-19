//! Shareable profiles.
//!
//! A bundle is one JSON file you can put in Discord. It holds the mod list, the
//! exact versions you are running, and (optionally) your tuned config files.
//!
//! What it deliberately does **not** hold is the mods themselves, or their
//! hashes presented as trustworthy. Your friend's copy resolves every mod from
//! GitHub on their own machine, verifies the download against GitHub's own
//! published digest, and runs the trust ladder locally. So a bundle from a
//! stranger can waste your time, but it cannot hand you a binary that nobody
//! else can see.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Context, Error, Result};
use crate::paths::write_atomic;
use crate::profile::{Lock, ModEntry, Profile};
use crate::source::ModId;

pub const BUNDLE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    /// Format version, so a future field cannot silently corrupt an old reader.
    pub modifile_profile: u32,
    pub name: String,
    pub game: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub targets: Vec<String>,
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

    fn bytes(&self) -> Result<Vec<u8>> {
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
            modifile_profile: BUNDLE_VERSION,
            name: profile.name.clone(),
            game: profile.game.clone(),
            description,
            targets: profile.targets.clone(),
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
                    prerelease: entry.prerelease,
                })
                .collect(),
            configs,
            exported_by: format!("modifile {}", env!("CARGO_PKG_VERSION")),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read(path).ctx(format!("reading {}", path.display()))?;
        let bundle: Bundle = serde_json::from_slice(&raw)
            .map_err(|e| Error::other(format!("{} is not a Modifile profile: {e}", path.display())))?;
        if bundle.modifile_profile > BUNDLE_VERSION {
            return Err(Error::other(format!(
                "this profile was made by a newer Modifile (format {}, this build understands {BUNDLE_VERSION})",
                bundle.modifile_profile
            )));
        }
        Ok(bundle)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    /// Turn a bundle back into a profile.
    ///
    /// `pin_versions` keeps the exporter's exact releases — the usual choice,
    /// because a shared setup is one that was known to work together.
    pub fn to_profile(&self, name: &str, pin_versions: bool) -> Profile {
        let mut profile = Profile::new(name, &self.game);
        profile.targets = self.targets.clone();
        profile.mods = self
            .mods
            .iter()
            .map(|m| ModEntry {
                id: m.id.clone(),
                enabled: m.enabled,
                targets: m.targets.clone(),
                pin: if pin_versions { m.version.clone() } else { None },
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
    pub fn write_configs(&self, profiles_dir: &Path, name: &str) -> Result<usize> {
        let mut written = 0;
        for (target, files) in &self.configs {
            let base = crate::state::profile_state_dir(profiles_dir, name, target);
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
fn safe_relative(name: &str) -> Option<std::path::PathBuf> {
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

    #[test]
    fn importing_pins_the_exporters_versions() {
        let bundle = Bundle {
            modifile_profile: 1,
            name: "raiding".into(),
            game: "valheim".into(),
            description: String::new(),
            targets: vec!["server".into()],
            mods: vec![BundleMod {
                id: ModId::github("Grantapher", "ValheimPlus"),
                enabled: true,
                targets: None,
                version: Some("0.9.9.15".into()),
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

    #[test]
    fn a_newer_format_is_refused_rather_than_misread() {
        let dir = std::env::temp_dir().join(format!("modifile-bundle-{}", crate::paths::now_millis()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("future.json");
        std::fs::write(
            &path,
            br#"{"modifile_profile":99,"name":"x","game":"valheim","mods":[]}"#,
        )
        .unwrap();

        let err = Bundle::load(&path).unwrap_err().to_string();
        assert!(err.contains("newer Modifile"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
