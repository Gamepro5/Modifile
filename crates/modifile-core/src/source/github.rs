use serde::Deserialize;

use crate::error::Result;
use crate::hash::normalize_digest;
use crate::http::Http;
use crate::source::{Asset, ModId, Release, RepoInfo};

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
                    })
                    .collect(),
            })
            .collect())
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
