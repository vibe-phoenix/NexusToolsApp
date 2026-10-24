use crate::error::{AppError, AppResult};
use crate::util::{
    is_windows_reserved, normalize_absolute_path, repo_path_segments, resolve_manifest_relative,
    validate_plugin_id,
};
use crate::version::{app_versions_match, versions_match};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ManifestAssets {
    #[serde(default)]
    pub banner: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ManifestContent {
    #[serde(default)]
    pub readme: String,
    #[serde(default)]
    pub features: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawAction {
    pub action: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub marker: Option<String>,
    #[serde(default)]
    pub replace: Option<String>,
    #[serde(default, flatten)]
    pub unsupported_fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestTarget {
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub supported_versions: Vec<String>,
    #[serde(default)]
    pub install: Vec<RawAction>,
    #[serde(default)]
    pub requires_app_closed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub overview: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub assets: Option<ManifestAssets>,
    #[serde(default)]
    pub content: Option<ManifestContent>,
    #[serde(default)]
    pub source_directory: Option<String>,
    #[serde(default)]
    pub targets: Vec<ManifestTarget>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestApplication {
    pub app: String,
    pub supported_versions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MetadataAssetPaths {
    pub banner: String,
    pub readme: String,
    pub features: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionKind {
    Copy,
    Mkdir,
    WriteFile,
    PatchText,
}

#[derive(Debug, Clone)]
pub struct ValidatedAction {
    pub kind: ActionKind,
    pub source: Option<String>,
    pub destination: String,
    pub content: Option<String>,
    pub marker: Option<String>,
    pub replacement: ReplacementRule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementRule {
    /// Replace only files recorded as owned by this plugin, after backup and
    /// modification checks. Unowned existing files are a conflict.
    Owned,
}

impl PluginManifest {
    pub fn from_bytes(bytes: &[u8]) -> AppResult<Self> {
        let manifest: Self = serde_json::from_slice(bytes).map_err(|error| {
            AppError::new(
                "invalid_manifest",
                format!("plugin.json is not valid JSON for schemaVersion 1: {error}"),
            )
        })?;
        if manifest.schema_version != 1 {
            return Err(AppError::new(
                "unsupported_manifest_schema",
                format!(
                    "plugin.json uses unsupported schemaVersion {}; NexusTools supports 1.",
                    manifest.schema_version
                ),
            ));
        }
        Ok(manifest)
    }

    pub fn validate_metadata(&self, expected_id: &str) -> AppResult<()> {
        if self.schema_version != 1 {
            return Err(AppError::new(
                "unsupported_manifest_schema",
                "The plugin manifest schemaVersion is not supported.",
            ));
        }
        validate_plugin_id(&self.id)?;
        if self.id != expected_id {
            return Err(AppError::new(
                "manifest_id_mismatch",
                format!(
                    "The catalog entry id '{}' does not match the plugin.json id '{}'.",
                    expected_id, self.id
                ),
            ));
        }
        for (field, value) in [
            ("name", self.name.as_str()),
            ("description", self.description.as_str()),
            ("overview", self.overview.as_str()),
            ("version", self.version.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(AppError::new(
                    "missing_metadata",
                    format!("The plugin manifest is missing required field '{field}'."),
                ));
            }
            if value.len() > 16 * 1024 {
                return Err(AppError::new(
                    "invalid_metadata",
                    format!("The plugin manifest field '{field}' is too long."),
                ));
            }
        }
        if crate::version::Version::from_label(&self.version).is_none() {
            return Err(AppError::new(
                "invalid_version",
                format!(
                    "The plugin manifest version '{}' is not a valid version.",
                    self.version
                ),
            ));
        }
        let assets = self.assets.as_ref().ok_or_else(|| {
            AppError::new(
                "missing_metadata",
                "The plugin manifest is missing the required assets object.",
            )
        })?;
        let content = self.content.as_ref().ok_or_else(|| {
            AppError::new(
                "missing_metadata",
                "The plugin manifest is missing the required content object.",
            )
        })?;
        for (label, path) in [
            ("assets.banner", assets.banner.as_str()),
            ("content.readme", content.readme.as_str()),
            ("content.features", content.features.as_str()),
        ] {
            if path.trim().is_empty() {
                return Err(AppError::new(
                    "missing_asset_path",
                    format!("plugin.json does not declare {label}."),
                ));
            }
            resolve_manifest_relative("plugin.json", path).map_err(|error| {
                AppError::new(
                    "invalid_asset_path",
                    format!("plugin.json declares an invalid {label}: {}", error.message),
                )
            })?;
        }
        Ok(())
    }

    pub fn metadata_asset_paths(&self, manifest_path: &str) -> AppResult<MetadataAssetPaths> {
        let assets = self
            .assets
            .as_ref()
            .ok_or_else(|| AppError::new("missing_metadata", "plugin.json is missing assets."))?;
        let content = self
            .content
            .as_ref()
            .ok_or_else(|| AppError::new("missing_metadata", "plugin.json is missing content."))?;
        let resolve = |path: &str, label: &str| {
            resolve_manifest_relative(manifest_path, path)
                .map(|(full, _)| full)
                .map_err(|error| {
                    AppError::new(
                        "invalid_asset_path",
                        format!("plugin.json declares an invalid {label}: {}", error.message),
                    )
                })
        };
        Ok(MetadataAssetPaths {
            banner: resolve(&assets.banner, "assets.banner")?,
            readme: resolve(&content.readme, "content.readme")?,
            features: resolve(&content.features, "content.features")?,
        })
    }

    pub fn application_summaries(&self) -> Vec<ManifestApplication> {
        let mut apps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for target in &self.targets {
            if !target.os.eq_ignore_ascii_case("windows") {
                continue;
            }
            apps.entry(target.app.to_ascii_lowercase())
                .or_default()
                .extend(target.supported_versions.iter().cloned());
        }
        apps.into_iter()
            .map(|(app, mut versions)| {
                versions.sort();
                versions.dedup();
                ManifestApplication {
                    app,
                    supported_versions: versions,
                }
            })
            .collect()
    }

    pub fn validate_for_installation(
        &self,
        expected_id: &str,
        release_version: &str,
    ) -> AppResult<()> {
        if self.schema_version != 1 {
            return Err(AppError::new(
                "unsupported_manifest_schema",
                "The bundled installation manifest schemaVersion is not supported.",
            ));
        }
        validate_plugin_id(&self.id)?;
        if self.id != expected_id {
            return Err(AppError::new(
                "bundled_manifest_id_mismatch",
                format!(
                    "The bundled plugin.json id '{}' does not match the catalog plugin '{}'.",
                    self.id, expected_id
                ),
            ));
        }
        if self.version.trim().is_empty() {
            return Err(AppError::new(
                "bundled_manifest_version_missing",
                "The bundled plugin.json is missing its version.",
            ));
        }
        if !versions_match(&self.version, release_version) {
            return Err(AppError::new(
                "bundled_manifest_version_mismatch",
                format!(
                    "The release version is {release_version}, but the bundled plugin.json says {}. The installation was stopped.",
                    self.version
                ),
            ));
        }
        let source = self.source_directory.as_deref().ok_or_else(|| {
            AppError::new(
                "missing_source_directory",
                "The bundled plugin.json is missing sourceDirectory.",
            )
        })?;
        repo_path_segments(source).map_err(|error| {
            AppError::new(
                "invalid_source_directory",
                format!("The bundled sourceDirectory is invalid: {}", error.message),
            )
        })?;
        if self.targets.is_empty() {
            return Err(AppError::new(
                "missing_targets",
                "The bundled plugin.json declares no installation targets.",
            ));
        }
        let supported_windows_targets: Vec<&ManifestTarget> = self
            .targets
            .iter()
            .filter(|target| {
                target.os.eq_ignore_ascii_case("windows")
                    && matches!(target.app.to_ascii_lowercase().as_str(), "maya" | "zbrush")
            })
            .collect();
        if supported_windows_targets.is_empty() {
            return Err(AppError::new(
                "no_windows_targets",
                "The bundled plugin.json has no Windows installation target for Maya or ZBrush.",
            ));
        }
        // Unsupported applications are simply not offered by NexusTools. A
        // malformed target for a supported application is still rejected before
        // installation begins.
        for target in supported_windows_targets {
            self.validated_actions(target)?;
        }
        Ok(())
    }

    pub fn matching_target(&self, app: &str, app_version: &str) -> AppResult<&ManifestTarget> {
        let matches: Vec<&ManifestTarget> = self
            .targets
            .iter()
            .filter(|target| {
                target.os.eq_ignore_ascii_case("windows")
                    && target.app.eq_ignore_ascii_case(app)
                    && target
                        .supported_versions
                        .iter()
                        .any(|version| app_versions_match(version, app_version))
            })
            .collect();
        match matches.len() {
            0 => {
                let known_versions = self
                    .targets
                    .iter()
                    .filter(|target| target.app.eq_ignore_ascii_case(app))
                    .flat_map(|target| target.supported_versions.iter().cloned())
                    .collect::<Vec<_>>();
                Err(AppError::new(
                    "unsupported_application_version",
                    if known_versions.is_empty() {
                        format!(
                            "This release does not support {app} {app_version} on Windows."
                        )
                    } else {
                        format!(
                            "This release does not support {app} {app_version}. Supported versions: {}.",
                            known_versions.join(", ")
                        )
                    },
                ))
            }
            1 => Ok(matches[0]),
            _ => Err(AppError::new(
                "ambiguous_installation_target",
                format!(
                    "The bundled plugin.json has multiple overlapping {app} {app_version} installation targets. Nothing was installed."
                ),
            )),
        }
    }

    pub fn validated_actions(&self, target: &ManifestTarget) -> AppResult<Vec<ValidatedAction>> {
        if !target.os.eq_ignore_ascii_case("windows") {
            return Err(AppError::new(
                "unsupported_os",
                format!(
                    "Installation target operating system '{}' is not supported.",
                    target.os
                ),
            ));
        }
        if !matches!(target.app.to_ascii_lowercase().as_str(), "maya" | "zbrush") {
            return Err(AppError::new(
                "unsupported_application",
                format!(
                    "Installation target application '{}' is not supported. NexusTools supports Maya and ZBrush.",
                    target.app
                ),
            ));
        }
        if target.supported_versions.is_empty() {
            return Err(AppError::new(
                "missing_supported_versions",
                format!(
                    "The {} installation target has no supportedVersions.",
                    target.app
                ),
            ));
        }
        if target.install.is_empty() {
            return Err(AppError::new(
                "empty_installation",
                format!("The {} installation target has no actions.", target.app),
            ));
        }
        let mut actions = Vec::with_capacity(target.install.len());
        let mut destinations: HashMap<String, ActionKind> = HashMap::new();
        for (index, raw) in target.install.iter().enumerate() {
            let action = validate_action(raw).map_err(|mut error| {
                error.message = format!(
                    "Installation action #{} for {} is invalid: {}",
                    index + 1,
                    target.app,
                    error.message
                );
                error
            })?;
            let destination_key = action.destination.to_ascii_lowercase();
            if let Some(existing) = destinations.insert(destination_key, action.kind.clone()) {
                return Err(AppError::new(
                    "duplicate_destination",
                    format!(
                        "Installation actions write to the same destination more than once ({:?} and {:?}): {}. NexusTools will not guess the intended result.",
                        existing,
                        action.kind,
                        action.destination
                    ),
                ));
            }
            actions.push(action);
        }
        Ok(actions)
    }
}

fn require_field(value: &Option<String>, name: &str, action: &str) -> AppResult<String> {
    value
        .clone()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AppError::new(
                "invalid_installation_action",
                format!("The '{action}' action requires '{name}'."),
            )
        })
}

fn reject_field(value: &Option<String>, name: &str, action: &str) -> AppResult<()> {
    if value.is_some() {
        return Err(AppError::new(
            "invalid_installation_action",
            format!("The '{action}' action does not support '{name}'."),
        ));
    }
    Ok(())
}

fn validate_source_path(source: &str) -> AppResult<()> {
    if source == "." {
        return Ok(());
    }
    if source.is_empty() {
        return Err(AppError::new(
            "invalid_source_path",
            "A copy action has an empty source.",
        ));
    }
    if source.starts_with('/')
        || source.starts_with('\\')
        || source.get(1..2) == Some(":")
        || source.contains("..")
    {
        return Err(AppError::new(
            "invalid_source_path",
            format!("Copy source must stay inside sourceDirectory: {source}"),
        ));
    }
    let normalized = source.replace('\\', "/");
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".."
            || segment.ends_with(' ')
            || segment.ends_with('.')
            || is_windows_reserved(segment)
        {
            return Err(AppError::new(
                "invalid_source_path",
                format!("Copy source has an invalid path segment: {source}"),
            ));
        }
    }
    Ok(())
}

fn validate_marker(marker: &str) -> AppResult<()> {
    let regex = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9 ._-]{0,63}$").unwrap();
    if !regex.is_match(marker) {
        return Err(AppError::new(
            "invalid_patch_marker",
            "patchText marker must be 1-64 letters, digits, spaces, dots, underscores, or hyphens and start with a letter or digit.",
        ));
    }
    Ok(())
}

fn validate_action(raw: &RawAction) -> AppResult<ValidatedAction> {
    if !raw.unsupported_fields.is_empty() {
        return Err(AppError::new(
            "unsupported_action_field",
            format!(
                "The '{}' action uses unsupported field(s): {}. Nothing was installed.",
                raw.action,
                raw.unsupported_fields
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let replacement = match raw.replace.as_deref() {
        None | Some("owned") => ReplacementRule::Owned,
        Some(rule) => {
            return Err(AppError::new(
                "unsupported_replacement_rule",
                format!(
                    "The manifest requests unsupported replacement rule '{rule}'. Only the default owned-file rule is supported."
                ),
            ))
        }
    };
    let action = raw.action.as_str();
    let destination = require_field(&raw.destination, "destination", action)?;
    match action {
        "copy" => {
            let source = require_field(&raw.source, "source", action)?;
            validate_source_path(&source)?;
            reject_field(&raw.content, "content", action)?;
            reject_field(&raw.marker, "marker", action)?;
            Ok(ValidatedAction {
                kind: ActionKind::Copy,
                source: Some(source),
                destination,
                content: None,
                marker: None,
                replacement,
            })
        }
        "mkdir" => {
            reject_field(&raw.source, "source", action)?;
            reject_field(&raw.content, "content", action)?;
            reject_field(&raw.marker, "marker", action)?;
            if raw.replace.is_some() {
                return Err(AppError::new(
                    "unsupported_replacement_rule",
                    "mkdir does not support a replacement rule.",
                ));
            }
            Ok(ValidatedAction {
                kind: ActionKind::Mkdir,
                source: None,
                destination,
                content: None,
                marker: None,
                replacement,
            })
        }
        "writeFile" => {
            let content = require_field(&raw.content, "content", action)?;
            reject_field(&raw.source, "source", action)?;
            reject_field(&raw.marker, "marker", action)?;
            Ok(ValidatedAction {
                kind: ActionKind::WriteFile,
                source: None,
                destination,
                content: Some(content),
                marker: None,
                replacement,
            })
        }
        "patchText" => {
            let content = require_field(&raw.content, "content", action)?;
            let marker = require_field(&raw.marker, "marker", action)?;
            validate_marker(&marker)?;
            reject_field(&raw.source, "source", action)?;
            if raw.replace.is_some() {
                return Err(AppError::new(
                    "unsupported_replacement_rule",
                    "patchText manages a marked block and does not support a replacement rule.",
                ));
            }
            Ok(ValidatedAction {
                kind: ActionKind::PatchText,
                source: None,
                destination,
                content: Some(content),
                marker: Some(marker),
                replacement,
            })
        }
        other => Err(AppError::new(
            "unsupported_action",
            format!(
                "Unsupported installation action '{other}'. NexusTools supports copy, mkdir, writeFile, and patchText only. No scripts are executed."
            ),
        )),
    }
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedPaths {
    pub user_scripts: Option<PathBuf>,
    pub user_modules: Option<PathBuf>,
    pub user_prefs: Option<PathBuf>,
    pub zplugins: Option<PathBuf>,
    pub plugin_data: PathBuf,
}

impl ResolvedPaths {
    pub fn approved_roots(&self) -> Vec<PathBuf> {
        [
            self.user_scripts.clone(),
            self.user_modules.clone(),
            self.user_prefs.clone(),
            self.zplugins.clone(),
            Some(self.plugin_data.clone()),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

#[derive(Debug, Clone)]
pub struct SubstitutionContext<'a> {
    pub paths: &'a ResolvedPaths,
    pub plugin_id: &'a str,
    pub plugin_name: &'a str,
    pub plugin_version: &'a str,
    pub target_version: &'a str,
}

impl<'a> SubstitutionContext<'a> {
    fn value_for(&self, name: &str, allow_paths: bool) -> AppResult<String> {
        if allow_paths {
            if let Some(path) = name.strip_prefix("paths.") {
                return match path {
                    "userScripts" => self
                        .paths
                        .user_scripts
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .ok_or_else(|| {
                            AppError::new(
                                "unresolved_path",
                                "The destination requires ${paths.userScripts}, but no Maya user scripts folder could be resolved.",
                            )
                        }),
                    "userModules" => self
                        .paths
                        .user_modules
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .ok_or_else(|| {
                            AppError::new(
                                "unresolved_path",
                                "The destination requires ${paths.userModules}, but no Maya user modules folder could be resolved.",
                            )
                        }),
                    "userPrefs" => self
                        .paths
                        .user_prefs
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .ok_or_else(|| {
                            AppError::new(
                                "unresolved_path",
                                "The destination requires ${paths.userPrefs}, but no Maya user preferences folder could be resolved.",
                            )
                        }),
                    "zplugins" => self
                        .paths
                        .zplugins
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .ok_or_else(|| {
                            AppError::new(
                                "unresolved_path",
                                "The destination requires ${paths.zplugins}, but no ZBrush plugins folder could be resolved.",
                            )
                        }),
                    "pluginData" => Ok(self.paths.plugin_data.to_string_lossy().to_string()),
                    _ => Err(AppError::new(
                        "unsupported_placeholder",
                        format!("Unsupported path placeholder ${{paths.{path}}}."),
                    )),
                };
            }
        }
        match name {
            "plugin.id" => Ok(self.plugin_id.to_string()),
            "plugin.name" => Ok(self.plugin_name.to_string()),
            "plugin.version" => Ok(self.plugin_version.to_string()),
            "target.version" => Ok(self.target_version.to_string()),
            name if name.starts_with("paths.") && !allow_paths => Err(AppError::new(
                "unsupported_placeholder",
                format!("${{{name}}} is not supported in text content."),
            )),
            _ => Err(AppError::new(
                "unsupported_placeholder",
                format!("Unsupported placeholder ${{{name}}}."),
            )),
        }
    }

    fn substitute(&self, template: &str, allow_paths: bool) -> AppResult<String> {
        let regex = Regex::new(r"\$\{([^}]+)\}").unwrap();
        let mut error: Option<AppError> = None;
        let replaced = regex.replace_all(template, |captures: &regex::Captures<'_>| {
            if error.is_some() {
                return captures[0].to_string();
            }
            match self.value_for(&captures[1], allow_paths) {
                Ok(value) => value,
                Err(found) => {
                    error = Some(found);
                    captures[0].to_string()
                }
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
        if replaced.contains("${") {
            return Err(AppError::new(
                "unresolved_placeholder",
                format!("The manifest contains an unresolved placeholder: {template}"),
            ));
        }
        Ok(replaced.into_owned())
    }

    pub fn resolve_destination(&self, template: &str) -> AppResult<PathBuf> {
        let substituted = self.substitute(template, true)?;
        normalize_absolute_path(&substituted)
    }

    pub fn resolve_content(&self, template: &str) -> AppResult<String> {
        self.substitute(template, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled_manifest() -> PluginManifest {
        let json = r#"{
          "schemaVersion": 1,
          "id": "example-plugin",
          "name": "Example Plugin",
          "description": "Short description.",
          "overview": "Extended description.",
          "version": "1.0.2",
          "sourceDirectory": "files",
          "targets": [
            {
              "app": "maya",
              "os": "windows",
              "supportedVersions": ["2024", "2027"],
              "install": [
                {"action":"copy","source":".","destination":"${paths.userScripts}/example_plugin"},
                {"action":"writeFile","destination":"${paths.pluginData}/state.txt","content":"id=${plugin.id} version=${plugin.version}"},
                {"action":"patchText","destination":"${paths.userPrefs}/startup.py","marker":"example-plugin","content":"import example_plugin"}
              ]
            }
          ]
        }"#;
        PluginManifest::from_bytes(json.as_bytes()).unwrap()
    }

    fn context<'a>(paths: &'a ResolvedPaths) -> SubstitutionContext<'a> {
        SubstitutionContext {
            paths,
            plugin_id: "example-plugin",
            plugin_name: "Example Plugin",
            plugin_version: "1.0.2",
            target_version: "2024",
        }
    }

    fn paths() -> ResolvedPaths {
        ResolvedPaths {
            user_scripts: Some(PathBuf::from(r"C:\Users\Test\Documents\maya\2024\scripts")),
            user_modules: Some(PathBuf::from(r"C:\Users\Test\Documents\maya\2024\modules")),
            user_prefs: Some(PathBuf::from(r"C:\Users\Test\Documents\maya\2024\prefs")),
            zplugins: None,
            plugin_data: PathBuf::from(
                r"C:\Users\Test\AppData\Local\NexusTools\plugin-data\example\maya-2024-abc",
            ),
        }
    }

    #[test]
    fn accepts_contract_example_and_resolves_actions() {
        let manifest = bundled_manifest();
        manifest
            .validate_for_installation("example-plugin", "1.0.2")
            .unwrap();
        let target = manifest.matching_target("maya", "2024").unwrap();
        let actions = manifest.validated_actions(target).unwrap();
        assert_eq!(actions.len(), 3);
        let paths = paths();
        let ctx = context(&paths);
        assert_eq!(
            ctx.resolve_destination(&actions[0].destination).unwrap(),
            PathBuf::from(r"C:\Users\Test\Documents\maya\2024\scripts\example_plugin")
        );
        assert_eq!(
            ctx.resolve_content(actions[1].content.as_deref().unwrap())
                .unwrap(),
            "id=example-plugin version=1.0.2"
        );
    }

    #[test]
    fn bundled_id_and_version_must_match_catalog_release() {
        let manifest = bundled_manifest();
        assert_eq!(
            manifest
                .validate_for_installation("other", "1.0.2")
                .unwrap_err()
                .code,
            "bundled_manifest_id_mismatch"
        );
        assert_eq!(
            manifest
                .validate_for_installation("example-plugin", "1.0.3")
                .unwrap_err()
                .code,
            "bundled_manifest_version_mismatch"
        );
        // A leading v on the release tag is normalized.
        manifest
            .validate_for_installation("example-plugin", "v1.0.2")
            .unwrap();
    }

    #[test]
    fn metadata_requires_declared_asset_paths() {
        let manifest: PluginManifest = serde_json::from_str(
            r#"{"schemaVersion":1,"id":"x","name":"X","description":"d","overview":"o","version":"1.0","assets":{"banner":"assets/Banner.PNG"},"content":{"readme":"README.md","features":"FEATURES.md"}}"#,
        )
        .unwrap();
        manifest.validate_metadata("x").unwrap();
        let assets = manifest
            .metadata_asset_paths("for installer/plugin.json")
            .unwrap();
        assert_eq!(assets.banner, "for installer/assets/Banner.PNG");
        assert_eq!(assets.readme, "for installer/README.md");
    }

    #[test]
    fn rejects_ambiguous_or_unsupported_application_targets() {
        let manifest: PluginManifest = serde_json::from_str(
            r#"{"schemaVersion":1,"id":"x","version":"1.0","sourceDirectory":"files","targets":[
                {"app":"maya","os":"windows","supportedVersions":["2024"],"install":[{"action":"mkdir","destination":"C:/x"}]},
                {"app":"maya","os":"windows","supportedVersions":["2024"],"install":[{"action":"mkdir","destination":"C:/y"}]},
                {"app":"photoshop","os":"windows","supportedVersions":["2025"],"install":[{"action":"mkdir","destination":"C:/z"}]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            manifest.matching_target("maya", "2024").unwrap_err().code,
            "ambiguous_installation_target"
        );
        let photoshop = manifest.matching_target("photoshop", "2025").unwrap();
        assert_eq!(
            manifest.validated_actions(photoshop).unwrap_err().code,
            "unsupported_application"
        );
        assert_eq!(
            manifest.matching_target("maya", "2023").unwrap_err().code,
            "unsupported_application_version"
        );
    }

    #[test]
    fn rejects_unsupported_actions_and_replacement_rules() {
        let manifest: PluginManifest = serde_json::from_str(
            r#"{"schemaVersion":1,"id":"x","version":"1.0","sourceDirectory":"files","targets":[
                {"app":"maya","os":"windows","supportedVersions":["2024"],"install":[
                    {"action":"runScript","source":"install.bat","destination":"C:/x"},
                    {"action":"copy","source":"x","destination":"C:/y","replace":"always"}
                ]}
            ]}"#,
        )
        .unwrap();
        let target = manifest.matching_target("maya", "2024").unwrap();
        assert_eq!(
            manifest.validated_actions(target).unwrap_err().code,
            "unsupported_action"
        );

        let manifest: PluginManifest = serde_json::from_str(
            r#"{"schemaVersion":1,"id":"x","version":"1.0","sourceDirectory":"files","targets":[
                {"app":"maya","os":"windows","supportedVersions":["2024"],"install":[
                    {"action":"copy","source":"x","destination":"C:/y","replace":"always"}
                ]}
            ]}"#,
        )
        .unwrap();
        let target = manifest.matching_target("maya", "2024").unwrap();
        assert_eq!(
            manifest.validated_actions(target).unwrap_err().code,
            "unsupported_replacement_rule"
        );
    }

    #[test]
    fn rejects_source_traversal_and_unsupported_fields() {
        let manifest: PluginManifest = serde_json::from_str(
            r#"{"schemaVersion":1,"id":"x","version":"1.0","sourceDirectory":"files","targets":[
                {"app":"maya","os":"windows","supportedVersions":["2024"],"install":[
                    {"action":"copy","source":"../escape.py","destination":"C:/y"}
                ]}
            ]}"#,
        )
        .unwrap();
        let target = manifest.matching_target("maya", "2024").unwrap();
        assert_eq!(
            manifest.validated_actions(target).unwrap_err().code,
            "invalid_source_path"
        );

        let manifest: PluginManifest = serde_json::from_str(
            r#"{"schemaVersion":1,"id":"x","version":"1.0","sourceDirectory":"files","targets":[
                {"app":"maya","os":"windows","supportedVersions":["2024"],"install":[
                    {"action":"mkdir","destination":"C:/y","recursive":true}
                ]}
            ]}"#,
        )
        .unwrap();
        let target = manifest.matching_target("maya", "2024").unwrap();
        assert_eq!(
            manifest.validated_actions(target).unwrap_err().code,
            "unsupported_action_field"
        );
    }

    #[test]
    fn unresolved_and_unknown_placeholders_are_rejected() {
        let paths = paths();
        let ctx = context(&paths);
        assert_eq!(
            ctx.resolve_destination("${paths.doesNotExist}/x")
                .unwrap_err()
                .code,
            "unsupported_placeholder"
        );
        assert_eq!(
            ctx.resolve_destination("${credentials.token}/x")
                .unwrap_err()
                .code,
            "unsupported_placeholder"
        );
        let mut no_paths = paths.clone();
        no_paths.user_scripts = None;
        let no_paths_ctx = context(&no_paths);
        assert_eq!(
            no_paths_ctx
                .resolve_destination("${paths.userScripts}/x")
                .unwrap_err()
                .code,
            "unresolved_path"
        );
        assert_eq!(
            ctx.resolve_destination("relative/path").unwrap_err().code,
            "unresolved_path"
        );
    }

    #[test]
    fn duplicate_destinations_are_rejected() {
        let manifest: PluginManifest = serde_json::from_str(
            r#"{"schemaVersion":1,"id":"x","version":"1.0","sourceDirectory":"files","targets":[
                {"app":"maya","os":"windows","supportedVersions":["2024"],"install":[
                    {"action":"mkdir","destination":"C:/y"},
                    {"action":"copy","source":"x","destination":"C:/Y"}
                ]}
            ]}"#,
        )
        .unwrap();
        let target = manifest.matching_target("maya", "2024").unwrap();
        assert_eq!(
            manifest.validated_actions(target).unwrap_err().code,
            "duplicate_destination"
        );
    }
}
