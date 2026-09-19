//! Mod loaders.
//!
//! A mod loader is not a mod: it is the thing that makes mods load at all, it
//! is installed into the game rather than alongside it, and it is tied to one
//! game version. Modifile treats it as its own concept so the whole job —
//! "install Fabric for 1.21.1, then my mods" — happens in one place instead of
//! sending you to a separate installer first.
//!
//! Fabric and Quilt publish a metadata service that returns a finished version
//! profile as JSON. Writing that into the game's `versions/` folder is the
//! entire installation: no installer, no Java, nothing to run. That is what
//! their own installers do underneath.
//!
//! Forge and NeoForge are not like this. Their installers patch the game and
//! must actually execute, so Modifile links to them rather than pretending.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Context, Error, Result};
use crate::http::Http;
use crate::pack::LoaderKind;
use crate::paths::write_atomic;

/// What is installed for one loader, for one game version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoaderState {
    /// Installed and matching the version the profile wants.
    Installed { version: String },
    /// Installed, but built for a different game version.
    WrongVersion { version: String },
    NotInstalled,
    /// We cannot install this one; the user runs its installer.
    Manual { page: String },
}

#[derive(Debug, Deserialize)]
struct MetaLoaderEntry {
    loader: MetaLoader,
}

#[derive(Debug, Deserialize)]
struct MetaLoader {
    version: String,
    #[serde(default)]
    stable: bool,
}

/// The `versions/` entry id both Fabric and Quilt use.
fn version_id(prefix: &str, loader: &str, game_version: &str) -> String {
    format!("{prefix}-loader-{loader}-{game_version}")
}

// --- launcher_profiles.json ----------------------------------------------
// The vanilla launcher keeps its profile list here. Adding an entry is what
// makes the installed loader selectable in the launcher's dropdown.

#[derive(Debug, Default, Serialize, Deserialize)]
struct LauncherProfiles {
    #[serde(default)]
    profiles: serde_json::Map<String, serde_json::Value>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

fn add_launcher_profile(mc_dir: &Path, id: &str, name: &str) -> Result<()> {
    let path = mc_dir.join("launcher_profiles.json");
    // A dedicated-server directory has no launcher; that is not an error.
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read(&path).ctx(format!("reading {}", path.display()))?;
    let mut file: LauncherProfiles = serde_json::from_slice(&raw).unwrap_or_default();

    let entry = serde_json::json!({
        "name": name,
        "type": "custom",
        "lastVersionId": id,
        "icon": "Furnace",
    });
    file.profiles.insert(id.to_string(), entry);

    write_atomic(&path, &serde_json::to_vec_pretty(&file)?)
}

/// Install Fabric or Quilt by writing the version profile their metadata
/// service hands out.
pub async fn install_meta_loader(
    http: &Http,
    meta_base: &str,
    prefix: &str,
    display_name: &str,
    mc_dir: &Path,
    game_version: &str,
) -> Result<String> {
    let url = format!("{meta_base}/versions/loader/{game_version}");
    let entries: Vec<MetaLoaderEntry> = http
        .get_json(&url)
        .await?
        .ok_or_else(|| Error::NotFound(format!("{display_name} builds for {game_version}")))?;

    // Prefer a stable loader; the list is newest first either way.
    let loader = entries
        .iter()
        .find(|e| e.loader.stable)
        .or_else(|| entries.first())
        .map(|e| e.loader.version.clone())
        .ok_or_else(|| {
            Error::NotFound(format!("a {display_name} build for Minecraft {game_version}"))
        })?;

    let profile_url = format!("{meta_base}/versions/loader/{game_version}/{loader}/profile/json");
    let profile: serde_json::Value = http
        .get_json(&profile_url)
        .await?
        .ok_or_else(|| Error::other(format!("{display_name} returned no profile to install")))?;

    let id = version_id(prefix, &loader, game_version);
    let dir = mc_dir.join("versions").join(&id);
    std::fs::create_dir_all(&dir).ctx(format!("creating {}", dir.display()))?;
    write_atomic(&dir.join(format!("{id}.json")), &serde_json::to_vec_pretty(&profile)?)?;

    add_launcher_profile(mc_dir, &id, &format!("{display_name} {game_version}"))?;
    Ok(loader)
}

/// What is installed right now, without touching the network.
pub fn detect(
    kind: LoaderKind,
    prefix: &str,
    page: &str,
    mc_dir: &Path,
    game_version: Option<&str>,
) -> LoaderState {
    if kind == LoaderKind::Installer {
        return LoaderState::Manual {
            page: page.to_string(),
        };
    }

    let versions = mc_dir.join("versions");
    let Ok(entries) = std::fs::read_dir(&versions) else {
        return LoaderState::NotInstalled;
    };

    let needle = format!("{prefix}-loader-");
    let mut wrong: Option<String> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&needle) {
            continue;
        }
        // `<prefix>-loader-<loader version>-<game version>`
        let tail = &name[needle.len()..];
        let (loader, installed_game) = match tail.rsplit_once('-') {
            Some((l, g)) => (l.to_string(), g.to_string()),
            None => continue,
        };
        match game_version {
            Some(want) if installed_game == want => {
                return LoaderState::Installed { version: loader }
            }
            Some(_) => wrong = Some(format!("{loader} for {installed_game}")),
            None => return LoaderState::Installed { version: loader },
        }
    }

    match wrong {
        Some(version) => LoaderState::WrongVersion { version },
        None => LoaderState::NotInstalled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "modifile-loader-{tag}-{}",
            crate::paths::now_millis()
        ));
        std::fs::create_dir_all(dir.join("versions")).unwrap();
        dir
    }

    #[test]
    fn detects_an_installed_loader_for_the_right_version() {
        let dir = temp("ok");
        std::fs::create_dir_all(dir.join("versions/fabric-loader-0.16.5-1.21.1")).unwrap();

        assert_eq!(
            detect(LoaderKind::FabricMeta, "fabric", "", &dir, Some("1.21.1")),
            LoaderState::Installed {
                version: "0.16.5".into()
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn spots_a_loader_built_for_a_different_game_version() {
        // The trap this catches: Fabric installed for 1.20.1 while the profile
        // is set to 1.21.1 looks "installed" if you only check for the folder.
        let dir = temp("wrong");
        std::fs::create_dir_all(dir.join("versions/fabric-loader-0.15.0-1.20.1")).unwrap();

        assert_eq!(
            detect(LoaderKind::FabricMeta, "fabric", "", &dir, Some("1.21.1")),
            LoaderState::WrongVersion {
                version: "0.15.0 for 1.20.1".into()
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_nothing_when_nothing_is_installed() {
        let dir = temp("none");
        assert_eq!(
            detect(LoaderKind::FabricMeta, "fabric", "", &dir, Some("1.21.1")),
            LoaderState::NotInstalled
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn installer_loaders_are_reported_as_manual() {
        let dir = temp("manual");
        assert_eq!(
            detect(
                LoaderKind::Installer,
                "neoforge",
                "https://neoforged.net/",
                &dir,
                Some("1.21.1")
            ),
            LoaderState::Manual {
                page: "https://neoforged.net/".into()
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_launcher_profile_is_added_without_losing_the_rest_of_the_file() {
        let dir = temp("launcher");
        std::fs::write(
            dir.join("launcher_profiles.json"),
            br#"{"profiles":{"vanilla":{"name":"Vanilla"}},"version":3,"settings":{"keepAlive":true}}"#,
        )
        .unwrap();

        add_launcher_profile(&dir, "fabric-loader-0.16.5-1.21.1", "Fabric 1.21.1").unwrap();

        let raw = std::fs::read_to_string(dir.join("launcher_profiles.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(value["profiles"]["vanilla"].is_object(), "existing profile kept");
        assert_eq!(
            value["profiles"]["fabric-loader-0.16.5-1.21.1"]["lastVersionId"],
            "fabric-loader-0.16.5-1.21.1"
        );
        // Unknown top-level keys must survive; the launcher owns this file.
        assert_eq!(value["settings"]["keepAlive"], true);
        assert_eq!(value["version"], 3);

        std::fs::remove_dir_all(&dir).ok();
    }
}
