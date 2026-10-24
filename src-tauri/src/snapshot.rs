use crate::error::{AppError, AppResult};
use crate::http::HttpValidators;
use crate::manifest::{ManifestApplication, PluginManifest};
use crate::release::GitHubRelease;
use crate::util::{
    banner_content_type, is_hex_commit, markdown_text, repo_path_segments, sha256_bytes,
    split_repository, validate_plugin_id, MAX_BANNER_BYTES, MAX_MANIFEST_BYTES,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const CACHE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileRole {
    Manifest,
    Banner,
    Readme,
    Features,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedFileRecord {
    pub role: FileRole,
    pub remote_path: String,
    pub cache_path: String,
    pub sha256: String,
    pub size: u64,
    pub content_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_sha: Option<String>,
    #[serde(default)]
    pub validators: HttpValidators,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSnapshot {
    pub cache_schema: u32,
    pub plugin_id: String,
    pub repository: String,
    pub git_ref: String,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_sha: Option<String>,
    pub manifest_path: String,
    pub fetched_at: DateTime<Utc>,
    #[serde(default)]
    pub ref_validators: HttpValidators,
    pub files: Vec<CachedFileRecord>,
    pub manifest: PluginManifest,
    #[serde(default)]
    pub releases: Vec<GitHubRelease>,
    #[serde(default)]
    pub releases_error: Option<String>,
    #[serde(default)]
    pub releases_validators: HttpValidators,
    #[serde(default)]
    pub release_fetched_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct CachedSnapshot {
    pub snapshot: PluginSnapshot,
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SnapshotAssets {
    pub banner: Vec<u8>,
    pub banner_content_type: String,
    pub readme: String,
    pub features: String,
}

impl PluginSnapshot {
    pub fn applications(&self) -> Vec<ManifestApplication> {
        self.manifest.application_summaries()
    }

    pub fn file(&self, role: FileRole) -> Option<&CachedFileRecord> {
        self.files.iter().find(|file| file.role == role)
    }

    pub fn cache_path_for(remote_path: &str) -> AppResult<String> {
        if remote_path == "." {
            return Err(AppError::new(
                "invalid_asset_path",
                "A plugin asset path cannot be the repository root.",
            ));
        }
        let segments = repo_path_segments(remote_path)?;
        Ok(format!("files/{}", segments.join("/")))
    }

    pub fn validate(&self) -> AppResult<()> {
        if self.cache_schema != CACHE_SCHEMA_VERSION {
            return Err(AppError::new(
                "unsupported_cache_schema",
                format!(
                    "Cached plugin data uses unsupported cache schema {}.",
                    self.cache_schema
                ),
            ));
        }
        validate_plugin_id(&self.plugin_id)?;
        split_repository(&self.repository)?;
        repo_path_segments(&self.manifest_path)?;
        if self.git_ref.trim().is_empty() {
            return Err(AppError::new(
                "invalid_cache",
                "Cached plugin data has no repository ref.",
            ));
        }
        if !is_hex_commit(&self.commit) {
            return Err(AppError::new(
                "invalid_cache",
                "Cached plugin data has an invalid commit.",
            ));
        }
        self.manifest.validate_metadata(&self.plugin_id)?;
        let assets = self.manifest.metadata_asset_paths(&self.manifest_path)?;

        let expected = [
            (FileRole::Manifest, self.manifest_path.clone()),
            (FileRole::Banner, assets.banner.clone()),
            (FileRole::Readme, assets.readme.clone()),
            (FileRole::Features, assets.features.clone()),
        ];
        let mut seen_roles = HashSet::new();
        let mut seen_cache_paths = HashSet::new();
        for (role, remote_path) in expected {
            let matches: Vec<&CachedFileRecord> =
                self.files.iter().filter(|file| file.role == role).collect();
            if matches.len() != 1 {
                return Err(AppError::new(
                    "invalid_cache",
                    format!(
                        "Cached plugin {} does not contain exactly one {} file.",
                        self.plugin_id,
                        role_name(role)
                    ),
                ));
            }
            let file = matches[0];
            if file.remote_path != remote_path {
                return Err(AppError::new(
                    "cache_mixed_content",
                    format!(
                        "Cached {} path '{}' does not match the manifest-declared path '{}'.",
                        role_name(role),
                        file.remote_path,
                        remote_path
                    ),
                ));
            }
            validate_cache_relative(&file.cache_path)?;
            let lower = file.cache_path.to_ascii_lowercase();
            if !seen_cache_paths.insert(lower) {
                return Err(AppError::new(
                    "invalid_cache",
                    "Cached plugin files share a destination path.",
                ));
            }
            if !seen_roles.insert(role) {
                return Err(AppError::new(
                    "invalid_cache",
                    "Cached plugin contains a duplicate file role.",
                ));
            }
            if file.size > MAX_BANNER_BYTES as u64 {
                return Err(AppError::new(
                    "invalid_cache",
                    "Cached plugin file exceeds the permitted size.",
                ));
            }
            if file.sha256.len() != 64 || !file.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(AppError::new(
                    "invalid_cache",
                    "Cached plugin file has an invalid SHA-256 digest.",
                ));
            }
            if file.remote_path.ends_with('/') {
                return Err(AppError::new(
                    "invalid_cache",
                    "Cached plugin file path points to a directory.",
                ));
            }
        }
        Ok(())
    }

    /// Validate every cached file against its recorded size and hash, so a
    /// partially written or corrupted snapshot is never exposed to the UI.
    pub fn validate_files(&self, snapshot_dir: &Path) -> AppResult<()> {
        self.validate()?;
        for record in &self.files {
            let path = snapshot_dir.join(cache_path_to_native(&record.cache_path)?);
            if !path.starts_with(snapshot_dir) {
                return Err(AppError::new(
                    "invalid_cache",
                    "Cached plugin file resolves outside its snapshot.",
                ));
            }
            let metadata = fs::symlink_metadata(&path).map_err(|_| {
                AppError::new(
                    "cache_incomplete",
                    format!("Cached plugin file is missing: {}", path.display()),
                )
            })?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(AppError::new(
                    "invalid_cache",
                    format!(
                        "Cached plugin path is not a regular file: {}",
                        path.display()
                    ),
                ));
            }
            if metadata.len() != record.size {
                return Err(AppError::new(
                    "cache_corrupt",
                    format!(
                        "Cached plugin file has an unexpected size: {}",
                        path.display()
                    ),
                ));
            }
            let bytes = fs::read(&path)?;
            if sha256_bytes(&bytes) != record.sha256 {
                return Err(AppError::new(
                    "cache_corrupt",
                    format!("Cached plugin file failed its checksum: {}", path.display()),
                ));
            }
        }
        // Also validate semantic content (banner magic, UTF-8 Markdown, and a
        // parseable manifest that matches the cached metadata) before a snapshot
        // can be considered complete.
        self.read_assets(snapshot_dir)?;
        Ok(())
    }

    pub fn read_assets(&self, snapshot_dir: &Path) -> AppResult<SnapshotAssets> {
        let manifest_file = self.file(FileRole::Manifest).ok_or_else(|| {
            AppError::new("cache_incomplete", "Cached plugin manifest is missing.")
        })?;
        let banner_file = self
            .file(FileRole::Banner)
            .ok_or_else(|| AppError::new("cache_incomplete", "Cached plugin banner is missing."))?;
        let readme_file = self.file(FileRole::Readme).ok_or_else(|| {
            AppError::new("cache_incomplete", "Cached plugin README.md is missing.")
        })?;
        let features_file = self.file(FileRole::Features).ok_or_else(|| {
            AppError::new("cache_incomplete", "Cached plugin FEATURES.md is missing.")
        })?;
        let manifest_bytes = read_cache_file(snapshot_dir, manifest_file, MAX_MANIFEST_BYTES)?;
        let parsed = PluginManifest::from_bytes(&manifest_bytes)?;
        if parsed.id != self.plugin_id
            || parsed.version != self.manifest.version
            || serde_json::to_value(&parsed).ok() != serde_json::to_value(&self.manifest).ok()
        {
            return Err(AppError::new(
                "cache_mixed_content",
                "Cached plugin.json does not match the cached metadata.",
            ));
        }
        let banner = read_cache_file(snapshot_dir, banner_file, MAX_BANNER_BYTES)?;
        let banner_content_type =
            banner_content_type(&banner_file.remote_path, &banner)?.to_string();
        let readme = markdown_text(
            &read_cache_file(snapshot_dir, readme_file, crate::util::MAX_MARKDOWN_BYTES)?,
            "README.md",
        )?;
        let features = markdown_text(
            &read_cache_file(snapshot_dir, features_file, crate::util::MAX_MARKDOWN_BYTES)?,
            "FEATURES.md",
        )?;
        Ok(SnapshotAssets {
            banner,
            banner_content_type,
            readme,
            features,
        })
    }
}

fn read_cache_file(
    snapshot_dir: &Path,
    record: &CachedFileRecord,
    limit: usize,
) -> AppResult<Vec<u8>> {
    let path = snapshot_dir.join(cache_path_to_native(&record.cache_path)?);
    let metadata = fs::metadata(&path)?;
    if metadata.len() > limit as u64 {
        return Err(AppError::new(
            "cache_corrupt",
            format!("Cached file exceeds the permitted size: {}", path.display()),
        ));
    }
    Ok(fs::read(path)?)
}

pub fn cache_path_to_native(cache_path: &str) -> AppResult<PathBuf> {
    validate_cache_relative(cache_path)?;
    Ok(cache_path
        .split('/')
        .fold(PathBuf::new(), |path, segment| path.join(segment)))
}

fn validate_cache_relative(cache_path: &str) -> AppResult<()> {
    if cache_path.is_empty()
        || cache_path.starts_with('/')
        || cache_path.starts_with('\\')
        || cache_path.contains(':')
        || cache_path.contains('\\')
    {
        return Err(AppError::new(
            "invalid_cache",
            format!("Invalid cache-relative file path: {cache_path}"),
        ));
    }
    for segment in cache_path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(AppError::new(
                "invalid_cache",
                format!("Invalid cache-relative file path: {cache_path}"),
            ));
        }
    }
    Ok(())
}

fn role_name(role: FileRole) -> &'static str {
    match role {
        FileRole::Manifest => "plugin.json",
        FileRole::Banner => "banner",
        FileRole::Readme => "README.md",
        FileRole::Features => "FEATURES.md",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::manifest::PluginManifest;

    pub fn test_snapshot(id: &str) -> PluginSnapshot {
        let manifest_bytes = br#"{
          "schemaVersion":1,
          "id":"example",
          "name":"Example",
          "description":"Description",
          "overview":"Overview",
          "version":"1.0.0",
          "assets":{"banner":"banner.png"},
          "content":{"readme":"README.md","features":"FEATURES.md"},
          "sourceDirectory":"files",
          "targets":[]
        }"#;
        let manifest = PluginManifest::from_bytes(manifest_bytes).unwrap();
        let banner = b"\x89PNG\r\n\x1a\nfake";
        let readme = b"# Read me";
        let features = b"# Features";
        let files = vec![
            CachedFileRecord {
                role: FileRole::Manifest,
                remote_path: "for installer/plugin.json".to_string(),
                cache_path: "plugin.json".to_string(),
                sha256: sha256_bytes(manifest_bytes),
                size: manifest_bytes.len() as u64,
                content_type: "application/json".to_string(),
                blob_sha: Some("blob-manifest".to_string()),
                validators: HttpValidators::default(),
            },
            CachedFileRecord {
                role: FileRole::Banner,
                remote_path: "for installer/banner.png".to_string(),
                cache_path: "files/for installer/banner.png".to_string(),
                sha256: sha256_bytes(banner),
                size: banner.len() as u64,
                content_type: "image/png".to_string(),
                blob_sha: Some("blob-banner".to_string()),
                validators: HttpValidators::default(),
            },
            CachedFileRecord {
                role: FileRole::Readme,
                remote_path: "for installer/README.md".to_string(),
                cache_path: "files/for installer/README.md".to_string(),
                sha256: sha256_bytes(readme),
                size: readme.len() as u64,
                content_type: "text/markdown".to_string(),
                blob_sha: Some("blob-readme".to_string()),
                validators: HttpValidators::default(),
            },
            CachedFileRecord {
                role: FileRole::Features,
                remote_path: "for installer/FEATURES.md".to_string(),
                cache_path: "files/for installer/FEATURES.md".to_string(),
                sha256: sha256_bytes(features),
                size: features.len() as u64,
                content_type: "text/markdown".to_string(),
                blob_sha: Some("blob-features".to_string()),
                validators: HttpValidators::default(),
            },
        ];
        PluginSnapshot {
            cache_schema: CACHE_SCHEMA_VERSION,
            plugin_id: id.to_string(),
            repository: "owner/repo".to_string(),
            git_ref: "main".to_string(),
            commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            tree_sha: None,
            manifest_path: "for installer/plugin.json".to_string(),
            fetched_at: Utc::now(),
            ref_validators: HttpValidators::default(),
            files,
            manifest,
            releases: vec![],
            releases_error: None,
            releases_validators: HttpValidators::default(),
            release_fetched_at: None,
        }
    }

    #[test]
    fn snapshot_validation_checks_complete_file_set_and_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let mut snapshot = test_snapshot("example");
        // Build exact files and records through the test helper by writing each
        // file and then recalculating its record.
        fs::create_dir_all(dir.path()).unwrap();
        for record in snapshot.files.iter_mut() {
            let bytes = match record.role {
                FileRole::Manifest => serde_json::to_vec(&snapshot.manifest).unwrap(),
                FileRole::Banner => b"\x89PNG\r\n\x1a\nfake".to_vec(),
                FileRole::Readme => b"# Read me".to_vec(),
                FileRole::Features => b"# Features".to_vec(),
            };
            record.sha256 = sha256_bytes(&bytes);
            record.size = bytes.len() as u64;
            let path = dir
                .path()
                .join(cache_path_to_native(&record.cache_path).unwrap());
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        snapshot.validate_files(dir.path()).unwrap();
        let assets = snapshot.read_assets(dir.path()).unwrap();
        assert_eq!(assets.readme, "# Read me");
        fs::write(
            dir.path()
                .join(cache_path_to_native(&snapshot.files[1].cache_path).unwrap()),
            b"tampered",
        )
        .unwrap();
        assert_eq!(
            snapshot.validate_files(dir.path()).unwrap_err().code,
            "cache_corrupt"
        );
    }

    #[test]
    fn cache_path_rejects_traversal_and_absolute_paths() {
        assert!(cache_path_to_native("files/../../x").is_err());
        assert!(cache_path_to_native("C:/x").is_err());
        assert!(cache_path_to_native("/x").is_err());
        assert!(PluginSnapshot::cache_path_for("for installer/README.md")
            .unwrap()
            .starts_with("files/"));
    }

    #[test]
    fn mismatched_asset_path_is_mixed_content() {
        let mut snapshot = test_snapshot("example");
        snapshot.files[1].remote_path = "for installer/other.png".to_string();
        assert_eq!(snapshot.validate().unwrap_err().code, "cache_mixed_content");
    }
}
