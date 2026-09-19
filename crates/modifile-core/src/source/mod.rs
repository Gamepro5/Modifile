//! Mod sources.
//!
//! v1 ships exactly one: GitHub Releases. The trait exists so Modrinth and
//! Thunderstore can slot in later without the engine caring, but the scope
//! decision is deliberate — GitHub is keyless, public, and the artifact can be
//! tied back to readable source.

pub mod curseforge;
pub mod forge;
pub mod github;
pub mod modrinth;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    GitHub,
    /// GitLab, gitlab.com or self-hosted.
    GitLab,
    /// Gitea or Forgejo, which share an API. Codeberg is the best-known.
    Gitea,
    Modrinth,
    CurseForge,
    /// A file the user supplied themselves. The escape hatch for mods no API
    /// will hand over — CurseForge projects whose authors disabled third-party
    /// downloads, private betas, your own local build.
    Local,
}

impl SourceKind {
    pub fn prefix(self) -> &'static str {
        match self {
            SourceKind::GitHub => "github",
            SourceKind::GitLab => "gitlab",
            SourceKind::Gitea => "gitea",
            SourceKind::Modrinth => "modrinth",
            SourceKind::CurseForge => "curseforge",
            SourceKind::Local => "local",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SourceKind::GitHub => "GitHub",
            SourceKind::GitLab => "GitLab",
            SourceKind::Gitea => "Gitea",
            SourceKind::Modrinth => "Modrinth",
            SourceKind::CurseForge => "CurseForge",
            SourceKind::Local => "your file",
        }
    }

    /// A git forge: owner/repo addressing, releases with attached files, and
    /// the source right there to read.
    pub fn is_forge(self) -> bool {
        matches!(
            self,
            SourceKind::GitHub | SourceKind::GitLab | SourceKind::Gitea
        )
    }

    /// Where the forge lives when the id does not say.
    pub fn default_host(self) -> Option<&'static str> {
        match self {
            SourceKind::GitHub => Some("github.com"),
            SourceKind::GitLab => Some("gitlab.com"),
            // Gitea has no canonical instance, so the host is always explicit.
            _ => None,
        }
    }

    /// Whether using this source needs a key the user must obtain themselves.
    pub fn needs_key(self) -> bool {
        matches!(self, SourceKind::CurseForge)
    }
}

/// A mod's stable identity, e.g. `github:WeakAuras/WeakAuras2`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModId {
    pub kind: SourceKind,
    pub owner: String,
    pub repo: String,
    /// The forge instance, for anything self-hosted. `None` means the kind's
    /// default host — github.com, gitlab.com.
    pub host: Option<String>,
}

impl ModId {
    pub fn github(owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            kind: SourceKind::GitHub,
            owner: owner.into(),
            repo: repo.into(),
            host: None,
        }
    }

    /// A repository on any git forge.
    pub fn forge(
        kind: SourceKind,
        host: Option<String>,
        owner: impl Into<String>,
        repo: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            owner: owner.into(),
            repo: repo.into(),
            host: host.filter(|h| Some(h.as_str()) != kind.default_host()),
        }
    }

    /// Modrinth and CurseForge address a project by one identifier, so `owner`
    /// stays empty for them.
    pub fn project(kind: SourceKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            owner: String::new(),
            repo: id.into(),
            host: None,
        }
    }

    /// The forge instance this lives on.
    pub fn host(&self) -> Option<&str> {
        self.host
            .as_deref()
            .or_else(|| self.kind.default_host())
    }

    /// Short name for display: just the project.
    pub fn short(&self) -> &str {
        &self.repo
    }

    /// How to show it in a list: `owner/repo` for a forge, the project id alone
    /// for sources that have no owner component. A self-hosted instance keeps
    /// its host so two same-named repos are never confused.
    pub fn display(&self) -> String {
        let body = if self.owner.is_empty() {
            self.repo.clone()
        } else {
            format!("{}/{}", self.owner, self.repo)
        };
        match &self.host {
            Some(host) => format!("{host}/{body}"),
            None => body,
        }
    }

    pub fn web_url(&self) -> String {
        match self.kind {
            SourceKind::GitHub | SourceKind::GitLab | SourceKind::Gitea => format!(
                "https://{}/{}/{}",
                self.host().unwrap_or("github.com"),
                self.owner,
                self.repo
            ),
            SourceKind::Modrinth => format!("https://modrinth.com/project/{}", self.repo),
            SourceKind::CurseForge => {
                format!("https://www.curseforge.com/projects/{}", self.repo)
            }
            SourceKind::Local => String::new(),
        }
    }
}

/// `owner/repo` or `host/owner/repo`. A leading segment containing a dot is a
/// hostname, which is how self-hosted instances are addressed.
fn parse_forge(kind: SourceKind, body: &str, original: &str) -> Result<ModId> {
    let body = body.trim_matches('/').trim_end_matches(".git");
    let parts: Vec<&str> = body.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [owner, repo] => Ok(ModId::forge(kind, None, *owner, *repo)),
        [host, rest @ ..] if host.contains('.') && rest.len() >= 2 => Ok(ModId::forge(
            kind,
            Some(host.to_string()),
            rest[rest.len() - 2],
            rest[rest.len() - 1],
        )),
        _ => Err(Error::other(format!(
            "`{original}` is not a {} id — expected `owner/repo` or `host/owner/repo`",
            kind.label()
        ))),
    }
}

impl FromStr for ModId {
    type Err = Error;

    /// Accepts a prefixed id (`github:owner/repo`, `modrinth:sodium`,
    /// `curseforge:238222`), a bare `owner/repo` for GitHub, or a page URL
    /// copied from any of the three sites.
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim().trim_end_matches('/');
        let bare = s
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("www.");

        // --- URLs ---------------------------------------------------------
        if let Some(rest) = bare.strip_prefix("github.com/") {
            let rest = rest.trim_end_matches(".git");
            let mut parts = rest.split('/').filter(|p| !p.is_empty());
            if let (Some(owner), Some(repo)) = (parts.next(), parts.next()) {
                return Ok(ModId::github(owner, repo));
            }
        }
        // Any other forge URL: the host tells us which kind it is, and
        // owner/repo follow. GitLab subgroups collapse to the last two parts.
        for (domain, kind) in [
            ("gitlab.com/", SourceKind::GitLab),
            ("codeberg.org/", SourceKind::Gitea),
        ] {
            if let Some(rest) = bare.strip_prefix(domain) {
                let rest = rest.trim_end_matches(".git");
                let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
                if parts.len() >= 2 {
                    return Ok(ModId::forge(
                        kind,
                        Some(domain.trim_end_matches('/').to_string()),
                        parts[parts.len() - 2],
                        parts[parts.len() - 1],
                    ));
                }
            }
        }
        if let Some(rest) = bare.strip_prefix("modrinth.com/") {
            // modrinth.com/mod/<slug>, /plugin/<slug>, /project/<slug>, ...
            let mut parts = rest.split('/').filter(|p| !p.is_empty());
            let (_kind, slug) = (parts.next(), parts.next());
            if let Some(slug) = slug {
                return Ok(ModId::project(SourceKind::Modrinth, slug));
            }
        }
        if let Some(rest) = bare.strip_prefix("curseforge.com/") {
            // curseforge.com/minecraft/mc-mods/<slug>
            if let Some(slug) = rest.split('/').filter(|p| !p.is_empty()).next_back() {
                return Ok(ModId::project(SourceKind::CurseForge, slug));
            }
        }

        // --- prefixed ids -------------------------------------------------
        if let Some((prefix, rest)) = s.split_once(':') {
            let rest = rest.trim_matches('/');
            return match prefix.to_ascii_lowercase().as_str() {
                "github" | "gh" => {
                    let rest = rest.trim_end_matches(".git");
                    let mut parts = rest.split('/').filter(|p| !p.is_empty());
                    match (parts.next(), parts.next()) {
                        (Some(owner), Some(repo)) => Ok(ModId::github(owner, repo)),
                        _ => Err(Error::other(format!(
                            "`{s}` is not a GitHub id — expected `github:owner/repo`"
                        ))),
                    }
                }
                "gitlab" | "gl" => parse_forge(SourceKind::GitLab, rest, s),
                "gitea" | "forgejo" | "codeberg" => {
                    let rest = if prefix.eq_ignore_ascii_case("codeberg") {
                        format!("codeberg.org/{rest}")
                    } else {
                        rest.to_string()
                    };
                    parse_forge(SourceKind::Gitea, &rest, s)
                }
                "local" => Ok(ModId::project(SourceKind::Local, rest)),
                "modrinth" | "mr" => Ok(ModId::project(SourceKind::Modrinth, rest)),
                "curseforge" | "cf" => Ok(ModId::project(SourceKind::CurseForge, rest)),
                other => Err(Error::other(format!(
                    "unknown mod source `{other}` — supported: github, gitlab, gitea, \
                     codeberg, modrinth, curseforge"
                ))),
            };
        }

        // --- bare owner/repo defaults to GitHub ---------------------------
        let mut parts = s.split('/').filter(|p| !p.is_empty());
        let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
            return Err(Error::other(format!(
                "`{s}` is not a mod id — paste a GitHub, Modrinth or CurseForge link, \
                 or use `owner/repo`"
            )));
        };
        Ok(ModId::github(owner, repo.trim_end_matches(".git")))
    }
}

impl fmt::Display for ModId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Sources without an owner component must not gain a stray slash — the
        // result round-trips back through `FromStr`.
        write!(f, "{}:{}", self.kind.prefix(), self.display())
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
    /// Upstream-reported SHA-256, when the source provides one. GitHub does.
    #[serde(default)]
    pub digest: Option<String>,
    /// Modrinth publishes SHA-512 instead, so integrity can still be checked
    /// against the source's own record rather than only against ourselves.
    #[serde(default)]
    pub sha512: Option<String>,
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

/// One result from searching for a mod by name, whichever index answered.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: ModId,
    pub title: String,
    pub description: String,
    /// A popularity signal, but not the same unit everywhere — see `popularity`.
    pub stars: u64,
    pub license: Option<String>,
    /// Where the code lives. The reason search exists: it is how you find the
    /// repository for a mod you only know from a storefront page.
    pub source_url: Option<String>,
    /// `Some(false)` means we checked and it publishes nothing installable.
    pub installable: Option<bool>,
}

impl SearchHit {
    /// The popularity figure with its unit, because a forge counts stars and a
    /// storefront counts downloads — printing "200618606 stars" is nonsense.
    pub fn popularity(&self) -> String {
        let n = self.stars;
        let rounded = if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            n.to_string()
        };
        if self.id.kind.is_forge() {
            format!("{rounded} stars")
        } else {
            format!("{rounded} downloads")
        }
    }

    /// Something readable for a list. CurseForge ids are bare numbers, so the
    /// project's own name is the only useful label.
    pub fn label(&self) -> String {
        if self.title.trim().is_empty() {
            self.id.display()
        } else {
            self.title.clone()
        }
    }
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
    /// Where the source says the code lives. Modrinth publishes this, which is
    /// how a mod whose GitHub link is not obvious can still be found.
    #[serde(default)]
    pub source_url: Option<String>,
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
    fn parses_modrinth_ids_and_urls() {
        let expected = ModId::project(SourceKind::Modrinth, "sodium");
        for input in [
            "modrinth:sodium",
            "mr:sodium",
            "https://modrinth.com/mod/sodium",
            "https://modrinth.com/plugin/sodium",
            "modrinth.com/project/sodium/",
        ] {
            assert_eq!(input.parse::<ModId>().unwrap(), expected, "input: {input}");
        }
    }

    #[test]
    fn parses_curseforge_ids_and_urls() {
        assert_eq!(
            "curseforge:238222".parse::<ModId>().unwrap(),
            ModId::project(SourceKind::CurseForge, "238222")
        );
        assert_eq!(
            "https://www.curseforge.com/minecraft/mc-mods/jei"
                .parse::<ModId>()
                .unwrap(),
            ModId::project(SourceKind::CurseForge, "jei")
        );
    }

    #[test]
    fn parses_other_git_forges() {
        assert_eq!(
            "gitlab:group/project".parse::<ModId>().unwrap(),
            ModId::forge(SourceKind::GitLab, None, "group", "project")
        );
        assert_eq!(
            "https://codeberg.org/owner/repo".parse::<ModId>().unwrap(),
            ModId::forge(SourceKind::Gitea, Some("codeberg.org".into()), "owner", "repo")
        );
        assert_eq!(
            "https://gitlab.com/group/project".parse::<ModId>().unwrap(),
            ModId::forge(SourceKind::GitLab, None, "group", "project")
        );
    }

    #[test]
    fn self_hosted_instances_keep_their_host() {
        let id = "gitea:git.example.com/owner/repo".parse::<ModId>().unwrap();
        assert_eq!(id.host(), Some("git.example.com"));
        assert_eq!(id.web_url(), "https://git.example.com/owner/repo");
        // And it round-trips, so a lockfile can store it.
        assert_eq!(id.to_string().parse::<ModId>().unwrap(), id);
    }

    #[test]
    fn a_default_host_is_not_written_into_the_id() {
        // gitlab.com is the default, so it should not clutter every id.
        let id = "https://gitlab.com/a/b".parse::<ModId>().unwrap();
        assert!(id.host.is_none());
        assert_eq!(id.to_string(), "gitlab:a/b");
    }

    #[test]
    fn a_bare_owner_repo_still_means_github() {
        assert_eq!(
            "sodium/sodium".parse::<ModId>().unwrap().kind,
            SourceKind::GitHub
        );
    }

    #[test]
    fn rejects_unknown_sources_by_name() {
        let err = "nexus:12345".parse::<ModId>().unwrap_err().to_string();
        assert!(err.contains("nexus"), "{err}");
        assert!(err.contains("modrinth"), "should list what is supported: {err}");
    }

    #[test]
    fn display_drops_the_empty_owner() {
        assert_eq!(ModId::project(SourceKind::Modrinth, "sodium").display(), "sodium");
        assert_eq!(ModId::github("a", "b").display(), "a/b");
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
