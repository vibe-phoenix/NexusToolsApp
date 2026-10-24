use crate::error::{AppError, AppResult};
use crate::util::{expand_windows_env, normalize_absolute_path, sha256_bytes};
use crate::version::{extract_app_version, Version};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AppKind {
    Maya,
    ZBrush,
}

impl AppKind {
    pub fn parse(value: &str) -> AppResult<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "maya" => Ok(AppKind::Maya),
            "zbrush" | "z-brush" => Ok(AppKind::ZBrush),
            other => Err(AppError::new(
                "unsupported_application",
                format!("Unsupported application '{other}'. NexusTools supports Maya and ZBrush."),
            )),
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            AppKind::Maya => "maya",
            AppKind::ZBrush => "zbrush",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            AppKind::Maya => "Maya",
            AppKind::ZBrush => "ZBrush",
        }
    }

    pub fn executable_names(&self) -> &'static [&'static str] {
        match self {
            AppKind::Maya => &["maya.exe"],
            AppKind::ZBrush => &["zbrush.exe"],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AppOverride {
    pub app: String,
    pub version: String,
    pub install_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_scripts: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_modules: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prefs: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zplugins: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct AppPaths {
    pub user_scripts: Option<PathBuf>,
    pub user_modules: Option<PathBuf>,
    pub user_prefs: Option<PathBuf>,
    pub zplugins: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct AppInstallation {
    pub target_id: String,
    pub app: AppKind,
    pub version: String,
    pub install_root: PathBuf,
    pub executable: Option<PathBuf>,
    pub source: String,
    pub manual: bool,
    pub paths: AppPaths,
    pub path_notes: Vec<String>,
    pub issues: Vec<String>,
    pub requires_app_closed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppPathView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_scripts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_modules: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_prefs: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zplugins: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInstallationView {
    pub target_id: String,
    pub app: String,
    pub app_name: String,
    pub version: String,
    pub install_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    pub source: String,
    pub manual: bool,
    pub paths: AppPathView,
    pub path_notes: Vec<String>,
    pub issues: Vec<String>,
    pub requires_app_closed: bool,
    pub running: bool,
}

impl AppInstallation {
    pub fn view(&self) -> AppInstallationView {
        AppInstallationView {
            target_id: self.target_id.clone(),
            app: self.app.id().to_string(),
            app_name: self.app.display_name().to_string(),
            version: self.version.clone(),
            install_root: self.install_root.to_string_lossy().to_string(),
            executable: self
                .executable
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
            source: self.source.clone(),
            manual: self.manual,
            paths: AppPathView {
                user_scripts: self
                    .paths
                    .user_scripts
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                user_modules: self
                    .paths
                    .user_modules
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                user_prefs: self
                    .paths
                    .user_prefs
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                zplugins: self
                    .paths
                    .zplugins
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
            },
            path_notes: self.path_notes.clone(),
            issues: self.issues.clone(),
            requires_app_closed: self.requires_app_closed,
            running: is_application_running(&self.app),
        }
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    app: AppKind,
    version: String,
    install_root: PathBuf,
    source: String,
    manual: bool,
    override_paths: Option<AppPaths>,
    display_name: Option<String>,
}

/// Detect every known installation rather than assuming default folders. Maya
/// installations are discovered from Autodesk registry keys, uninstall entries,
/// MAYA_LOCATION, and standard Autodesk folders on each local disk. ZBrush is
/// discovered from Maxon/Pixologic uninstall entries and application folders.
/// User-data paths follow the official Maya and ZBrush layouts and honor
/// MAYA_APP_DIR, MAYA_SCRIPT_PATH, MAYA_MODULE_PATH, ZBRUSH_USER_ASSETS_DIR and
/// manual overrides.
pub fn detect_applications(overrides: &[AppOverride]) -> Vec<AppInstallation> {
    let mut candidates: Vec<Candidate> = Vec::new();
    candidates.extend(registry_candidates());
    candidates.extend(scan_candidates());
    candidates.extend(manual_candidates(overrides));
    candidates.extend(environment_candidates());

    // Merge by app + install root, preferring manual overrides.
    let mut by_root: HashMap<String, Candidate> = HashMap::new();
    for candidate in candidates {
        if !candidate.install_root.is_dir() {
            continue;
        }
        let key = format!(
            "{}|{}",
            candidate.app.id(),
            normalize_for_compare(&candidate.install_root)
        );
        match by_root.get(&key) {
            Some(existing) if existing.manual && !candidate.manual => {}
            _ => {
                by_root.insert(key, candidate);
            }
        }
    }
    let mut installations: Vec<AppInstallation> = by_root
        .into_values()
        .filter_map(build_installation)
        .collect();

    // Multiple installations of the same version may share plugin folders
    // (Maya user data, or ZBrush 2026.1+ user assets). Their effective target is
    // the same, so collapse those duplicates while retaining genuinely separate
    // targets with different plugin locations.
    installations.sort_by(|a, b| {
        a.app
            .display_name()
            .cmp(b.app.display_name())
            .then_with(|| b.version.cmp(&a.version))
            .then_with(|| a.install_root.cmp(&b.install_root))
    });
    let mut seen_targets = HashMap::new();
    let mut result = Vec::new();
    for installation in installations {
        if seen_targets.contains_key(&installation.target_id) {
            continue;
        }
        seen_targets.insert(
            installation.target_id.clone(),
            installation.install_root.clone(),
        );
        result.push(installation);
    }
    result
}

fn manual_candidates(overrides: &[AppOverride]) -> Vec<Candidate> {
    overrides
        .iter()
        .filter_map(|value| {
            let app = AppKind::parse(&value.app).ok()?;
            let install_root =
                normalize_absolute_path(&value.install_root.to_string_lossy()).ok()?;
            if !install_root.is_dir() {
                return None;
            }
            let normalize_optional = |path: &Option<PathBuf>| {
                path.as_ref().and_then(|path| {
                    normalize_absolute_path(&expand_windows_env(&path.to_string_lossy())).ok()
                })
            };
            Some(Candidate {
                app,
                version: value.version.trim().to_string(),
                install_root,
                source: "Manual path".to_string(),
                manual: true,
                override_paths: Some(AppPaths {
                    user_scripts: normalize_optional(&value.user_scripts),
                    user_modules: normalize_optional(&value.user_modules),
                    user_prefs: normalize_optional(&value.user_prefs),
                    zplugins: normalize_optional(&value.zplugins),
                }),
                display_name: None,
            })
        })
        .collect()
}

fn environment_candidates() -> Vec<Candidate> {
    let mut result = Vec::new();
    if let Ok(location) = std::env::var("MAYA_LOCATION") {
        let expanded = expand_windows_env(&location);
        if let Ok(root) = normalize_absolute_path(&expanded) {
            if let Some(version) = version_from_path(&AppKind::Maya, &root) {
                result.push(Candidate {
                    app: AppKind::Maya,
                    version,
                    install_root: root,
                    source: "MAYA_LOCATION".to_string(),
                    manual: false,
                    override_paths: None,
                    display_name: None,
                });
            }
        }
    }
    result
}

fn build_installation(candidate: Candidate) -> Option<AppInstallation> {
    let executable = find_executable(&candidate.install_root, &candidate.app);
    let (paths, mut path_notes, mut issues) = resolve_app_paths(
        &candidate.app,
        &candidate.version,
        &candidate.install_root,
        candidate.override_paths.as_ref(),
    );
    if executable.is_none() {
        issues.push(format!(
            "{} was not found under {}. Check the installation folder.",
            candidate.app.executable_names().join(" / "),
            candidate.install_root.display()
        ));
    }
    if candidate.app == AppKind::Maya {
        if paths.user_scripts.is_none() {
            issues.push("The Maya user scripts folder could not be resolved.".to_string());
        }
        if paths.user_modules.is_none() {
            issues.push("The Maya user modules folder could not be resolved.".to_string());
        }
        if paths.user_prefs.is_none() {
            issues.push("The Maya user preferences folder could not be resolved.".to_string());
        }
    }
    if candidate.app == AppKind::ZBrush && paths.zplugins.is_none() {
        issues.push("The ZBrush plugins folder could not be resolved.".to_string());
    }
    let target_id = make_target_id(
        &candidate.app,
        &candidate.version,
        &candidate.install_root,
        &paths,
    );
    path_notes.dedup();
    issues.dedup();
    Some(AppInstallation {
        target_id,
        app: candidate.app.clone(),
        version: candidate.version,
        install_root: candidate.install_root,
        executable,
        source: candidate.source,
        manual: candidate.manual,
        paths,
        path_notes,
        issues,
        requires_app_closed: candidate.app == AppKind::ZBrush,
    })
}

fn resolve_app_paths(
    app: &AppKind,
    version: &str,
    install_root: &Path,
    override_paths: Option<&AppPaths>,
) -> (AppPaths, Vec<String>, Vec<String>) {
    match app {
        AppKind::Maya => resolve_maya_paths(version, override_paths),
        AppKind::ZBrush => resolve_zbrush_paths(version, install_root, override_paths),
    }
}

fn resolve_maya_paths(
    version: &str,
    override_paths: Option<&AppPaths>,
) -> (AppPaths, Vec<String>, Vec<String>) {
    let mut notes = Vec::new();
    let mut issues = Vec::new();
    let override_paths = override_paths.cloned().unwrap_or_default();
    let mut base = if let Some(paths) = override_paths.user_scripts.clone() {
        let _ = paths;
        None
    } else {
        None
    };

    // MAYA_APP_DIR overrides the default user application directory. When it
    // is absent, use the known Documents folder, which correctly handles
    // OneDrive/redirected profiles.
    let mut app_dir = std::env::var("MAYA_APP_DIR")
        .ok()
        .map(|value| expand_windows_env(&value))
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| normalize_absolute_path(&value).ok());
    if app_dir.is_some() {
        notes.push("Maya user directory from MAYA_APP_DIR.".to_string());
    } else {
        app_dir = dirs::document_dir().map(|documents| documents.join("maya"));
        if app_dir.is_some() {
            notes.push("Maya user directory from the Windows Documents known folder.".to_string());
        }
    }
    if let Some(path) = app_dir.as_mut() {
        if let Some(config_dir) = versioned_maya_dir(path, version, &mut issues) {
            base = Some(config_dir);
        } else {
            base = None;
        }
    } else {
        issues.push(
            "The Maya user application directory could not be determined. Set a manual user scripts, modules and preferences path."
                .to_string(),
        );
    }

    let config_dir = base;
    let env_scripts = parse_windows_path_list(std::env::var("MAYA_SCRIPT_PATH").ok());
    let env_modules = parse_windows_path_list(std::env::var("MAYA_MODULE_PATH").ok());
    if !env_scripts.is_empty() {
        notes.push("Maya user scripts overridden by MAYA_SCRIPT_PATH.".to_string());
    }
    if !env_modules.is_empty() {
        notes.push("Maya user modules overridden by MAYA_MODULE_PATH.".to_string());
    }

    let scripts = override_paths.user_scripts.or_else(|| {
        env_scripts
            .first()
            .cloned()
            .or_else(|| config_dir.as_ref().map(|dir| dir.join("scripts")))
    });
    let modules = override_paths.user_modules.or_else(|| {
        env_modules
            .first()
            .cloned()
            .or_else(|| config_dir.as_ref().map(|dir| dir.join("modules")))
    });
    let prefs = override_paths
        .user_prefs
        .or_else(|| config_dir.as_ref().map(|dir| dir.join("prefs")));
    if config_dir.is_none() && scripts.is_none() && modules.is_none() && prefs.is_none() {
        // issue already added above
    }
    (
        AppPaths {
            user_scripts: scripts,
            user_modules: modules,
            user_prefs: prefs,
            zplugins: None,
        },
        notes,
        issues,
    )
}

fn versioned_maya_dir(base: &Path, version: &str, issues: &mut Vec<String>) -> Option<PathBuf> {
    if !base.exists() {
        return Some(base.join(version));
    }
    let exact = base.join(version);
    if exact.is_dir() {
        return Some(exact);
    }
    let mut candidates = Vec::new();
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let lower = name.to_ascii_lowercase();
            let version_lower = version.to_ascii_lowercase();
            if lower == version_lower {
                return Some(entry.path());
            }
            if lower.starts_with(&version_lower) {
                let next = lower.as_bytes().get(version_lower.len()).copied();
                if matches!(next, Some(b'-') | Some(b'.') | Some(b'_')) {
                    candidates.push(entry.path());
                }
            }
        }
    }
    match candidates.len() {
        0 => Some(base.join(version)),
        1 => {
            issues.push(format!(
                "Maya user data was found in {} rather than the default version folder.",
                candidates[0].display()
            ));
            Some(candidates.remove(0))
        }
        _ => {
            issues.push(format!(
                "Multiple Maya user-data folders match {version} in {}. Choose the correct user paths manually.",
                base.display()
            ));
            None
        }
    }
}

fn resolve_zbrush_paths(
    version: &str,
    install_root: &Path,
    override_paths: Option<&AppPaths>,
) -> (AppPaths, Vec<String>, Vec<String>) {
    let mut notes = Vec::new();
    let mut issues = Vec::new();
    let override_paths = override_paths.cloned().unwrap_or_default();
    if let Some(path) = override_paths.zplugins.clone() {
        notes.push("ZBrush plugins path supplied manually.".to_string());
        return (
            AppPaths {
                user_scripts: None,
                user_modules: None,
                user_prefs: None,
                zplugins: Some(path),
            },
            notes,
            issues,
        );
    }

    let user_asset_env = std::env::var("ZBRUSH_USER_ASSETS_DIR")
        .ok()
        .map(|value| expand_windows_env(&value))
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| normalize_absolute_path(&value).ok());

    let modern = zbrush_uses_user_assets(version);
    let zplugins = if let Some(root) = user_asset_env {
        notes.push("ZBrush user assets path from ZBRUSH_USER_ASSETS_DIR.".to_string());
        Some(root.join("ZStartup").join("ZPlugs64"))
    } else if modern {
        match user_asset_directories() {
            Ok(mut dirs) => {
                if dirs.len() == 1 {
                    notes.push(
                        "ZBrush 2026.1+ user assets folder detected under AppData\\Roaming\\Maxon."
                            .to_string(),
                    );
                    Some(dirs.remove(0).join("ZStartup").join("ZPlugs64"))
                } else if dirs.is_empty() {
                    issues.push(
                        "ZBrush 2026.1+ keeps plugins in a user assets folder under AppData\\Roaming\\Maxon\\ZBrush_<id>. Launch ZBrush once or choose a ZBrush plugins folder manually."
                            .to_string(),
                    );
                    None
                } else {
                    issues.push(format!(
                        "Multiple ZBrush user asset folders were found ({}). NexusTools will not guess which one belongs to this installation; choose the plugins folder manually.",
                        dirs.iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                    None
                }
            }
            Err(error) => {
                issues.push(format!(
                    "Could not inspect ZBrush user asset folders: {error}"
                ));
                None
            }
        }
    } else {
        let zplugs64 = install_root.join("ZStartup").join("ZPlugs64");
        let zplugs = install_root.join("ZStartup").join("ZPlugs");
        if zplugs64.exists() || !zplugs.exists() {
            notes.push(
                "ZBrush 2026.0.1-and-earlier plugin path under the installation folder."
                    .to_string(),
            );
            Some(zplugs64)
        } else {
            notes.push("Legacy ZBrush ZPlugs path under the installation folder.".to_string());
            Some(zplugs)
        }
    };
    (
        AppPaths {
            user_scripts: None,
            user_modules: None,
            user_prefs: None,
            zplugins,
        },
        notes,
        issues,
    )
}

fn zbrush_uses_user_assets(version: &str) -> bool {
    match Version::parse(version) {
        Ok(parsed) => parsed >= Version::parse("2026.1").unwrap(),
        Err(_) => {
            // Unknown labels are treated conservatively as pre-2026.1 only when
            // the install root actually has the expected folder.
            false
        }
    }
}

fn user_asset_directories() -> std::io::Result<Vec<PathBuf>> {
    let root = dirs::config_dir()
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .map(|path| path.join("Maxon"));
    let Some(root) = root else {
        return Ok(Vec::new());
    };
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.starts_with("zbrush_") && !name.contains("core") {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn parse_windows_path_list(value: Option<String>) -> Vec<PathBuf> {
    let Some(value) = value else {
        return Vec::new();
    };
    value
        .split(';')
        .map(|part| part.trim().trim_matches('"'))
        .filter(|part| !part.is_empty())
        .map(expand_windows_env)
        .filter_map(|part| normalize_absolute_path(&part).ok())
        .collect()
}

fn find_executable(root: &Path, app: &AppKind) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    let mut stack = vec![root.to_path_buf()];
    let mut depth = 0;
    while let Some(dir) = stack.pop() {
        if depth > 500 {
            break;
        }
        depth += 1;
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.eq_ignore_ascii_case("bin") || name.eq_ignore_ascii_case("ZBrush")
                        })
                    {
                        stack.push(path);
                    }
                } else if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                    if app
                        .executable_names()
                        .iter()
                        .any(|executable| name.eq_ignore_ascii_case(executable))
                    {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

fn make_target_id(app: &AppKind, version: &str, install_root: &Path, paths: &AppPaths) -> String {
    let mut identity = format!("{}\\{}\\", app.id(), version);
    match app {
        AppKind::Maya => {
            for path in [&paths.user_scripts, &paths.user_modules, &paths.user_prefs] {
                identity.push_str(
                    path.as_ref()
                        .map(|value| normalize_for_compare(value))
                        .unwrap_or_else(|| "<unresolved>".to_string())
                        .as_str(),
                );
                identity.push('|');
            }
        }
        AppKind::ZBrush => {
            identity.push_str(
                paths
                    .zplugins
                    .as_ref()
                    .map(|value| normalize_for_compare(value))
                    .unwrap_or_else(|| normalize_for_compare(install_root))
                    .as_str(),
            );
        }
    }
    if paths.user_scripts.is_none()
        && paths.user_modules.is_none()
        && paths.user_prefs.is_none()
        && paths.zplugins.is_none()
    {
        identity.push_str(&normalize_for_compare(install_root));
    }
    let digest = sha256_bytes(identity.as_bytes());
    format!(
        "{}-{}-{}",
        app.id(),
        sanitize_version(version),
        &digest[..16]
    )
}

fn sanitize_version(version: &str) -> String {
    version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(24)
        .collect()
}

fn normalize_for_compare(path: &Path) -> String {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let value = path.to_string_lossy().replace('/', "\\");
    value.trim_end_matches('\\').to_ascii_lowercase()
}

fn version_from_path(app: &AppKind, path: &Path) -> Option<String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    let marker = match app {
        AppKind::Maya => "maya",
        AppKind::ZBrush => "zbrush",
    };
    if !lower.contains(marker) {
        return None;
    }
    extract_app_version(&name)
}

#[derive(Debug, Clone)]
struct RawCandidate {
    app: AppKind,
    version: String,
    install_root: PathBuf,
    source: String,
    display_name: Option<String>,
}

impl From<RawCandidate> for Candidate {
    fn from(value: RawCandidate) -> Self {
        Candidate {
            app: value.app,
            version: value.version,
            install_root: value.install_root,
            source: value.source,
            manual: false,
            override_paths: None,
            display_name: value.display_name,
        }
    }
}

fn registry_candidates() -> Vec<Candidate> {
    let mut result = Vec::new();
    for root in registry_roots() {
        if !root.install_root.is_dir() {
            continue;
        }
        if let Some(app) = infer_kind_from_name(&root.display_name) {
            // The installation folder naming is usually the product year used by
            // the plugin manifest. Prefer it over an uninstall DisplayVersion
            // such as "26.0.2" that may use a completely different numbering.
            let version = version_from_path(&app, &root.install_root)
                .or_else(|| root.version.clone())
                .or_else(|| extract_app_version(&root.display_name));
            if let Some(version) = version {
                if root.display_name.to_ascii_lowercase().contains("zbrush")
                    && root.display_name.to_ascii_lowercase().contains("core")
                {
                    continue;
                }
                result.push(Candidate::from(RawCandidate {
                    app,
                    version,
                    install_root: root.install_root,
                    source: root.source,
                    display_name: Some(root.display_name),
                }));
            }
        }
    }
    result
}

struct RegistryRoot {
    install_root: PathBuf,
    version: Option<String>,
    display_name: String,
    source: String,
}

fn infer_kind_from_name(name: &str) -> Option<AppKind> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("zbrush") && !lower.contains("core") {
        Some(AppKind::ZBrush)
    } else if lower.contains("maya") {
        Some(AppKind::Maya)
    } else {
        None
    }
}

#[cfg(windows)]
fn registry_roots() -> Vec<RegistryRoot> {
    use winreg::enums::{
        HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY,
    };
    use winreg::RegKey;

    let mut result = Vec::new();
    let hives = [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER];
    let views = [KEY_WOW64_64KEY, KEY_WOW64_32KEY];

    for hive in hives {
        for view in views {
            if let Ok(uninstall) = RegKey::predef(hive).open_subkey_with_flags(
                r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
                KEY_READ | view,
            ) {
                for key in uninstall.enum_keys().flatten() {
                    if let Ok(entry) = uninstall.open_subkey_with_flags(&key, KEY_READ | view) {
                        let display_name: String =
                            entry.get_value("DisplayName").unwrap_or_default();
                        let kind = infer_kind_from_name(&display_name);
                        if kind.is_none() {
                            continue;
                        }
                        let install_location: String =
                            entry.get_value("InstallLocation").unwrap_or_default();
                        let display_version: String =
                            entry.get_value("DisplayVersion").unwrap_or_default();
                        let display_icon: String =
                            entry.get_value("DisplayIcon").unwrap_or_default();
                        let root = if !install_location.trim().is_empty() {
                            normalize_absolute_path(install_location.trim()).ok()
                        } else if !display_icon.trim().is_empty() {
                            let icon = display_icon
                                .split(',')
                                .next()
                                .unwrap_or_default()
                                .trim()
                                .trim_matches('"');
                            normalize_absolute_path(icon).ok().and_then(|path| {
                                let parent = path.parent()?.to_path_buf();
                                if parent
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
                                {
                                    parent.parent().map(Path::to_path_buf).or(Some(parent))
                                } else {
                                    Some(parent)
                                }
                            })
                        } else {
                            None
                        };
                        if let Some(root) = root {
                            result.push(RegistryRoot {
                                install_root: root,
                                version: Some(display_version).filter(|v| !v.trim().is_empty()),
                                display_name,
                                source: "Windows uninstall registry".to_string(),
                            });
                        }
                    }
                }
            }
            for hive_name in [
                r"SOFTWARE\Autodesk\Maya",
                r"SOFTWARE\WOW6432Node\Autodesk\Maya",
            ] {
                if let Ok(maya) =
                    RegKey::predef(hive).open_subkey_with_flags(hive_name, KEY_READ | view)
                {
                    for version_key in maya.enum_keys().flatten() {
                        if let Ok(install) = maya.open_subkey_with_flags(
                            format!(r"{version_key}\InstallPath"),
                            KEY_READ | view,
                        ) {
                            let default: String = install.get_value("").unwrap_or_default();
                            let path_value: String = install.get_value("Path").unwrap_or_default();
                            let root = if !default.trim().is_empty() {
                                normalize_absolute_path(default.trim()).ok()
                            } else if !path_value.trim().is_empty() {
                                normalize_absolute_path(path_value.trim()).ok()
                            } else {
                                None
                            };
                            if let Some(root) = root {
                                result.push(RegistryRoot {
                                    install_root: root,
                                    version: Some(version_key.clone()),
                                    display_name: format!("Autodesk Maya {version_key}"),
                                    source: "Autodesk registry".to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    result
}

#[cfg(not(windows))]
fn registry_roots() -> Vec<RegistryRoot> {
    Vec::new()
}

fn scan_candidates() -> Vec<Candidate> {
    let mut roots = Vec::new();
    if let Ok(program_files) = std::env::var("ProgramFiles") {
        roots.push(PathBuf::from(program_files));
    }
    if let Ok(program_files_x86) = std::env::var("ProgramFiles(x86)") {
        roots.push(PathBuf::from(program_files_x86));
    }
    for disk in sysinfo::Disks::new_with_refreshed_list().list() {
        let mount = disk.mount_point();
        roots.push(mount.join("Program Files"));
        roots.push(mount.join("Program Files (x86)"));
        roots.push(mount.to_path_buf());
    }

    let mut candidate_roots = Vec::new();
    for root in roots {
        for suffix in ["Autodesk", "Pixologic", "Maxon"] {
            let path = root.join(suffix);
            if path.is_dir() {
                candidate_roots.push(path);
            }
        }
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if (name.contains("maya")
                    || name.contains("zbrush")
                    || name.contains("maxon zbrush"))
                    && !name.contains("core")
                {
                    candidate_roots.push(path);
                }
            }
        }
    }

    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in candidate_roots {
        if !root.is_dir() {
            continue;
        }
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let app = if name.to_ascii_lowercase().contains("zbrush") {
            Some(AppKind::ZBrush)
        } else if name.to_ascii_lowercase().contains("maya") {
            Some(AppKind::Maya)
        } else {
            // A vendor folder: inspect direct children.
            if let Ok(entries) = fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let child_name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if let Some(app) = infer_kind_from_name(&child_name) {
                        if let Some(version) = version_from_path(&app, &path) {
                            let key = normalize_for_compare(&path);
                            if seen.insert(key) {
                                result.push(Candidate::from(RawCandidate {
                                    app,
                                    version,
                                    install_root: path,
                                    source: "Application folder scan".to_string(),
                                    display_name: None,
                                }));
                            }
                        }
                    }
                }
            }
            None
        };
        if let Some(app) = app {
            if let Some(version) = version_from_path(&app, &root) {
                let key = normalize_for_compare(&root);
                if seen.insert(key) {
                    result.push(Candidate::from(RawCandidate {
                        app,
                        version,
                        install_root: root,
                        source: "Application folder scan".to_string(),
                        display_name: None,
                    }));
                }
            }
        }
    }
    result
}

/// Validate a manual path before storing it. Saving never touches installed
/// plugin files.
pub fn validate_override(value: &AppOverride) -> AppResult<()> {
    let app = AppKind::parse(&value.app)?;
    if value.version.trim().is_empty() || value.version.len() > 64 {
        return Err(AppError::new(
            "invalid_app_version",
            "Enter the application version, for example 2025.",
        ));
    }
    let root = normalize_absolute_path(&value.install_root.to_string_lossy())?;
    if !root.is_dir() {
        return Err(AppError::new(
            "invalid_install_root",
            format!("The installation folder does not exist: {}", root.display()),
        ));
    }
    for path in [
        &value.user_scripts,
        &value.user_modules,
        &value.user_prefs,
        &value.zplugins,
    ]
    .into_iter()
    .flatten()
    {
        if !path.is_absolute() {
            return Err(AppError::new(
                "invalid_path",
                format!("An override path must be absolute: {}", path.display()),
            ));
        }
    }
    let _ = app;
    Ok(())
}

pub fn is_application_running(app: &AppKind) -> bool {
    let names = app.executable_names();
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    system.processes().values().any(|process| {
        let process_name = process.name().to_string_lossy();
        names
            .iter()
            .any(|name| process_name.eq_ignore_ascii_case(name))
    })
}

/// Read the manual path overrides without touching installed plugin records.
pub fn load_settings_for_detection(
    paths: &crate::store::NexusPaths,
) -> AppResult<Vec<AppOverride>> {
    let settings = crate::store::SettingsStore::open(paths.settings_file())?;
    Ok(settings.snapshot().app_overrides)
}

pub fn supported_app_names() -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    names.insert("maya".to_string(), "Maya".to_string());
    names.insert("zbrush".to_string(), "ZBrush".to_string());
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> AppPaths {
        AppPaths {
            user_scripts: Some(PathBuf::from(r"C:\Users\Test\Documents\maya\2024\scripts")),
            user_modules: Some(PathBuf::from(r"C:\Users\Test\Documents\maya\2024\modules")),
            user_prefs: Some(PathBuf::from(r"C:\Users\Test\Documents\maya\2024\prefs")),
            zplugins: None,
        }
    }

    #[test]
    fn target_id_is_stable_and_distinguishes_versions_and_paths() {
        let a = make_target_id(&AppKind::Maya, "2024", Path::new(r"C:\Maya2024"), &paths());
        let b = make_target_id(&AppKind::Maya, "2024", Path::new(r"C:\Maya2024"), &paths());
        assert_eq!(a, b);
        let mut other = paths();
        other.user_scripts = Some(PathBuf::from(r"D:\MayaScripts"));
        let c = make_target_id(&AppKind::Maya, "2024", Path::new(r"C:\Maya2024"), &other);
        assert_ne!(a, c);
    }

    #[test]
    fn zbrush_2026_1_uses_user_assets_folder() {
        assert!(zbrush_uses_user_assets("2026.1"));
        assert!(!zbrush_uses_user_assets("2026.0.1"));
        assert!(!zbrush_uses_user_assets("2025"));
    }

    #[test]
    fn manual_override_validation_rejects_relative_and_missing_roots() {
        let override_path = AppOverride {
            app: "maya".to_string(),
            version: "2024".to_string(),
            install_root: PathBuf::from("relative"),
            ..Default::default()
        };
        assert!(validate_override(&override_path).is_err());
    }

    #[test]
    fn path_list_parsing_keeps_first_absolute_entry() {
        std::env::set_var("NEXUSTOOLS_APP_TEST", "C:\\Custom");
        let values = parse_windows_path_list(Some(
            "relative;%NEXUSTOOLS_APP_TEST%\\scripts;D:\\Other".to_string(),
        ));
        assert_eq!(
            values,
            vec![
                PathBuf::from(r"C:\Custom\scripts"),
                PathBuf::from(r"D:\Other")
            ]
        );
    }

    #[test]
    fn inference_ignores_zbrush_core() {
        assert_eq!(
            infer_kind_from_name("Maxon ZBrush 2025"),
            Some(AppKind::ZBrush)
        );
        assert_eq!(infer_kind_from_name("ZBrushCore 2021"), None);
        assert_eq!(
            infer_kind_from_name("Autodesk Maya 2024"),
            Some(AppKind::Maya)
        );
    }
}
