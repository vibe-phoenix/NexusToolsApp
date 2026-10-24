use crate::error::{AppError, AppResult};
use crate::http::HttpValidators;
use crate::snapshot::{CachedSnapshot, PluginSnapshot, CACHE_SCHEMA_VERSION};
use crate::util::{atomic_write, atomic_write_json, validate_plugin_id};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const CURRENT_POINTER: &str = "current.json";
const SNAPSHOT_METADATA: &str = "snapshot.json";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheIssue {
    pub plugin_id: Option<String>,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct CacheLoad {
    pub snapshots: Vec<CachedSnapshot>,
    pub issues: Vec<CacheIssue>,
}

#[derive(Debug, Clone)]
pub struct StagingSnapshot {
    pub plugin_id: String,
    pub snapshot_id: String,
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurrentPointer {
    snapshot_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedCatalog {
    pub cache_schema: u32,
    pub url: String,
    pub fetched_at: DateTime<Utc>,
    #[serde(default)]
    pub validators: HttpValidators,
    pub raw_json: String,
}

/// Per-user persistent metadata cache. This directory intentionally contains no
/// installed-plugin state: clearing the cache can never uninstall a plugin or
/// remove a user setting.
#[derive(Debug, Clone)]
pub struct CacheManager {
    root: PathBuf,
}

impl CacheManager {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn plugins_dir(&self) -> PathBuf {
        self.root.join("plugins")
    }

    fn plugin_dir(&self, plugin_id: &str) -> AppResult<PathBuf> {
        validate_plugin_id(plugin_id)?;
        Ok(self.plugins_dir().join(plugin_id))
    }

    fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    fn catalog_path(&self) -> PathBuf {
        self.root.join("catalog.json")
    }

    pub fn prepare(&self) -> AppResult<()> {
        fs::create_dir_all(self.plugins_dir())?;
        fs::create_dir_all(self.staging_dir())?;
        Ok(())
    }

    /// Remove abandoned staging directories. A previous crash may leave files
    /// there; they are never considered part of a visible snapshot.
    pub fn cleanup_stale_staging(&self) -> AppResult<()> {
        let staging = self.staging_dir();
        if !staging.exists() {
            return Ok(());
        }
        let now = std::time::SystemTime::now();
        for entry in fs::read_dir(&staging)? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let old = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age.as_secs() > 24 * 60 * 60);
            if old {
                let _ = fs::remove_dir_all(path);
            }
        }
        Ok(())
    }

    pub fn begin_staging(&self, plugin_id: &str) -> AppResult<StagingSnapshot> {
        validate_plugin_id(plugin_id)?;
        self.prepare()?;
        let snapshot_id = format!(
            "{}-{}",
            Utc::now().format("%Y%m%dT%H%M%S%3fZ"),
            &Uuid::new_v4().simple().to_string()[..8]
        );
        let dir = self
            .staging_dir()
            .join(format!("{plugin_id}-{snapshot_id}"));
        fs::create_dir_all(&dir)?;
        Ok(StagingSnapshot {
            plugin_id: plugin_id.to_string(),
            snapshot_id,
            dir,
        })
    }

    pub fn commit_staged(
        &self,
        staging: StagingSnapshot,
        snapshot: PluginSnapshot,
    ) -> AppResult<CachedSnapshot> {
        if staging.plugin_id != snapshot.plugin_id {
            return Err(AppError::new(
                "invalid_cache",
                "The staged cache directory belongs to a different plugin.",
            ));
        }
        snapshot.validate()?;
        let destination_plugin = self.plugin_dir(&snapshot.plugin_id)?;
        let snapshots_dir = destination_plugin.join("snapshots");
        fs::create_dir_all(&snapshots_dir)?;
        atomic_write_json(&staging.dir.join(SNAPSHOT_METADATA), &snapshot)?;
        snapshot.validate_files(&staging.dir)?;
        let destination = snapshots_dir.join(&staging.snapshot_id);
        if destination.exists() {
            let _ = fs::remove_dir_all(&staging.dir);
            return Err(AppError::new(
                "cache_collision",
                "A cache snapshot with the same identifier already exists.",
            ));
        }
        match fs::rename(&staging.dir, &destination) {
            Ok(()) => {}
            Err(error) => {
                let _ = fs::remove_dir_all(&staging.dir);
                return Err(AppError::io(
                    &format!(
                        "Cannot activate the validated cache snapshot at {}",
                        destination.display()
                    ),
                    error,
                ));
            }
        }
        let pointer = CurrentPointer {
            snapshot_id: staging.snapshot_id.clone(),
        };
        if let Err(error) = atomic_write_json(&destination_plugin.join(CURRENT_POINTER), &pointer) {
            // The old pointer still references the previous complete snapshot.
            return Err(error);
        }
        self.cleanup_old_snapshots(&snapshot.plugin_id, &staging.snapshot_id)?;
        Ok(CachedSnapshot {
            snapshot,
            dir: destination,
        })
    }

    fn cleanup_old_snapshots(&self, plugin_id: &str, current_id: &str) -> AppResult<()> {
        let snapshots_dir = self.plugin_dir(plugin_id)?.join("snapshots");
        let mut snapshot_dirs = list_directories(&snapshots_dir)?;
        snapshot_dirs.sort_by(|a, b| b.0.cmp(&a.0));
        // Keep the active snapshot and one previous validated snapshot so a
        // failed refresh or a corrupt current file can still fall back safely.
        let mut kept = 0;
        for (name, path) in snapshot_dirs {
            if name == current_id {
                kept += 1;
                continue;
            }
            if kept < 2 {
                kept += 1;
            } else {
                let _ = fs::remove_dir_all(path);
            }
        }
        Ok(())
    }

    pub fn load_plugin(
        &self,
        plugin_id: &str,
    ) -> AppResult<(Option<CachedSnapshot>, Option<CacheIssue>)> {
        validate_plugin_id(plugin_id)?;
        let plugin_dir = self.plugin_dir(plugin_id)?;
        if !plugin_dir.exists() {
            return Ok((None, None));
        }
        let snapshot_dirs = plugin_dir.join("snapshots");
        let pointer_path = plugin_dir.join(CURRENT_POINTER);
        let mut problem: Option<String> = None;
        let current_id = match fs::read(&pointer_path) {
            Ok(bytes) => match serde_json::from_slice::<CurrentPointer>(&bytes) {
                Ok(pointer) if is_safe_snapshot_id(&pointer.snapshot_id) => {
                    Some(pointer.snapshot_id)
                }
                Ok(_) => {
                    problem =
                        Some("The cache pointer contains an invalid snapshot id.".to_string());
                    None
                }
                Err(error) => {
                    problem = Some(format!("The cache pointer is invalid JSON: {error}"));
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                problem = Some(format!("The cache pointer cannot be read: {error}"));
                None
            }
        };

        if let Some(id) = current_id {
            let dir = snapshot_dirs.join(&id);
            match self.load_snapshot_dir(plugin_id, &dir) {
                Ok(snapshot) => return Ok((Some(snapshot), problem.map(issue_for(plugin_id)))),
                Err(error) => {
                    problem = Some(format!(
                        "The active cache snapshot was rejected: {}",
                        error.message
                    ));
                }
            }
        }

        let mut candidates = list_directories(&snapshot_dirs)?;
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, dir) in candidates {
            match self.load_snapshot_dir(plugin_id, &dir) {
                Ok(snapshot) => {
                    let pointer = CurrentPointer {
                        snapshot_id: dir
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                            .to_string(),
                    };
                    if !is_safe_snapshot_id(&pointer.snapshot_id) {
                        continue;
                    }
                    let _ = atomic_write_json(&pointer_path, &pointer);
                    let message = problem.unwrap_or_else(|| {
                        "Recovered a previous complete cached snapshot.".to_string()
                    });
                    return Ok((
                        Some(snapshot),
                        Some(CacheIssue {
                            plugin_id: Some(plugin_id.to_string()),
                            code: "cache_recovered".to_string(),
                            message,
                        }),
                    ));
                }
                Err(_) => continue,
            }
        }
        let issue = problem.map(|message| CacheIssue {
            plugin_id: Some(plugin_id.to_string()),
            code: "cache_unavailable".to_string(),
            message,
        });
        Ok((None, issue))
    }

    fn load_snapshot_dir(&self, plugin_id: &str, dir: &Path) -> AppResult<CachedSnapshot> {
        if !dir.is_dir() {
            return Err(AppError::new(
                "cache_incomplete",
                "The cached snapshot directory is missing.",
            ));
        }
        let metadata = fs::symlink_metadata(dir)?;
        if metadata.file_type().is_symlink() {
            return Err(AppError::new(
                "invalid_cache",
                "The cached snapshot is a symbolic link.",
            ));
        }
        let snapshot_file = dir.join(SNAPSHOT_METADATA);
        let bytes = fs::read(&snapshot_file).map_err(|error| {
            AppError::new(
                "cache_incomplete",
                format!("Cached plugin metadata cannot be read: {error}"),
            )
        })?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(AppError::new(
                "invalid_cache",
                "Cached plugin metadata is unreasonably large.",
            ));
        }
        let snapshot: PluginSnapshot = serde_json::from_slice(&bytes).map_err(|error| {
            AppError::new(
                "invalid_cache",
                format!("Cached plugin metadata is invalid: {error}"),
            )
        })?;
        if snapshot.plugin_id != plugin_id {
            return Err(AppError::new(
                "cache_mixed_content",
                "Cached plugin metadata belongs to a different plugin id.",
            ));
        }
        snapshot.validate_files(dir)?;
        Ok(CachedSnapshot {
            snapshot,
            dir: dir.to_path_buf(),
        })
    }

    pub fn load_all(&self) -> AppResult<CacheLoad> {
        self.prepare()?;
        let mut snapshots = Vec::new();
        let mut issues = Vec::new();
        for entry in fs::read_dir(self.plugins_dir())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if validate_plugin_id(&name).is_err() {
                continue;
            }
            match self.load_plugin(&name)? {
                (Some(snapshot), issue) => {
                    if let Some(issue) = issue {
                        issues.push(issue);
                    }
                    snapshots.push(snapshot);
                }
                (None, Some(issue)) => issues.push(issue),
                (None, None) => {}
            }
        }
        Ok(CacheLoad { snapshots, issues })
    }

    pub fn load_cached_catalog(&self) -> AppResult<Option<CachedCatalog>> {
        let path = self.catalog_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if bytes.len() > 10 * 1024 * 1024 {
            return Err(AppError::new(
                "invalid_cache",
                "Cached catalog data is unreasonably large.",
            ));
        }
        let catalog: CachedCatalog = match serde_json::from_slice(&bytes) {
            Ok(catalog) => catalog,
            Err(_) => return Ok(None),
        };
        if catalog.cache_schema != CACHE_SCHEMA_VERSION || catalog.raw_json.is_empty() {
            return Ok(None);
        }
        Ok(Some(catalog))
    }

    pub fn save_catalog(
        &self,
        url: &str,
        raw_json: &str,
        validators: HttpValidators,
    ) -> AppResult<CachedCatalog> {
        self.prepare()?;
        let cached = CachedCatalog {
            cache_schema: CACHE_SCHEMA_VERSION,
            url: url.to_string(),
            fetched_at: Utc::now(),
            validators,
            raw_json: raw_json.to_string(),
        };
        atomic_write_json(&self.catalog_path(), &cached)?;
        Ok(cached)
    }

    pub fn clear_all(&self) -> AppResult<()> {
        if self.root.exists() {
            fs::remove_dir_all(&self.root)?;
        }
        Ok(())
    }

    pub fn clear_plugin(&self, plugin_id: &str) -> AppResult<()> {
        let dir = self.plugin_dir(plugin_id)?;
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    pub fn cached_plugins(&self) -> AppResult<Vec<CachedSnapshot>> {
        Ok(self.load_all()?.snapshots)
    }
}

fn issue_for(plugin_id: &str) -> impl FnOnce(String) -> CacheIssue + '_ {
    move |message| CacheIssue {
        plugin_id: Some(plugin_id.to_string()),
        code: "cache_warning".to_string(),
        message,
    }
}

fn is_safe_snapshot_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !id.contains("..")
}

fn list_directories(path: &Path) -> AppResult<Vec<(String, PathBuf)>> {
    let mut values = Vec::new();
    if !path.exists() {
        return Ok(values);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        values.push((
            entry.file_name().to_string_lossy().to_string(),
            entry.path(),
        ));
    }
    Ok(values)
}

/// Create or update a cache file without ever writing into an activated
/// snapshot. Used by the refresh builder.
pub fn write_staged_file(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

pub fn remove_dir_if_exists(path: &Path) {
    if path.exists() {
        let _ = fs::remove_dir_all(path);
    }
}

pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

pub fn atomic_catalog_update(
    cache: &CacheManager,
    url: &str,
    raw_json: &str,
    validators: HttpValidators,
) -> AppResult<CachedCatalog> {
    cache.save_catalog(url, raw_json, validators)
}

// Keep these imports used in both test and non-test builds where paths are used
// by helper consumers.
#[allow(dead_code)]
fn _ensure_atomic_write_reference(path: &Path, bytes: &[u8]) -> AppResult<()> {
    atomic_write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::test_snapshot;
    use crate::snapshot::{cache_path_to_native, FileRole};
    use crate::util::sha256_bytes;
    use std::fs;

    fn write_files(snapshot: &mut PluginSnapshot, dir: &Path) {
        for record in snapshot.files.iter_mut() {
            let bytes = match record.role {
                FileRole::Manifest => serde_json::to_vec(&snapshot.manifest).unwrap(),
                FileRole::Banner => b"\x89PNG\r\n\x1a\nfake".to_vec(),
                FileRole::Readme => b"# Read me".to_vec(),
                FileRole::Features => b"# Features".to_vec(),
            };
            record.sha256 = sha256_bytes(&bytes);
            record.size = bytes.len() as u64;
            let path = dir.join(cache_path_to_native(&record.cache_path).unwrap());
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
    }

    fn commit(cache: &CacheManager, mut snapshot: PluginSnapshot) -> CachedSnapshot {
        let staging = cache.begin_staging(&snapshot.plugin_id).unwrap();
        write_files(&mut snapshot, &staging.dir);
        cache.commit_staged(staging, snapshot).unwrap()
    }

    #[test]
    fn complete_snapshot_loads_and_incomplete_snapshot_is_never_visible() {
        let temp = tempfile::tempdir().unwrap();
        let cache = CacheManager::new(temp.path().join("cache"));
        let first = commit(&cache, test_snapshot("example"));
        assert!(first.snapshot.validate_files(&first.dir).is_ok());
        let loaded = cache.load_plugin("example").unwrap().0.unwrap();
        assert_eq!(loaded.snapshot.plugin_id, "example");

        // Stage a replacement missing README/FEATURES and attempt to activate.
        let mut broken = test_snapshot("example");
        broken.commit = "fedcba9876543210fedcba9876543210fedcba98".to_string();
        let staging = cache.begin_staging("example").unwrap();
        // Only write files manually for roles that exist. Validation occurs
        // after snapshot.json, so the commit must fail.
        let manifest_bytes = serde_json::to_vec(&broken.manifest).unwrap();
        let manifest_path = staging.dir.join("plugin.json");
        fs::write(&manifest_path, &manifest_bytes).unwrap();
        for record in broken
            .files
            .iter_mut()
            .filter(|f| f.role == FileRole::Manifest)
        {
            record.sha256 = sha256_bytes(&manifest_bytes);
            record.size = manifest_bytes.len() as u64;
        }
        for record in broken.files.iter_mut() {
            if record.role != FileRole::Manifest {
                record.sha256 = sha256_bytes(b"missing");
                record.size = 7;
            }
        }
        let error = cache.commit_staged(staging, broken).unwrap_err();
        assert!(matches!(
            error.code.as_str(),
            "cache_incomplete" | "invalid_cache" | "io_error"
        ));

        let still_loaded = cache.load_plugin("example").unwrap().0.unwrap();
        assert_eq!(still_loaded.snapshot.commit, first.snapshot.commit);
    }

    #[test]
    fn corrupt_current_snapshot_falls_back_to_previous_complete_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let cache = CacheManager::new(temp.path().join("cache"));
        let first = commit(&cache, test_snapshot("example"));
        let mut second = test_snapshot("example");
        second.commit = "abcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string();
        second.fetched_at = chrono::Utc::now() + chrono::Duration::seconds(1);
        let second = commit(&cache, second);
        // Corrupt a file in the active snapshot.
        let readme = second.snapshot.file(FileRole::Readme).unwrap();
        fs::write(
            second
                .dir
                .join(cache_path_to_native(&readme.cache_path).unwrap()),
            b"corrupted",
        )
        .unwrap();
        let (loaded, issue) = cache.load_plugin("example").unwrap();
        let loaded = loaded.expect("previous snapshot should be available");
        assert_eq!(loaded.snapshot.commit, first.snapshot.commit);
        assert!(issue.is_some());
    }

    #[test]
    fn catalog_cache_roundtrips_and_check_clear_is_scoped_to_cache() {
        let temp = tempfile::tempdir().unwrap();
        let cache = CacheManager::new(temp.path().join("cache"));
        let state = temp.path().join("installed.json");
        fs::write(&state, b"installed-state").unwrap();
        cache.prepare().unwrap();
        cache
            .save_catalog(
                "https://raw.githubusercontent.com/example/catalog.json",
                "{\"schemaVersion\":1,\"plugins\":[]}",
                HttpValidators {
                    etag: Some("\"etag\"".to_string()),
                    last_modified: None,
                },
            )
            .unwrap();
        let loaded = cache.load_cached_catalog().unwrap().unwrap();
        assert_eq!(loaded.validators.etag.as_deref(), Some("\"etag\""));
        cache.clear_all().unwrap();
        assert!(cache.load_cached_catalog().unwrap().is_none());
        assert_eq!(fs::read_to_string(state).unwrap(), "installed-state");
    }

    #[test]
    fn current_pointer_is_safe_against_path_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let cache = CacheManager::new(temp.path().to_path_buf());
        cache.prepare().unwrap();
        let plugin_dir = temp.path().join("plugins/example");
        fs::create_dir_all(plugin_dir.join("snapshots")).unwrap();
        fs::write(
            plugin_dir.join(CURRENT_POINTER),
            br#"{"snapshotId":"../../escape"}"#,
        )
        .unwrap();
        let (loaded, _) = cache.load_plugin("example").unwrap();
        assert!(loaded.is_none());
    }
}
