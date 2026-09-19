use serde::Deserialize;

use crate::error::Result;
use crate::hash::normalize_digest;
use crate::http::Http;
use crate::source::modrinth::urlencode;
use crate::source::{Asset, ModId, Release, RepoInfo, SearchHit};

const API: &str = "https://api.github.com";

/// How many releases to look back through when hunting for one that actually
/// carries a matching asset. Authors sometimes tag a release with no build.
const RELEASE_WINDOW: usize = 10;

#[derive(Clone)]
pub struct GitHub {
    http: Http,
}

// --- wire types -----------------------------------------------------------
// Deliberately narrow: GitHub's release payload is enormous and we want the
// cache entries small.

#[derive(Debug, Deserialize)]
struct WireRelease {
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
    assets: Vec<WireAsset>,
}

#[derive(Debug, Deserialize)]
struct WireAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireRepo {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    license: Option<WireLicense>,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    stargazers_count: u64,
}

#[derive(Debug, Deserialize)]
struct WireLicense {
    #[serde(default)]
    spdx_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireSearch {
    #[serde(default)]
    items: Vec<WireSearchItem>,
}

#[derive(Debug, Deserialize)]
struct WireSearchItem {
    name: String,
    #[serde(default)]
    full_name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    stargazers_count: u64,
    #[serde(default)]
    owner: Option<WireOwner>,
    #[serde(default)]
    license: Option<WireLicense>,
}

#[derive(Debug, Deserialize)]
struct WireOwner {
    login: String,
}

#[derive(Debug, Deserialize)]
struct WireAttestations {
    #[serde(default)]
    attestations: Vec<serde_json::Value>,
}

impl GitHub {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    /// Recent releases, newest first, drafts removed.
    pub async fn releases(&self, id: &ModId) -> Result<Vec<Release>> {
        let url = format!(
            "{API}/repos/{}/{}/releases?per_page={RELEASE_WINDOW}",
            id.owner, id.repo
        );
        let wire: Vec<WireRelease> = self.http.get_json(&url).await?.unwrap_or_default();
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
                        // browser_download_url is served off the CDN and does
                        // not spend API budget, unlike the api.github.com asset
                        // endpoint.
                        download_url: a.browser_download_url,
                        size: a.size,
                        digest: a.digest.map(|d| normalize_digest(&d)),
                        sha512: None,
                    })
                    .collect(),
            })
            .collect())
    }

    /// Find repositories by name.
    ///
    /// For games with no usable third-party index — World of Warcraft being the
    /// case in point — GitHub is the index, and it has the advantage of only
    /// showing things Modifile can actually install.
    pub async fn search_repos(
        &self,
        query: &str,
        topics: &[String],
        terms: &[String],
    ) -> Result<Vec<SearchHit>> {
        // GitHub ANDs repeated qualifiers, so `topic:a topic:b` means "has both"
        // and finds almost nothing. One query per topic, merged, is the only way
        // to express "any of these".
        let base = query.trim();
        let mut queries: Vec<String> = topics
            .iter()
            .map(|topic| format!("{base} topic:{topic} archived:false"))
            .collect();
        if queries.is_empty() {
            let extra = terms.join(" ");
            queries.push(format!("{base} {extra} archived:false"));
        }

        let mut seen: Vec<SearchHit> = Vec::new();
        for q in queries {
            let url = format!(
                "{API}/search/repositories?q={}&sort=stars&order=desc&per_page=15",
                urlencode(&q)
            );
            let wire: WireSearch = self
                .http
                .get_json(&url)
                .await
                .unwrap_or(None)
                .unwrap_or(WireSearch { items: Vec::new() });

            for item in wire.items {
                let id = ModId::github(
                    item.owner.map(|o| o.login).unwrap_or_default(),
                    item.name,
                );
                if seen.iter().any(|h| h.id == id) {
                    continue;
                }
                seen.push(SearchHit {
                    id,
                    title: item.full_name,
                    description: item.description.unwrap_or_default(),
                    stars: item.stargazers_count,
                    license: item
                        .license
                        .and_then(|l| l.spdx_id)
                        .filter(|s| s != "NOASSERTION"),
                    source_url: Some(item.html_url),
                    installable: None,
                });
            }
        }

        // Topic searches miss repositories whose authors never set topics, so
        // fall back to plain words when the tagged results are thin.
        if seen.len() < 5 && !terms.is_empty() {
            let q = format!("{base} {} archived:false", terms.join(" "));
            let url = format!(
                "{API}/search/repositories?q={}&sort=stars&order=desc&per_page=15",
                urlencode(&q)
            );
            if let Ok(Some(wire)) = self.http.get_json::<WireSearch>(&url).await {
                for item in wire.items {
                    let id = ModId::github(
                        item.owner.map(|o| o.login).unwrap_or_default(),
                        item.name,
                    );
                    if seen.iter().any(|h| h.id == id) {
                        continue;
                    }
                    seen.push(SearchHit {
                        id,
                        title: item.full_name,
                        description: item.description.unwrap_or_default(),
                        stars: item.stargazers_count,
                        license: item
                            .license
                            .and_then(|l| l.spdx_id)
                            .filter(|s| s != "NOASSERTION"),
                        source_url: Some(item.html_url),
                        installable: None,
                    });
                }
            }
        }

        seen.sort_by(|a, b| b.stars.cmp(&a.stars));
        seen.truncate(20);
        Ok(seen)
    }

    /// Whether a repository publishes release assets at all — the difference
    /// between "found it" and "can install it".
    pub async fn has_releases(&self, id: &ModId) -> bool {
        self.releases(id)
            .await
            .map(|r| r.iter().any(|rel| !rel.assets.is_empty()))
            .unwrap_or(false)
    }

    pub async fn repo(&self, id: &ModId) -> Result<Option<RepoInfo>> {
        let url = format!("{API}/repos/{}/{}", id.owner, id.repo);
        let Some(wire) = self.http.get_json::<WireRepo>(&url).await? else {
            return Ok(None);
        };
        Ok(Some(RepoInfo {
            description: wire.description.unwrap_or_default(),
            license: wire
                .license
                .and_then(|l| l.spdx_id)
                // GitHub says "NOASSERTION" when it sees a LICENSE file it
                // cannot identify, which is not a license for our purposes.
                .filter(|s| !s.is_empty() && s != "NOASSERTION"),
            archived: wire.archived,
            stars: wire.stargazers_count,
            // The repository is the source, so it is its own source_url.
            source_url: Some(id.web_url()),
        }))
    }

    /// Whether GitHub holds a build-provenance attestation whose subject is
    /// this artifact digest, under this repository.
    ///
    /// This proves an attestation exists and is bound to the repo. It is not
    /// yet full offline Sigstore bundle verification (certificate chain,
    /// transparency log inclusion) — that is the next step for this function,
    /// and until it lands the `Verified` rung is reported as attested-by-API
    /// rather than cryptographically checked locally.
    pub async fn has_attestation(&self, id: &ModId, sha256: &str) -> Result<bool> {
        let url = format!(
            "{API}/repos/{}/{}/attestations/sha256:{}",
            id.owner, id.repo, sha256
        );
        match self.http.get_json::<WireAttestations>(&url).await {
            Ok(Some(v)) => Ok(!v.attestations.is_empty()),
            Ok(None) => Ok(false),
            // A missing attestation must never fail an install; it just means
            // the mod does not reach the top rung.
            Err(_) => Ok(false),
        }
    }
}
