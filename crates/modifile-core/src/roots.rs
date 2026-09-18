//! Remembered game directories.
//!
//! Autodetection covers the common cases, but a Prism instance or a game on a
//! drive Steam does not know about has to be pointed at by hand. Once. These
//! live outside any profile so every profile for that game benefits.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Context, Result};
use crate::paths::write_atomic;

/// game id -> target id -> directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalRoots {
    #[serde(default)]
    games: BTreeMap<String, BTreeMap<String, PathBuf>>,
}

impl GlobalRoots {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read(path).ctx(format!("reading {}", path.display()))?;
        Ok(serde_json::from_slice(&raw).unwrap_or_default())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn get(&self, game: &str, target: &str) -> Option<&PathBuf> {
        self.games.get(game)?.get(target)
    }

    pub fn set(&mut self, game: &str, target: &str, path: PathBuf) {
        self.games
            .entry(game.to_string())
            .or_default()
            .insert(target.to_string(), path);
    }

    pub fn clear(&mut self, game: &str, target: &str) {
        if let Some(targets) = self.games.get_mut(game) {
            targets.remove(target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_clear() {
        let mut roots = GlobalRoots::default();
        assert!(roots.get("valheim", "server").is_none());

        roots.set("valheim", "server", PathBuf::from("/srv/valheim"));
        assert_eq!(
            roots.get("valheim", "server"),
            Some(&PathBuf::from("/srv/valheim"))
        );

        // Different target of the same game is untouched.
        assert!(roots.get("valheim", "client").is_none());

        roots.clear("valheim", "server");
        assert!(roots.get("valheim", "server").is_none());
    }
}
