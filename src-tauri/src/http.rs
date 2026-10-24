use crate::error::{AppError, AppResult};
use crate::release::{GitHubAsset, GitHubRelease};
use crate::util::{
    api_repository_url, raw_github_url, sha256_bytes, MAX_CATALOG_BYTES, MAX_RELEASE_JSON_BYTES,
};
use chrono::{DateTime, Utc};
use reqwest::header::{
    HeaderMap, ACCEPT, AUTHORIZATION, CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH,
    LAST_MODIFIED, USER_AGENT,
};
use reqwest::{Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const USER_AGENT_VALUE: &str =
    "NexusTools/1.0 (Windows; +https://github.com/vibe-phoenix/NexusToolsApp)";
const MAX_RETRIES: usize = 3;
const MAX_RELEASE_PAGES: usize = 50;
const DOWNLOAD_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HttpValidators {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

impl HttpValidators {
    fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            etag: headers
                .get(ETAG)
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            last_modified: headers
                .get(LAST_MODIFIED)
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }
}

#[derive(Debug, Clone)]
pub enum Conditional<T> {
    Modified {
        value: T,
        validators: HttpValidators,
    },
    NotModified,
}

#[derive(Debug, Clone)]
pub struct RawFile {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    pub validators: HttpValidators,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub tree_sha: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub path: String,
    pub blob_sha: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct TreeInfo {
    pub entries: HashMap<String, TreeEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct ReleaseFetch {
    pub releases: Vec<GitHubRelease>,
    pub validators: HttpValidators,
}

#[derive(Deserialize)]
struct CommitResponse {
    sha: String,
    commit: CommitPayload,
}

#[derive(Deserialize)]
struct CommitPayload {
    tree: TreePointer,
}

#[derive(Deserialize)]
struct TreePointer {
    sha: String,
}

#[derive(Deserialize)]
struct TreeResponse {
    #[serde(default)]
    tree: Vec<TreeFile>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct TreeFile {
    path: String,
    #[serde(default)]
    sha: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default, rename = "type")]
    kind: String,
}

pub struct GitHubClient {
    client: reqwest::Client,
    token: Option<String>,
}

impl GitHubClient {
    pub fn new() -> AppResult<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT_VALUE)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error("too many GitHub redirects");
                }
                let allowed = match attempt.url().scheme() {
                    "https" => is_allowed_github_host(attempt.url().host_str().unwrap_or_default()),
                    _ => false,
                };
                if allowed {
                    attempt.follow()
                } else {
                    attempt.error("GitHub redirected to an unsupported host")
                }
            }))
            .build()
            .map_err(|error| {
                AppError::new(
                    "http_initialization",
                    format!("Could not initialize the secure HTTP client: {error}"),
                )
            })?;
        // A token is never written to the cache or bundled with the app. It is
        // an optional runtime convenience for public API rate limits.
        let token = std::env::var("NEXUSTOOLS_GITHUB_TOKEN")
            .or_else(|_| std::env::var("GITHUB_TOKEN"))
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty());
        Ok(Self { client, token })
    }

    fn request(&self, method: reqwest::Method, url: Url) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .request(method, url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        request
    }

    async fn send_api(&self, url: Url, validators: Option<&HttpValidators>) -> AppResult<Response> {
        let mut attempt = 0usize;
        loop {
            let mut request = self.request(reqwest::Method::GET, url.clone());
            if let Some(validators) = validators {
                if let Some(etag) = &validators.etag {
                    request = request.header(IF_NONE_MATCH, etag);
                }
                if let Some(last_modified) = &validators.last_modified {
                    request = request.header(IF_MODIFIED_SINCE, last_modified);
                }
            }
            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() || status == StatusCode::NOT_MODIFIED {
                        return Ok(response);
                    }
                    let rate_limited = matches!(
                        status,
                        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
                    ) && is_rate_limited(&response);
                    let retryable = status.is_server_error()
                        || status == StatusCode::REQUEST_TIMEOUT
                        || status == StatusCode::TOO_MANY_REQUESTS;
                    if retryable && attempt + 1 < MAX_RETRIES && !rate_limited {
                        attempt += 1;
                        tokio::time::sleep(backoff(attempt)).await;
                        continue;
                    }
                    return Err(response_error(response).await);
                }
                Err(error) => {
                    if attempt + 1 < MAX_RETRIES
                        && (error.is_timeout() || error.is_connect() || error.is_request())
                    {
                        attempt += 1;
                        tokio::time::sleep(backoff(attempt)).await;
                        continue;
                    }
                    return Err(AppError::new(
                        "network",
                        format!("Could not reach GitHub: {error}"),
                    )
                    .retryable(true));
                }
            }
        }
    }

    pub async fn fetch_catalog(
        &self,
        url: &str,
        validators: Option<&HttpValidators>,
    ) -> AppResult<Conditional<(Vec<u8>, HttpValidators)>> {
        let parsed = Url::parse(url).map_err(|_| {
            AppError::new(
                "invalid_catalog_url",
                "The catalog URL is not a valid HTTPS URL.",
            )
        })?;
        if parsed.scheme() != "https"
            || !is_allowed_github_host(parsed.host_str().unwrap_or_default())
        {
            return Err(AppError::new(
                "invalid_catalog_url",
                "The catalog URL must be an HTTPS GitHub URL.",
            ));
        }
        let response = self.send_api(parsed, validators).await?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(Conditional::NotModified);
        }
        let headers = response.headers().clone();
        let bytes = read_limited_response(response, MAX_CATALOG_BYTES).await?;
        Ok(Conditional::Modified {
            value: (bytes, HttpValidators::from_headers(&headers)),
            validators: HttpValidators::from_headers(&headers),
        })
    }

    pub async fn resolve_ref(
        &self,
        repository: &str,
        git_ref: &str,
        validators: Option<&HttpValidators>,
    ) -> AppResult<Conditional<CommitInfo>> {
        let base = api_repository_url(repository)?;
        let mut url = Url::parse(&format!("{base}/commits"))
            .map_err(|_| AppError::new("invalid_url", "Could not build the GitHub commit URL."))?;
        url.query_pairs_mut()
            .append_pair("sha", git_ref)
            .append_pair("per_page", "1");
        let response = self.send_api(url, validators).await?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(Conditional::NotModified);
        }
        let headers = response.headers().clone();
        let bytes = read_limited_response(response, MAX_RELEASE_JSON_BYTES).await?;
        let commits: Vec<CommitResponse> = serde_json::from_slice(&bytes).map_err(|error| {
            AppError::new(
                "invalid_commit_response",
                format!("GitHub returned an unexpected commit response: {error}"),
            )
        })?;
        let first = commits.into_iter().next().ok_or_else(|| {
            AppError::new(
                "repository_unavailable",
                format!(
                    "The ref '{git_ref}' in {repository} did not resolve to a commit. The branch may not exist."
                ),
            )
        })?;
        Ok(Conditional::Modified {
            value: CommitInfo {
                sha: first.sha,
                tree_sha: Some(first.commit.tree.sha),
            },
            validators: HttpValidators::from_headers(&headers),
        })
    }

    pub async fn fetch_tree(&self, repository: &str, tree_sha: &str) -> AppResult<TreeInfo> {
        let base = api_repository_url(repository)?;
        let encoded_tree = crate::util::url_encode_component(tree_sha);
        let mut url = Url::parse(&format!("{base}/git/trees/{encoded_tree}"))
            .map_err(|_| AppError::new("invalid_url", "Could not build the GitHub tree URL."))?;
        url.query_pairs_mut().append_pair("recursive", "1");
        let response = self.send_api(url, None).await?;
        let bytes = read_limited_response(response, MAX_RELEASE_JSON_BYTES).await?;
        let tree: TreeResponse = serde_json::from_slice(&bytes).map_err(|error| {
            AppError::new(
                "invalid_tree_response",
                format!("GitHub returned an unexpected tree response: {error}"),
            )
        })?;
        let mut entries = HashMap::new();
        for file in tree.tree {
            if file.kind == "blob" && !file.sha.is_empty() {
                entries.insert(
                    file.path.clone(),
                    TreeEntry {
                        path: file.path,
                        blob_sha: file.sha,
                        size: file.size,
                    },
                );
            }
        }
        Ok(TreeInfo {
            entries,
            truncated: tree.truncated,
        })
    }

    pub async fn fetch_raw(
        &self,
        repository: &str,
        commit: &str,
        repo_path: &str,
        limit: usize,
        validators: Option<&HttpValidators>,
    ) -> AppResult<Conditional<RawFile>> {
        let url = raw_github_url(repository, commit, repo_path)?;
        let parsed = Url::parse(&url)
            .map_err(|_| AppError::new("invalid_url", "Could not build the GitHub file URL."))?;
        let mut attempt = 0usize;
        loop {
            let mut request = self
                .client
                .get(parsed.clone())
                .header(USER_AGENT, USER_AGENT_VALUE)
                .header(ACCEPT, "application/octet-stream");
            if let Some(token) = &self.token {
                request = request.header(AUTHORIZATION, format!("Bearer {token}"));
            }
            if let Some(validators) = validators {
                if let Some(etag) = &validators.etag {
                    request = request.header(IF_NONE_MATCH, etag);
                }
                if let Some(last_modified) = &validators.last_modified {
                    request = request.header(IF_MODIFIED_SINCE, last_modified);
                }
            }
            let result = request.send().await;
            match result {
                Ok(response) if response.status() == StatusCode::NOT_MODIFIED => {
                    return Ok(Conditional::NotModified);
                }
                Ok(response) if response.status().is_success() => {
                    let headers = response.headers().clone();
                    let content_type = headers
                        .get(CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .map(ToString::to_string);
                    let bytes = read_limited_response(response, limit).await?;
                    let sha256 = sha256_bytes(&bytes);
                    return Ok(Conditional::Modified {
                        value: RawFile {
                            bytes,
                            content_type,
                            validators: HttpValidators::from_headers(&headers),
                            sha256,
                        },
                        validators: HttpValidators::from_headers(&headers),
                    });
                }
                Ok(response) => {
                    let status = response.status();
                    let mut error = response_error(response).await;
                    error.details.push(format!(
                        "Required file: {repo_path} (repository {repository}, commit {commit})."
                    ));
                    if status.is_server_error() && attempt + 1 < MAX_RETRIES {
                        attempt += 1;
                        tokio::time::sleep(backoff(attempt)).await;
                        continue;
                    }
                    return Err(error);
                }
                Err(error) => {
                    if attempt + 1 < MAX_RETRIES && (error.is_timeout() || error.is_connect()) {
                        attempt += 1;
                        tokio::time::sleep(backoff(attempt)).await;
                        continue;
                    }
                    return Err(AppError::with_details(
                        "network",
                        format!("Could not download a plugin file from GitHub: {error}"),
                        [format!(
                            "Required file: {repo_path} (repository {repository}, commit {commit})."
                        )],
                    )
                    .retryable(true));
                }
            }
        }
    }

    pub async fn fetch_releases(
        &self,
        repository: &str,
        validators: Option<&HttpValidators>,
    ) -> AppResult<Conditional<ReleaseFetch>> {
        let base = api_repository_url(repository)?;
        let mut page = 1usize;
        let mut releases = Vec::new();
        let mut first_validators = HttpValidators::default();
        loop {
            let mut url = Url::parse(&format!("{base}/releases")).map_err(|_| {
                AppError::new("invalid_url", "Could not build the GitHub releases URL.")
            })?;
            url.query_pairs_mut()
                .append_pair("per_page", "100")
                .append_pair("page", &page.to_string());
            let conditional = if page == 1 { validators } else { None };
            let response = self.send_api(url, conditional).await?;
            if page == 1 && response.status() == StatusCode::NOT_MODIFIED {
                return Ok(Conditional::NotModified);
            }
            let headers = response.headers().clone();
            if page == 1 {
                first_validators = HttpValidators::from_headers(&headers);
            }
            let bytes = read_limited_response(response, MAX_RELEASE_JSON_BYTES).await?;
            let page_releases: Vec<GitHubRelease> =
                serde_json::from_slice(&bytes).map_err(|error| {
                    AppError::new(
                        "invalid_releases_response",
                        format!("GitHub returned an unexpected releases response: {error}"),
                    )
                })?;
            let count = page_releases.len();
            releases.extend(page_releases);
            if count < 100 {
                break;
            }
            page += 1;
            if page > MAX_RELEASE_PAGES {
                return Err(AppError::new(
                    "release_pagination_limit",
                    "The repository has more release pages than NexusTools can safely load. Please report this to the plugin author.",
                ));
            }
        }
        Ok(Conditional::Modified {
            value: ReleaseFetch {
                releases,
                validators: first_validators.clone(),
            },
            validators: first_validators,
        })
    }

    pub async fn fetch_checksum_text(&self, url: &str, limit: usize) -> AppResult<Vec<u8>> {
        let parsed = Url::parse(url)
            .map_err(|_| AppError::new("invalid_asset_url", "A release asset URL is invalid."))?;
        if parsed.scheme() != "https"
            || !is_allowed_github_host(parsed.host_str().unwrap_or_default())
        {
            return Err(AppError::new(
                "invalid_asset_url",
                "A release asset URL did not use an allowed HTTPS GitHub host.",
            ));
        }
        let response = self.send_asset(parsed).await?;
        read_limited_response(response, limit).await
    }

    async fn send_asset(&self, url: Url) -> AppResult<Response> {
        let mut attempt = 0usize;
        loop {
            let result = self
                .client
                .get(url.clone())
                .header(USER_AGENT, USER_AGENT_VALUE)
                .header(ACCEPT, "application/octet-stream")
                .send()
                .await;
            match result {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let error = response_error(response).await;
                    if status.is_server_error() && attempt + 1 < MAX_RETRIES {
                        attempt += 1;
                        tokio::time::sleep(backoff(attempt)).await;
                        continue;
                    }
                    return Err(error);
                }
                Err(error) => {
                    if attempt + 1 < MAX_RETRIES && (error.is_timeout() || error.is_connect()) {
                        attempt += 1;
                        tokio::time::sleep(backoff(attempt)).await;
                        continue;
                    }
                    return Err(AppError::new(
                        "network",
                        format!("Could not download the release package: {error}"),
                    )
                    .retryable(true));
                }
            }
        }
    }

    /// Stream a release package to `destination`, reporting progress and
    /// honoring cancellation. The file is only considered complete after all
    /// bytes have been written and flushed.
    pub async fn download_package(
        &self,
        url: &str,
        destination: &Path,
        expected_size: Option<u64>,
        max_bytes: u64,
        cancel: &AtomicBool,
        progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    ) -> AppResult<String> {
        let parsed = Url::parse(url).map_err(|_| {
            AppError::new("invalid_asset_url", "The release package URL is invalid.")
        })?;
        if parsed.scheme() != "https"
            || !is_allowed_github_host(parsed.host_str().unwrap_or_default())
        {
            return Err(AppError::new(
                "invalid_asset_url",
                "The release package URL did not use an allowed HTTPS GitHub host.",
            ));
        }
        if expected_size.is_some_and(|size| size > max_bytes) {
            return Err(AppError::new(
                "package_too_large",
                "The release package exceeds NexusTools's safe extraction limit.",
            ));
        }
        let mut last_error = None;
        for attempt in 0..MAX_RETRIES {
            if cancel.load(Ordering::SeqCst) {
                return Err(AppError::new("cancelled", "Installation was cancelled."));
            }
            match self
                .download_once(
                    parsed.clone(),
                    destination,
                    expected_size,
                    max_bytes,
                    cancel,
                    progress,
                )
                .await
            {
                Ok(hash) => return Ok(hash),
                Err(error) if error.code == "cancelled" => return Err(error),
                Err(error) if error.retryable && attempt + 1 < MAX_RETRIES => {
                    last_error = Some(error);
                    tokio::time::sleep(backoff(attempt + 1)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            AppError::new(
                "download_failed",
                "The release package could not be downloaded.",
            )
        }))
    }

    async fn download_once(
        &self,
        url: Url,
        destination: &Path,
        expected_size: Option<u64>,
        max_bytes: u64,
        cancel: &AtomicBool,
        progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    ) -> AppResult<String> {
        let mut response = self
            .client
            .get(url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, "application/octet-stream")
            .send()
            .await
            .map_err(|error| {
                AppError::new(
                    "network",
                    format!("Could not download the release package: {error}"),
                )
                .retryable(true)
            })?;
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }
        let total = response.content_length().or(expected_size);
        if total.is_some_and(|size| size > max_bytes) {
            return Err(AppError::new(
                "package_too_large",
                "The release package exceeds NexusTools's safe extraction limit.",
            ));
        }
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let partial = destination.with_extension("download");
        let _ = tokio::fs::remove_file(&partial).await;
        let mut file = tokio::fs::File::create(&partial).await.map_err(|error| {
            AppError::io(
                &format!("Cannot create the download at {}", partial.display()),
                error,
            )
        })?;
        let mut hasher = sha2::Sha256::new();
        use sha2::Digest;
        let mut downloaded = 0u64;
        progress(downloaded, total);
        let result: AppResult<()> = async {
            while let Some(chunk) = response.chunk().await.map_err(|error| {
                AppError::new(
                    "network",
                    format!("The release package download was interrupted: {error}"),
                )
                .retryable(true)
            })? {
                if cancel.load(Ordering::SeqCst) {
                    return Err(AppError::new("cancelled", "Installation was cancelled."));
                }
                downloaded = downloaded.checked_add(chunk.len() as u64).ok_or_else(|| {
                    AppError::new("package_too_large", "The package size overflowed.")
                })?;
                if downloaded > max_bytes {
                    return Err(AppError::new(
                        "package_too_large",
                        "The release package exceeded NexusTools's safe download limit.",
                    ));
                }
                hasher.update(&chunk);
                file.write_all(&chunk).await.map_err(|error| {
                    AppError::io(
                        &format!("Cannot write the download to {}", partial.display()),
                        error,
                    )
                })?;
                progress(downloaded, total);
                // Avoid starving UI events on very fast local connections.
                if downloaded % (DOWNLOAD_CHUNK as u64) == 0 {
                    tokio::task::yield_now().await;
                }
            }
            file.flush().await?;
            file.sync_all().await?;
            Ok(())
        }
        .await;
        drop(file);
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(error);
        }
        if let Some(total) = total {
            if downloaded != total {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(AppError::new(
                    "download_incomplete",
                    format!("The package download ended early ({downloaded} of {total} bytes)."),
                )
                .retryable(true));
            }
        }
        tokio::fs::rename(&partial, destination)
            .await
            .map_err(|error| {
                AppError::io(
                    &format!("Cannot finalize the download at {}", destination.display()),
                    error,
                )
            })?;
        Ok(hex::encode(hasher.finalize()))
    }
}

fn backoff(attempt: usize) -> Duration {
    Duration::from_millis(350 * (1 << attempt.min(4)) as u64)
}

fn is_allowed_github_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "github.com"
        || host == "api.github.com"
        || host == "raw.githubusercontent.com"
        || host == "objects.githubusercontent.com"
        || host == "release-assets.githubusercontent.com"
        || host.ends_with(".githubusercontent.com")
}

fn is_rate_limited(response: &Response) -> bool {
    response
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "0")
        || response.status() == StatusCode::TOO_MANY_REQUESTS
}

async fn response_error(response: Response) -> AppError {
    let status = response.status();
    let headers = response.headers().clone();
    let body = read_limited_response(response, 512 * 1024)
        .await
        .unwrap_or_default();
    let body_text = String::from_utf8_lossy(&body);
    let api_message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(|v| v.as_str())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| body_text.chars().take(300).collect());
    match status {
        StatusCode::NOT_FOUND => AppError::new(
            "repository_unavailable",
            "The GitHub repository, branch, commit, or file was not found. It may be private or unpublished.",
        ),
        StatusCode::UNAUTHORIZED => AppError::new(
            "github_auth_failed",
            "GitHub rejected the optional access token. Remove NEXUSTOOLS_GITHUB_TOKEN or GITHUB_TOKEN and try again.",
        ),
        StatusCode::TOO_MANY_REQUESTS => rate_limit_error(&headers),
        StatusCode::FORBIDDEN if is_rate_limited_headers(&headers) || api_message.to_ascii_lowercase().contains("rate limit") => {
            rate_limit_error(&headers)
        }
        StatusCode::FORBIDDEN => AppError::new(
            "github_forbidden",
            format!("GitHub refused the request. The repository may be private or access may be restricted. {api_message}"),
        ),
        _ => AppError::new(
            "github_error",
            format!("GitHub returned HTTP {status}. {api_message}"),
        )
        .retryable(status.is_server_error()),
    }
}

fn is_rate_limited_headers(headers: &HeaderMap) -> bool {
    headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "0")
}

fn rate_limit_error(headers: &HeaderMap) -> AppError {
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0))
        .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string());
    let mut error = AppError::new(
        "github_rate_limit",
        "GitHub's public API rate limit was reached. Cached plugins remain available. Try again later, or start NexusTools with NEXUSTOOLS_GITHUB_TOKEN set to a read-only token.",
    )
    .retryable(true);
    if let Some(reset) = reset {
        error = error.detail(format!("The rate limit resets at {reset}."));
    }
    error
}

async fn read_limited_response(mut response: Response, limit: usize) -> AppResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(AppError::new(
            "response_too_large",
            format!("A GitHub response exceeded the permitted {limit} byte limit."),
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        AppError::new(
            "network",
            format!("The GitHub response was interrupted: {error}"),
        )
        .retryable(true)
    })? {
        if bytes.len() + chunk.len() > limit {
            return Err(AppError::new(
                "response_too_large",
                format!("A GitHub response exceeded the permitted {limit} byte limit."),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// The exact package checksum sidecar lookup. A sidecar is only used if there
/// is precisely one file whose base name matches `<zip>.sha256` and its contents
/// contain a single SHA-256 digest.
pub fn checksum_sidecar(asset: &GitHubAsset, assets: &[GitHubAsset]) -> Option<GitHubAsset> {
    let expected = format!("{}.sha256", asset.name.to_ascii_lowercase());
    let candidates: Vec<&GitHubAsset> = assets
        .iter()
        .filter(|candidate| candidate.name.to_ascii_lowercase() == expected)
        .collect();
    (candidates.len() == 1).then(|| candidates[0].clone())
}

pub fn parse_sha256_sidecar(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    let candidate = first
        .strip_prefix("sha256:")
        .or_else(|| first.strip_prefix("SHA256:"))
        .unwrap_or(first)
        .trim_matches('*');
    if candidate.len() == 64 && candidate.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(candidate.to_ascii_lowercase())
    } else {
        None
    }
}

pub fn is_allowed_release_asset_url(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https" && is_allowed_github_host(url.host_str().unwrap_or_default())
    })
}

pub async fn sha256_file_async(path: &Path) -> AppResult<String> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| AppError::io(&format!("Cannot open {}", path.display()), error))?;
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    let mut buffer = [0u8; DOWNLOAD_CHUNK];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> GitHubAsset {
        GitHubAsset {
            name: name.to_string(),
            size: 1,
            content_type: None,
            digest: None,
            browser_download_url: format!("https://github.com/example/releases/download/v1/{name}"),
        }
    }

    #[test]
    fn parses_sha256_sidecars() {
        let hash = "a".repeat(64);
        assert_eq!(
            parse_sha256_sidecar(&format!("{hash}  package.zip")),
            Some(hash.clone())
        );
        assert_eq!(
            parse_sha256_sidecar(&format!("sha256:{hash}")),
            Some(hash.clone())
        );
        assert_eq!(parse_sha256_sidecar("not-a-hash"), None);
    }

    #[test]
    fn sidecar_must_be_unambiguous() {
        let zip = asset("Plugin.ZIP");
        let sidecar = asset("Plugin.ZIP.SHA256");
        assert!(checksum_sidecar(&zip, &[sidecar.clone()]).is_some());
        assert!(checksum_sidecar(&zip, &[sidecar.clone(), sidecar]).is_none());
    }

    #[test]
    fn only_https_github_asset_urls_are_allowed() {
        assert!(is_allowed_release_asset_url(
            "https://github.com/owner/repo/releases/download/v1/x.zip"
        ));
        assert!(is_allowed_release_asset_url(
            "https://release-assets.githubusercontent.com/x"
        ));
        assert!(!is_allowed_release_asset_url("http://github.com/x.zip"));
        assert!(!is_allowed_release_asset_url("https://example.com/x.zip"));
        assert!(!is_allowed_release_asset_url("file:///C:/x.zip"));
    }
}
