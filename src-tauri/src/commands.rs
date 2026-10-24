use crate::apps::{detect_applications, AppInstallation, AppInstallationView, AppOverride};
use crate::error::{AppError, AppResult};
use crate::installer::{recover_transactions, InstallProgress};
use crate::loader::{PluginService, RefreshReport};
use crate::operations::{
    install_plugin as install_operation, uninstall_plugins, InstallReport, InstallRequest,
    UninstallReport, UninstallRequest,
};
use crate::store::{InstalledStore, NexusPaths, SettingsStore};
use crate::view::{build_bootstrap, Bootstrap};
use parking_lot::RwLock;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;
use uuid::Uuid;

pub struct AppState {
    pub paths: NexusPaths,
    pub service: PluginService,
    pub store: InstalledStore,
    pub settings: SettingsStore,
    pub apps: RwLock<Vec<AppInstallation>>,
    pub recovery_warnings: Vec<String>,
    operation_lock: Mutex<()>,
    cancel_tokens: parking_lot::Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl AppState {
    pub fn initialize() -> AppResult<Self> {
        let paths = NexusPaths::for_current_user()?;
        paths.prepare()?;
        let store = InstalledStore::open(paths.installed_state_file())?;
        let recovery_warnings = recover_transactions(&paths, &store);
        let settings_store = SettingsStore::open(paths.settings_file())?;
        let service = PluginService::initialize(&paths)?;
        let overrides = settings_store.snapshot().app_overrides;
        let apps = detect_applications(&overrides);
        Ok(Self {
            paths,
            service,
            store,
            settings: settings_store,
            apps: RwLock::new(apps),
            recovery_warnings,
            operation_lock: Mutex::new(()),
            cancel_tokens: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    fn app_views(&self) -> Vec<AppInstallationView> {
        self.apps.read().iter().map(AppInstallation::view).collect()
    }

    fn refresh_apps(&self) {
        let overrides = self.settings.snapshot().app_overrides;
        let apps = detect_applications(&overrides);
        *self.apps.write() = apps;
    }

    fn bootstrap(&self, refresh: Option<RefreshReport>) -> AppResult<Bootstrap> {
        let settings = self.settings.snapshot();
        build_bootstrap(
            &self.service,
            &self.store,
            &self.paths,
            &self.apps.read(),
            &settings,
            &self.recovery_warnings,
            refresh,
        )
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallProgressEvent {
    job_id: String,
    plugin_id: String,
    phase: String,
    percent: u8,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StateChangedEvent {
    plugin_id: Option<String>,
    reason: String,
}

#[tauri::command]
pub fn bootstrap(state: State<'_, AppState>) -> Result<Bootstrap, AppError> {
    state.bootstrap(None)
}

#[tauri::command]
pub async fn refresh_catalog(state: State<'_, AppState>) -> Result<Bootstrap, AppError> {
    let report = state.service.refresh().await?;
    let apps_overrides = state.settings.snapshot().app_overrides;
    let apps = detect_applications(&apps_overrides);
    *state.apps.write() = apps;
    state.bootstrap(Some(report))
}

#[tauri::command]
pub fn refresh_applications(
    state: State<'_, AppState>,
) -> Result<Vec<AppInstallationView>, AppError> {
    state.refresh_apps();
    Ok(state.app_views())
}

#[tauri::command]
pub fn save_app_override(
    state: State<'_, AppState>,
    app_override: AppOverride,
) -> Result<Vec<AppInstallationView>, AppError> {
    state.settings.save_app_override(app_override)?;
    state.refresh_apps();
    Ok(state.app_views())
}

#[tauri::command]
pub fn remove_app_override(
    state: State<'_, AppState>,
    app: String,
    install_root: String,
) -> Result<Vec<AppInstallationView>, AppError> {
    state.settings.remove_app_override(&app, &install_root)?;
    state.refresh_apps();
    Ok(state.app_views())
}

#[tauri::command]
pub async fn pick_directory(initial: Option<String>) -> Result<Option<String>, AppError> {
    let value = tokio::task::spawn_blocking(move || {
        let mut dialog = rfd::FileDialog::new();
        if let Some(initial) = initial.as_deref() {
            let path = std::path::Path::new(initial);
            if path.is_dir() {
                dialog = dialog.set_directory(path);
            }
        }
        dialog
            .pick_folder()
            .map(|path| path.to_string_lossy().to_string())
    })
    .await
    .map_err(|error| AppError::new("dialog_failed", format!("Folder dialog failed: {error}")))?;
    Ok(value)
}

#[tauri::command]
pub async fn clear_metadata_cache(state: State<'_, AppState>) -> Result<Bootstrap, AppError> {
    let _operation = state.operation_lock.try_lock().map_err(|_| {
        AppError::new(
            "operation_in_progress",
            "An installation is running. Wait for it to finish before clearing the metadata cache.",
        )
    })?;
    // Clearing only the cache directory preserves installed records and user
    // settings by construction.
    state.service.clear_cache().await?;
    state.bootstrap(None)
}

#[tauri::command]
pub async fn install_plugin(
    app: AppHandle,
    state: State<'_, AppState>,
    mut request: InstallRequest,
) -> Result<InstallReport, AppError> {
    let _operation = state.operation_lock.try_lock().map_err(|_| {
        AppError::new(
            "operation_in_progress",
            "Another NexusTools installation or removal is already running.",
        )
    })?;
    if request.job_id.trim().is_empty() {
        request.job_id = Uuid::new_v4().simple().to_string();
    }
    let job_id = request.job_id.clone();
    let plugin_id = request.plugin_id.clone();
    if state.cancel_tokens.lock().contains_key(&job_id) {
        return Err(AppError::new(
            "job_id_in_use",
            "That installation job identifier is already active.",
        ));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    state
        .cancel_tokens
        .lock()
        .insert(job_id.clone(), cancel.clone());

    let event_app = app.clone();
    let event_job = job_id.clone();
    let event_plugin = plugin_id.clone();
    let progress = move |event: InstallProgress| {
        let _ = event_app.emit(
            "install-progress",
            InstallProgressEvent {
                job_id: event_job.clone(),
                plugin_id: event_plugin.clone(),
                phase: event.phase,
                percent: event.percent,
                message: event.message,
            },
        );
    };

    let result = install_operation(
        &state.service,
        &state.paths,
        &state.store,
        request,
        cancel,
        &progress,
    )
    .await;
    state.cancel_tokens.lock().remove(&job_id);
    match &result {
        Ok(report) => {
            let _ = app.emit(
                "state-changed",
                StateChangedEvent {
                    plugin_id: Some(plugin_id.clone()),
                    reason: if report.all_succeeded {
                        "installed".to_string()
                    } else {
                        "install-partial".to_string()
                    },
                },
            );
        }
        Err(error) if error.code == "cancelled" => {
            let _ = app.emit(
                "state-changed",
                StateChangedEvent {
                    plugin_id: Some(plugin_id.clone()),
                    reason: "cancelled".to_string(),
                },
            );
        }
        Err(_) => {}
    }
    result
}

#[tauri::command]
pub fn cancel_job(state: State<'_, AppState>, job_id: String) -> bool {
    if let Some(token) = state.cancel_tokens.lock().get(&job_id) {
        token.store(true, Ordering::SeqCst);
        true
    } else {
        false
    }
}

#[tauri::command]
pub async fn uninstall_plugin(
    app: AppHandle,
    state: State<'_, AppState>,
    request: UninstallRequest,
) -> Result<UninstallReport, AppError> {
    let _operation = state.operation_lock.try_lock().map_err(|_| {
        AppError::new(
            "operation_in_progress",
            "Another NexusTools installation or removal is already running.",
        )
    })?;
    let plugin_id = request.plugin_id.clone();
    let report = uninstall_plugins(&state.paths, &state.store, request)?;
    let _ = app.emit(
        "state-changed",
        StateChangedEvent {
            plugin_id: Some(plugin_id),
            reason: if report.all_removed {
                "uninstalled".to_string()
            } else {
                "uninstall-partial".to_string()
            },
        },
    );
    Ok(report)
}

#[tauri::command]
pub fn open_data_folder(state: State<'_, AppState>) -> Result<(), AppError> {
    std::process::Command::new("explorer.exe")
        .arg(state.paths.root())
        .spawn()
        .map(|_| ())
        .map_err(|error| {
            AppError::io(
                &format!("Cannot open {}", state.paths.root().display()),
                error,
            )
        })
}

#[tauri::command]
pub fn get_apps(state: State<'_, AppState>) -> Vec<AppInstallationView> {
    state.app_views()
}

pub fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    // Create the window in Rust rather than config so WebView2's user-data
    // folder is guaranteed to live under the writable per-user NexusTools data
    // root. The former config-created window could fall back to a
    // `<exe>.WebView2` folder next to the binary, which would be unwritable in
    // the machine-wide Program Files installation.
    let state = app.state::<AppState>();
    let webview_data = state.paths.root().join("webview");
    std::fs::create_dir_all(&webview_data)?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))?;
    let window =
        tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
            .title("NexusTools")
            .inner_size(1280.0, 860.0)
            .min_inner_size(940.0, 650.0)
            .center()
            .resizable(true)
            .theme(Some(tauri::Theme::Dark))
            .background_color(tauri::window::Color(0, 0, 0, 255))
            .data_directory(webview_data)
            .icon(icon)?
            .visible(false)
            .build()?;
    window.show()?;
    window.set_focus()?;
    Ok(())
}
