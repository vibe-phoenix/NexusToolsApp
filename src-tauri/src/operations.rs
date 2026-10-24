use crate::apps::{detect_applications, AppInstallation};
use crate::error::{AppError, AppResult};
use crate::http::{checksum_sidecar, parse_sha256_sidecar};
use crate::installer::{uninstall_target, InstallProgress, Installer, ReleaseRecord};
use crate::loader::PluginService;
use crate::manifest::{ManifestTarget, ValidatedAction};
use crate::package::{extract_zip, inspect_package, PackageWorkspace, ValidatedPackage};
use crate::release::{
    asset_sha256, find_release_for_install, select_zip_asset, versioned_releases, GitHubRelease,
    VersionedRelease,
};
use crate::store::{InstalledStore, NexusPaths};
use crate::util::{path_is_within, MAX_EXTRACTED_BYTES, MAX_PACKAGE_BYTES};
use crate::version::Version;
use serde::Serialize;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub type OperationProgress<'a> = dyn Fn(InstallProgress) + Send + Sync + 'a;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallRequest {
    pub plugin_id: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub release_id: Option<i64>,
    #[serde(default)]
    pub explicit_prerelease: bool,
    #[serde(default)]
    pub target_ids: Vec<String>,
    #[serde(default)]
    pub approved_outside_destinations: Vec<String>,
    #[serde(default)]
    pub job_id: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallRequest {
    pub plugin_id: String,
    #[serde(default)]
    pub target_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInstallOutcome {
    pub target_id: String,
    pub app: String,
    pub app_version: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AppError>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReport {
    pub plugin_id: String,
    pub plugin_name: String,
    pub version: String,
    pub release_tag: String,
    pub targets: Vec<TargetInstallOutcome>,
    pub all_succeeded: bool,
    pub package_sha256: String,
    pub checksum_verified: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetUninstallOutcome {
    pub target_id: String,
    pub app: String,
    pub app_version: String,
    pub success: bool,
    pub fully_removed: bool,
    pub removed_files: usize,
    pub removed_blocks: usize,
    pub skipped: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AppError>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallReport {
    pub plugin_id: String,
    pub targets: Vec<TargetUninstallOutcome>,
    pub all_removed: bool,
}

#[derive(Debug)]
struct PreparedTarget {
    app: AppInstallation,
    manifest_target: ManifestTarget,
    actions: Vec<ValidatedAction>,
}

pub async fn install_plugin(
    service: &PluginService,
    paths: &NexusPaths,
    store: &InstalledStore,
    request: InstallRequest,
    cancel: Arc<AtomicBool>,
    progress: &OperationProgress<'_>,
) -> AppResult<InstallReport> {
    let state = service.find_state(&request.plugin_id).ok_or_else(|| {
        AppError::new(
            "plugin_unavailable",
            "That plugin is not available in the current catalog.",
        )
    })?;
    let snapshot = &state.cached.snapshot;
    let settings = crate::apps::load_settings_for_detection(paths)?;
    let apps = detect_applications(&settings);
    if request.target_ids.is_empty() {
        return Err(AppError::new(
            "no_install_targets",
            "Select at least one detected application installation.",
        ));
    }
    let mut target_ids = Vec::new();
    for target_id in &request.target_ids {
        if !target_ids.contains(target_id) {
            target_ids.push(target_id.clone());
        }
    }
    let (release, selected_version) = find_release_for_install(
        &request.plugin_id,
        &snapshot.manifest.version,
        &snapshot.releases,
        request.version.as_deref(),
        request.release_id,
        request.explicit_prerelease,
    )?;
    let zip_asset = zip_asset_for(&release, &request.plugin_id, &selected_version)?;
    let mut expected_sha256 = asset_sha256(&zip_asset);
    let mut checksum_source = expected_sha256
        .as_ref()
        .map(|_| "GitHub asset digest".to_string());
    if expected_sha256.is_none() {
        if let Some(sidecar) = checksum_sidecar(&zip_asset, &release.assets) {
            let text = service
                .github_client()
                .fetch_checksum_text(&sidecar.browser_download_url, 16 * 1024)
                .await
                .map_err(|error| {
                    AppError::new(
                        "checksum_unavailable",
                        format!(
                            "The release publishes a checksum file, but NexusTools could not retrieve it: {}",
                            error.message
                        ),
                    )
                })?;
            let text = String::from_utf8(text).map_err(|_| {
                AppError::new(
                    "invalid_checksum",
                    "The release checksum file is not valid UTF-8 text.",
                )
            })?;
            expected_sha256 = parse_sha256_sidecar(&text);
            if expected_sha256.is_none() {
                return Err(AppError::new(
                    "invalid_checksum",
                    "The release checksum file did not contain a valid SHA-256 digest.",
                ));
            }
            checksum_source = Some("release .sha256 sidecar".to_string());
        }
    }

    let workspace = PackageWorkspace::new(&paths.package_staging_dir(), &request.plugin_id)?;
    let package_path = workspace.download_path();
    progress(InstallProgress {
        phase: "download".to_string(),
        percent: 1,
        message: format!("Downloading {}", zip_asset.name),
    });
    let download_cancel = cancel.clone();
    let progress_ref = progress;
    let downloaded_hash = service
        .github_client()
        .download_package(
            &zip_asset.browser_download_url,
            &package_path,
            Some(zip_asset.size),
            MAX_PACKAGE_BYTES.min(MAX_EXTRACTED_BYTES),
            &download_cancel,
            &move |done, total| {
                let percent = total
                    .filter(|total| *total > 0)
                    .map(|total| ((done.saturating_mul(40) / total).min(40)) as u8 + 2)
                    .unwrap_or(5);
                progress_ref(InstallProgress {
                    phase: "download".to_string(),
                    percent,
                    message: "Downloading plugin package".to_string(),
                });
            },
        )
        .await?;
    if let Some(expected) = &expected_sha256 {
        if !downloaded_hash.eq_ignore_ascii_case(expected) {
            return Err(AppError::new(
                "checksum_mismatch",
                format!(
                    "The downloaded package failed its SHA-256 verification (expected {expected}, got {downloaded_hash}). Nothing was installed."
                ),
            ));
        }
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(AppError::new("cancelled", "Installation was cancelled."));
    }
    progress(InstallProgress {
        phase: "extract".to_string(),
        percent: 44,
        message: "Validating and extracting the package".to_string(),
    });
    let extraction = workspace.extraction_dir();
    extract_zip(&package_path, &extraction)?;
    if cancel.load(Ordering::SeqCst) {
        return Err(AppError::new("cancelled", "Installation was cancelled."));
    }
    let package = inspect_package(
        &extraction,
        &request.plugin_id,
        &selected_version.to_string(),
    )?;
    if package.manifest.version != snapshot.manifest.version
        && !crate::version::versions_match(&package.manifest.version, &selected_version.to_string())
    {
        return Err(AppError::new(
            "bundled_manifest_version_mismatch",
            "The package's bundled manifest version does not match the selected release.",
        ));
    }

    let release_record = ReleaseRecord {
        repository: snapshot.repository.clone(),
        release_id: Some(release.id),
        release_tag: release.tag_name.clone(),
        release_version: selected_version.to_string(),
        asset_name: zip_asset.name.clone(),
        asset_sha256: downloaded_hash.clone(),
        digest_verified: expected_sha256.is_some(),
    };

    let mut outcomes: Vec<TargetInstallOutcome> = Vec::new();
    let mut prepared: Vec<PreparedTarget> = Vec::new();
    for target_id in &target_ids {
        let Some(app) = apps.iter().find(|app| &app.target_id == target_id).cloned() else {
            outcomes.push(TargetInstallOutcome {
                target_id: target_id.clone(),
                app: "unknown".to_string(),
                app_version: String::new(),
                success: false,
                installed_version: None,
                error: Some(AppError::new(
                    "target_not_detected",
                    "That application installation is no longer detected. Refresh applications in Settings or add its path manually.",
                )),
                warnings: vec![],
            });
            continue;
        };
        let manifest_target = match package.manifest.matching_target(app.app.id(), &app.version) {
            Ok(target) => target.clone(),
            Err(error) => {
                outcomes.push(TargetInstallOutcome {
                    target_id: app.target_id.clone(),
                    app: app.app.display_name().to_string(),
                    app_version: app.version.clone(),
                    success: false,
                    installed_version: None,
                    error: Some(error),
                    warnings: vec![],
                });
                continue;
            }
        };
        let actions = match package.manifest.validated_actions(&manifest_target) {
            Ok(actions) => actions,
            Err(error) => {
                outcomes.push(TargetInstallOutcome {
                    target_id: app.target_id.clone(),
                    app: app.app.display_name().to_string(),
                    app_version: app.version.clone(),
                    success: false,
                    installed_version: None,
                    error: Some(error),
                    warnings: vec![],
                });
                continue;
            }
        };
        prepared.push(PreparedTarget {
            app,
            manifest_target,
            actions,
        });
    }

    let installer = Installer { paths, store };
    // Preflight every target before modifying a single file. Required outside
    // destinations are collected across all targets so one approval covers the
    // complete operation.
    let mut outside_paths: BTreeSet<String> = BTreeSet::new();
    let mut preflight_failed_ids: BTreeSet<String> = BTreeSet::new();
    for target in &prepared {
        progress(InstallProgress {
            phase: "preflight".to_string(),
            percent: 52,
            message: format!(
                "Checking {} {}",
                target.app.app.display_name(),
                target.app.version
            ),
        });
        match installer.preflight_target(
            &request.plugin_id,
            &package,
            &target.app,
            &target.manifest_target,
            &target.actions,
            &release_record,
            &request.approved_outside_destinations,
            &cancel,
        ) {
            Ok(paths) => outside_paths.extend(paths),
            Err(error) if error.code == "cancelled" => {
                return Err(error);
            }
            Err(error) => {
                preflight_failed_ids.insert(target.app.target_id.clone());
                outcomes.push(TargetInstallOutcome {
                    target_id: target.app.target_id.clone(),
                    app: target.app.app.display_name().to_string(),
                    app_version: target.app.version.clone(),
                    success: false,
                    installed_version: None,
                    error: Some(error),
                    warnings: vec![],
                });
            }
        }
    }
    if !outside_paths.is_empty() {
        return Err(AppError::with_details(
            "outside_destination_roots",
            "This plugin needs to write outside NexusTools's managed application folders. Review these exact paths and approve them to continue. No files have been installed.",
            outside_paths.into_iter().collect::<Vec<_>>(),
        ));
    }
    if prepared.is_empty() {
        return Ok(InstallReport {
            plugin_id: request.plugin_id,
            plugin_name: snapshot.manifest.name.clone(),
            version: package.manifest.version.clone(),
            release_tag: release.tag_name.clone(),
            targets: outcomes,
            all_succeeded: false,
            package_sha256: downloaded_hash,
            checksum_verified: expected_sha256.is_some(),
        });
    }

    for (index, target) in prepared.iter().enumerate() {
        if preflight_failed_ids.contains(&target.app.target_id) {
            continue;
        }
        if cancel.load(Ordering::SeqCst) {
            outcomes.push(TargetInstallOutcome {
                target_id: target.app.target_id.clone(),
                app: target.app.app.display_name().to_string(),
                app_version: target.app.version.clone(),
                success: false,
                installed_version: None,
                error: Some(AppError::new("cancelled", "Installation was cancelled.")),
                warnings: vec![],
            });
            continue;
        }
        let base_percent = 58 + ((index * 35) / prepared.len().max(1)) as u8;
        let span = 35 / prepared.len().max(1) as u8;
        let app_name = target.app.app.display_name().to_string();
        let version = target.app.version.clone();
        let target_progress = |mut event: InstallProgress| {
            event.percent = base_percent
                .saturating_add(event.percent.saturating_mul(span) / 100)
                .min(99);
            event.message = format!("{app_name} {version}: {}", event.message);
            progress(event);
        };
        match installer.install_target(
            &request.plugin_id,
            &package,
            &target.app,
            &target.manifest_target,
            &target.actions,
            &release_record,
            &request.approved_outside_destinations,
            &cancel,
            &target_progress,
        ) {
            Ok(result) => {
                let warnings = result.warnings;
                outcomes.push(TargetInstallOutcome {
                    target_id: result.target_id,
                    app: result.app,
                    app_version: result.app_version,
                    success: true,
                    installed_version: Some(result.installed_version),
                    error: None,
                    warnings,
                });
            }
            Err(error) => {
                outcomes.push(TargetInstallOutcome {
                    target_id: target.app.target_id.clone(),
                    app: target.app.app.display_name().to_string(),
                    app_version: target.app.version.clone(),
                    success: false,
                    installed_version: None,
                    error: Some(error),
                    warnings: vec![],
                });
            }
        }
    }

    let all_succeeded = !outcomes.is_empty() && outcomes.iter().all(|outcome| outcome.success);
    progress(InstallProgress {
        phase: "complete".to_string(),
        percent: 100,
        message: if all_succeeded {
            "All selected installations completed".to_string()
        } else {
            "Installation finished with problems".to_string()
        },
    });
    Ok(InstallReport {
        plugin_id: request.plugin_id,
        plugin_name: snapshot.manifest.name.clone(),
        version: package.manifest.version.clone(),
        release_tag: release.tag_name.clone(),
        targets: outcomes,
        all_succeeded,
        package_sha256: downloaded_hash,
        checksum_verified: expected_sha256.is_some(),
    })
}

fn zip_asset_for(
    release: &GitHubRelease,
    plugin_id: &str,
    version: &Version,
) -> AppResult<crate::release::GitHubAsset> {
    select_zip_asset(plugin_id, Some(version), &release.assets)
        .map_err(|message| AppError::new("ambiguous_release_package", message))?
        .ok_or_else(|| {
            AppError::new(
                "missing_release_package",
                "The selected release has no uploaded installation ZIP.",
            )
        })
}

pub fn uninstall_plugins(
    paths: &NexusPaths,
    store: &InstalledStore,
    request: UninstallRequest,
) -> AppResult<UninstallReport> {
    let plugin = store.plugin(&request.plugin_id).ok_or_else(|| {
        AppError::new(
            "not_installed",
            format!("Plugin '{}' is not installed.", request.plugin_id),
        )
    })?;
    let target_ids: Vec<String> = if request.target_ids.is_empty() {
        plugin.targets.keys().cloned().collect()
    } else {
        request.target_ids.clone()
    };
    let mut outcomes = Vec::new();
    for target_id in target_ids {
        let Some(target) = plugin.targets.get(&target_id) else {
            outcomes.push(TargetUninstallOutcome {
                target_id,
                app: String::new(),
                app_version: String::new(),
                success: false,
                fully_removed: false,
                removed_files: 0,
                removed_blocks: 0,
                skipped: vec![],
                error: Some(AppError::new(
                    "target_not_installed",
                    "No local installation record exists for that target.",
                )),
            });
            continue;
        };
        match uninstall_target(paths, store, &request.plugin_id, &target_id) {
            Ok(result) => outcomes.push(TargetUninstallOutcome {
                target_id: result.target_id,
                app: result.app,
                app_version: result.app_version,
                success: true,
                fully_removed: result.fully_removed,
                removed_files: result.removed_files,
                removed_blocks: result.removed_blocks,
                skipped: result.skipped,
                error: None,
            }),
            Err(error) => outcomes.push(TargetUninstallOutcome {
                target_id,
                app: target.app_name.clone(),
                app_version: target.app_version.clone(),
                success: false,
                fully_removed: false,
                removed_files: 0,
                removed_blocks: 0,
                skipped: vec![],
                error: Some(error),
            }),
        }
    }
    let all_removed = !outcomes.is_empty()
        && outcomes
            .iter()
            .all(|outcome| outcome.success && outcome.fully_removed);
    Ok(UninstallReport {
        plugin_id: request.plugin_id,
        targets: outcomes,
        all_removed,
    })
}

/// Find all locally recorded target versions, used by the UI for independent
/// status labels. This never performs network access.
pub fn installed_versions_for(store: &InstalledStore, plugin_id: &str) -> Vec<(String, String)> {
    store
        .plugin(plugin_id)
        .map(|plugin| {
            plugin
                .targets
                .values()
                .map(|target| (target.target_id.clone(), target.plugin_version.clone()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn selected_release_for(
    snapshot_releases: &[GitHubRelease],
    release_id: i64,
) -> Option<GitHubRelease> {
    versioned_releases(snapshot_releases)
        .0
        .into_iter()
        .find(|item: &VersionedRelease| item.release.id == release_id)
        .map(|item| item.release)
}

pub fn path_inside(scope: &std::path::Path, path: &std::path::Path) -> bool {
    path_is_within(path, scope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::GitHubAsset;

    fn asset(name: &str) -> GitHubAsset {
        GitHubAsset {
            name: name.to_string(),
            size: 10,
            content_type: Some("application/zip".to_string()),
            digest: None,
            browser_download_url: format!("https://github.com/example/releases/download/v1/{name}"),
        }
    }

    #[test]
    fn zip_asset_selection_reports_ambiguity() {
        let release = GitHubRelease {
            id: 1,
            tag_name: "v1.0.0".to_string(),
            name: None,
            draft: false,
            prerelease: false,
            published_at: None,
            created_at: None,
            body: None,
            assets: vec![asset("one.zip"), asset("two.zip")],
        };
        assert_eq!(
            zip_asset_for(&release, "example", &Version::parse("1.0.0").unwrap())
                .unwrap_err()
                .code,
            "ambiguous_release_package"
        );
    }

    #[test]
    fn versioned_release_lookup_uses_local_cache_only() {
        let release = GitHubRelease {
            id: 7,
            tag_name: "v1.0.1".to_string(),
            name: None,
            draft: false,
            prerelease: false,
            published_at: None,
            created_at: None,
            body: None,
            assets: vec![asset("plugin.zip")],
        };
        assert_eq!(selected_release_for(&[release.clone()], 7).unwrap().id, 7);
        assert!(selected_release_for(&[release], 8).is_none());
    }

    #[test]
    fn path_boundary_helper_is_inclusive() {
        let root = std::path::Path::new(r"C:\Users\Test\NexusTools");
        assert!(path_inside(
            root,
            std::path::Path::new(r"C:\Users\Test\NexusTools\a")
        ));
        assert!(!path_inside(
            root,
            std::path::Path::new(r"C:\Users\Test\NexusTools2\a")
        ));
    }
}
