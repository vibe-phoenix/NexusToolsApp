use crate::apps::{validate_override, AppKind, AppOverride};
use crate::error::{AppError, AppResult};
use crate::util::{
    atomic_write_json, normalize_absolute_path, split_repository, validate_plugin_id,
};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

pub const STATE_SCHEMA_VERSION: u32 = 1;

/// All mutable NexusTools data lives under the per-user LocalAppData folder,
/// never under the application installation directory. The metadata cache is a
/// child of this root but is managed separately and can be cleared without
/// affecting settings or installed records.
#[derive(Debug, Clone)]
pub struct NexusPaths {
    root: PathBuf,
}

impl NexusPaths {
    pub fn for_current_user() -> AppResult<Self> {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(dirs::data_local_dir)
            .ok_or_else(|| {
                AppError::new(
                    "missing_local_app_data",
                    "Windows LocalAppData could not be resolved. NexusTools needs a per-user writable data folder.",
                )
            })?;
        Ok(Self::new(local.join("NexusTools")))
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    pub fn installed_state_file(&self) -> PathBuf {
        self.root.join("installed-state.json")
    }

    pub fn transactions_dir(&self) -> PathBuf {
        self.root.join("transactions")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    pub fn package_staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    pub fn plugin_data_dir(&self, plugin_id: &str, target_id: &str) -> PathBuf {
        self.root
            .join("plugin-data")
            .join(plugin_id)
            .join(target_id)
    }

    pub fn prepare(&self) -> AppResult<()> {
        for dir in [
            self.cache_dir(),
            self.transactions_dir(),
            self.backups_dir(),
            self.package_staging_dir(),
        ] {
            fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledFileRecord {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    #[serde(default)]
    pub obsolete: bool,
    #[serde(default)]
    pub preserved_modified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedBlockRecord {
    pub path: String,
    pub marker: String,
    pub comment_prefix: String,
    pub block_sha256: String,
    #[serde(default)]
    pub obsolete: bool,
    #[serde(default)]
    pub preserved_modified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledTarget {
    pub target_id: String,
    pub app: String,
    pub app_name: String,
    pub app_version: String,
    pub install_root: String,
    pub repository: String,
    pub plugin_version: String,
    #[serde(default)]
    pub release_id: Option<i64>,
    pub release_tag: String,
    pub asset_name: String,
    pub asset_sha256: String,
    #[serde(default)]
    pub digest_verified: bool,
    pub installed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: String,
    pub operation_id: String,
    pub files: Vec<InstalledFileRecord>,
    pub directories_created: Vec<String>,
    pub managed_blocks: Vec<ManagedBlockRecord>,
    #[serde(default)]
    pub backup_dir: Option<String>,
    pub bundled_manifest: serde_json::Value,
    #[serde(default)]
    pub resolved_paths: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPlugin {
    pub id: String,
    pub name: String,
    pub targets: BTreeMap<String, InstalledTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledRegistry {
    pub schema_version: u32,
    pub plugins: BTreeMap<String, InstalledPlugin>,
}

impl Default for InstalledRegistry {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            plugins: BTreeMap::new(),
        }
    }
}

impl InstalledRegistry {
    pub fn all_targets(&self) -> Vec<(&InstalledPlugin, &InstalledTarget)> {
        self.plugins
            .values()
            .flat_map(|plugin| plugin.targets.values().map(move |target| (plugin, target)))
            .collect()
    }

    pub fn latest_installed_version(&self, plugin_id: &str) -> Option<String> {
        self.plugins.get(plugin_id).and_then(|plugin| {
            plugin
                .targets
                .values()
                .next()
                .map(|t| t.plugin_version.clone())
        })
    }

    pub fn target(&self, plugin_id: &str, target_id: &str) -> Option<&InstalledTarget> {
        self.plugins
            .get(plugin_id)
            .and_then(|plugin| plugin.targets.get(target_id))
    }
}

pub struct InstalledStore {
    path: PathBuf,
    registry: Mutex<InstalledRegistry>,
}

impl InstalledStore {
    pub fn open(path: PathBuf) -> AppResult<Self> {
        let registry = if path.exists() {
            let bytes = fs::read(&path)?;
            if bytes.len() > 32 * 1024 * 1024 {
                return Err(AppError::new(
                    "invalid_installed_state",
                    "The installed plugin state file is unreasonably large.",
                ));
            }
            let registry: InstalledRegistry = serde_json::from_slice(&bytes).map_err(|error| {
                AppError::new(
                    "invalid_installed_state",
                    format!(
                        "The installed plugin state cannot be read ({error}). NexusTools stopped instead of guessing which files it owns. The file is {}.",
                        path.display()
                    ),
                )
            })?;
            if registry.schema_version != STATE_SCHEMA_VERSION {
                return Err(AppError::new(
                    "unsupported_installed_state",
                    format!(
                        "Installed plugin state uses unsupported schema {}.",
                        registry.schema_version
                    ),
                ));
            }
            registry
        } else {
            InstalledRegistry::default()
        };
        Ok(Self {
            path,
            registry: Mutex::new(registry),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn snapshot(&self) -> InstalledRegistry {
        self.registry.lock().clone()
    }

    pub fn plugin(&self, plugin_id: &str) -> Option<InstalledPlugin> {
        self.registry.lock().plugins.get(plugin_id).cloned()
    }

    pub fn target(&self, plugin_id: &str, target_id: &str) -> Option<InstalledTarget> {
        self.registry.lock().target(plugin_id, target_id).cloned()
    }

    pub fn previous_target_for_rollback(
        &self,
        plugin_id: &str,
        target_id: &str,
    ) -> Option<InstalledTarget> {
        self.target(plugin_id, target_id)
    }

    pub fn put_target(
        &self,
        plugin_id: &str,
        plugin_name: &str,
        target: InstalledTarget,
    ) -> AppResult<()> {
        validate_plugin_id(plugin_id)?;
        self.registry
            .lock()
            .save_target(plugin_id, plugin_name, target, &self.path)
    }

    pub fn put_target_without_save(
        &self,
        plugin_id: &str,
        plugin_name: &str,
        target: InstalledTarget,
    ) -> AppResult<()> {
        validate_plugin_id(plugin_id)?;
        self.registry
            .lock()
            .put_target_in_memory(plugin_id, plugin_name, target);
        Ok(())
    }

    pub fn save(&self) -> AppResult<()> {
        let registry = self.registry.lock().clone();
        atomic_write_json(&self.path, &registry)
    }

    pub fn replace_target(
        &self,
        plugin_id: &str,
        plugin_name: &str,
        target: InstalledTarget,
    ) -> AppResult<()> {
        self.put_target(plugin_id, plugin_name, target)
    }

    pub fn remove_target(&self, plugin_id: &str, target_id: &str) -> AppResult<()> {
        let mut registry = self.registry.lock();
        if let Some(plugin) = registry.plugins.get_mut(plugin_id) {
            plugin.targets.remove(target_id);
            if plugin.targets.is_empty() {
                registry.plugins.remove(plugin_id);
            }
        }
        atomic_write_json(&self.path, &*registry)
    }

    pub fn restore_target(
        &self,
        plugin_id: &str,
        target_id: &str,
        previous: Option<InstalledTarget>,
    ) -> AppResult<()> {
        let mut registry = self.registry.lock();
        match previous {
            Some(previous) => {
                let entry = registry
                    .plugins
                    .entry(plugin_id.to_string())
                    .or_insert_with(|| InstalledPlugin {
                        id: plugin_id.to_string(),
                        name: plugin_id.to_string(),
                        targets: BTreeMap::new(),
                    });
                entry.targets.insert(target_id.to_string(), previous);
            }
            None => {
                if let Some(plugin) = registry.plugins.get_mut(plugin_id) {
                    plugin.targets.remove(target_id);
                    if plugin.targets.is_empty() {
                        registry.plugins.remove(plugin_id);
                    }
                }
            }
        }
        atomic_write_json(&self.path, &*registry)
    }

    pub fn set_registry(&self, registry: InstalledRegistry) -> AppResult<()> {
        if registry.schema_version != STATE_SCHEMA_VERSION {
            return Err(AppError::new(
                "unsupported_installed_state",
                "Cannot save an unsupported installed-state schema.",
            ));
        }
        atomic_write_json(&self.path, &registry)?;
        *self.registry.lock() = registry;
        Ok(())
    }

    pub fn save_target(repository: &str, target: &InstalledTarget) -> AppResult<()> {
        split_repository(repository)?;
        validate_installed_target(target)
    }

    pub fn ownership_conflicts(
        &self,
        plugin_id: &str,
        target_id: &str,
        destination: &Path,
    ) -> Option<OwnershipEntry> {
        let registry = self.registry.lock();
        find_owner(&registry, plugin_id, target_id, destination)
    }
}

impl InstalledRegistry {
    fn put_target_in_memory(
        &mut self,
        plugin_id: &str,
        plugin_name: &str,
        target: InstalledTarget,
    ) {
        let entry = self
            .plugins
            .entry(plugin_id.to_string())
            .or_insert_with(|| InstalledPlugin {
                id: plugin_id.to_string(),
                name: plugin_name.to_string(),
                targets: BTreeMap::new(),
            });
        entry.name = plugin_name.to_string();
        entry.targets.insert(target.target_id.clone(), target);
    }

    fn save_target(
        &mut self,
        plugin_id: &str,
        plugin_name: &str,
        target: InstalledTarget,
        path: &Path,
    ) -> AppResult<()> {
        validate_installed_target(&target)?;
        let previous = self.clone();
        self.put_target_in_memory(plugin_id, plugin_name, target);
        if let Err(error) = atomic_write_json(path, &*self) {
            *self = previous;
            return Err(error);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct OwnershipEntry {
    pub plugin_id: String,
    pub target_id: String,
    pub sha256: String,
}

pub fn find_owner(
    registry: &InstalledRegistry,
    plugin_id: &str,
    target_id: &str,
    destination: &Path,
) -> Option<OwnershipEntry> {
    let destination_lower = normalize_path_string(destination);
    for (plugin, target) in registry.all_targets() {
        for file in &target.files {
            let recorded = normalize_path_string(Path::new(&file.path));
            if recorded == destination_lower {
                if plugin.id == plugin_id && target.target_id == target_id {
                    return None;
                }
                return Some(OwnershipEntry {
                    plugin_id: plugin.id.clone(),
                    target_id: target.target_id.clone(),
                    sha256: file.sha256.clone(),
                });
            }
        }
    }
    None
}

pub fn find_block_owner(
    registry: &InstalledRegistry,
    destination: &Path,
    marker: &str,
) -> Option<(String, String)> {
    let destination_lower = normalize_path_string(destination);
    for (plugin, target) in registry.all_targets() {
        for block in &target.managed_blocks {
            if normalize_path_string(Path::new(&block.path)) == destination_lower
                && block.marker == marker
            {
                return Some((plugin.id.clone(), target.target_id.clone()));
            }
        }
    }
    None
}

fn normalize_path_string(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn validate_installed_target(target: &InstalledTarget) -> AppResult<()> {
    validate_plugin_id(&target.target_id).or_else(|_| {
        // Target ids contain a colon-free app-version-digest form, but allow any
        // safe nonempty id for previously installed targets.
        if !target.target_id.is_empty()
            && target.target_id.len() <= 200
            && !target.target_id.contains(['/', '\\'])
        {
            Ok(())
        } else {
            Err(AppError::new(
                "invalid_installed_target",
                "Installed target has an invalid identity.",
            ))
        }
    })?;
    for field in [
        &target.app,
        &target.app_version,
        &target.install_root,
        &target.plugin_version,
        &target.asset_name,
        &target.asset_sha256,
    ] {
        if field.trim().is_empty() {
            return Err(AppError::new(
                "invalid_installed_target",
                "Installed target record is missing required information.",
            ));
        }
    }
    split_repository(&target.repository)?;
    let now = Utc::now();
    let _ = now;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub schema_version: u32,
    #[serde(default)]
    pub app_overrides: Vec<AppOverride>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            app_overrides: Vec::new(),
        }
    }
}

pub struct SettingsStore {
    path: PathBuf,
    data: Mutex<Settings>,
}

impl SettingsStore {
    pub fn open(path: PathBuf) -> AppResult<Self> {
        let data = if path.exists() {
            let bytes = fs::read(&path)?;
            let settings: Settings = serde_json::from_slice(&bytes).map_err(|error| {
                AppError::new(
                    "invalid_settings",
                    format!("NexusTools settings could not be read: {error}"),
                )
            })?;
            if settings.schema_version != 1 {
                return Err(AppError::new(
                    "unsupported_settings",
                    "NexusTools settings use an unsupported schema version.",
                ));
            }
            settings
        } else {
            Settings::default()
        };
        Ok(Self {
            path,
            data: Mutex::new(data),
        })
    }

    pub fn snapshot(&self) -> Settings {
        self.data.lock().clone()
    }

    pub fn save_app_override(&self, value: AppOverride) -> AppResult<Settings> {
        validate_override(&value)?;
        let app = AppKind::parse(&value.app)?;
        let root = normalize_absolute_path(&value.install_root.to_string_lossy())?;
        let normalize_optional = |path: Option<PathBuf>| -> Option<PathBuf> {
            path.and_then(|path| {
                normalize_absolute_path(&crate::util::expand_windows_env(&path.to_string_lossy()))
                    .ok()
            })
        };
        let value = AppOverride {
            app: value.app,
            version: value.version,
            install_root: root.clone(),
            user_scripts: normalize_optional(value.user_scripts),
            user_modules: normalize_optional(value.user_modules),
            user_prefs: normalize_optional(value.user_prefs),
            zplugins: normalize_optional(value.zplugins),
        };
        let mut data = self.data.lock();
        data.app_overrides.retain(|existing| {
            if let Ok(existing_app) = AppKind::parse(&existing.app) {
                !(existing_app == app
                    && normalize_absolute_path(&existing.install_root.to_string_lossy())
                        .map(|path| {
                            path.to_string_lossy()
                                .eq_ignore_ascii_case(&root.to_string_lossy())
                        })
                        .unwrap_or(false))
            } else {
                true
            }
        });
        data.app_overrides.push(value);
        atomic_write_json(&self.path, &*data)?;
        Ok(data.clone())
    }

    pub fn remove_app_override(&self, app: &str, install_root: &str) -> AppResult<Settings> {
        let app = AppKind::parse(app)?;
        let root = normalize_absolute_path(install_root)?;
        let mut data = self.data.lock();
        data.app_overrides.retain(|existing| {
            if let Ok(existing_app) = AppKind::parse(&existing.app) {
                !(existing_app == app
                    && normalize_absolute_path(&existing.install_root.to_string_lossy())
                        .map(|path| {
                            path.to_string_lossy()
                                .eq_ignore_ascii_case(&root.to_string_lossy())
                        })
                        .unwrap_or(false))
            } else {
                true
            }
        });
        atomic_write_json(&self.path, &*data)?;
        Ok(data.clone())
    }
}

pub fn all_owned_files(registry: &InstalledRegistry) -> HashMap<String, (String, String)> {
    let mut map = HashMap::new();
    for (plugin, target) in registry.all_targets() {
        for file in &target.files {
            map.insert(
                normalize_path_string(Path::new(&file.path)),
                (plugin.id.clone(), target.target_id.clone()),
            );
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn target() -> InstalledTarget {
        InstalledTarget {
            target_id: "maya-2024-abc".to_string(),
            app: "maya".to_string(),
            app_name: "Maya".to_string(),
            app_version: "2024".to_string(),
            install_root: r"C:\Program Files\Autodesk\Maya2024".to_string(),
            repository: "owner/repo".to_string(),
            plugin_version: "1.0.0".to_string(),
            release_id: Some(1),
            release_tag: "v1.0.0".to_string(),
            asset_name: "plugin.zip".to_string(),
            asset_sha256: "a".repeat(64),
            digest_verified: true,
            installed_at: Utc::now(),
            updated_at: Utc::now(),
            status: "installed".to_string(),
            operation_id: "op".to_string(),
            files: vec![InstalledFileRecord {
                path: r"C:\Users\Test\Documents\maya\2024\scripts\plugin\a.py".to_string(),
                sha256: "b".repeat(64),
                size: 10,
                obsolete: false,
                preserved_modified: false,
            }],
            directories_created: vec![],
            managed_blocks: vec![],
            backup_dir: None,
            bundled_manifest: serde_json::json!({}),
            resolved_paths: BTreeMap::new(),
        }
    }

    #[test]
    fn installed_state_roundtrips_atomically_and_tracks_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = InstalledStore::open(path.clone()).unwrap();
        store.put_target("example", "Example", target()).unwrap();
        let reloaded = InstalledStore::open(path).unwrap();
        assert_eq!(
            reloaded
                .snapshot()
                .plugins
                .get("example")
                .unwrap()
                .targets
                .len(),
            1
        );
        let owner = reloaded
            .ownership_conflicts(
                "other-plugin",
                "target",
                Path::new(r"C:\Users\Test\Documents\maya\2024\scripts\plugin\a.py"),
            )
            .unwrap();
        assert_eq!(owner.plugin_id, "example");
        assert!(reloaded
            .ownership_conflicts(
                "example",
                "maya-2024-abc",
                Path::new(r"C:\Users\Test\Documents\maya\2024\scripts\plugin\a.py")
            )
            .is_none());
    }

    #[test]
    fn removing_last_target_removes_plugin_and_keeps_other_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let store = InstalledStore::open(dir.path().join("state.json")).unwrap();
        store.put_target("one", "One", target()).unwrap();
        let mut two = target();
        two.target_id = "maya-2025-def".to_string();
        two.app_version = "2025".to_string();
        store.put_target("two", "Two", two).unwrap();
        store.remove_target("one", "maya-2024-abc").unwrap();
        let registry = store.snapshot();
        assert!(registry.plugins.get("one").is_none());
        assert!(registry.plugins.get("two").is_some());
    }

    #[test]
    fn settings_persist_and_removal_does_not_touch_cache_or_state() {
        let dir = tempfile::tempdir().unwrap();
        let paths = NexusPaths::new(dir.path().to_path_buf());
        paths.prepare().unwrap();
        let installed = paths.installed_state_file();
        fs::write(&installed, b"state").unwrap();
        let cache = paths.cache_dir().join("catalog.json");
        fs::write(&cache, b"cache").unwrap();
        let store = SettingsStore::open(paths.settings_file()).unwrap();
        let root = tempfile::tempdir().unwrap();
        let value = AppOverride {
            app: "maya".to_string(),
            version: "2024".to_string(),
            install_root: root.path().to_path_buf(),
            ..Default::default()
        };
        store.save_app_override(value).unwrap();
        let reopened = SettingsStore::open(paths.settings_file()).unwrap();
        assert_eq!(reopened.snapshot().app_overrides.len(), 1);
        reopened
            .remove_app_override("maya", &root.path().to_string_lossy())
            .unwrap();
        assert_eq!(fs::read(installed).unwrap(), b"state");
        assert_eq!(fs::read(cache).unwrap(), b"cache");
    }
}
