use std::path::{Path, PathBuf};

use crate::error::{Context, Error, Result};

/// Every directory the loader owns. Nothing is ever written inside a game
/// directory except the deployed mod files themselves — state lives here, so a
/// game update or a Steam "verify files" can never eat our bookkeeping.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Root of all persistent state.
    pub home: PathBuf,
    /// Content-addressed mod store. Shared by every profile; this is why
    /// profiles cost ~0 bytes.
    pub store: PathBuf,
    /// Game pack definitions (`*.toml`).
    pub packs: PathBuf,
    /// Profile definitions and their lockfiles.
    pub profiles: PathBuf,
    /// Deployment manifests, keyed by game/target.
    pub state: PathBuf,
    /// HTTP conditional-request cache and in-flight downloads.
    pub cache: PathBuf,
}

impl Paths {
    /// `MODIFILE_HOME` wins, then the platform data dir.
    pub fn discover() -> Result<Self> {
        let home = match std::env::var_os("MODIFILE_HOME") {
            Some(v) => PathBuf::from(v),
            None => directories::ProjectDirs::from("", "", "modifile")
                .ok_or_else(|| Error::other("cannot determine a data directory for this platform"))?
                .data_dir()
                .to_path_buf(),
        };
        Ok(Self::rooted(home))
    }

    pub fn rooted(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            store: home.join("store"),
            packs: home.join("packs"),
            profiles: home.join("profiles"),
            state: home.join("state"),
            cache: home.join("cache"),
            home,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [
            &self.home,
            &self.store,
            &self.packs,
            &self.profiles,
            &self.state,
            &self.cache,
        ] {
            std::fs::create_dir_all(dir).ctx(format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn profile_file(&self, name: &str) -> PathBuf {
        self.profiles.join(format!("{name}.toml"))
    }

    pub fn lock_file(&self, name: &str) -> PathBuf {
        self.profiles.join(format!("{name}.lock.json"))
    }

    /// Deployment manifest for one (game, target) pair. Deploying a different
    /// profile to the same target replaces this file.
    pub fn manifest_file(&self, game: &str, target: &str) -> PathBuf {
        self.state.join(game).join(format!("{target}.manifest.json"))
    }

    /// Remembered game directories, shared by every profile.
    pub fn roots_file(&self) -> PathBuf {
        self.home.join("roots.json")
    }

    /// Where the GitHub token is kept.
    pub fn token_file(&self) -> PathBuf {
        self.home.join("token")
    }

    pub fn http_cache(&self) -> PathBuf {
        self.cache.join("http")
    }

    pub fn downloads(&self) -> PathBuf {
        self.cache.join("downloads")
    }
}

/// Write via a temp file in the same directory, then rename. A killed process
/// leaves either the old file or the new one, never a truncated one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "tmp{}",
        std::process::id() as u64 ^ now_millis()
    ));
    std::fs::write(&tmp, bytes).ctx(format!("writing {}", tmp.display()))?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(Error::Io(e)).ctx(format!("replacing {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn unc_shares_are_remote() {
        // A game folder on another machine: we cannot see its process list, so
        // the running-game guard must not silently pass.
        assert!(is_network_path(Path::new(
            r"\\Gamepro5-Server\shared\Desktop\ValheimServer\server"
        )));
        assert!(is_network_path(Path::new(r"\\?\UNC\server\share\dir")));
    }

    #[test]
    #[cfg(windows)]
    fn local_drives_are_not_remote() {
        assert!(!is_network_path(Path::new(r"C:\Program Files\Valheim")));
        assert!(!is_network_path(Path::new(r"F:\SteamLibrary")));
        assert!(!is_network_path(Path::new(r"\\?\C:\Games")));
    }

    #[test]
    fn env_expansion_needs_every_variable() {
        std::env::set_var("MODIFILE_TEST_VAR", "ok");
        assert_eq!(
            expand_env("a/${MODIFILE_TEST_VAR}/b").as_deref(),
            Some("a/ok/b")
        );
        assert!(expand_env("a/${MODIFILE_NOT_SET_XYZZY}/b").is_none());
    }
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Is this path on another machine?
///
/// It matters because everything we know about whether a game is running comes
/// from the local process list. A game directory on a file share belongs to a
/// machine whose processes we cannot see, so the running-game guard cannot
/// speak for it and must say so rather than silently passing.
pub fn is_network_path(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if let Some(Component::Prefix(prefix)) = path.components().next() {
            return matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..));
        }
        // A mapped drive letter is also remote, but resolving that needs
        // GetDriveType; UNC covers the common case and the check is advisory.
        false
    }
    #[cfg(not(windows))]
    {
        // Find the longest mount point containing this path and look at its type.
        let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
            return false;
        };
        let mut best: Option<(usize, bool)> = None;
        for line in mounts.lines() {
            let mut fields = line.split_whitespace();
            let (Some(_device), Some(point), Some(fstype)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if !path.starts_with(point) {
                continue;
            }
            let remote = matches!(
                fstype,
                "cifs" | "smbfs" | "smb3" | "nfs" | "nfs4" | "fuse.sshfs" | "afs" | "9p"
            );
            if best.map(|(len, _)| point.len() > len).unwrap_or(true) {
                best = Some((point.len(), remote));
            }
        }
        best.map(|(_, remote)| remote).unwrap_or(false)
    }
}

/// Expand `${VAR}` against the environment. Unset variables make the whole
/// expansion fail so we never probe a path like `/steamapps/common/Valheim`.
pub fn expand_env(input: &str) -> Option<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}')?;
        let var = &after[..end];
        out.push_str(&std::env::var(var).ok()?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}
