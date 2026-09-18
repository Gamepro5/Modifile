//! Mod sources.
//!
//! v1 ships exactly one: GitHub Releases. The trait exists so Modrinth and
//! Thunderstore can slot in later without the engine caring, but the scope
//! decision is deliberate — GitHub is keyless, public, and the artifact can be
//! tied back to readable source.

pub mod github;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    GitHub,
}

impl SourceKind {
    pub fn prefix(self) -> &'static str {
        match self {
            SourceKind::GitHub => "github",
        }
    }
}

/// A mod's stable identity, e.g. `github:WeakAuras/WeakAuras2`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModId {
    pub kind: SourceKind,
    pub owner: String,
    pub repo: String,
}

impl ModId {
    pub fn github(owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            kind: SourceKind::GitHub,
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    /// Short name for display: just the repo.
    pub fn short(&self) -> &str {
        &self.repo
    }

    pub fn web_url(&self) -> String {
        match self.kind {
            SourceKind::GitHub => format!("https://github.com/{}/{}", self.owner, self.repo),
        }
    }
}

impl FromStr for ModId {
    type Err = Error;

    /// Accepts `github:owner/repo`, a bare `owner/repo`, or a full GitHub URL.
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let body = if let Some(rest) = s.strip_prefix("github:") {
            rest
        } else if let Some(rest) = s
            .strip_prefix("https://github.com/")
            .or_else(|| s.strip_prefix("http://github.com/"))
            .or_else(|| s.strip_prefix("github.com/"))
        {
            rest
        } else if s.contains(':') {
            let (prefix, _) = s.split_once(':').unwrap();
            return Err(Error::other(format!(
                "unknown mod source `{prefix}` — this build only supports `github:`"
            )));
        } else {
            s
        };

        let body = body.trim_end_matches('/').trim_end_matches(".git");
        let mut parts = body.split('/').filter(|p| !p.is_empty());
        let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
            return Err(Error::other(format!(
                "`{s}` is not a mod id — expected `owner/repo`"
            )));
        };
        Ok(ModId::github(owner, repo))
    }
}

impl fmt::Display for ModId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}/{}", self.kind.prefix(), self.owner, self.repo)
    }
}

impl TryFrom<String> for ModId {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

impl From<ModId> for String {
    fn from(value: ModId) -> Self {
        value.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub download_url: String,
    pub size: u64,
    /// Upstream-reported SHA-256, when the source provides one.
    #[serde(default)]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub tag: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub published_at: String,
    #[serde(default)]
    pub prerelease: bool,
    pub assets: Vec<Asset>,
    #[serde(default)]
    pub web_url: String,
}

/// Repository facts used for the trust ladder, not for installing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoInfo {
    #[serde(default)]
    pub description: String,
    /// SPDX id, e.g. `MIT`. `None` means no detected license, which is not the
    /// same as open source.
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub stars: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_spelling() {
        let expected = ModId::github("WeakAuras", "WeakAuras2");
        for input in [
            "github:WeakAuras/WeakAuras2",
            "WeakAuras/WeakAuras2",
            "https://github.com/WeakAuras/WeakAuras2",
            "https://github.com/WeakAuras/WeakAuras2/",
            "github.com/WeakAuras/WeakAuras2.git",
        ] {
            assert_eq!(input.parse::<ModId>().unwrap(), expected, "input: {input}");
        }
    }

    #[test]
    fn rejects_other_sources_by_name() {
        let err = "curseforge:12345".parse::<ModId>().unwrap_err().to_string();
        assert!(err.contains("curseforge"), "{err}");
    }

    #[test]
    fn rejects_incomplete_ids() {
        assert!("WeakAuras".parse::<ModId>().is_err());
    }

    #[test]
    fn round_trips_through_string() {
        let id = ModId::github("a", "b");
        assert_eq!(String::from(id.clone()).parse::<ModId>().unwrap(), id);
    }
}
