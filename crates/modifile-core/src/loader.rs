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

/// What a game was *built* for, which is not the same as what you are running.
///
/// A Windows game under Proton needs the Windows loader — `winhttp.dll` is
/// useless to a native Linux build and essential to a Proton one. And a Linux
/// dedicated server needs the Linux loader even when Modifile is driving it
/// from a Windows desktop over a file share. So the game's own files decide,
/// never the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GamePlatform {
    Windows,
    Linux,
    MacOs,
}

impl GamePlatform {
    pub fn label(self) -> &'static str {
        match self {
            GamePlatform::Windows => "Windows",
            GamePlatform::Linux => "Linux",
            GamePlatform::MacOs => "macOS",
        }
    }

    /// The machine we are running on, as a last resort.
    pub fn host() -> Self {
        if cfg!(windows) {
            GamePlatform::Windows
        } else if cfg!(target_os = "macos") {
            GamePlatform::MacOs
        } else {
            GamePlatform::Linux
        }
    }
}

/// Work out which build of a game is in this directory.
///
/// Returns `None` when the directory says nothing useful, so the caller can
/// fall back rather than guess wrongly.
pub fn detect_game_platform(root: &Path) -> Option<GamePlatform> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return None;
    };

    let mut windows = false;
    let mut linux = false;
    let mut macos = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".exe") {
            windows = true;
        } else if name.ends_with(".x86_64") || name.ends_with(".x86") || name.ends_with(".so") {
            linux = true;
        } else if name.ends_with(".app") {
            macos = true;
        }
    }

    // A Windows executable is the strongest signal: a Unity game shipping
    // `game.exe` needs the Windows loader whether it runs natively or through
    // Proton. Some installs carry both, and the .exe is what actually launches.
    match (windows, linux, macos) {
        (true, _, _) => Some(GamePlatform::Windows),
        (false, true, _) => Some(GamePlatform::Linux),
        (false, false, true) => Some(GamePlatform::MacOs),
        _ => None,
    }
}

/// Does this game root already have the loader's files?
pub fn markers_present(root: &Path, markers: &[String]) -> bool {
    !markers.is_empty() && markers.iter().any(|m| root.join(m).exists())
}

/// What is installed right now, without touching the network.
pub fn detect(
    kind: LoaderKind,
    prefix: &str,
    page: &str,
    markers: &[String],
    // Where the loader's files live: the game root for most, a subfolder for a
    // loader that declares `into`.
    install_dir: &Path,
    mc_dir: &Path,
    game_version: Option<&str>,
) -> LoaderState {
    if kind == LoaderKind::Installer {
        return LoaderState::Manual {
            page: page.to_string(),
        };
    }
    if kind == LoaderKind::Archive {
        // An archive loader is either laid over the game or it is not; it has
        // no per-game-version identity to be wrong about.
        return if markers_present(install_dir, markers) {
            LoaderState::Installed {
                version: "installed".to_string(),
            }
        } else {
            LoaderState::NotInstalled
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
            detect(LoaderKind::FabricMeta, "fabric", "", &[], &dir, &dir, Some("1.21.1")),
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
            detect(LoaderKind::FabricMeta, "fabric", "", &[], &dir, &dir, Some("1.21.1")),
            LoaderState::WrongVersion {
                version: "0.15.0 for 1.20.1".into()
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_windows_build_needs_the_windows_loader_even_on_linux() {
        // Proton. `winhttp.dll` is useless to a native Linux build and
        // essential to a Windows one, whatever the host happens to be.
        let dir = temp("proton");
        std::fs::write(dir.join("valheim.exe"), b"").unwrap();
        assert_eq!(detect_game_platform(&dir), Some(GamePlatform::Windows));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_linux_server_needs_the_linux_loader_even_from_windows() {
        // The real case: a Linux dedicated server managed over a file share
        // from a Windows desktop. Host-based selection would fetch the wrong
        // build and the server would silently load nothing.
        let dir = temp("linux-server");
        std::fs::write(dir.join("valheim_server.x86_64"), b"").unwrap();
        std::fs::write(dir.join("UnityPlayer.so"), b"").unwrap();
        assert_eq!(detect_game_platform(&dir), Some(GamePlatform::Linux));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_install_carrying_both_is_treated_as_windows() {
        // Some directories hold both; the .exe is what actually launches.
        let dir = temp("both");
        std::fs::write(dir.join("valheim.exe"), b"").unwrap();
        std::fs::write(dir.join("valheim.x86_64"), b"").unwrap();
        assert_eq!(detect_game_platform(&dir), Some(GamePlatform::Windows));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_directory_says_nothing_rather_than_guessing() {
        assert_eq!(detect_game_platform(Path::new("/no/such/place")), None);
    }

    #[test]
    fn reports_nothing_when_nothing_is_installed() {
        let dir = temp("none");
        assert_eq!(
            detect(LoaderKind::FabricMeta, "fabric", "", &[], &dir, &dir, Some("1.21.1")),
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
                &[],
                &dir,
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
