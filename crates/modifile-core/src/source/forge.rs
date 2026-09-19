//! GitLab, Gitea and Forgejo.
//!
//! GitHub is not the only place mods live, and for some communities it is not
//! even the main one. These forges all publish releases with attached files
//! over public, keyless APIs, so supporting them costs two small adapters and
//! removes "it has to be on GitHub" as a limitation.
//!
//! Gitea and Forgejo share an API and are handled together; Codeberg is simply
//! a well-known Forgejo instance. Self-hosted instances work by naming the host
//! in the id — `gitea:git.example.com/owner/repo`.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::http::Http;
use crate::source::{Asset, ModId, Release, RepoInfo, SourceKind};

#[derive(Clone)]
pub struct Forge {
    http: Http,
}

// --- GitLab ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GitLabRelease {
    #[serde(default)]
    name: Option<String>,
    tag_name: String,
    #[serde(default)]
    released_at: Option<String>,
    #[serde(default)]
    upcoming_release: bool,
    #[serde(default)]
    assets: GitLabAssets,
}

#[derive(Debug, Default, Deserialize)]
struct GitLabAssets {
    #[serde(default)]
    links: Vec<GitLabLink>,
    #[serde(default)]
    sources: Vec<GitLabSource>,
}

#[derive(Debug, Deserialize)]
struct GitLabLink {
    name: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct GitLabSource {
    format: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct GitLabProject {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    web_url: String,
    #[serde(default)]
    star_count: u64,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    license: Option<GitLabLicense>,
}

#[derive(Debug, Deserialize)]
struct GitLabLicense {
    #[serde(default)]
    key: Option<String>,
}

// --- Gitea / Forgejo ------------------------------------------------------
// Deliberately GitHub-shaped; the projects designed it that way.

#[derive(Debug, Deserialize)]
struct GiteaRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GiteaAsset>,
}

#[derive(Debug, Deserialize)]
struct GiteaAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Deserialize)]
struct GiteaRepo {
    #[serde(default)]
    description: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    stars_count: u64,
    #[serde(default)]
    archived: bool,
}

impl Forge {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn host(id: &ModId) -> Result<&str> {
        id.host().ok_or_else(|| {
            Error::other(format!(
                "{id} does not say which server it is on — use \
                 `{}:host/owner/repo`",
                id.kind.prefix()
            ))
        })
    }

    /// GitLab addresses projects by a URL-encoded `owner/repo` path.
    fn gitlab_path(id: &ModId) -> String {
        crate::source::modrinth::urlencode(&format!("{}/{}", id.owner, id.repo))
            .replace('+', "%20")
    }

    pub async fn releases(&self, id: &ModId) -> Result<Vec<Release>> {
        let host = Self::host(id)?;
        match id.kind {
            SourceKind::GitLab => {
                let url = format!(
                    "https://{host}/api/v4/projects/{}/releases?per_page=10",
                    Self::gitlab_path(id)
                );
                let wire: Vec<GitLabRelease> =
                    self.http.get_json(&url).await?.unwrap_or_default();
                Ok(wire
                    .into_iter()
                    .filter(|r| !r.upcoming_release)
                    .map(|r| Release {
                        assets: r
                            .assets
                            .links
                            .into_iter()
                            .map(|l| Asset {
                                name: l.name,
                                download_url: l.url,
                                size: 0,
                                digest: None,
                                sha512: None,
                            })
                            // Source tarballs are a last resort, and the pack's
                            // asset rules usually reject them anyway.
                            .chain(r.assets.sources.into_iter().map(|s| Asset {
                                name: format!("{}.{}", r.tag_name, s.format),
                                download_url: s.url,
                                size: 0,
                                digest: None,
                                sha512: None,
                            }))
                            .collect(),
                        name: r.name.unwrap_or_default(),
                        published_at: r.released_at.unwrap_or_default(),
                        prerelease: false,
                        web_url: id.web_url(),
                        tag: r.tag_name,
                    })
                    .collect())
            }
            SourceKind::Gitea => {
                let url = format!(
                    "https://{host}/api/v1/repos/{}/{}/releases?limit=10",
                    id.owner, id.repo
                );
                let wire: Vec<GiteaRelease> =
                    self.http.get_json(&url).await?.unwrap_or_default();
                Ok(wire
                    .into_iter()
                    .filter(|r| !r.draft)
                    .map(|r| Release {
                        tag: r.tag_name,
                        name: r.name.unwrap_or_default(),
                        published_at: r.published_at.unwrap_or_default(),
                        prerelease: r.prerelease,
                        web_url: r.html_url,
                        assets: r
                            .assets
                            .into_iter()
                            .map(|a| Asset {
                                name: a.name,
                                download_url: a.browser_download_url,
                                size: a.size,
                                digest: None,
                                sha512: None,
                            })
                            .collect(),
                    })
                    .collect())
            }
            other => Err(Error::other(format!(
                "{} is not a git forge this adapter handles",
                other.label()
            ))),
        }
    }

    pub async fn repo(&self, id: &ModId) -> Result<Option<RepoInfo>> {
        let host = Self::host(id)?;
        match id.kind {
            SourceKind::GitLab => {
                let url = format!(
                    "https://{host}/api/v4/projects/{}?license=true",
                    Self::gitlab_path(id)
                );
                let Some(wire) = self.http.get_json::<GitLabProject>(&url).await? else {
                    return Ok(None);
                };
                Ok(Some(RepoInfo {
                    description: wire.description.unwrap_or_default(),
                    license: wire
                        .license
                        .and_then(|l| l.key)
                        .map(|k| k.to_uppercase())
                        .filter(|k| !k.is_empty()),
                    archived: wire.archived,
                    stars: wire.star_count,
                    source_url: Some(if wire.web_url.is_empty() {
                        id.web_url()
                    } else {
                        wire.web_url
                    }),
                }))
            }
            SourceKind::Gitea => {
                let url = format!(
                    "https://{host}/api/v1/repos/{}/{}",
                    id.owner, id.repo
                );
                let Some(wire) = self.http.get_json::<GiteaRepo>(&url).await? else {
                    return Ok(None);
                };
                Ok(Some(RepoInfo {
                    description: wire.description,
                    // Gitea does not report a licence on the repo endpoint.
                    license: None,
                    archived: wire.archived,
                    stars: wire.stars_count,
                    source_url: Some(if wire.html_url.is_empty() {
                        id.web_url()
                    } else {
                        wire.html_url
                    }),
                }))
            }
            _ => Ok(None),
        }
    }
}
