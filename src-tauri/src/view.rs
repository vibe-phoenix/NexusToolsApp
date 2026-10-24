use crate::apps::{AppInstallation, AppInstallationView};
use crate::error::AppResult;
use crate::loader::{CatalogStatus, PluginIssue, PluginService, PluginState, RefreshReport};
use crate::manifest::ManifestApplication;
use crate::release::{build_release_views, ReleaseView};
use crate::store::{InstalledStore, InstalledTarget, NexusPaths, Settings};
use crate::util::encode_data_url;
use crate::version::versions_match;
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledTargetView {
    pub target_id: String,
    pub app: String,
    pub app_name: String,
    pub app_version: String,
    pub install_root: String,
    pub plugin_version: String,
    pub release_tag: String,
    pub status: String,
    pub installed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub file_count: usize,
    pub managed_block_count: usize,
    pub backup_dir: Option<String>,
    pub current: bool,
}

impl InstalledTargetView {
    fn from_target(target: &InstalledTarget, latest_version: Option<&str>) -> Self {
        Self {
            target_id: target.target_id.clone(),
            app: target.app.clone(),
            app_name: target.app_name.clone(),
            app_version: target.app_version.clone(),
            install_root: target.install_root.clone(),
            plugin_version: target.plugin_version.clone(),
            release_tag: target.release_tag.clone(),
            status: target.status.clone(),
            installed_at: target.installed_at,
            updated_at: target.updated_at,
            file_count: target.files.len(),
            managed_block_count: target.managed_blocks.len(),
            backup_dir: target.backup_dir.clone(),
            current: latest_version
                .map(|latest| versions_match(&target.plugin_version, latest))
                .unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPluginView {
    pub id: String,
    pub name: String,
    pub targets: Vec<InstalledTargetView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub overview: String,
    pub version: String,
    pub repository: String,
    pub git_ref: String,
    pub manifest_path: String,
    pub commit: String,
    pub fetched_at: DateTime<Utc>,
    pub applications: Vec<ManifestApplication>,
    pub releases: Vec<ReleaseView>,
    pub recommended_release_id: Option<i64>,
    pub install_unavailable: Option<String>,
    pub release_error: Option<String>,
    pub banner_data_url: String,
    pub readme_markdown: String,
    pub features_markdown: String,
    pub cached_online: bool,
    pub warnings: Vec<String>,
    pub installed_targets: Vec<InstalledTargetView>,
    pub installed: bool,
    pub update_available: bool,
    pub current_on_all_detected_targets: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bootstrap {
    pub product: String,
    pub version: String,
    pub plugins: Vec<PluginView>,
    pub installed_plugins: Vec<InstalledPluginView>,
    pub apps: Vec<AppInstallationView>,
    pub catalog: CatalogStatus,
    pub issues: Vec<PluginIssue>,
    pub settings: Settings,
    pub data_root: String,
    pub cache_root: String,
    pub recovery_warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh: Option<RefreshReport>,
}

pub fn build_bootstrap(
    service: &PluginService,
    store: &InstalledStore,
    _paths: &NexusPaths,
    apps: &[AppInstallation],
    settings: &Settings,
    recovery_warnings: &[String],
    refresh: Option<RefreshReport>,
) -> AppResult<Bootstrap> {
    let states = service.plugin_states();
    let registry = store.snapshot();
    let mut issues = service.issues();
    let mut plugins = Vec::new();
    for state in &states {
        match build_plugin_view(service, state, store, &registry) {
            Ok(view) => plugins.push(view),
            Err(error) => {
                issues.push(PluginIssue {
                    plugin_id: Some(state.cached.snapshot.plugin_id.clone()),
                    code: "plugin_asset_read_failed".to_string(),
                    message: format!(
                        "Plugin '{}' was hidden because its cached assets could not be read: {}",
                        state.cached.snapshot.manifest.name, error.message
                    ),
                    details: error.details,
                    using_cached: state.cached_online,
                });
            }
        }
    }
    let installed_plugins = registry
        .plugins
        .values()
        .map(|plugin| {
            let latest = states
                .iter()
                .find(|state| state.cached.snapshot.plugin_id == plugin.id)
                .map(|state| state.cached.snapshot.manifest.version.as_str());
            let mut targets: Vec<InstalledTargetView> = plugin
                .targets
                .values()
                .map(|target| InstalledTargetView::from_target(target, latest))
                .collect();
            targets.sort_by(|a, b| {
                a.app_name
                    .cmp(&b.app_name)
                    .then_with(|| a.app_version.cmp(&b.app_version))
                    .then_with(|| a.target_id.cmp(&b.target_id))
            });
            InstalledPluginView {
                id: plugin.id.clone(),
                name: plugin.name.clone(),
                targets,
            }
        })
        .collect();
    let mut app_views: Vec<AppInstallationView> = apps.iter().map(AppInstallation::view).collect();
    app_views.sort_by(|a, b| {
        a.app_name
            .cmp(&b.app_name)
            .then_with(|| b.version.cmp(&a.version))
            .then_with(|| a.install_root.cmp(&b.install_root))
    });

    Ok(Bootstrap {
        product: "NexusTools".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        plugins,
        installed_plugins,
        apps: app_views,
        catalog: service.status(),
        issues,
        settings: settings.clone(),
        data_root: service
            .cache_manager()
            .root()
            .parent()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default(),
        cache_root: service.cache_manager().root().to_string_lossy().to_string(),
        recovery_warnings: recovery_warnings.to_vec(),
        refresh,
    })
}

fn build_plugin_view(
    service: &PluginService,
    state: &PluginState,
    _store: &InstalledStore,
    registry: &crate::store::InstalledRegistry,
) -> AppResult<PluginView> {
    let snapshot = &state.cached.snapshot;
    let assets = service.cached_assets(state)?;
    let banner_data_url = encode_data_url(&assets.banner_content_type, &assets.banner);
    let releases = build_release_views(
        &snapshot.plugin_id,
        &snapshot.manifest.version,
        &snapshot.releases,
    );
    let recommended_release_id = releases
        .iter()
        .find(|release| release.recommended)
        .map(|release| release.id);
    let install_unavailable = if recommended_release_id.is_some() {
        None
    } else if releases.is_empty() {
        Some(
            snapshot
                .releases_error
                .clone()
                .unwrap_or_else(|| "This plugin has no published releases.".to_string()),
        )
    } else {
        Some(format!(
            "No stable release with an uploaded installation ZIP matches the catalog manifest version {}. Choose a published release in Past versions or ask the author to publish a matching release.",
            snapshot.manifest.version
        ))
    };
    let installed_targets: Vec<InstalledTargetView> = registry
        .plugins
        .get(&snapshot.plugin_id)
        .map(|plugin| {
            plugin
                .targets
                .values()
                .map(|target| {
                    InstalledTargetView::from_target(
                        target,
                        Some(snapshot.manifest.version.as_str()),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let installed = !installed_targets.is_empty();
    let update_available = installed_targets.iter().any(|target| !target.current);
    let current_on_all_detected_targets =
        !installed_targets.is_empty() && installed_targets.iter().all(|target| target.current);
    let mut warnings = state.warnings.clone();
    if let Some(release_error) = &snapshot.releases_error {
        if !warnings.iter().any(|warning| warning == release_error) {
            warnings.push(release_error.clone());
        }
    }
    Ok(PluginView {
        id: snapshot.plugin_id.clone(),
        name: snapshot.manifest.name.clone(),
        description: snapshot.manifest.description.clone(),
        overview: snapshot.manifest.overview.clone(),
        version: snapshot.manifest.version.clone(),
        repository: snapshot.repository.clone(),
        git_ref: snapshot.git_ref.clone(),
        manifest_path: snapshot.manifest_path.clone(),
        commit: snapshot.commit.clone(),
        fetched_at: snapshot.fetched_at,
        applications: snapshot.applications(),
        releases,
        recommended_release_id,
        install_unavailable,
        release_error: snapshot.releases_error.clone(),
        banner_data_url,
        readme_markdown: assets.readme,
        features_markdown: assets.features,
        cached_online: state.cached_online,
        warnings,
        installed_targets,
        installed,
        update_available,
        current_on_all_detected_targets,
    })
}

/// Compute which local target records could be updated to a given latest
/// version. Used by batch-update actions without any network request.
pub fn updatable_target_ids(store: &InstalledStore, plugin_id: &str, latest: &str) -> Vec<String> {
    store
        .plugin(plugin_id)
        .map(|plugin| {
            plugin
                .targets
                .values()
                .filter(|target| !versions_match(&target.plugin_version, latest))
                .map(|target| target.target_id.clone())
                .collect()
        })
        .unwrap_or_default()
}
