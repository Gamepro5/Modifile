//! HTTP with an on-disk conditional-request cache.
//!
//! GitHub gives unauthenticated clients 60 requests per hour, and — contrary to
//! what its docs imply in several places — a `304 Not Modified` still spends
//! one of them. Only an authenticated 304 is free. So the cache here is not a
//! nicety: without it a 40-addon profile cannot complete a single update check.
//! With a token the same cache turns update checks into free 304s.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Context, Error, Result};
use crate::hash::sha256_bytes;
use crate::paths::write_atomic;

const USER_AGENT: &str = concat!("modifile/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    etag: Option<String>,
    last_modified: Option<String>,
    body: String,
}

/// What the last response told us about our remaining budget.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateLimit {
    pub remaining: u32,
    pub limit: u32,
    pub reset_epoch: u64,
}

#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    cache_dir: PathBuf,
    token: Option<String>,
    remaining: Arc<AtomicU32>,
    limit: Arc<AtomicU32>,
    reset: Arc<AtomicU32>,
}

impl Http {
    pub fn new(cache_dir: PathBuf, token: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(60))
            .connect_timeout(std::time::Duration::from_secs(15))
            // One connection per host, reused across the whole update check.
            .pool_max_idle_per_host(8)
            .build()?;
        std::fs::create_dir_all(&cache_dir).ctx(format!("creating {}", cache_dir.display()))?;
        Ok(Self {
            client,
            cache_dir,
            token,
            remaining: Arc::new(AtomicU32::new(u32::MAX)),
            limit: Arc::new(AtomicU32::new(0)),
            reset: Arc::new(AtomicU32::new(0)),
        })
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    pub fn rate_limit(&self) -> RateLimit {
        RateLimit {
            remaining: self.remaining.load(Ordering::Relaxed),
            limit: self.limit.load(Ordering::Relaxed),
            reset_epoch: self.reset.load(Ordering::Relaxed) as u64,
        }
    }

    fn cache_path(&self, url: &str) -> PathBuf {
        self.cache_dir.join(format!("{}.json", sha256_bytes(url.as_bytes())))
    }

    fn read_cache(&self, url: &str) -> Option<CacheEntry> {
        let raw = std::fs::read(self.cache_path(url)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    fn record_limits(&self, headers: &reqwest::header::HeaderMap) {
        let get = |name: &str| -> Option<u32> {
            headers.get(name)?.to_str().ok()?.parse().ok()
        };
        if let Some(v) = get("x-ratelimit-remaining") {
            self.remaining.store(v, Ordering::Relaxed);
        }
        if let Some(v) = get("x-ratelimit-limit") {
            self.limit.store(v, Ordering::Relaxed);
        }
        if let Some(v) = get("x-ratelimit-reset") {
            self.reset.store(v, Ordering::Relaxed);
        }
    }

    fn request(&self, url: &str) -> reqwest::RequestBuilder {
        // GitHub's headers go to GitHub and nowhere else. Two reasons, and
        // both bit us:
        //
        //  - `Accept: application/vnd.github+json` is a GitHub media type.
        //    Thunderstore's API answers a request carrying it with 406.
        //  - The token is a *GitHub* credential. Sending it to a mod CDN
        //    because the same client happened to be reused hands someone
        //    else's server a secret it has no business seeing.
        if !is_github(url) {
            return self.client.get(url).header("Accept", "application/json");
        }

        let mut req = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req
    }

    /// Send a request, waiting and trying again if the server says to.
    ///
    /// Not every index is as forgiving as GitHub. Thunderstore answers `429
    /// Too Many Requests` well within the concurrency this resolves mods at,
    /// and a modpack is the worst case: ninety pinned dependencies resolved at
    /// once is exactly the shape that trips it. Without this, a pack imports
    /// with eight or nine mods randomly missing and a different eight or nine
    /// missing the next time — which reads as a broken pack rather than as
    /// backpressure.
    ///
    /// `Retry-After` is honoured when the server sends it; otherwise the wait
    /// doubles each attempt. Bounded, because a request that is never going to
    /// succeed should fail while someone is still watching.
    async fn send_with_backoff(
        &self,
        req: reqwest::RequestBuilder,
        url: &str,
    ) -> Result<reqwest::Response> {
        const ATTEMPTS: u32 = 5;

        let mut wait = std::time::Duration::from_millis(500);
        for attempt in 1..=ATTEMPTS {
            let Some(attempt_req) = req.try_clone() else {
                // A streaming body cannot be replayed. Nothing here has one.
                return req.send().await.ctx(format!("GET {url}"));
            };

            let resp = attempt_req.send().await.ctx(format!("GET {url}"))?;
            let status = resp.status();
            // 502 and 504 belong here too: a CDN in front of a busy origin
            // returns them for a moment under exactly the load a modpack puts
            // on it, and they are not a statement about the file.
            let retryable = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
                || status == reqwest::StatusCode::BAD_GATEWAY
                || status == reqwest::StatusCode::GATEWAY_TIMEOUT;

            if !retryable || attempt == ATTEMPTS {
                return Ok(resp);
            }

            // The server's own number wins over our guess.
            let after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(std::time::Duration::from_secs);
            let delay = after.unwrap_or(wait).min(std::time::Duration::from_secs(10));

            tokio::time::sleep(delay).await;
            wait *= 2;
        }
        unreachable!("the loop returns on its last attempt")
    }

    /// GET a JSON document, revalidating a cached copy when we have one.
    ///
    /// `Ok(None)` means the server said 404 — a missing release is a normal
    /// answer, not a failure.
    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<Option<T>> {
        self.get_json_with(url, &[]).await
    }

    /// As `get_json`, with extra headers — CurseForge authenticates with its
    /// own `x-api-key` rather than a bearer token.
    pub async fn get_json_with<T: DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<Option<T>> {
        let cached = self.read_cache(url);

        let mut req = self.request(url);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        if let Some(entry) = &cached {
            if let Some(etag) = &entry.etag {
                req = req.header("If-None-Match", etag.clone());
            } else if let Some(lm) = &entry.last_modified {
                req = req.header("If-Modified-Since", lm.clone());
            }
        }

        let resp = self.send_with_backoff(req, url).await?;
        self.record_limits(resp.headers());
        let status = resp.status();

        if status == reqwest::StatusCode::NOT_MODIFIED {
            let entry = cached.ok_or_else(|| {
                Error::other("server returned 304 but the local cache entry is gone")
            })?;
            return Ok(Some(serde_json::from_str(&entry.body)?));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            let limits = self.rate_limit();
            if limits.remaining == 0 {
                return Err(Error::RateLimited {
                    remaining: limits.remaining,
                    reset: format_epoch(limits.reset_epoch),
                });
            }
        }
        if !status.is_success() {
            return Err(Error::other(format!("GET {url} returned {status}")));
        }

        let etag = header_string(resp.headers(), "etag");
        let last_modified = header_string(resp.headers(), "last-modified");
        let body = resp.text().await?;

        let value = serde_json::from_str::<T>(&body)
            .map_err(|e| Error::other(format!("GET {url}: unexpected response shape: {e}")))?;

        // Cache failures are not request failures.
        let _ = write_atomic(
            &self.cache_path(url),
            &serde_json::to_vec(&CacheEntry {
                etag,
                last_modified,
                body,
            })?,
        );

        Ok(Some(value))
    }

    /// POST a JSON body and read a JSON reply.
    ///
    /// Deliberately uncached: a POST has no etag to revalidate against, and the
    /// only reason this exists is the batch endpoints. CurseForge will hand
    /// back 300 file records for one request, and Modrinth will map 300 hashes
    /// to their versions — the difference between importing a modpack in one
    /// round trip and being rate-limited out of it.
    pub async fn post_json_with<T: DeserializeOwned, B: serde::Serialize>(
        &self,
        url: &str,
        body: &B,
        headers: &[(&str, &str)],
    ) -> Result<Option<T>> {
        let mut req = self.client.post(url).json(body);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }

        let resp = self.send_with_backoff(req, url).await?;
        self.record_limits(resp.headers());
        let status = resp.status();

        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            let limits = self.rate_limit();
            if limits.remaining == 0 {
                return Err(Error::RateLimited {
                    remaining: limits.remaining,
                    reset: format_epoch(limits.reset_epoch),
                });
            }
        }
        if !status.is_success() {
            return Err(Error::other(format!("POST {url} returned {status}")));
        }

        let body = resp.text().await?;
        serde_json::from_str::<T>(&body)
            .map(Some)
            .map_err(|e| Error::other(format!("POST {url}: unexpected response shape: {e}")))
    }

    /// Fetch a small file into memory.
    ///
    /// For artwork, which is a few tens of kilobytes and wanted as bytes rather
    /// than as a file on a path. `Ok(None)` for a 404, because a project
    /// whose icon has gone is normal and not worth an error.
    ///
    /// Capped, because this holds the whole body in memory and a URL from a
    /// mod index is not something to trust about its own size.
    pub async fn get_bytes(&self, url: &str) -> Result<Option<Vec<u8>>> {
        const MAX: u64 = 16 * 1024 * 1024;

        let req = self.client.get(url).header("Accept", "image/*");
        let resp = self.send_with_backoff(req, url).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(Error::other(format!("GET {url} returned {status}")));
        }
        if resp.content_length().is_some_and(|len| len > MAX) {
            return Err(Error::other(format!("{url} is larger than expected")));
        }

        let bytes = resp.bytes().await?;
        if bytes.len() as u64 > MAX {
            return Err(Error::other(format!("{url} is larger than expected")));
        }
        Ok(Some(bytes.to_vec()))
    }

    /// Stream a download to disk, hashing as it goes so we never hold a 200 MB
    /// modpack in memory and never hash the file a second time.
    pub async fn download_to(&self, url: &str, dest: &std::path::Path) -> Result<(u64, String)> {
        use futures::StreamExt;
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncWriteExt;

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut req = self.client.get(url).header("Accept", "application/octet-stream");
        // Same rule as `request`: the token is GitHub's. Every source's
        // downloads used to go out through this one client, so a Modrinth or
        // CurseForge CDN was handed the user's GitHub token with every mod.
        if is_github(url) {
            if let Some(token) = &self.token {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
        }
        // Downloads get the same backpressure handling as API calls: a CDN
        // under load answers 502 or 429 for a moment, and a modpack pulling a
        // hundred files will meet that at least once.
        let resp = self.send_with_backoff(req, url).await?;
        self.record_limits(resp.headers());
        if !resp.status().is_success() {
            return Err(Error::other(format!(
                "downloading {url} returned {}",
                resp.status()
            )));
        }

        let mut file = tokio::fs::File::create(dest).await?;
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            written += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok((written, hex::encode(hasher.finalize())))
    }
}

/// Is this URL GitHub's, and therefore entitled to a GitHub credential?
///
/// Host-matched rather than substring-matched, so that a URL merely *mentioning*
/// github — `https://evil.test/?to=api.github.com` — is not treated as GitHub.
fn is_github(url: &str) -> bool {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    // Authority ends at the first `/`, `?` or `#`; userinfo ends at `@`.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    // Strip any port.
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    let host = host.trim_end_matches('.').to_ascii_lowercase();

    host == "github.com"
        || host == "githubusercontent.com"
        || host.ends_with(".github.com")
        || host.ends_with(".githubusercontent.com")
}

fn header_string(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::is_github;

    #[test]
    fn recognises_githubs_own_hosts() {
        for url in [
            "https://api.github.com/repos/a/b/releases",
            "https://github.com/a/b/releases/download/v1/x.zip",
            "https://objects.githubusercontent.com/some/signed/url",
            "https://GITHUB.COM/a/b",
            "https://api.github.com:443/repos/a/b",
        ] {
            assert!(is_github(url), "should be GitHub: {url}");
        }
    }

    #[test]
    fn never_hands_the_token_to_anyone_else() {
        for url in [
            "https://api.modrinth.com/v2/search",
            "https://thunderstore.io/api/experimental/package/A/B/",
            "https://api.curseforge.com/v1/mods/search",
            "https://edge.forgecdn.net/files/1/2/mod.jar",
            "https://cdn.modrinth.com/data/AAA/versions/x/mod.jar",
            // Lookalikes: the host is what counts, not the spelling.
            "https://evil.test/?redirect=https://api.github.com",
            "https://github.com.evil.test/a/b",
            "https://notgithub.com/a/b",
            "https://user@evil.test/api.github.com",
        ] {
            assert!(!is_github(url), "must not be treated as GitHub: {url}");
        }
    }
}

fn format_epoch(epoch: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if epoch <= now {
        return "now".to_string();
    }
    let mins = (epoch - now).div_ceil(60);
    format!("in {mins} min")
}
