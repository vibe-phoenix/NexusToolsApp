use crate::cache::{write_staged_file, CacheManager, StagingSnapshot};
use crate::catalog::{parse_catalog, CatalogEntry, CatalogIssue, DEFAULT_CATALOG_URL};
use crate::error::{AppError, AppResult};
use crate::http::{Conditional, GitHubClient, HttpValidators, TreeInfo};
use crate::manifest::PluginManifest;
use crate::release::GitHubRelease;
use crate::snapshot::{
    cache_path_to_native, CachedFileRecord, CachedSnapshot, FileRole, PluginSnapshot,
    SnapshotAssets, CACHE_SCHEMA_VERSION,
};
use crate::store::NexusPaths;
use crate::util::{
    banner_content_type, encode_data_url, markdown_text, sha256_bytes, MAX_BANNER_BYTES,
    MAX_MANIFEST_BYTES, MAX_MARKDOWN_BYTES,
};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct PluginState {
    pub cached: CachedSnapshot,
    pub cached_online: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginIssue {
    pub plugin_id: Option<String>,
    pub code: String,
    pub message: String,
    pub details: Vec<String>,
    pub using_cached: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogStatus {
    pub catalog_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<DateTime<Utc>>,
    pub using_cached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AppError>,
    pub plugin_count: usize,
    pub cached_plugin_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshReport {
    pub checked_at: DateTime<Utc>,
    pub plugin_count: usize,
    pub failed_plugin_count: usize,
    pub cached_plugin_count: usize,
    pub using_cached_catalog: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_error: Option<AppError>,
    pub issues: Vec<PluginIssue>,
}

#[derive(Clone)]
struct ServiceState {
    plugins: Vec<PluginState>,
    issues: Vec<PluginIssue>,
    status: CatalogStatus,
}

pub struct PluginService {
    client: GitHubClient,
    cache: CacheManager,
    catalog_url: String,
    state: RwLock<ServiceState>,
    refresh_lock: Mutex<()>,
}

impl PluginService {
    pub fn initialize(paths: &NexusPaths) -> AppResult<Self> {
        let cache = CacheManager::new(paths.cache_dir());
        cache.prepare()?;
        let _ = cache.cleanup_stale_staging();
        let client = GitHubClient::new()?;
        let loaded = cache.load_all()?;
        let mut issues = loaded
            .issues
            .into_iter()
            .map(|issue| PluginIssue {
                plugin_id: issue.plugin_id,
                code: issue.code,
                message: issue.message,
                details: vec![],
                using_cached: true,
            })
            .collect::<Vec<_>>();

        let cached_catalog = match cache.load_cached_catalog() {
            Ok(value) => value,
            Err(error) => {
                issues.push(PluginIssue {
                    plugin_id: None,
                    code: error.code.clone(),
                    message: error.message.clone(),
                    details: error.details.clone(),
                    using_cached: true,
                });
                None
            }
        };

        let mut allowed: Option<HashMap<String, CatalogEntry>> = None;
        if let Some(cached) = &cached_catalog {
            match parse_catalog(cached.raw_json.as_bytes()) {
                Ok(parsed) => {
                    for issue in parsed.issues {
                        issues.push(catalog_issue_to_plugin_issue(issue, true));
                    }
                    allowed = Some(
                        parsed
                            .entries
                            .into_iter()
                            .map(|entry| (entry.id.clone(), entry))
                            .collect(),
                    );
                }
                Err(error) => {
                    issues.push(PluginIssue {
                        plugin_id: None,
                        code: "cached_catalog_invalid".to_string(),
                        message: format!(
                            "The cached catalog was invalid; complete cached plugins were loaded independently: {}",
                            error.message
                        ),
                        details: error.details,
                        using_cached: true,
                    });
                }
            }
        }

        let mut states = Vec::new();
        for cached in loaded.snapshots {
            if let Some(allowed) = &allowed {
                let Some(entry) = allowed.get(&cached.snapshot.plugin_id) else {
                    continue;
                };
                if entry.repository != cached.snapshot.repository {
                    issues.push(PluginIssue {
                        plugin_id: Some(cached.snapshot.plugin_id.clone()),
                        code: "cached_repository_changed".to_string(),
                        message: format!(
                            "Cached plugin '{}' was omitted because the catalog now points to {} instead of {}.",
                            cached.snapshot.plugin_id, entry.repository, cached.snapshot.repository
                        ),
                        details: vec![],
                        using_cached: true,
                    });
                    continue;
                }
            }
            states.push(PluginState {
                cached,
                cached_online: true,
                warnings: vec![],
            });
        }
        sort_plugin_states(&mut states);

        let last_success = cached_catalog.as_ref().map(|catalog| catalog.fetched_at);
        let status = CatalogStatus {
            catalog_url: DEFAULT_CATALOG_URL.to_string(),
            last_checked_at: None,
            last_success_at: last_success,
            using_cached: true,
            error: None,
            plugin_count: states.len(),
            cached_plugin_count: states.len(),
        };
        Ok(Self {
            client,
            cache,
            catalog_url: DEFAULT_CATALOG_URL.to_string(),
            state: RwLock::new(ServiceState {
                plugins: states,
                issues,
                status,
            }),
            refresh_lock: Mutex::new(()),
        })
    }

    pub fn plugin_states(&self) -> Vec<PluginState> {
        self.state.read().plugins.clone()
    }

    pub fn issues(&self) -> Vec<PluginIssue> {
        self.state.read().issues.clone()
    }

    pub fn status(&self) -> CatalogStatus {
        self.state.read().status.clone()
    }

    pub fn find_state(&self, plugin_id: &str) -> Option<PluginState> {
        self.state
            .read()
            .plugins
            .iter()
            .find(|state| state.cached.snapshot.plugin_id == plugin_id)
            .cloned()
    }

    pub fn cached_assets(&self, state: &PluginState) -> AppResult<SnapshotAssets> {
        state.cached.snapshot.read_assets(&state.cached.dir)
    }

    pub fn cache_manager(&self) -> &CacheManager {
        &self.cache
    }

    pub fn github_client(&self) -> &GitHubClient {
        &self.client
    }

    pub fn catalog_url(&self) -> &str {
        &self.catalog_url
    }

    /// Refresh online metadata in the background or from the manual refresh
    /// button. Only one refresh runs at a time. Previously complete snapshots
    /// remain serviceable when any step fails.
    pub async fn refresh(&self) -> AppResult<RefreshReport> {
        let _guard = self.refresh_lock.try_lock().map_err(|_| {
            AppError::new(
                "refresh_in_progress",
                "A plugin refresh is already running. Cached plugins remain available.",
            )
        })?;
        let checked_at = Utc::now();
        let previous_states = self.plugin_states();
        let old_by_id: HashMap<String, PluginState> = previous_states
            .iter()
            .cloned()
            .map(|state| (state.cached.snapshot.plugin_id.clone(), state))
            .collect();

        let cached_catalog = self.cache.load_cached_catalog().ok().flatten();
        let fetched = match self
            .client
            .fetch_catalog(
                &self.catalog_url,
                cached_catalog.as_ref().map(|catalog| &catalog.validators),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return self.finish_catalog_failure(error, checked_at);
            }
        };

        let (catalog_bytes, validators, from_cache_only) = match fetched {
            Conditional::NotModified => match &cached_catalog {
                Some(catalog) => (
                    catalog.raw_json.clone().into_bytes(),
                    catalog.validators.clone(),
                    true,
                ),
                None => {
                    return self.finish_catalog_failure(
                        AppError::new(
                            "missing_catalog_cache",
                            "GitHub reported that the catalog was unchanged, but there is no valid cached catalog.",
                        ),
                        checked_at,
                    );
                }
            },
            Conditional::Modified {
                value: (bytes, returned_validators),
                validators,
            } => (
                bytes,
                if validators.is_empty() {
                    returned_validators
                } else {
                    validators
                },
                false,
            ),
        };

        let parsed = match parse_catalog(&catalog_bytes) {
            Ok(parsed) => parsed,
            Err(error) => return self.finish_catalog_failure(error, checked_at),
        };
        let catalog_json = match String::from_utf8(catalog_bytes) {
            Ok(text) => text,
            Err(_) => {
                return self.finish_catalog_failure(
                    AppError::new("invalid_catalog", "The catalog is not valid UTF-8 JSON."),
                    checked_at,
                );
            }
        };

        let mut cache_issues = Vec::new();
        if !from_cache_only {
            if let Err(error) =
                self.cache
                    .save_catalog(&self.catalog_url, &catalog_json, validators)
            {
                cache_issues.push(PluginIssue {
                    plugin_id: None,
                    code: "catalog_cache_write_failed".to_string(),
                    message: format!(
                        "The catalog was refreshed, but its cache could not be saved: {}",
                        error.message
                    ),
                    details: error.details,
                    using_cached: false,
                });
            }
        }

        let mut issues = parsed
            .issues
            .into_iter()
            .map(|issue| catalog_issue_to_plugin_issue(issue, false))
            .collect::<Vec<_>>();
        issues.extend(cache_issues);
        let mut states = Vec::new();
        let mut failed_plugin_count = 0usize;

        for entry in &parsed.entries {
            let old = old_by_id.get(&entry.id).cloned();
            match self.refresh_plugin(entry, old.as_ref()).await {
                Ok(mut state) => {
                    for warning in &state.warnings {
                        issues.push(PluginIssue {
                            plugin_id: Some(state.cached.snapshot.plugin_id.clone()),
                            code: "plugin_warning".to_string(),
                            message: warning.clone(),
                            details: vec![],
                            using_cached: true,
                        });
                    }
                    state.cached_online = false;
                    states.push(state);
                }
                Err(error) => {
                    failed_plugin_count += 1;
                    if let Some(old) = old {
                        let mut fallback = old;
                        fallback.cached_online = true;
                        let message = format!(
                            "Refresh failed; showing the last complete cached snapshot. {}",
                            error.message
                        );
                        fallback.warnings.push(message.clone());
                        issues.push(PluginIssue {
                            plugin_id: Some(entry.id.clone()),
                            code: "refresh_failed_cached".to_string(),
                            message: message.clone(),
                            details: error.details.clone(),
                            using_cached: true,
                        });
                        states.push(fallback);
                    } else {
                        issues.push(PluginIssue {
                            plugin_id: Some(entry.id.clone()),
                            code: "plugin_unavailable".to_string(),
                            message: format!(
                                "This plugin is unavailable because no complete cache exists and its metadata could not be downloaded: {}",
                                error.message
                            ),
                            details: error.details.clone(),
                            using_cached: false,
                        });
                    }
                }
            }
        }

        sort_plugin_states(&mut states);
        let cached_plugin_count = states.iter().filter(|state| state.cached_online).count();
        let status = CatalogStatus {
            catalog_url: self.catalog_url.clone(),
            last_checked_at: Some(checked_at),
            last_success_at: Some(checked_at),
            using_cached: cached_plugin_count > 0,
            error: None,
            plugin_count: states.len(),
            cached_plugin_count,
        };
        let report = RefreshReport {
            checked_at,
            plugin_count: states.len(),
            failed_plugin_count,
            cached_plugin_count,
            using_cached_catalog: from_cache_only,
            catalog_error: None,
            issues: issues.clone(),
        };
        {
            let mut write = self.state.write();
            write.plugins = states;
            write.issues = issues;
            write.status = status;
        }
        Ok(report)
    }

    fn finish_catalog_failure(
        &self,
        error: AppError,
        checked_at: DateTime<Utc>,
    ) -> AppResult<RefreshReport> {
        let mut write = self.state.write();
        for plugin in &mut write.plugins {
            plugin.cached_online = true;
        }
        let issue = PluginIssue {
            plugin_id: None,
            code: error.code.clone(),
            message: error.message.clone(),
            details: error.details.clone(),
            using_cached: true,
        };
        // Keep per-plugin diagnostics and add one catalog failure.
        if !write
            .issues
            .iter()
            .any(|existing| existing.code == error.code && existing.message == error.message)
        {
            write.issues.push(issue);
        }
        write.status.last_checked_at = Some(checked_at);
        write.status.using_cached = !write.plugins.is_empty();
        write.status.error = Some(error.clone());
        write.status.plugin_count = write.plugins.len();
        write.status.cached_plugin_count = write.plugins.iter().filter(|p| p.cached_online).count();
        let report = RefreshReport {
            checked_at,
            plugin_count: write.plugins.len(),
            failed_plugin_count: 0,
            cached_plugin_count: write.status.cached_plugin_count,
            using_cached_catalog: true,
            catalog_error: Some(error),
            issues: write.issues.clone(),
        };
        Ok(report)
    }

    async fn refresh_plugin(
        &self,
        entry: &CatalogEntry,
        old: Option<&PluginState>,
    ) -> AppResult<PluginState> {
        let reusable = old.filter(|state| {
            state.cached.snapshot.repository == entry.repository
                && state.cached.snapshot.manifest_path == entry.manifest_path
        });
        let old_ref_validators = reusable.map(|state| &state.cached.snapshot.ref_validators);
        let resolved = self
            .client
            .resolve_ref(&entry.repository, &entry.git_ref, old_ref_validators)
            .await?;
        let (commit, tree_sha, ref_validators) = match resolved {
            Conditional::Modified { value, validators } => (value.sha, value.tree_sha, validators),
            Conditional::NotModified => {
                let old = reusable.ok_or_else(|| {
                    AppError::new(
                        "missing_cached_revision",
                        "GitHub reported the branch was unchanged but there is no cached revision.",
                    )
                })?;
                (
                    old.cached.snapshot.commit.clone(),
                    old.cached.snapshot.tree_sha.clone(),
                    old.cached.snapshot.ref_validators.clone(),
                )
            }
        };

        if let Some(old) = reusable {
            if old.cached.snapshot.commit == commit
                && old.cached.snapshot.validate_files(&old.cached.dir).is_ok()
            {
                return self
                    .reuse_snapshot_for_same_commit(entry, old, tree_sha, ref_validators)
                    .await;
            }
        }

        self.full_refresh(entry, reusable, commit, tree_sha, ref_validators)
            .await
    }

    async fn reuse_snapshot_for_same_commit(
        &self,
        entry: &CatalogEntry,
        old: &PluginState,
        tree_sha: Option<String>,
        ref_validators: HttpValidators,
    ) -> AppResult<PluginState> {
        let (releases, releases_error, releases_validators, release_fetched_at) =
            self.refresh_releases(entry, Some(old), true).await;
        let releases_changed = serde_json::to_value(&releases).ok()
            != serde_json::to_value(&old.cached.snapshot.releases).ok();
        let should_rewrite = releases_changed
            || releases_error != old.cached.snapshot.releases_error
            || releases_validators != old.cached.snapshot.releases_validators
            || release_fetched_at != old.cached.snapshot.release_fetched_at;
        if !should_rewrite {
            let mut warnings = warnings_for_releases(&releases_error);
            if old.cached.snapshot.releases_error.is_some() {
                // An unchanged successful HTTP response supersedes an earlier
                // transient error.
                warnings.retain(|warning| !warning.contains("cached release"));
            }
            return Ok(PluginState {
                cached: old.cached.clone(),
                cached_online: false,
                warnings,
            });
        }

        let mut snapshot = old.cached.snapshot.clone();
        snapshot.tree_sha = tree_sha.or(snapshot.tree_sha);
        snapshot.ref_validators = ref_validators;
        snapshot.releases = releases;
        snapshot.releases_error = releases_error;
        snapshot.releases_validators = releases_validators;
        snapshot.release_fetched_at = release_fetched_at;
        snapshot.fetched_at = Utc::now();
        let staged = self.stage_reused_files(entry, &snapshot, &old.cached)?;
        let cached = self.cache.commit_staged(staged, snapshot)?;
        Ok(PluginState {
            warnings: warnings_for_releases(&cached.snapshot.releases_error),
            cached,
            cached_online: false,
        })
    }

    async fn full_refresh(
        &self,
        entry: &CatalogEntry,
        old: Option<&PluginState>,
        commit: String,
        tree_sha: Option<String>,
        ref_validators: HttpValidators,
    ) -> AppResult<PluginState> {
        let tree = match tree_sha.as_deref() {
            Some(tree_sha) => match self.client.fetch_tree(&entry.repository, tree_sha).await {
                Ok(tree) => Some(tree),
                Err(_) => None,
            },
            None => None,
        };
        let manifest_fetched = self
            .fetch_role_file(
                entry,
                old.map(|state| &state.cached),
                tree.as_ref(),
                FileRole::Manifest,
                &entry.manifest_path,
                MAX_MANIFEST_BYTES,
                &commit,
            )
            .await?;
        let manifest = PluginManifest::from_bytes(&manifest_fetched.1)?;
        manifest.validate_metadata(&entry.id)?;
        let paths = manifest.metadata_asset_paths(&entry.manifest_path)?;

        let banner = self
            .fetch_role_file(
                entry,
                old.map(|state| &state.cached),
                tree.as_ref(),
                FileRole::Banner,
                &paths.banner,
                MAX_BANNER_BYTES,
                &commit,
            )
            .await?;
        banner_content_type(&paths.banner, &banner.1)?;
        let readme = self
            .fetch_role_file(
                entry,
                old.map(|state| &state.cached),
                tree.as_ref(),
                FileRole::Readme,
                &paths.readme,
                MAX_MARKDOWN_BYTES,
                &commit,
            )
            .await?;
        markdown_text(&readme.1, "README.md")?;
        let features = self
            .fetch_role_file(
                entry,
                old.map(|state| &state.cached),
                tree.as_ref(),
                FileRole::Features,
                &paths.features,
                MAX_MARKDOWN_BYTES,
                &commit,
            )
            .await?;
        markdown_text(&features.1, "FEATURES.md")?;

        let (releases, releases_error, releases_validators, release_fetched_at) =
            self.refresh_releases(entry, old, false).await;

        let files = vec![manifest_fetched.0, banner.0, readme.0, features.0];
        let snapshot = PluginSnapshot {
            cache_schema: CACHE_SCHEMA_VERSION,
            plugin_id: entry.id.clone(),
            repository: entry.repository.clone(),
            git_ref: entry.git_ref.clone(),
            commit,
            tree_sha,
            manifest_path: entry.manifest_path.clone(),
            fetched_at: Utc::now(),
            ref_validators,
            files,
            manifest,
            releases,
            releases_error,
            releases_validators,
            release_fetched_at,
        };
        snapshot.validate()?;

        let staging = self.cache.begin_staging(&entry.id)?;
        let write_result = self.write_staged_snapshot(
            &staging,
            &snapshot,
            &[manifest_fetched.1, banner.1, readme.1, features.1],
        );
        if let Err(error) = write_result {
            let _ = fs::remove_dir_all(&staging.dir);
            return Err(error);
        }
        let cached = self.cache.commit_staged(staging, snapshot)?;
        Ok(PluginState {
            warnings: warnings_for_releases(&cached.snapshot.releases_error),
            cached,
            cached_online: false,
        })
    }

    async fn refresh_releases(
        &self,
        entry: &CatalogEntry,
        old: Option<&PluginState>,
        _same_commit: bool,
    ) -> (
        Vec<GitHubRelease>,
        Option<String>,
        HttpValidators,
        Option<DateTime<Utc>>,
    ) {
        let validators = old.map(|state| &state.cached.snapshot.releases_validators);
        match self
            .client
            .fetch_releases(&entry.repository, validators)
            .await
        {
            Ok(Conditional::NotModified) => match old {
                Some(old) => (
                    old.cached.snapshot.releases.clone(),
                    None,
                    old.cached.snapshot.releases_validators.clone(),
                    old.cached.snapshot.release_fetched_at,
                ),
                None => (
                    vec![],
                    Some(
                        "GitHub reported unchanged release data but no cached releases exist."
                            .to_string(),
                    ),
                    HttpValidators::default(),
                    old.and_then(|old| old.cached.snapshot.release_fetched_at),
                ),
            },
            Ok(Conditional::Modified { value, validators }) => {
                let mut releases = value.releases;
                releases.retain(|release| !release.draft);
                (releases, None, validators, Some(Utc::now()))
            }
            Err(error) => {
                let cached = old
                    .map(|state| state.cached.snapshot.releases.clone())
                    .unwrap_or_default();
                let message = if cached.is_empty() {
                    format!("Release information is unavailable: {}", error.message)
                } else {
                    format!(
                        "Release information could not be refreshed; showing cached release information. {}",
                        error.message
                    )
                };
                (
                    cached,
                    Some(message),
                    old.map(|state| state.cached.snapshot.releases_validators.clone())
                        .unwrap_or_default(),
                    old.and_then(|state| state.cached.snapshot.release_fetched_at),
                )
            }
        }
    }

    async fn fetch_role_file(
        &self,
        entry: &CatalogEntry,
        old: Option<&CachedSnapshot>,
        tree: Option<&TreeInfo>,
        role: FileRole,
        remote_path: &str,
        limit: usize,
        commit: &str,
    ) -> AppResult<(CachedFileRecord, Vec<u8>)> {
        let tree_blob_sha = tree
            .and_then(|tree| tree.entries.get(remote_path))
            .map(|entry| entry.blob_sha.clone());
        let old_match = old.and_then(|old| read_old_file(old, role, limit));
        if let (Some((old_record, old_bytes)), Some(tree_sha)) = (&old_match, &tree_blob_sha) {
            if old_record.blob_sha.as_deref() == Some(tree_sha.as_str())
                && sha256_bytes(old_bytes) == old_record.sha256
            {
                return Ok((
                    make_file_record(
                        role,
                        remote_path,
                        old_record.content_type.clone(),
                        tree_blob_sha,
                        old_record.validators.clone(),
                        old_bytes,
                    )?,
                    old_bytes.clone(),
                ));
            }
        }

        let conditional = old_match
            .as_ref()
            .map(|(record, _)| record.validators.clone())
            .filter(|validators| !validators.is_empty());
        let conditional_ref = conditional.as_ref();
        match self
            .client
            .fetch_raw(
                &entry.repository,
                commit,
                remote_path,
                limit,
                conditional_ref,
            )
            .await
        {
            Ok(Conditional::NotModified) => {
                let (old_record, old_bytes) = old_match.ok_or_else(|| {
                    AppError::new(
                        "missing_cached_file",
                        "GitHub reported a file was unchanged but no cached copy is available.",
                    )
                })?;
                if sha256_bytes(&old_bytes) != old_record.sha256 {
                    return Err(AppError::new(
                        "cache_corrupt",
                        "A cached plugin file failed its checksum while handling a 304 response.",
                    ));
                }
                let content_type = if role == FileRole::Banner {
                    banner_content_type(remote_path, &old_bytes)?.to_string()
                } else {
                    old_record.content_type.clone()
                };
                Ok((
                    make_file_record(
                        role,
                        remote_path,
                        content_type,
                        tree_blob_sha.or(old_record.blob_sha.clone()),
                        old_record.validators.clone(),
                        &old_bytes,
                    )?,
                    old_bytes,
                ))
            }
            Ok(Conditional::Modified { value, .. }) => {
                let content_type = match role {
                    FileRole::Manifest => "application/json".to_string(),
                    FileRole::Banner => banner_content_type(remote_path, &value.bytes)?.to_string(),
                    FileRole::Readme | FileRole::Features => "text/markdown".to_string(),
                };
                if role == FileRole::Readme {
                    markdown_text(&value.bytes, "README.md")?;
                }
                if role == FileRole::Features {
                    markdown_text(&value.bytes, "FEATURES.md")?;
                }
                let record = make_file_record(
                    role,
                    remote_path,
                    content_type,
                    tree_blob_sha,
                    value.validators,
                    &value.bytes,
                )?;
                Ok((record, value.bytes))
            }
            Err(mut error) => {
                error.details.push(format!(
                    "Required plugin metadata could not be loaded for {}: {remote_path}",
                    entry.id
                ));
                Err(error)
            }
        }
    }

    fn stage_reused_files(
        &self,
        entry: &CatalogEntry,
        snapshot: &PluginSnapshot,
        old: &CachedSnapshot,
    ) -> AppResult<StagingSnapshot> {
        let staging = self.cache.begin_staging(&entry.id)?;
        for record in &snapshot.files {
            let bytes = fs::read(old.dir.join(cache_path_to_native(&record.cache_path)?)).map_err(
                |error| {
                    AppError::io(
                        &format!("Cannot reuse cached plugin file {}", record.remote_path),
                        error,
                    )
                },
            )?;
            if sha256_bytes(&bytes) != record.sha256 {
                return Err(AppError::new(
                    "cache_corrupt",
                    format!(
                        "Cached plugin file {} changed while assembling a snapshot.",
                        record.remote_path
                    ),
                ));
            }
            let path = staging.dir.join(cache_path_to_native(&record.cache_path)?);
            write_staged_file(&path, &bytes)?;
        }
        Ok(staging)
    }

    fn write_staged_snapshot(
        &self,
        staging: &StagingSnapshot,
        snapshot: &PluginSnapshot,
        contents: &[Vec<u8>],
    ) -> AppResult<()> {
        if snapshot.files.len() != contents.len() {
            return Err(AppError::new(
                "internal_error",
                "Plugin file metadata and downloaded contents do not line up.",
            ));
        }
        for (record, bytes) in snapshot.files.iter().zip(contents.iter()) {
            if sha256_bytes(bytes) != record.sha256 {
                return Err(AppError::new(
                    "internal_error",
                    "Downloaded plugin file hash changed before staging.",
                ));
            }
            if bytes.len() as u64 != record.size {
                return Err(AppError::new(
                    "internal_error",
                    "Downloaded plugin file size changed before staging.",
                ));
            }
            let path = staging.dir.join(cache_path_to_native(&record.cache_path)?);
            write_staged_file(&path, bytes)?;
        }
        Ok(())
    }

    pub fn banner_data_url(&self, state: &PluginState, assets: &SnapshotAssets) -> String {
        let _ = state;
        encode_data_url(&assets.banner_content_type, &assets.banner)
    }

    pub async fn clear_cache(&self) -> AppResult<()> {
        let _guard = self.refresh_lock.try_lock().map_err(|_| {
            AppError::new(
                "refresh_in_progress",
                "A catalog refresh is running. Try clearing the metadata cache again after it finishes.",
            )
        })?;
        self.cache.clear_all()?;
        let mut write = self.state.write();
        write.plugins.clear();
        write.issues.clear();
        write.status.plugin_count = 0;
        write.status.cached_plugin_count = 0;
        write.status.using_cached = false;
        write.status.error = None;
        Ok(())
    }
}

fn make_file_record(
    role: FileRole,
    remote_path: &str,
    content_type: String,
    blob_sha: Option<String>,
    validators: HttpValidators,
    bytes: &[u8],
) -> AppResult<CachedFileRecord> {
    Ok(CachedFileRecord {
        role,
        remote_path: remote_path.to_string(),
        cache_path: PluginSnapshot::cache_path_for(remote_path)?,
        sha256: sha256_bytes(bytes),
        size: bytes.len() as u64,
        content_type,
        blob_sha,
        validators,
    })
}

fn read_old_file(
    old: &CachedSnapshot,
    role: FileRole,
    limit: usize,
) -> Option<(CachedFileRecord, Vec<u8>)> {
    let record = old.snapshot.file(role)?.clone();
    let path = old.dir.join(cache_path_to_native(&record.cache_path).ok()?);
    if !path.starts_with(&old.dir) {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    if bytes.len() > limit || bytes.len() as u64 != record.size {
        return None;
    }
    if sha256_bytes(&bytes) != record.sha256 {
        return None;
    }
    Some((record, bytes))
}

fn warnings_for_releases(releases_error: &Option<String>) -> Vec<String> {
    releases_error.iter().cloned().collect()
}

fn sort_plugin_states(states: &mut [PluginState]) {
    states.sort_by(|a, b| {
        a.cached
            .snapshot
            .manifest
            .name
            .to_ascii_lowercase()
            .cmp(&b.cached.snapshot.manifest.name.to_ascii_lowercase())
            .then_with(|| {
                a.cached
                    .snapshot
                    .plugin_id
                    .cmp(&b.cached.snapshot.plugin_id)
            })
    });
}

fn catalog_issue_to_plugin_issue(issue: CatalogIssue, using_cached: bool) -> PluginIssue {
    PluginIssue {
        plugin_id: issue.plugin_id,
        code: issue.code,
        message: issue.message,
        details: issue.details,
        using_cached,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_are_serializable_for_the_ui() {
        let issue = PluginIssue {
            plugin_id: Some("x".to_string()),
            code: "network".to_string(),
            message: "Could not refresh".to_string(),
            details: vec!["Retry later".to_string()],
            using_cached: true,
        };
        let json = serde_json::to_value(issue).unwrap();
        assert_eq!(json["usingCached"], true);
    }

    #[test]
    fn warning_list_tracks_release_fallbacks() {
        assert!(warnings_for_releases(&None).is_empty());
        assert_eq!(
            warnings_for_releases(&Some("cached release data".to_string())),
            vec!["cached release data".to_string()]
        );
    }
}
