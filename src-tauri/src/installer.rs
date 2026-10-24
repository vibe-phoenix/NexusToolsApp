use crate::apps::{is_application_running, AppInstallation};
use crate::error::{AppError, AppResult};
use crate::manifest::{ManifestTarget, ResolvedPaths, SubstitutionContext, ValidatedAction};
use crate::package::ValidatedPackage;
use crate::store::{
    find_block_owner, find_owner, InstalledFileRecord, InstalledStore, InstalledTarget,
    ManagedBlockRecord, NexusPaths,
};
use crate::util::{
    atomic_write, contains_reparse_point, is_windows_reserved, normalize_absolute_path,
    path_is_within, sanitize_filename_component, sha256_bytes, sha256_file,
};
use chrono::Utc;
use fs2::available_space;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;
use walkdir::WalkDir;

const MAX_TEXT_DESTINATION_BYTES: u64 = 32 * 1024 * 1024;
const MAX_WRITTEN_TEXT_BYTES: usize = 16 * 1024 * 1024;
const FREE_SPACE_MARGIN: u64 = 16 * 1024 * 1024;
const JOURNAL_SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
static TEST_FAIL_AFTER_FILES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(usize::MAX);

#[derive(Debug, Clone)]
pub struct ReleaseRecord {
    pub repository: String,
    pub release_id: Option<i64>,
    pub release_tag: String,
    pub release_version: String,
    pub asset_name: String,
    pub asset_sha256: String,
    pub digest_verified: bool,
}

#[derive(Debug, Clone)]
pub struct InstallProgress {
    pub phase: String,
    pub percent: u8,
    pub message: String,
}

pub type ProgressSink<'a> = dyn Fn(InstallProgress) + Send + Sync + 'a;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallResult {
    pub target_id: String,
    pub app: String,
    pub app_version: String,
    pub installed_version: String,
    pub file_count: usize,
    pub warnings: Vec<String>,
}

pub struct Installer<'a> {
    pub paths: &'a NexusPaths,
    pub store: &'a InstalledStore,
}

impl<'a> Installer<'a> {
    /// Read-only (apart from harmless write probes) preflight for one target.
    /// Returns the exact outside destination paths that need explicit user
    /// approval, if any. Compatibility, ownership, path, source, and disk
    /// problems are returned as errors before anything is modified.
    pub fn preflight_target(
        &self,
        plugin_id: &str,
        package: &ValidatedPackage,
        target: &AppInstallation,
        manifest_target: &ManifestTarget,
        actions: &[ValidatedAction],
        release: &ReleaseRecord,
        approved_outside: &[String],
        cancel: &AtomicBool,
    ) -> AppResult<Vec<String>> {
        check_cancel(cancel)?;
        let manifest = &package.manifest;
        manifest.validate_for_installation(plugin_id, &release.release_version)?;
        let matching_target = manifest.matching_target(target.app.id(), &target.version)?;
        if matching_target.install.len() != manifest_target.install.len() {
            return Err(AppError::new(
                "manifest_changed",
                "The bundled installation target changed unexpectedly during preflight.",
            ));
        }
        if target.app == crate::apps::AppKind::ZBrush
            || manifest_target.requires_app_closed.unwrap_or(false)
        {
            if is_application_running(&target.app) {
                return Err(AppError::new(
                    "application_running",
                    format!(
                        "Close {} before installing this plugin.",
                        target.app.display_name()
                    ),
                ));
            }
        }
        let plugin_data = self.paths.plugin_data_dir(&manifest.id, &target.target_id);
        let resolved_paths = ResolvedPaths {
            user_scripts: target.paths.user_scripts.clone(),
            user_modules: target.paths.user_modules.clone(),
            user_prefs: target.paths.user_prefs.clone(),
            zplugins: target.paths.zplugins.clone(),
            plugin_data,
        };
        let substitution = SubstitutionContext {
            paths: &resolved_paths,
            plugin_id: &manifest.id,
            plugin_name: &manifest.name,
            plugin_version: &manifest.version,
            target_version: &target.version,
        };
        let previous_target = self.store.target(&manifest.id, &target.target_id);
        let plan = build_plan(
            self.paths,
            self.store,
            package,
            target,
            &resolved_paths,
            &substitution,
            actions,
            approved_outside,
            previous_target,
            cancel,
        )?;
        Ok(plan
            .required_outside
            .into_iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect())
    }

    pub fn install_target(
        &self,
        plugin_id: &str,
        package: &ValidatedPackage,
        target: &AppInstallation,
        manifest_target: &ManifestTarget,
        actions: &[ValidatedAction],
        release: &ReleaseRecord,
        approved_outside: &[String],
        cancel: &AtomicBool,
        progress: &ProgressSink<'_>,
    ) -> AppResult<InstallResult> {
        check_cancel(cancel)?;
        let manifest = &package.manifest;
        // This is the authoritative bundled manifest for the selected release.
        // It checks the catalog id and the release version (including a leading
        // v) before any path is touched.
        manifest.validate_for_installation(plugin_id, &release.release_version)?;
        let matching_target = manifest.matching_target(target.app.id(), &target.version)?;
        if matching_target.install.len() != manifest_target.install.len() {
            return Err(AppError::new(
                "manifest_changed",
                "The bundled installation target changed unexpectedly during installation.",
            ));
        }

        let plugin_data = self.paths.plugin_data_dir(&manifest.id, &target.target_id);
        let resolved_paths = ResolvedPaths {
            user_scripts: target.paths.user_scripts.clone(),
            user_modules: target.paths.user_modules.clone(),
            user_prefs: target.paths.user_prefs.clone(),
            zplugins: target.paths.zplugins.clone(),
            plugin_data,
        };
        let substitution = SubstitutionContext {
            paths: &resolved_paths,
            plugin_id: &manifest.id,
            plugin_name: &manifest.name,
            plugin_version: &manifest.version,
            target_version: &target.version,
        };
        if target.app == crate::apps::AppKind::ZBrush
            || manifest_target.requires_app_closed.unwrap_or(false)
        {
            if is_application_running(&target.app) {
                return Err(AppError::new(
                    "application_running",
                    format!(
                        "Close {} before installing this plugin. Its plugin files are loaded at application startup.",
                        target.app.display_name()
                    ),
                ));
            }
        }
        progress(InstallProgress {
            phase: "preflight".to_string(),
            percent: 8,
            message: "Checking files and paths".to_string(),
        });

        let previous_target = self.store.target(&manifest.id, &target.target_id);
        let plan = build_plan(
            self.paths,
            self.store,
            package,
            target,
            &resolved_paths,
            &substitution,
            actions,
            approved_outside,
            previous_target.clone(),
            cancel,
        )?;
        if !plan.required_outside.is_empty() {
            let details = plan
                .required_outside
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect::<Vec<_>>();
            return Err(AppError::with_details(
                "outside_destination_roots",
                "This plugin needs to write outside its managed application folders. Review and approve the exact paths, then retry.",
                details,
            ));
        }

        check_cancel(cancel)?;
        progress(InstallProgress {
            phase: "backup".to_string(),
            percent: 22,
            message: "Backing up affected files".to_string(),
        });
        let operation_id = Uuid::new_v4().simple().to_string();
        let backup_dir = self
            .paths
            .backups_dir()
            .join(&manifest.id)
            .join(safe_target_segment(&target.target_id))
            .join(&operation_id);
        fs::create_dir_all(&backup_dir)?;
        let backup_plan = prepare_backups(&plan, &backup_dir)?;
        let mut journal = TransactionJournal::new(
            operation_id.clone(),
            manifest.id.clone(),
            manifest.name.clone(),
            target.target_id.clone(),
            previous_target.clone(),
            backup_dir.to_string_lossy().to_string(),
            &plan,
            &backup_plan,
        );
        let journal_path = self
            .paths
            .transactions_dir()
            .join(format!("{operation_id}.json"));
        crate::util::atomic_write_json(&journal_path, &journal)?;

        let apply_result = apply_plan(&plan, &backup_plan, cancel, progress);
        if let Err(error) = apply_result {
            let rollback_errors = rollback_journal(self.paths, self.store, &journal);
            journal.phase = JournalPhase::RolledBack;
            let _ = crate::util::atomic_write_json(&journal_path, &journal);
            let _ = fs::remove_file(&journal_path);
            let mut returned = error;
            for rollback_error in rollback_errors {
                returned.details.push(format!("Rollback: {rollback_error}"));
            }
            if returned.code == "cancelled" {
                returned.message =
                    "Installation was cancelled and its changes were rolled back.".to_string();
            }
            return Err(returned);
        }

        check_cancel(cancel)?;
        progress(InstallProgress {
            phase: "record".to_string(),
            percent: 88,
            message: "Recording installed files".to_string(),
        });
        let now = Utc::now();
        let installed_at = previous_target
            .as_ref()
            .map(|target| target.installed_at)
            .unwrap_or(now);
        let installed = InstalledTarget {
            target_id: target.target_id.clone(),
            app: target.app.id().to_string(),
            app_name: target.app.display_name().to_string(),
            app_version: target.version.clone(),
            install_root: target.install_root.to_string_lossy().to_string(),
            repository: release.repository.clone(),
            plugin_version: manifest.version.clone(),
            release_id: release.release_id,
            release_tag: release.release_tag.clone(),
            asset_name: release.asset_name.clone(),
            asset_sha256: release.asset_sha256.clone(),
            digest_verified: release.digest_verified,
            installed_at,
            updated_at: now,
            status: "installed".to_string(),
            operation_id: operation_id.clone(),
            files: plan.final_files.clone(),
            directories_created: plan
                .final_directories
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect(),
            managed_blocks: plan.final_blocks.clone(),
            backup_dir: Some(backup_dir.to_string_lossy().to_string()),
            bundled_manifest: package.manifest_json.clone(),
            resolved_paths: path_map(&resolved_paths),
        };

        journal.phase = JournalPhase::Committing;
        crate::util::atomic_write_json(&journal_path, &journal)?;
        if let Err(error) = self
            .store
            .put_target(&manifest.id, &manifest.name, installed.clone())
        {
            let rollback_errors = rollback_journal(self.paths, self.store, &journal);
            journal.phase = JournalPhase::RolledBack;
            let _ = crate::util::atomic_write_json(&journal_path, &journal);
            let _ = fs::remove_file(&journal_path);
            let mut returned = error;
            for rollback_error in rollback_errors {
                returned.details.push(format!("Rollback: {rollback_error}"));
            }
            return Err(returned);
        }
        journal.phase = JournalPhase::Committed;
        let _ = crate::util::atomic_write_json(&journal_path, &journal);
        let _ = fs::remove_file(&journal_path);

        progress(InstallProgress {
            phase: "complete".to_string(),
            percent: 100,
            message: "Installation complete".to_string(),
        });
        Ok(InstallResult {
            target_id: target.target_id.clone(),
            app: target.app.display_name().to_string(),
            app_version: target.version.clone(),
            installed_version: manifest.version.clone(),
            file_count: installed.files.len(),
            warnings: plan.warnings,
        })
    }
}

// The catalog id is available through the target id's plugin context; the
// installer receives its manifest from the release, so this helper validates
// against itself while the outer operation separately checked the catalog id.
#[derive(Debug, Clone)]
struct PlannedWrite {
    destination: PathBuf,
    content: PlannedContent,
    new_sha256: String,
    size: u64,
    patch: Option<PatchRecord>,
    scope_root: PathBuf,
}

#[derive(Debug, Clone)]
enum PlannedContent {
    Copy(PathBuf),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone)]
struct PatchRecord {
    marker: String,
    comment_prefix: String,
    block_sha256: String,
}

#[derive(Debug, Clone)]
struct PlannedBlockRemoval {
    destination: PathBuf,
    new_content: Vec<u8>,
    new_sha256: String,
    previous: ManagedBlockRecord,
    scope_root: PathBuf,
}

#[derive(Debug, Clone)]
struct PlannedRemoval {
    destination: PathBuf,
    previous: InstalledFileRecord,
    scope_root: PathBuf,
}

#[derive(Debug, Clone)]
struct InstallPlan {
    writes: Vec<PlannedWrite>,
    block_removals: Vec<PlannedBlockRemoval>,
    file_removals: Vec<PlannedRemoval>,
    create_dirs: BTreeSet<PathBuf>,
    required_outside: Vec<PathBuf>,
    final_files: Vec<InstalledFileRecord>,
    final_blocks: Vec<ManagedBlockRecord>,
    final_directories: BTreeSet<PathBuf>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct BackupPlanEntry {
    destination: PathBuf,
    backup: PathBuf,
    existed: bool,
    new_sha256: Option<String>,
}

fn build_plan(
    paths: &NexusPaths,
    store: &InstalledStore,
    package: &ValidatedPackage,
    target: &AppInstallation,
    resolved_paths: &ResolvedPaths,
    substitution: &SubstitutionContext<'_>,
    actions: &[ValidatedAction],
    approved_outside: &[String],
    previous_target: Option<InstalledTarget>,
    cancel: &AtomicBool,
) -> AppResult<InstallPlan> {
    let approved_roots = resolved_paths.approved_roots();
    let mut approved_extra = Vec::new();
    for raw in approved_outside {
        let path = normalize_absolute_path(raw)?;
        approved_extra.push(path);
    }
    let canonical_source = fs::canonicalize(&package.source_dir)?;
    let registry = store.snapshot();
    let previous_files = previous_target
        .as_ref()
        .map(|target| target.files.clone())
        .unwrap_or_default();
    let previous_blocks = previous_target
        .as_ref()
        .map(|target| target.managed_blocks.clone())
        .unwrap_or_default();

    // First resolve only top-level destinations and collect the minimum exact
    // approval roots for paths outside the managed folders. No plan structures
    // or parent directories are built until those approvals exist.
    let mut required_outside: BTreeSet<PathBuf> = BTreeSet::new();
    for action in actions {
        check_cancel(cancel)?;
        let destination = substitution.resolve_destination(&action.destination)?;
        validate_destination_components(&destination)?;
        if scope_for_destination(&destination, &approved_roots, &approved_extra).is_some() {
            continue;
        }
        let approval_scope = match action.kind {
            crate::manifest::ActionKind::Mkdir => Some(destination.clone()),
            crate::manifest::ActionKind::Copy => {
                let source = action.source.as_deref().ok_or_else(|| {
                    AppError::new("invalid_installation_action", "copy source is missing")
                })?;
                let source_path = if source == "." {
                    canonical_source.clone()
                } else {
                    fs::canonicalize(canonical_source.join(source.replace('/', "\\"))).map_err(
                        |_| {
                            AppError::new(
                                "missing_source",
                                format!("The release package is missing copy source '{source}'."),
                            )
                        },
                    )?
                };
                if source_path.is_dir() {
                    Some(destination.clone())
                } else {
                    destination.parent().map(Path::to_path_buf)
                }
            }
            crate::manifest::ActionKind::WriteFile | crate::manifest::ActionKind::PatchText => {
                destination.parent().map(Path::to_path_buf)
            }
        };
        let approval_scope = approval_scope.ok_or_else(|| {
            AppError::new(
                "unresolved_path",
                format!(
                    "Cannot determine an approval root for {}",
                    destination.display()
                ),
            )
        })?;
        required_outside.insert(normalize_absolute_path(&approval_scope.to_string_lossy())?);
    }
    if !required_outside.is_empty() {
        return Ok(InstallPlan {
            writes: vec![],
            block_removals: vec![],
            file_removals: vec![],
            create_dirs: BTreeSet::new(),
            required_outside: required_outside.into_iter().collect(),
            final_files: vec![],
            final_blocks: vec![],
            final_directories: BTreeSet::new(),
            warnings: vec![],
        });
    }

    let mut writes: Vec<PlannedWrite> = Vec::new();
    let mut create_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut patch_by_destination: HashMap<String, PatchRecord> = HashMap::new();
    let mut planned_file_destinations: HashSet<String> = HashSet::new();
    let mut new_block_records: Vec<ManagedBlockRecord> = Vec::new();
    let mut warnings = Vec::new();
    let mut final_scope: HashMap<String, PathBuf> = HashMap::new();

    for action in actions {
        check_cancel(cancel)?;
        let destination = substitution.resolve_destination(&action.destination)?;
        validate_destination_components(&destination)?;
        let scope_root = scope_for_destination(&destination, &approved_roots, &approved_extra)
            .ok_or_else(|| {
                AppError::new(
                    "outside_destination_roots",
                    format!(
                        "The destination {} is outside the managed folders and was not approved.",
                        destination.display()
                    ),
                )
            })?;
        ensure_existing_destination_safe(&destination, &scope_root)?;
        match action.kind {
            crate::manifest::ActionKind::Mkdir => {
                ensure_destination_directory(&destination)?;
                collect_missing_directories(&destination, &scope_root, &mut create_dirs)?;
                final_scope.insert(normalize_key(&destination), scope_root);
            }
            crate::manifest::ActionKind::Copy => {
                let source = action.source.as_deref().ok_or_else(|| {
                    AppError::new("invalid_installation_action", "copy source is missing")
                })?;
                let source_path = if source == "." {
                    canonical_source.clone()
                } else {
                    let candidate = canonical_source.join(source.replace('/', "\\"));
                    let canonical = fs::canonicalize(&candidate).map_err(|_| {
                        AppError::new(
                            "missing_source",
                            format!("The release package is missing copy source '{source}'."),
                        )
                    })?;
                    if !path_is_within(&canonical, &canonical_source) {
                        return Err(AppError::new(
                            "invalid_source_path",
                            format!("Copy source '{source}' resolves outside sourceDirectory."),
                        ));
                    }
                    canonical
                };
                if source_path.is_dir() {
                    if destination.exists() && !destination.is_dir() {
                        return Err(AppError::new(
                            "file_conflict",
                            format!(
                                "Cannot merge a directory into an existing file: {}",
                                destination.display()
                            ),
                        ));
                    }
                    collect_missing_directories(&destination, &scope_root, &mut create_dirs)?;
                    let mut copied_any = false;
                    for entry in WalkDir::new(&source_path).follow_links(false) {
                        check_cancel(cancel)?;
                        let entry = entry.map_err(|error| {
                            AppError::new(
                                "package_read_failed",
                                format!("Could not inspect a package directory: {error}"),
                            )
                        })?;
                        if entry.path() == source_path {
                            continue;
                        }
                        if entry.file_type().is_symlink()
                            || is_reparse(entry.metadata().ok().as_ref())
                        {
                            return Err(AppError::new(
                                "archive_symlink",
                                format!(
                                    "The package source contains a symbolic link or junction: {}",
                                    entry.path().display()
                                ),
                            ));
                        }
                        let relative = entry.path().strip_prefix(&source_path).map_err(|_| {
                            AppError::new(
                                "invalid_source_path",
                                "Could not compute a package-relative source path.",
                            )
                        })?;
                        let target_path = destination.join(relative);
                        if entry.file_type().is_dir() {
                            collect_missing_directories(
                                &target_path,
                                &scope_root,
                                &mut create_dirs,
                            )?;
                            continue;
                        }
                        if !entry.file_type().is_file() {
                            continue;
                        }
                        copied_any = true;
                        add_copy_write(
                            &mut writes,
                            &mut planned_file_destinations,
                            entry.path().to_path_buf(),
                            target_path,
                            scope_root.clone(),
                        )?;
                    }
                    let _ = copied_any;
                } else {
                    add_copy_write(
                        &mut writes,
                        &mut planned_file_destinations,
                        source_path,
                        destination.clone(),
                        scope_root.clone(),
                    )?;
                    if let Some(parent) = destination.parent() {
                        collect_missing_directories(parent, &scope_root, &mut create_dirs)?;
                    }
                }
            }
            crate::manifest::ActionKind::WriteFile => {
                let content = substitution.resolve_content(
                    action.content.as_deref().ok_or_else(|| {
                        AppError::new(
                            "invalid_installation_action",
                            "writeFile content is missing",
                        )
                    })?,
                )?;
                let bytes = content.into_bytes();
                if bytes.len() > MAX_WRITTEN_TEXT_BYTES {
                    return Err(AppError::new(
                        "write_file_too_large",
                        "writeFile content exceeds the permitted size.",
                    ));
                }
                let hash = sha256_bytes(&bytes);
                let size = bytes.len() as u64;
                if !planned_file_destinations.insert(normalize_key(&destination)) {
                    return Err(AppError::new(
                        "duplicate_destination",
                        format!(
                            "More than one installation action would write {}.",
                            destination.display()
                        ),
                    ));
                }
                ensure_destination_can_be_file(&destination)?;
                if let Some(parent) = destination.parent() {
                    collect_missing_directories(parent, &scope_root, &mut create_dirs)?;
                }
                writes.push(PlannedWrite {
                    destination: destination.clone(),
                    content: PlannedContent::Bytes(bytes),
                    new_sha256: hash,
                    size,
                    patch: None,
                    scope_root: scope_root.clone(),
                });
                final_scope.insert(normalize_key(&destination), scope_root.clone());
            }
            crate::manifest::ActionKind::PatchText => {
                let content = substitution.resolve_content(
                    action.content.as_deref().ok_or_else(|| {
                        AppError::new(
                            "invalid_installation_action",
                            "patchText content is missing",
                        )
                    })?,
                )?;
                let marker = action.marker.as_deref().ok_or_else(|| {
                    AppError::new("invalid_installation_action", "patchText marker is missing")
                })?;
                let comment = comment_prefix(&destination)?;
                if destination.exists() && !destination.is_file() {
                    return Err(AppError::new(
                        "file_conflict",
                        format!(
                            "patchText destination is not a file: {}",
                            destination.display()
                        ),
                    ));
                }
                let current = read_text_destination(&destination)?;
                let prior_record = previous_blocks.iter().find(|block| {
                    normalize_key(Path::new(&block.path)) == normalize_key(&destination)
                        && block.marker == marker
                });
                if prior_record.is_some_and(|record| record.preserved_modified) {
                    return Err(AppError::new(
                        "file_modified",
                        format!(
                            "The managed block in {} was modified locally and will not be overwritten automatically.",
                            destination.display()
                        ),
                    ));
                }
                let newline = preferred_newline(current.as_deref().unwrap_or(""));
                let patched = compute_patch(
                    current.as_deref().unwrap_or(""),
                    marker,
                    comment,
                    &content,
                    newline,
                    prior_record.map(|record| record.block_sha256.as_str()),
                )?;
                if patched.bytes.len() > MAX_WRITTEN_TEXT_BYTES {
                    return Err(AppError::new(
                        "patch_too_large",
                        "patchText produced a file larger than the permitted size.",
                    ));
                }
                let key = normalize_key(&destination);
                if !planned_file_destinations.insert(key.clone()) {
                    return Err(AppError::new(
                        "duplicate_destination",
                        format!(
                            "More than one installation action would write {}.",
                            destination.display()
                        ),
                    ));
                }
                if let Some(parent) = destination.parent() {
                    collect_missing_directories(parent, &scope_root, &mut create_dirs)?;
                }
                let hash = sha256_bytes(&patched.bytes);
                let patch = PatchRecord {
                    marker: marker.to_string(),
                    comment_prefix: comment.to_string(),
                    block_sha256: patched.block_sha256.clone(),
                };
                patch_by_destination.insert(key.clone(), patch.clone());
                new_block_records.push(ManagedBlockRecord {
                    path: destination.to_string_lossy().to_string(),
                    marker: marker.to_string(),
                    comment_prefix: comment.to_string(),
                    block_sha256: patched.block_sha256,
                    obsolete: false,
                    preserved_modified: false,
                });
                writes.push(PlannedWrite {
                    destination: destination.clone(),
                    content: PlannedContent::Bytes(patched.bytes),
                    new_sha256: hash,
                    size: patched.size,
                    patch: Some(patch),
                    scope_root: scope_root.clone(),
                });
                final_scope.insert(key, scope_root);
            }
        }
    }

    // Ownership and local-modification checks for every destination.
    for write in &writes {
        let owner = find_owner(
            &registry,
            &package.manifest.id,
            &target.target_id,
            &write.destination,
        );
        if let Some(owner) = owner {
            return Err(AppError::new(
                "file_owned_by_other",
                format!(
                    "{} is owned by plugin '{}' (target '{}'). NexusTools will not overwrite another plugin's file.",
                    write.destination.display(),
                    owner.plugin_id,
                    owner.target_id
                ),
            ));
        }
        if write.destination.exists() {
            if !write.destination.is_file() {
                return Err(AppError::new(
                    "file_conflict",
                    format!(
                        "A plugin file destination is a directory: {}",
                        write.destination.display()
                    ),
                ));
            }
            ensure_not_readonly(&write.destination)?;
            let previous = previous_files.iter().find(|file| {
                normalize_key(Path::new(&file.path)) == normalize_key(&write.destination)
            });
            if write.patch.is_none() {
                match previous {
                    Some(record) if !record.preserved_modified => {
                        let on_disk = sha256_file(&write.destination)?;
                        if on_disk != record.sha256 {
                            return Err(AppError::new(
                                "file_modified",
                                format!(
                                    "The file {} was modified after NexusTools installed it. It was left untouched.",
                                    write.destination.display()
                                ),
                            ));
                        }
                    }
                    Some(record) => {
                        return Err(AppError::new(
                            "file_modified",
                            format!(
                                "The plugin-owned file {} was previously preserved because of local changes.",
                                write.destination.display()
                            ),
                        ));
                    }
                    None => {
                        return Err(AppError::new(
                            "file_conflict",
                            format!(
                                "{} already exists and is not owned by this plugin. NexusTools will not overwrite it.",
                                write.destination.display()
                            ),
                        ));
                    }
                }
            } else if let Some(previous) = previous {
                if previous.preserved_modified {
                    return Err(AppError::new(
                        "file_modified",
                        format!(
                            "The file {} was preserved because of local changes.",
                            write.destination.display()
                        ),
                    ));
                }
            }
        }
        ensure_no_reparse(&write.scope_root, &write.destination)?;
    }
    for mkdir in &create_dirs {
        if mkdir.exists() && !mkdir.is_dir() {
            return Err(AppError::new(
                "file_conflict",
                format!(
                    "A required directory path exists as a file: {}",
                    mkdir.display()
                ),
            ));
        }
        let scope = final_scope
            .get(&normalize_key(mkdir))
            .cloned()
            .or_else(|| scope_for_destination(mkdir, &approved_roots, &approved_extra));
        if let Some(scope) = scope {
            ensure_no_reparse(&scope, mkdir)?;
        }
    }

    let mut final_files: Vec<InstalledFileRecord> = writes
        .iter()
        .filter(|write| write.patch.is_none())
        .map(|write| InstalledFileRecord {
            path: write.destination.to_string_lossy().to_string(),
            sha256: write.new_sha256.clone(),
            size: write.size,
            obsolete: false,
            preserved_modified: false,
        })
        .collect();
    let mut final_blocks = new_block_records;
    let planned_all_destinations: HashSet<String> = writes
        .iter()
        .map(|write| normalize_key(&write.destination))
        .collect();
    let planned_file_hashes: HashSet<String> = writes
        .iter()
        .filter(|write| write.patch.is_none())
        .map(|write| normalize_key(&write.destination))
        .collect();

    // Remove files that this plugin owned in a prior version but the new
    // manifest no longer installs. Modified files are preserved and reported.
    let mut file_removals = Vec::new();
    for previous in &previous_files {
        let path = PathBuf::from(&previous.path);
        if planned_all_destinations.contains(&normalize_key(&path)) {
            if !planned_file_hashes.contains(&normalize_key(&path)) {
                warnings.push(format!(
                    "Stopped treating {} as a fully plugin-owned file because this release now manages it with a marker block.",
                    path.display()
                ));
            }
            continue;
        }
        if previous.preserved_modified {
            final_files.push(previous.clone());
            warnings.push(format!(
                "Preserved modified file that is no longer part of the plugin: {}",
                path.display()
            ));
            continue;
        }
        if !path.exists() {
            continue;
        }
        if !path.is_file() {
            warnings.push(format!(
                "Obsolete plugin path is no longer a file and was left untouched: {}",
                path.display()
            ));
            continue;
        }
        ensure_not_readonly(&path)?;
        let current_hash = sha256_file(&path)?;
        if current_hash != previous.sha256 {
            let mut preserved = previous.clone();
            preserved.sha256 = current_hash;
            preserved.obsolete = true;
            preserved.preserved_modified = true;
            final_files.push(preserved);
            warnings.push(format!(
                "Preserved locally modified obsolete file: {}",
                path.display()
            ));
            continue;
        }
        let owner = find_owner(&registry, &package.manifest.id, &target.target_id, &path);
        if let Some(owner) = owner {
            return Err(AppError::new(
                "file_owned_by_other",
                format!(
                    "{} is also owned by plugin '{}'; obsolete-file cleanup was stopped.",
                    path.display(),
                    owner.plugin_id
                ),
            ));
        }
        let Some(scope) = scope_for_destination(&path, &approved_roots, &approved_extra) else {
            // The user may have changed a manual path since the previous
            // release. Do not reach outside the current approved roots to delete
            // an old file; keep tracking it for a later uninstall instead.
            let mut preserved = previous.clone();
            preserved.obsolete = true;
            final_files.push(preserved);
            warnings.push(format!(
                "Preserved an obsolete plugin file that is outside the current approved paths: {}",
                path.display()
            ));
            continue;
        };
        ensure_no_reparse(&scope, &path)?;
        file_removals.push(PlannedRemoval {
            destination: path,
            previous: previous.clone(),
            scope_root: scope,
        });
    }

    // Remove managed blocks no longer present in the bundled manifest.
    let new_block_keys: HashSet<(String, String)> = final_blocks
        .iter()
        .map(|block| (normalize_key(Path::new(&block.path)), block.marker.clone()))
        .collect();
    let mut block_removals = Vec::new();
    let mut handled_block_files = HashMap::<String, usize>::new();
    for block in &previous_blocks {
        let key = (normalize_key(Path::new(&block.path)), block.marker.clone());
        if new_block_keys.contains(&key) {
            continue;
        }
        if block.preserved_modified {
            final_blocks.push(block.clone());
            warnings.push(format!(
                "Preserved a modified managed block in {}",
                block.path
            ));
            continue;
        }
        let path = PathBuf::from(&block.path);
        if !path.exists() {
            continue;
        }
        if !path.is_file() {
            warnings.push(format!(
                "Obsolete managed-block file is no longer a regular file and was left untouched: {}",
                path.display()
            ));
            continue;
        }
        ensure_not_readonly(&path)?;
        let file_key = normalize_key(&path);
        if planned_all_destinations.contains(&file_key)
            || patch_by_destination.contains_key(&file_key)
            || handled_block_files.contains_key(&file_key)
        {
            return Err(AppError::new(
                "unsupported_managed_block_overlap",
                format!(
                    "The update would need to write a file and modify its managed block in {}. NexusTools does not guess how to combine them; remove this plugin and reinstall it.",
                    path.display()
                ),
            ));
        }
        let Some(scope) = scope_for_destination(&path, &approved_roots, &approved_extra) else {
            let mut preserved = block.clone();
            preserved.obsolete = true;
            final_blocks.push(preserved);
            warnings.push(format!(
                "Preserved an obsolete managed block outside the current approved paths: {}",
                path.display()
            ));
            continue;
        };
        ensure_no_reparse(&scope, &path)?;
        let current = fs::read_to_string(&path).map_err(|error| {
            AppError::io(
                &format!("Cannot read the managed text file {}", path.display()),
                error,
            )
        })?;
        match remove_patch_block(&current, &block.marker, &block.comment_prefix)? {
            PatchRemoval::NotPresent => continue,
            PatchRemoval::Modified => {
                let mut preserved = block.clone();
                preserved.obsolete = true;
                preserved.preserved_modified = true;
                final_blocks.push(preserved);
                warnings.push(format!(
                    "Preserved a locally modified managed block in {}",
                    path.display()
                ));
                continue;
            }
            PatchRemoval::Removed {
                content,
                block_sha256,
            } => {
                if block_sha256 != block.block_sha256 {
                    let mut preserved = block.clone();
                    preserved.obsolete = true;
                    preserved.preserved_modified = true;
                    final_blocks.push(preserved);
                    warnings.push(format!(
                        "Preserved a managed block whose contents changed: {}",
                        path.display()
                    ));
                    continue;
                }
                // Preserve the surrounding file's exact newline bytes. Only the
                // managed block is removed.
                let bytes = content.into_bytes();
                block_removals.push(PlannedBlockRemoval {
                    destination: path.clone(),
                    new_content: bytes.clone(),
                    new_sha256: sha256_bytes(&bytes),
                    previous: block.clone(),
                    scope_root: scope,
                });
                *handled_block_files.entry(file_key).or_insert(0) += 1;
            }
        }
    }

    validate_path_topology(&writes, &create_dirs, &block_removals)?;

    final_files.sort_by(|a, b| a.path.cmp(&b.path));
    final_blocks.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.marker.cmp(&b.marker)));
    let mut final_directories: BTreeSet<PathBuf> = create_dirs.clone();
    if let Some(previous) = &previous_target {
        for directory in &previous.directories_created {
            let path = PathBuf::from(directory);
            if path.exists() {
                final_directories.insert(path);
            }
        }
    }

    check_disk_and_permissions(
        &writes,
        &block_removals,
        &file_removals,
        &create_dirs,
        paths,
    )?;

    Ok(InstallPlan {
        writes,
        block_removals,
        file_removals,
        create_dirs,
        required_outside: vec![],
        final_files,
        final_blocks,
        final_directories,
        warnings,
    })
}

fn validate_path_topology(
    writes: &[PlannedWrite],
    create_dirs: &BTreeSet<PathBuf>,
    block_removals: &[PlannedBlockRemoval],
) -> AppResult<()> {
    let write_paths: HashMap<String, &PlannedWrite> = writes
        .iter()
        .map(|write| (normalize_key(&write.destination), write))
        .collect();
    for write in writes {
        let mut ancestor = write.destination.parent();
        while let Some(path) = ancestor {
            if let Some(parent) = write_paths.get(&normalize_key(path)) {
                return Err(AppError::new(
                    "file_conflict",
                    format!(
                        "Two installation files overlap: {} is a parent of {}. Nothing was installed.",
                        parent.destination.display(),
                        write.destination.display()
                    ),
                ));
            }
            ancestor = path.parent();
        }
    }
    for directory in create_dirs {
        if let Some(write) = write_paths.get(&normalize_key(directory)) {
            return Err(AppError::new(
                "file_conflict",
                format!(
                    "The same path is both a file and a required directory: {}.",
                    write.destination.display()
                ),
            ));
        }
    }
    for removal in block_removals {
        if write_paths.contains_key(&normalize_key(&removal.destination)) {
            return Err(AppError::new(
                "unsupported_managed_block_overlap",
                format!(
                    "Both a file write and a managed-block update target {}.",
                    removal.destination.display()
                ),
            ));
        }
    }
    Ok(())
}

fn add_copy_write(
    writes: &mut Vec<PlannedWrite>,
    planned_destinations: &mut HashSet<String>,
    source: PathBuf,
    destination: PathBuf,
    scope_root: PathBuf,
) -> AppResult<()> {
    if !planned_destinations.insert(normalize_key(&destination)) {
        return Err(AppError::new(
            "duplicate_destination",
            format!(
                "More than one installation action would write {}.",
                destination.display()
            ),
        ));
    }
    if destination.exists() && !destination.is_file() {
        return Err(AppError::new(
            "file_conflict",
            format!(
                "A plugin file destination already exists as a directory: {}",
                destination.display()
            ),
        ));
    }
    let hash = sha256_file(&source)?;
    let size = fs::metadata(&source)?.len();
    writes.push(PlannedWrite {
        destination,
        content: PlannedContent::Copy(source),
        new_sha256: hash,
        size,
        patch: None,
        scope_root,
    });
    Ok(())
}

fn scope_for_destination(
    destination: &Path,
    approved_roots: &[PathBuf],
    approved_extra: &[PathBuf],
) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = approved_roots
        .iter()
        .chain(approved_extra.iter())
        .filter(|root| path_is_within(destination, root))
        .cloned()
        .collect();
    candidates.sort_by_key(|path| path.components().count());
    candidates.pop()
}

fn ensure_existing_destination_safe(destination: &Path, scope_root: &Path) -> AppResult<()> {
    if let Some(reparse) = contains_reparse_point(scope_root, destination) {
        return Err(AppError::new(
            "unsafe_destination",
            format!(
                "The destination resolves through a symbolic link or junction and was refused: {}",
                reparse.display()
            ),
        ));
    }
    Ok(())
}

fn ensure_no_reparse(scope_root: &Path, destination: &Path) -> AppResult<()> {
    ensure_existing_destination_safe(destination, scope_root)
}

fn ensure_not_readonly(path: &Path) -> AppResult<()> {
    let metadata = fs::metadata(path)?;
    if metadata.permissions().readonly() {
        return Err(AppError::new(
            "permission_denied",
            format!(
                "A plugin-owned file is marked read-only and cannot be replaced safely: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn validate_destination_components(path: &Path) -> AppResult<()> {
    #[cfg(windows)]
    {
        use std::path::Component;
        for component in path.components() {
            if let Component::Normal(part) = component {
                let text = part.to_string_lossy();
                if text.contains(':') {
                    return Err(AppError::new(
                        "invalid_destination",
                        format!(
                            "A destination contains an alternate-data-stream separator and was rejected: {}",
                            path.display()
                        ),
                    ));
                }
                if text.ends_with(' ') || text.ends_with('.') || is_windows_reserved(&text) {
                    return Err(AppError::new(
                        "invalid_destination",
                        format!(
                            "A destination contains an unsafe Windows path segment and was rejected: {}",
                            path.display()
                        ),
                    ));
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    Ok(())
}

fn ensure_destination_directory(destination: &Path) -> AppResult<()> {
    if destination.exists() && !destination.is_dir() {
        return Err(AppError::new(
            "file_conflict",
            format!(
                "A required directory is an existing file: {}",
                destination.display()
            ),
        ));
    }
    Ok(())
}

fn ensure_destination_can_be_file(destination: &Path) -> AppResult<()> {
    if destination.exists() && !destination.is_file() {
        return Err(AppError::new(
            "file_conflict",
            format!(
                "A plugin file destination is an existing directory: {}",
                destination.display()
            ),
        ));
    }
    Ok(())
}

fn collect_missing_directories(
    directory: &Path,
    scope_root: &Path,
    output: &mut BTreeSet<PathBuf>,
) -> AppResult<()> {
    if !path_is_within(directory, scope_root) {
        return Err(AppError::new(
            "unsafe_destination",
            format!(
                "Destination {} is outside its approved root {}.",
                directory.display(),
                scope_root.display()
            ),
        ));
    }
    let mut current = Some(directory.to_path_buf());
    while let Some(path) = current {
        if path.exists() {
            if !path.is_dir() {
                return Err(AppError::new(
                    "file_conflict",
                    format!("A required parent path is a file: {}", path.display()),
                ));
            }
            break;
        }
        output.insert(path.clone());
        if path == *scope_root {
            break;
        }
        current = path.parent().map(Path::to_path_buf);
    }
    Ok(())
}

fn read_text_destination(path: &Path) -> AppResult<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_TEXT_DESTINATION_BYTES {
        return Err(AppError::new(
            "patch_destination_too_large",
            format!("patchText destination is too large: {}", path.display()),
        ));
    }
    Ok(Some(fs::read_to_string(path).map_err(|_| {
        AppError::new(
            "patch_destination_not_utf8",
            format!(
                "patchText destination is not UTF-8 text: {}",
                path.display()
            ),
        )
    })?))
}

fn comment_prefix(path: &Path) -> AppResult<&'static str> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "mel" | "js" | "jsx" | "ts" | "tsx" | "css" | "scss" => Ok("//"),
        "lua" => Ok("--"),
        "py" | "txt" | "cfg" | "conf" | "toml" | "properties" | "ini" => Ok("#"),
        _ => Err(AppError::new(
            "unsupported_patch_file",
            format!(
                "patchText does not support the destination file type '.{extension}'. Use .mel, .py, .js, .ts, .css, .lua, .txt, .cfg, .conf, .toml, .properties, or .ini."
            ),
        )),
    }
}

fn preferred_newline(content: &str) -> &'static str {
    if content.is_empty() {
        if cfg!(windows) {
            "\r\n"
        } else {
            "\n"
        }
    } else {
        let crlf = content.matches("\r\n").count();
        let lf = content.matches('\n').count();
        if crlf > 0 && crlf * 2 >= lf {
            "\r\n"
        } else {
            "\n"
        }
    }
}

fn normalize_newlines_to(content: &str, newline: &str) -> String {
    content.replace("\r\n", "\n").replace('\n', newline)
}

#[derive(Debug)]
struct PatchComputation {
    bytes: Vec<u8>,
    block_sha256: String,
    size: u64,
}

fn patch_markers(marker: &str, comment: &str) -> (String, String) {
    (
        format!("{comment} >>> NexusTools:{marker} >>>"),
        format!("{comment} <<< NexusTools:{marker} <<<"),
    )
}

fn line_without_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn compute_patch(
    current: &str,
    marker: &str,
    comment: &str,
    content: &str,
    newline: &str,
    previous_block_hash: Option<&str>,
) -> AppResult<PatchComputation> {
    let (begin, end) = patch_markers(marker, comment);
    let lines: Vec<&str> = current.split_inclusive('\n').collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line_without_ending(line) == begin)
        .map(|(index, _)| index)
        .collect();
    let ends: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line_without_ending(line) == end)
        .map(|(index, _)| index)
        .collect();
    if starts.len() > 1 || ends.len() > 1 {
        return Err(AppError::new(
            "duplicate_managed_block",
            format!(
                "The managed block marker '{marker}' appears more than once in a destination file."
            ),
        ));
    }
    let normalized_content = content.replace("\r\n", "\n");
    for line in normalized_content.lines() {
        let trimmed = line.trim_start();
        if trimmed == begin
            || trimmed == end
            || trimmed.starts_with(&format!("{comment} >>> NexusTools:"))
            || trimmed.starts_with(&format!("{comment} <<< NexusTools:"))
        {
            return Err(AppError::new(
                "invalid_patch_content",
                format!(
                    "patchText content contains a NexusTools marker line for '{marker}', which would make the managed block ambiguous."
                ),
            ));
        }
    }
    let content_block = format!(
        "{}{}{}{}",
        begin,
        newline,
        normalize_newlines_to(normalized_content.trim_end_matches('\n'), newline),
        if normalized_content.trim_end().is_empty() {
            ""
        } else {
            newline
        }
    );
    let full_block = format!("{content_block}{end}");
    let block_sha256 = sha256_bytes(full_block.as_bytes());
    let full_block_with_newline = format!("{full_block}{newline}");

    if starts.is_empty() && ends.is_empty() {
        let mut output = String::new();
        if current.is_empty() {
            output.push_str(&full_block_with_newline);
        } else {
            output.push_str(current);
            if !current.ends_with('\n') {
                output.push_str(newline);
            }
            if !current.ends_with(&format!("{newline}{newline}")) {
                output.push_str(newline);
            }
            output.push_str(&full_block_with_newline);
        }
        let bytes = output.into_bytes();
        return Ok(PatchComputation {
            size: bytes.len() as u64,
            bytes,
            block_sha256,
        });
    }
    if starts.len() != ends.len() {
        return Err(AppError::new(
            "malformed_managed_block",
            format!("The managed block marker '{marker}' has only one of its begin/end markers."),
        ));
    }
    let start = starts[0];
    let finish = ends[0];
    if finish <= start {
        return Err(AppError::new(
            "malformed_managed_block",
            format!("The managed block marker '{marker}' ends before it begins."),
        ));
    }
    let existing_block = lines[start..=finish].concat();
    let existing_hash = sha256_bytes(existing_block.trim_end_matches(['\r', '\n']).as_bytes());
    if let Some(expected) = previous_block_hash {
        if existing_hash != expected {
            return Err(AppError::new(
                "file_modified",
                format!(
                    "The managed block '{marker}' was modified locally. NexusTools did not overwrite it."
                ),
            ));
        }
    } else {
        return Err(AppError::new(
            "file_conflict",
            format!(
                "A NexusTools block with marker '{marker}' already exists but is not owned by this plugin target."
            ),
        ));
    }
    let start_offset: usize = lines[..start].iter().map(|line| line.len()).sum();
    let end_offset: usize = lines[..=finish].iter().map(|line| line.len()).sum();
    let mut output = String::new();
    output.push_str(&current[..start_offset]);
    output.push_str(&full_block_with_newline);
    output.push_str(&current[end_offset..]);
    let bytes = output.into_bytes();
    Ok(PatchComputation {
        size: bytes.len() as u64,
        bytes,
        block_sha256,
    })
}

enum PatchRemoval {
    NotPresent,
    Modified,
    Removed {
        content: String,
        block_sha256: String,
    },
}

fn remove_patch_block(content: &str, marker: &str, comment: &str) -> AppResult<PatchRemoval> {
    let (begin, end) = patch_markers(marker, comment);
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line_without_ending(line) == begin)
        .map(|(index, _)| index)
        .collect();
    let ends: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line_without_ending(line) == end)
        .map(|(index, _)| index)
        .collect();
    if starts.is_empty() && ends.is_empty() {
        return Ok(PatchRemoval::NotPresent);
    }
    if starts.len() != 1 || ends.len() != 1 || ends[0] <= starts[0] {
        return Ok(PatchRemoval::Modified);
    }
    let start = starts[0];
    let finish = ends[0];
    let block = lines[start..=finish].concat();
    let block_sha256 = sha256_bytes(block.trim_end_matches(['\r', '\n']).as_bytes());
    let start_offset: usize = lines[..start].iter().map(|line| line.len()).sum();
    let end_offset: usize = lines[..=finish].iter().map(|line| line.len()).sum();
    let mut output = String::new();
    output.push_str(&content[..start_offset]);
    output.push_str(&content[end_offset..]);
    Ok(PatchRemoval::Removed {
        content: output,
        block_sha256,
    })
}

fn prepare_backups(plan: &InstallPlan, backup_dir: &Path) -> AppResult<Vec<BackupPlanEntry>> {
    fs::create_dir_all(backup_dir)?;
    let mut destinations: BTreeMap<String, (PathBuf, Option<String>)> = BTreeMap::new();
    for write in &plan.writes {
        destinations.insert(
            normalize_key(&write.destination),
            (write.destination.clone(), Some(write.new_sha256.clone())),
        );
    }
    for removal in &plan.block_removals {
        destinations.insert(
            normalize_key(&removal.destination),
            (
                removal.destination.clone(),
                Some(removal.new_sha256.clone()),
            ),
        );
    }
    for removal in &plan.file_removals {
        destinations.insert(
            normalize_key(&removal.destination),
            (removal.destination.clone(), None),
        );
    }
    let mut entries = Vec::new();
    for (index, (_, (destination, new_sha256))) in destinations.into_iter().enumerate() {
        let existed = destination.exists();
        if !existed {
            entries.push(BackupPlanEntry {
                destination,
                backup: PathBuf::new(),
                existed,
                new_sha256,
            });
            continue;
        }
        if !destination.is_file() {
            return Err(AppError::new(
                "file_conflict",
                format!(
                    "Cannot back up a non-file destination: {}",
                    destination.display()
                ),
            ));
        }
        let backup = backup_dir.join(format!(
            "{:04}-{}",
            index,
            sanitize_filename_component(
                destination
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("backup")
            )
        ));
        fs::copy(&destination, &backup).map_err(|error| {
            AppError::io(
                &format!("Cannot back up {} before installing", destination.display()),
                error,
            )
        })?;
        entries.push(BackupPlanEntry {
            destination,
            backup,
            existed,
            new_sha256,
        });
    }
    Ok(entries)
}

fn apply_plan(
    plan: &InstallPlan,
    backups: &[BackupPlanEntry],
    cancel: &AtomicBool,
    progress: &ProgressSink<'_>,
) -> AppResult<()> {
    let mut directories: Vec<&Path> = plan.create_dirs.iter().map(PathBuf::as_path).collect();
    directories.sort_by_key(|path| path.components().count());
    let total_units = (directories.len()
        + plan.writes.len()
        + plan.block_removals.len()
        + plan.file_removals.len())
    .max(1);
    let mut completed = 0usize;
    for directory in directories {
        check_cancel(cancel)?;
        fs::create_dir_all(directory).map_err(|error| {
            AppError::io(
                &format!("Cannot create the plugin directory {}", directory.display()),
                error,
            )
        })?;
        completed += 1;
        progress(InstallProgress {
            phase: "install".to_string(),
            percent: 25 + ((completed * 60 / total_units) as u8).min(60),
            message: "Creating plugin directories".to_string(),
        });
    }
    let mut writes_applied = 0usize;
    for write in &plan.writes {
        check_cancel(cancel)?;
        write_content(write)?;
        writes_applied += 1;
        #[cfg(test)]
        {
            if TEST_FAIL_AFTER_FILES.load(Ordering::SeqCst) == writes_applied {
                return Err(AppError::new(
                    "test_injected_failure",
                    "Injected installation failure for rollback testing.",
                ));
            }
        }
        completed += 1;
        progress(InstallProgress {
            phase: "install".to_string(),
            percent: 25 + ((completed * 60 / total_units) as u8).min(60),
            message: format!("Installing {}", write.destination.display()),
        });
    }
    for removal in &plan.block_removals {
        check_cancel(cancel)?;
        atomic_write(&removal.destination, &removal.new_content)?;
        completed += 1;
        progress(InstallProgress {
            phase: "install".to_string(),
            percent: 25 + ((completed * 60 / total_units) as u8).min(60),
            message: "Updating managed configuration blocks".to_string(),
        });
    }
    for removal in &plan.file_removals {
        check_cancel(cancel)?;
        if removal.destination.exists() {
            fs::remove_file(&removal.destination).map_err(|error| {
                AppError::io(
                    &format!(
                        "Cannot remove obsolete plugin file {}",
                        removal.destination.display()
                    ),
                    error,
                )
            })?;
        }
        completed += 1;
        progress(InstallProgress {
            phase: "install".to_string(),
            percent: 25 + ((completed * 60 / total_units) as u8).min(60),
            message: "Removing obsolete plugin files".to_string(),
        });
    }
    // Validate that all recorded writes are present with the expected bytes.
    for write in &plan.writes {
        let metadata = fs::metadata(&write.destination).map_err(|_| {
            AppError::new(
                "installation_incomplete",
                format!("Installed file is missing: {}", write.destination.display()),
            )
        })?;
        if !metadata.is_file() || metadata.len() != write.size {
            return Err(AppError::new(
                "installation_incomplete",
                format!(
                    "Installed file has an unexpected size: {}",
                    write.destination.display()
                ),
            ));
        }
        let hash = sha256_file(&write.destination)?;
        if hash != write.new_sha256 {
            return Err(AppError::new(
                "installation_incomplete",
                format!(
                    "Installed file failed verification: {}",
                    write.destination.display()
                ),
            ));
        }
    }
    let _ = backups;
    Ok(())
}

fn write_content(write: &PlannedWrite) -> AppResult<()> {
    if let Some(parent) = write.destination.parent() {
        fs::create_dir_all(parent)?;
    }
    match &write.content {
        PlannedContent::Bytes(bytes) => {
            atomic_write(&write.destination, bytes)?;
        }
        PlannedContent::Copy(source) => {
            let bytes = fs::read(source).map_err(|error| {
                AppError::io(
                    &format!("Cannot read package file {}", source.display()),
                    error,
                )
            })?;
            if sha256_bytes(&bytes) != write.new_sha256 {
                return Err(AppError::new(
                    "package_changed",
                    format!(
                        "A staged package file changed unexpectedly: {}",
                        source.display()
                    ),
                ));
            }
            atomic_write(&write.destination, &bytes)?;
        }
    }
    Ok(())
}

fn check_disk_and_permissions(
    writes: &[PlannedWrite],
    block_removals: &[PlannedBlockRemoval],
    file_removals: &[PlannedRemoval],
    directories: &BTreeSet<PathBuf>,
    paths: &NexusPaths,
) -> AppResult<()> {
    let mut per_volume: HashMap<String, u64> = HashMap::new();
    let mut backup_bytes = 0u64;
    for write in writes {
        let existing = fs::metadata(&write.destination)
            .map(|m| m.len())
            .unwrap_or(0);
        backup_bytes += existing;
        let volume = nearest_existing_volume(&write.destination)?;
        *per_volume.entry(volume).or_default() += write.size + existing;
    }
    for removal in block_removals {
        let size = fs::metadata(&removal.destination)
            .map(|m| m.len())
            .unwrap_or(0);
        backup_bytes += size;
        let volume = nearest_existing_volume(&removal.destination)?;
        *per_volume.entry(volume).or_default() += removal.new_content.len() as u64 + size;
    }
    for removal in file_removals {
        let size = fs::metadata(&removal.destination)
            .map(|m| m.len())
            .unwrap_or(0);
        backup_bytes += size;
        let volume = nearest_existing_volume(&removal.destination)?;
        *per_volume.entry(volume).or_default() += size;
    }
    let _ = directories;
    for (volume, needed) in per_volume {
        if let Ok(available) = available_space(&volume) {
            if available < needed.saturating_add(FREE_SPACE_MARGIN) {
                return Err(AppError::new(
                    "insufficient_disk_space",
                    format!(
                        "Not enough free disk space on {volume}. Required about {} bytes plus a safety margin, available {} bytes.",
                        needed,
                        available
                    ),
                ));
            }
        }
    }
    if backup_bytes > 0 {
        if let Ok(available) = available_space(paths.backups_dir()) {
            if available < backup_bytes.saturating_add(FREE_SPACE_MARGIN) {
                return Err(AppError::new(
                    "insufficient_disk_space",
                    "Not enough free space is available for NexusTools's backup copies.",
                ));
            }
        }
    }

    let mut probes: BTreeSet<PathBuf> = BTreeSet::new();
    for write in writes {
        probes.insert(nearest_existing_ancestor(&write.destination)?);
    }
    for removal in block_removals {
        probes.insert(nearest_existing_ancestor(&removal.destination)?);
    }
    for removal in file_removals {
        probes.insert(nearest_existing_ancestor(&removal.destination)?);
    }
    for directory in directories {
        probes.insert(nearest_existing_ancestor(directory)?);
    }
    for directory in probes {
        let probe = directory.join(format!(".nexustools-write-test-{}", Uuid::new_v4()));
        let result = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe);
        match result {
            Ok(mut file) => {
                let _ = file.write_all(b"ok");
                drop(file);
                let _ = fs::remove_file(&probe);
            }
            Err(error) => {
                return Err(AppError::new(
                    "permission_denied",
                    format!(
                        "NexusTools cannot write to {}. Check folder permissions and try a manual path override. ({error})",
                        directory.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn nearest_existing_volume(path: &Path) -> AppResult<String> {
    let ancestor = nearest_existing_ancestor(path)?;
    let mut current = ancestor.as_path();
    loop {
        if current.parent().is_none() {
            return Ok(current.to_string_lossy().to_string());
        }
        let parent = current.parent().unwrap();
        if parent.parent().is_none() {
            return Ok(parent.to_string_lossy().to_string());
        }
        current = parent;
    }
}

fn nearest_existing_ancestor(path: &Path) -> AppResult<PathBuf> {
    let mut current = Some(path.to_path_buf());
    while let Some(candidate) = current {
        if candidate.exists() {
            if candidate.is_dir() {
                return Ok(candidate);
            }
            return candidate.parent().map(Path::to_path_buf).ok_or_else(|| {
                AppError::new("unresolved_path", "No existing parent folder was found.")
            });
        }
        current = candidate.parent().map(Path::to_path_buf);
    }
    Err(AppError::new(
        "unresolved_path",
        format!("No existing parent folder was found for {}", path.display()),
    ))
}

fn check_cancel(cancel: &AtomicBool) -> AppResult<()> {
    if cancel.load(Ordering::SeqCst) {
        Err(AppError::new("cancelled", "Installation was cancelled."))
    } else {
        Ok(())
    }
}

fn normalize_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn safe_target_segment(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect()
}

fn path_map(paths: &ResolvedPaths) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(path) = &paths.user_scripts {
        map.insert(
            "userScripts".to_string(),
            path.to_string_lossy().to_string(),
        );
    }
    if let Some(path) = &paths.user_modules {
        map.insert(
            "userModules".to_string(),
            path.to_string_lossy().to_string(),
        );
    }
    if let Some(path) = &paths.user_prefs {
        map.insert("userPrefs".to_string(), path.to_string_lossy().to_string());
    }
    if let Some(path) = &paths.zplugins {
        map.insert("zplugins".to_string(), path.to_string_lossy().to_string());
    }
    map.insert(
        "pluginData".to_string(),
        paths.plugin_data.to_string_lossy().to_string(),
    );
    map
}

fn is_reparse(metadata: Option<&fs::Metadata>) -> bool {
    let Some(metadata) = metadata else {
        return false;
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 || metadata.file_type().is_symlink()
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum JournalPhase {
    Prepared,
    Committing,
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JournalFileOperation {
    destination: String,
    backup: Option<String>,
    new_sha256: Option<String>,
    existed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransactionJournal {
    schema_version: u32,
    operation_id: String,
    plugin_id: String,
    plugin_name: String,
    target_id: String,
    phase: JournalPhase,
    previous_target: Option<InstalledTarget>,
    backup_dir: String,
    files: Vec<JournalFileOperation>,
    created_dirs: Vec<String>,
}

impl TransactionJournal {
    fn new(
        operation_id: String,
        plugin_id: String,
        plugin_name: String,
        target_id: String,
        previous_target: Option<InstalledTarget>,
        backup_dir: String,
        plan: &InstallPlan,
        backups: &[BackupPlanEntry],
    ) -> Self {
        let files = backups
            .iter()
            .map(|backup| JournalFileOperation {
                destination: backup.destination.to_string_lossy().to_string(),
                backup: backup
                    .existed
                    .then(|| backup.backup.to_string_lossy().to_string()),
                new_sha256: backup.new_sha256.clone(),
                existed: backup.existed,
            })
            .collect();
        let mut created_dirs: Vec<String> = plan
            .create_dirs
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect();
        created_dirs.sort_by_key(|path| std::cmp::Reverse(path.len()));
        Self {
            schema_version: JOURNAL_SCHEMA_VERSION,
            operation_id,
            plugin_id,
            plugin_name,
            target_id,
            phase: JournalPhase::Prepared,
            previous_target,
            backup_dir,
            files,
            created_dirs,
        }
    }
}

fn rollback_journal(
    _paths: &NexusPaths,
    store: &InstalledStore,
    journal: &TransactionJournal,
) -> Vec<String> {
    let mut errors = Vec::new();
    for operation in &journal.files {
        let destination = PathBuf::from(&operation.destination);
        if let Some(backup) = &operation.backup {
            if fs::metadata(backup).is_ok() {
                if let Some(parent) = destination.parent() {
                    if let Err(error) = fs::create_dir_all(parent) {
                        errors.push(format!("Cannot restore {}: {error}", destination.display()));
                        continue;
                    }
                }
                if let Err(error) = fs::copy(backup, &destination) {
                    errors.push(format!("Cannot restore {}: {error}", destination.display()));
                }
            } else {
                errors.push(format!(
                    "Backup file is missing while rolling back {}: {}",
                    destination.display(),
                    backup
                ));
            }
        } else if !operation.existed && destination.exists() {
            if let Some(expected) = &operation.new_sha256 {
                match sha256_file(&destination) {
                    Ok(hash) if hash == *expected => {
                        if let Err(error) = fs::remove_file(&destination) {
                            errors
                                .push(format!("Cannot remove {}: {error}", destination.display()));
                        }
                    }
                    Ok(_) => errors.push(format!(
                        "Left a file that changed during rollback untouched: {}",
                        destination.display()
                    )),
                    Err(error) => errors.push(format!(
                        "Cannot verify a new file while rolling back {}: {error}",
                        destination.display()
                    )),
                }
            }
        }
    }
    for directory in &journal.created_dirs {
        let path = Path::new(directory);
        if path.is_dir() {
            let _ = fs::remove_dir(path);
        }
    }
    if let Err(error) = store.restore_target(
        &journal.plugin_id,
        &journal.target_id,
        journal.previous_target.clone(),
    ) {
        errors.push(format!(
            "Cannot restore installed-state metadata: {}",
            error.message
        ));
    }
    errors
}

/// Detect interrupted transactions and either finish a committed state update
/// or roll back file changes. Runs before any cached plugin is shown.
pub fn recover_transactions(paths: &NexusPaths, store: &InstalledStore) -> Vec<String> {
    let mut problems = Vec::new();
    let directory = paths.transactions_dir();
    let _ = fs::create_dir_all(&directory);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) => {
            return vec![format!("Cannot inspect transaction journals: {error}")];
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let journal: TransactionJournal = match fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<TransactionJournal>(&bytes).ok())
        {
            Some(journal) if journal.schema_version == JOURNAL_SCHEMA_VERSION => journal,
            _ => {
                problems.push(format!(
                    "Ignored an unreadable installation journal at {}.",
                    path.display()
                ));
                continue;
            }
        };
        match journal.phase {
            JournalPhase::Committed | JournalPhase::RolledBack => {
                let _ = fs::remove_file(&path);
            }
            JournalPhase::Committing => {
                let committed = store
                    .target(&journal.plugin_id, &journal.target_id)
                    .is_some_and(|target| target.operation_id == journal.operation_id);
                if committed {
                    let _ = fs::remove_file(&path);
                } else {
                    let errors = rollback_journal(paths, store, &journal);
                    if errors.is_empty() {
                        let _ = fs::remove_file(&path);
                    } else {
                        problems.extend(errors);
                    }
                }
            }
            JournalPhase::Prepared => {
                let errors = rollback_journal(paths, store, &journal);
                if errors.is_empty() {
                    let _ = fs::remove_file(&path);
                } else {
                    problems.extend(errors);
                }
            }
        }
    }
    problems
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallResult {
    pub target_id: String,
    pub app: String,
    pub app_version: String,
    pub removed_files: usize,
    pub removed_blocks: usize,
    pub skipped: Vec<String>,
    pub warnings: Vec<String>,
    pub fully_removed: bool,
}

/// Uninstall strictly from local installation records. Repository and release
/// availability are irrelevant because no network operation occurs here.
pub fn uninstall_target(
    paths: &NexusPaths,
    store: &InstalledStore,
    plugin_id: &str,
    target_id: &str,
) -> AppResult<UninstallResult> {
    let record = store.target(plugin_id, target_id).ok_or_else(|| {
        AppError::new(
            "not_installed",
            format!("Plugin '{plugin_id}' is not installed for target '{target_id}'."),
        )
    })?;
    let backup_dir = paths
        .backups_dir()
        .join(plugin_id)
        .join(safe_target_segment(target_id))
        .join(format!("uninstall-{}", Uuid::new_v4().simple()));
    fs::create_dir_all(&backup_dir)?;
    let registry = store.snapshot();
    let mut skipped = Vec::new();
    let mut remaining_files = Vec::new();
    let mut removed_files = 0usize;
    let mut to_delete: Vec<(PathBuf, PathBuf)> = Vec::new();
    for file in &record.files {
        let path = PathBuf::from(&file.path);
        if file.preserved_modified {
            skipped.push(format!(
                "Preserved locally modified file: {}",
                path.display()
            ));
            remaining_files.push(file.clone());
            continue;
        }
        if !path.exists() {
            continue;
        }
        if !path.is_file() {
            skipped.push(format!(
                "Not a regular file; left untouched: {}",
                path.display()
            ));
            continue;
        }
        let current = sha256_file(&path)?;
        if current != file.sha256 {
            skipped.push(format!(
                "File was modified; left untouched: {}",
                path.display()
            ));
            let mut preserved = file.clone();
            preserved.sha256 = current;
            preserved.preserved_modified = true;
            remaining_files.push(preserved);
            continue;
        }
        if let Some(owner) = find_owner(&registry, plugin_id, target_id, &path) {
            skipped.push(format!(
                "File is still recorded by plugin '{}' target '{}'; left untouched: {}",
                owner.plugin_id,
                owner.target_id,
                path.display()
            ));
            remaining_files.push(file.clone());
            continue;
        }
        let backup = backup_dir.join(format!("file-{:04}", to_delete.len()));
        fs::copy(&path, &backup)?;
        to_delete.push((path, backup));
    }

    let mut remaining_blocks = Vec::new();
    let mut removed_blocks = 0usize;
    let mut block_backups: Vec<(PathBuf, PathBuf)> = Vec::new();
    for block in &record.managed_blocks {
        let path = PathBuf::from(&block.path);
        if block.preserved_modified {
            skipped.push(format!(
                "Preserved modified managed block in {}",
                path.display()
            ));
            remaining_blocks.push(block.clone());
            continue;
        }
        if !path.exists() {
            continue;
        }
        if let Some((owner, owner_target)) = find_block_owner(&registry, &path, &block.marker) {
            if owner != plugin_id || owner_target != target_id {
                skipped.push(format!(
                    "Managed block '{}' is owned by plugin '{}'; left untouched.",
                    block.marker, owner
                ));
                remaining_blocks.push(block.clone());
                continue;
            }
        }
        let current = fs::read_to_string(&path).map_err(|error| {
            AppError::io(
                &format!("Cannot read managed text file {}", path.display()),
                error,
            )
        })?;
        match remove_patch_block(&current, &block.marker, &block.comment_prefix)? {
            PatchRemoval::NotPresent => continue,
            PatchRemoval::Modified => {
                skipped.push(format!(
                    "Managed block '{}' was modified; left untouched in {}.",
                    block.marker,
                    path.display()
                ));
                let mut preserved = block.clone();
                preserved.preserved_modified = true;
                remaining_blocks.push(preserved);
                continue;
            }
            PatchRemoval::Removed {
                content,
                block_sha256,
            } => {
                if block_sha256 != block.block_sha256 {
                    skipped.push(format!(
                        "Managed block '{}' was modified; left untouched in {}.",
                        block.marker,
                        path.display()
                    ));
                    let mut preserved = block.clone();
                    preserved.preserved_modified = true;
                    remaining_blocks.push(preserved);
                    continue;
                }
                if block_backups
                    .iter()
                    .all(|(backed_up, _)| *backed_up != path)
                {
                    let backup = backup_dir.join(format!("block-{:04}", block_backups.len()));
                    fs::copy(&path, &backup)?;
                    block_backups.push((path.clone(), backup));
                }
                atomic_write(&path, content.as_bytes())?;
                removed_blocks += 1;
            }
        }
    }

    // The directory tree is removed from the deepest level up and only when it
    // is completely empty, preserving any unrelated user content.
    for (path, _) in &to_delete {
        fs::remove_file(path).map_err(|error| {
            AppError::io(
                &format!("Cannot remove plugin file {}", path.display()),
                error,
            )
        })?;
        removed_files += 1;
    }
    let mut directories = record.directories_created.clone();
    directories.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for directory in directories {
        let path = PathBuf::from(directory);
        if path.is_dir() {
            let _ = fs::remove_dir(&path);
        }
    }

    let fully_removed = remaining_files.is_empty() && remaining_blocks.is_empty();
    if fully_removed {
        store.remove_target(plugin_id, target_id)?;
    } else {
        let mut remaining = record.clone();
        remaining.files = remaining_files;
        remaining.managed_blocks = remaining_blocks;
        remaining.status = "partial-uninstall".to_string();
        remaining.backup_dir = Some(backup_dir.to_string_lossy().to_string());
        let plugin_name = store
            .plugin(plugin_id)
            .map(|plugin| plugin.name)
            .unwrap_or_else(|| plugin_id.to_string());
        store.put_target(plugin_id, &plugin_name, remaining)?;
    }
    Ok(UninstallResult {
        target_id: target_id.to_string(),
        app: record.app_name,
        app_version: record.app_version,
        removed_files,
        removed_blocks,
        skipped,
        warnings: vec![],
        fully_removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::{AppInstallation, AppKind, AppPaths};
    use crate::manifest::PluginManifest;
    use crate::package::ValidatedPackage;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    fn write_package(
        root: &std::path::Path,
        plugin_id: &str,
        version: &str,
        source_content: &str,
        extra_action: Option<&str>,
    ) -> (ValidatedPackage, ReleaseRecord, Vec<ValidatedAction>) {
        fs::create_dir_all(root.join("files")).unwrap();
        fs::write(root.join("files/main.py"), source_content).unwrap();
        let extra = extra_action.unwrap_or("");
        let manifest_text = format!(
            r#"{{
              "schemaVersion":1,
              "id":"{plugin_id}",
              "name":"Example",
              "description":"Example plugin",
              "overview":"Example overview",
              "version":"{version}",
              "sourceDirectory":"files",
              "targets":[{{
                "app":"maya","os":"windows","supportedVersions":["2024"],
                "install":[
                  {{"action":"copy","source":".","destination":"${{paths.userScripts}}/example"}},
                  {{"action":"writeFile","destination":"${{paths.userScripts}}/example/generated.txt","content":"version=${{plugin.version}}"}},
                  {{"action":"patchText","destination":"${{paths.userPrefs}}/startup.py","marker":"example","content":"import example"}}
                  {extra}
                ]
              }}]
            }}"#
        );
        let manifest = PluginManifest::from_bytes(manifest_text.as_bytes()).unwrap();
        let manifest_target = manifest.matching_target("maya", "2024").unwrap().clone();
        let actions = manifest.validated_actions(&manifest_target).unwrap();
        let manifest_json: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
        let package = ValidatedPackage {
            package_root: root.to_path_buf(),
            source_dir: root.join("files"),
            manifest,
            manifest_json,
        };
        let release = ReleaseRecord {
            repository: "owner/example".to_string(),
            release_id: Some(1),
            release_tag: "v1.0.0".to_string(),
            release_version: version.to_string(),
            asset_name: "example.zip".to_string(),
            asset_sha256: "a".repeat(64),
            digest_verified: true,
        };
        (package, release, actions)
    }

    fn app_for(
        root: &std::path::Path,
        target_id: &str,
        install_root: &std::path::Path,
    ) -> AppInstallation {
        fs::create_dir_all(install_root).unwrap();
        AppInstallation {
            target_id: target_id.to_string(),
            app: AppKind::Maya,
            version: "2024".to_string(),
            install_root: install_root.to_path_buf(),
            executable: None,
            source: "test".to_string(),
            manual: true,
            paths: AppPaths {
                user_scripts: Some(root.join("user/scripts")),
                user_modules: Some(root.join("user/modules")),
                user_prefs: Some(root.join("user/prefs")),
                zplugins: None,
            },
            path_notes: vec![],
            issues: vec![],
            requires_app_closed: false,
        }
    }

    fn test_environment() -> (
        tempfile::TempDir,
        NexusPaths,
        InstalledStore,
        AppInstallation,
        std::path::PathBuf,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let paths = NexusPaths::new(temp.path().join("data"));
        paths.prepare().unwrap();
        let store = InstalledStore::open(paths.installed_state_file()).unwrap();
        let install_root = temp.path().join("fake-maya");
        let app = app_for(temp.path(), "maya-2024-test", &install_root);
        (temp, paths, store, app, install_root)
    }

    fn install(
        installer: &Installer<'_>,
        package: &ValidatedPackage,
        release: &ReleaseRecord,
        actions: &[ValidatedAction],
        app: &AppInstallation,
        plugin_id: &str,
        approved: &[String],
    ) -> AppResult<InstallResult> {
        let target = package.manifest.matching_target("maya", "2024")?.clone();
        let cancel = AtomicBool::new(false);
        let progress = |_event: InstallProgress| {};
        installer.install_target(
            plugin_id, package, app, &target, actions, release, approved, &cancel, &progress,
        )
    }

    #[test]
    fn complete_package_install_records_ownership_and_repeats_safely() {
        let _serial = TEST_SERIAL.lock().unwrap();
        TEST_FAIL_AFTER_FILES.store(usize::MAX, Ordering::SeqCst);
        let (temp, paths, store, app, _) = test_environment();
        let package_root = temp.path().join("package");
        let (package, release, actions) =
            write_package(&package_root, "example", "1.0.0", "print('v1')\n", None);
        let installer = Installer {
            paths: &paths,
            store: &store,
        };
        let result = install(
            &installer,
            &package,
            &release,
            &actions,
            &app,
            "example",
            &[],
        )
        .unwrap();
        assert_eq!(result.file_count, 2);
        let scripts = app.paths.user_scripts.as_ref().unwrap();
        assert_eq!(
            fs::read_to_string(scripts.join("example/main.py")).unwrap(),
            "print('v1')\n"
        );
        assert!(scripts.join("example/generated.txt").is_file());
        let startup = app.paths.user_prefs.as_ref().unwrap().join("startup.py");
        let startup_text = fs::read_to_string(&startup).unwrap();
        assert_eq!(startup_text.matches("NexusTools:example").count(), 2);

        let recorded = store.snapshot();
        let target = recorded
            .target("example", "maya-2024-test")
            .expect("target record");
        assert_eq!(target.files.len(), 2);
        assert_eq!(target.managed_blocks.len(), 1);

        // Installing again must update the same marked block rather than
        // duplicating it, and must be allowed because files are plugin-owned.
        install(
            &installer,
            &package,
            &release,
            &actions,
            &app,
            "example",
            &[],
        )
        .unwrap();
        let startup_text = fs::read_to_string(&startup).unwrap();
        assert_eq!(
            startup_text.matches(">>> NexusTools:example >>>").count(),
            1
        );
    }

    #[test]
    fn modified_owned_file_blocks_overwrite() {
        let _serial = TEST_SERIAL.lock().unwrap();
        TEST_FAIL_AFTER_FILES.store(usize::MAX, Ordering::SeqCst);
        let (temp, paths, store, app, _) = test_environment();
        let package_root = temp.path().join("package");
        let (package, release, actions) =
            write_package(&package_root, "example", "1.0.0", "original\n", None);
        let installer = Installer {
            paths: &paths,
            store: &store,
        };
        install(
            &installer,
            &package,
            &release,
            &actions,
            &app,
            "example",
            &[],
        )
        .unwrap();
        let file = app
            .paths
            .user_scripts
            .as_ref()
            .unwrap()
            .join("example/main.py");
        fs::write(&file, "locally edited\n").unwrap();
        let error = install(
            &installer,
            &package,
            &release,
            &actions,
            &app,
            "example",
            &[],
        )
        .unwrap_err();
        assert_eq!(error.code, "file_modified");
        assert_eq!(fs::read_to_string(&file).unwrap(), "locally edited\n");
    }

    #[test]
    fn concurrent_plugin_ownership_conflict_is_rejected() {
        let _serial = TEST_SERIAL.lock().unwrap();
        TEST_FAIL_AFTER_FILES.store(usize::MAX, Ordering::SeqCst);
        let (temp, paths, store, app, _) = test_environment();
        let package_root = temp.path().join("package");
        let (package, release, actions) =
            write_package(&package_root, "example", "1.0.0", "original\n", None);
        let installer = Installer {
            paths: &paths,
            store: &store,
        };
        install(
            &installer,
            &package,
            &release,
            &actions,
            &app,
            "example",
            &[],
        )
        .unwrap();

        let other_root = temp.path().join("other-package");
        fs::create_dir_all(other_root.join("files")).unwrap();
        fs::write(other_root.join("files/main.py"), "other\n").unwrap();
        let manifest_text = r#"{
          "schemaVersion":1,"id":"other","name":"Other","description":"d","overview":"o","version":"1.0.0",
          "sourceDirectory":"files",
          "targets":[{"app":"maya","os":"windows","supportedVersions":["2024"],"install":[
            {"action":"copy","source":"main.py","destination":"${paths.userScripts}/example/main.py"}
          ]}]
        }"#;
        let other_manifest = PluginManifest::from_bytes(manifest_text.as_bytes()).unwrap();
        let target = other_manifest
            .matching_target("maya", "2024")
            .unwrap()
            .clone();
        let other_actions = other_manifest.validated_actions(&target).unwrap();
        let other_package = ValidatedPackage {
            package_root: other_root.clone(),
            source_dir: other_root.join("files"),
            manifest: other_manifest,
            manifest_json: serde_json::from_str(manifest_text).unwrap(),
        };
        let other_release = ReleaseRecord {
            repository: "owner/other".to_string(),
            release_id: Some(2),
            release_tag: "v1.0.0".to_string(),
            release_version: "1.0.0".to_string(),
            asset_name: "other.zip".to_string(),
            asset_sha256: "b".repeat(64),
            digest_verified: true,
        };
        let error = install(
            &installer,
            &other_package,
            &other_release,
            &other_actions,
            &app,
            "other",
            &[],
        )
        .unwrap_err();
        assert_eq!(error.code, "file_owned_by_other");
        assert_eq!(
            fs::read_to_string(
                app.paths
                    .user_scripts
                    .as_ref()
                    .unwrap()
                    .join("example/main.py")
            )
            .unwrap(),
            "original\n"
        );
    }

    #[test]
    fn rollback_restores_previous_install_and_state() {
        let _serial = TEST_SERIAL.lock().unwrap();
        TEST_FAIL_AFTER_FILES.store(usize::MAX, Ordering::SeqCst);
        let (temp, paths, store, app, _) = test_environment();
        let package_root = temp.path().join("package-old");
        let (package_old, release_old, actions_old) =
            write_package(&package_root, "example", "1.0.0", "old content\n", None);
        let installer = Installer {
            paths: &paths,
            store: &store,
        };
        install(
            &installer,
            &package_old,
            &release_old,
            &actions_old,
            &app,
            "example",
            &[],
        )
        .unwrap();
        let file = app
            .paths
            .user_scripts
            .as_ref()
            .unwrap()
            .join("example/main.py");
        assert_eq!(fs::read_to_string(&file).unwrap(), "old content\n");

        let new_root = temp.path().join("package-new");
        let (package_new, release_new, actions_new) =
            write_package(&new_root, "example", "1.0.0", "new content\n", None);
        // Fail after the first new file is written. The transaction must restore
        // all files and the previous installed-state record.
        TEST_FAIL_AFTER_FILES.store(1, Ordering::SeqCst);
        let error = install(
            &installer,
            &package_new,
            &release_new,
            &actions_new,
            &app,
            "example",
            &[],
        )
        .unwrap_err();
        TEST_FAIL_AFTER_FILES.store(usize::MAX, Ordering::SeqCst);
        assert_eq!(error.code, "test_injected_failure");
        assert_eq!(fs::read_to_string(&file).unwrap(), "old content\n");
        let recorded = store.snapshot();
        let target = recorded.target("example", "maya-2024-test").unwrap();
        assert_eq!(target.plugin_version, "1.0.0");
        let remaining_journals = fs::read_dir(paths.transactions_dir()).unwrap().count();
        assert_eq!(remaining_journals, 0);
    }

    #[test]
    fn outside_destination_requires_explicit_exact_approval() {
        let _serial = TEST_SERIAL.lock().unwrap();
        TEST_FAIL_AFTER_FILES.store(usize::MAX, Ordering::SeqCst);
        let (temp, paths, store, app, _) = test_environment();
        let package_root = temp.path().join("package");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&outside_dir).unwrap();
        let outside_file = outside_dir.join("icons/note.txt");
        let extra = format!(
            r#",{{"action":"writeFile","destination":"{}","content":"outside"}}"#,
            outside_file.to_string_lossy().replace('\\', "/")
        );
        let (package, release, actions) =
            write_package(&package_root, "example", "1.0.0", "x\n", Some(&extra));
        let installer = Installer {
            paths: &paths,
            store: &store,
        };
        let error = install(
            &installer,
            &package,
            &release,
            &actions,
            &app,
            "example",
            &[],
        )
        .unwrap_err();
        assert_eq!(error.code, "outside_destination_roots");
        assert!(!outside_file.exists());
        let approval_root = outside_file.parent().unwrap();
        assert!(error
            .details
            .iter()
            .any(|detail| detail.ends_with("icons\\") || detail.ends_with("icons")));
        install(
            &installer,
            &package,
            &release,
            &actions,
            &app,
            "example",
            &[approval_root.to_string_lossy().to_string()],
        )
        .unwrap();
        assert_eq!(fs::read_to_string(outside_file).unwrap(), "outside");
    }

    #[test]
    fn uninstall_removes_owned_content_and_preserves_modified_files() {
        let _serial = TEST_SERIAL.lock().unwrap();
        TEST_FAIL_AFTER_FILES.store(usize::MAX, Ordering::SeqCst);
        let (temp, paths, store, app, _) = test_environment();
        let package_root = temp.path().join("package");
        let (package, release, actions) =
            write_package(&package_root, "example", "1.0.0", "content\n", None);
        let installer = Installer {
            paths: &paths,
            store: &store,
        };
        install(
            &installer,
            &package,
            &release,
            &actions,
            &app,
            "example",
            &[],
        )
        .unwrap();
        let generated = app
            .paths
            .user_scripts
            .as_ref()
            .unwrap()
            .join("example/generated.txt");
        fs::write(&generated, "user modification\n").unwrap();
        let result = uninstall_target(&paths, &store, "example", "maya-2024-test").unwrap();
        assert!(!result.fully_removed);
        assert!(!result.skipped.is_empty());
        assert!(generated.exists());
        assert_eq!(
            fs::read_to_string(generated).unwrap(),
            "user modification\n"
        );
        assert!(!app
            .paths
            .user_scripts
            .as_ref()
            .unwrap()
            .join("example/main.py")
            .exists());
        let startup = app.paths.user_prefs.as_ref().unwrap().join("startup.py");
        assert!(!fs::read_to_string(&startup)
            .unwrap()
            .contains("NexusTools:example"));
        let target = store.target("example", "maya-2024-test").unwrap();
        assert_eq!(target.status, "partial-uninstall");
        assert_eq!(store.plugin("example").unwrap().name, "Example");
    }

    #[test]
    fn interrupted_prepared_transaction_is_recovered_from_journal() {
        let _serial = TEST_SERIAL.lock().unwrap();
        let (temp, paths, store, _app, _) = test_environment();
        let destination = temp.path().join("user/scripts/example/file.txt");
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"partially applied").unwrap();
        let backup_dir = paths.backups_dir().join("example").join("target");
        fs::create_dir_all(&backup_dir).unwrap();
        let backup = backup_dir.join("0000-file.txt");
        fs::write(&backup, b"original").unwrap();
        let journal = TransactionJournal {
            schema_version: JOURNAL_SCHEMA_VERSION,
            operation_id: "interrupted123".to_string(),
            plugin_id: "example".to_string(),
            plugin_name: "Example".to_string(),
            target_id: "maya-2024-test".to_string(),
            phase: JournalPhase::Prepared,
            previous_target: None,
            backup_dir: backup_dir.to_string_lossy().to_string(),
            files: vec![JournalFileOperation {
                destination: destination.to_string_lossy().to_string(),
                backup: Some(backup.to_string_lossy().to_string()),
                new_sha256: None,
                existed: true,
            }],
            created_dirs: vec![destination.parent().unwrap().to_string_lossy().to_string()],
        };
        let journal_path = paths.transactions_dir().join("interrupted123.json");
        fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        let warnings = recover_transactions(&paths, &store);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(fs::read(&destination).unwrap(), b"original");
        assert!(!journal_path.exists());
    }

    #[test]
    fn patch_content_cannot_inject_managed_markers() {
        let error = compute_patch(
            "before\n",
            "demo",
            "#",
            "safe\n# >>> NexusTools:demo >>>\nunsafe",
            "\n",
            None,
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_patch_content");
    }

    #[test]
    fn multiple_targets_keep_independent_installation_records() {
        let _serial = TEST_SERIAL.lock().unwrap();
        TEST_FAIL_AFTER_FILES.store(usize::MAX, Ordering::SeqCst);
        let (temp, paths, store, app_a, _) = test_environment();
        let install_root_b = temp.path().join("fake-maya-b");
        let root_b = temp.path().join("user-b");
        let app_b = AppInstallation {
            target_id: "maya-2024-b".to_string(),
            paths: AppPaths {
                user_scripts: Some(root_b.join("scripts")),
                user_modules: Some(root_b.join("modules")),
                user_prefs: Some(root_b.join("prefs")),
                zplugins: None,
            },
            install_root: install_root_b,
            ..app_a.clone()
        };
        let package_root = temp.path().join("package");
        let (package, release, actions) =
            write_package(&package_root, "example", "1.0.0", "content\n", None);
        let installer = Installer {
            paths: &paths,
            store: &store,
        };
        install(
            &installer,
            &package,
            &release,
            &actions,
            &app_a,
            "example",
            &[],
        )
        .unwrap();
        install(
            &installer,
            &package,
            &release,
            &actions,
            &app_b,
            "example",
            &[],
        )
        .unwrap();
        assert_eq!(
            store.plugin("example").unwrap().targets.len(),
            2,
            "both app installations should be tracked separately"
        );
        uninstall_target(&paths, &store, "example", "maya-2024-test").unwrap();
        assert!(store.target("example", "maya-2024-test").is_none());
        assert!(store.target("example", "maya-2024-b").is_some());
        assert!(root_b.join("scripts/example/main.py").is_file());
        assert!(!app_a
            .paths
            .user_scripts
            .as_ref()
            .unwrap()
            .join("example/main.py")
            .exists());
    }

    #[test]
    fn failed_target_does_not_undo_a_successful_target() {
        let _serial = TEST_SERIAL.lock().unwrap();
        TEST_FAIL_AFTER_FILES.store(usize::MAX, Ordering::SeqCst);
        let (temp, paths, store, app_a, _) = test_environment();
        let package_root = temp.path().join("package");
        let (package, release, actions) =
            write_package(&package_root, "example", "1.0.0", "content\n", None);
        let installer = Installer {
            paths: &paths,
            store: &store,
        };
        install(
            &installer,
            &package,
            &release,
            &actions,
            &app_a,
            "example",
            &[],
        )
        .unwrap();
        let mut broken = app_a.clone();
        broken.target_id = "maya-2024-broken".to_string();
        broken.paths.user_prefs = None;
        let error = install(
            &installer,
            &package,
            &release,
            &actions,
            &broken,
            "example",
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            error.code.as_str(),
            "unresolved_path" | "io_error"
        ));
        assert!(store.target("example", "maya-2024-test").is_some());
        assert!(store.target("example", "maya-2024-broken").is_none());
        assert!(app_a
            .paths
            .user_scripts
            .as_ref()
            .unwrap()
            .join("example/main.py")
            .is_file());
    }

    #[test]
    fn update_removes_clean_obsolete_files_and_preserves_modified_ones() {
        let _serial = TEST_SERIAL.lock().unwrap();
        TEST_FAIL_AFTER_FILES.store(usize::MAX, Ordering::SeqCst);
        let (temp, paths, store, app, _) = test_environment();
        let package_v1 = temp.path().join("package-v1");
        let (package_old, release_old, actions_old) =
            write_package(&package_v1, "example", "1.0.0", "v1\n", None);
        fs::write(package_v1.join("files/clean_obsolete.py"), "clean\n").unwrap();
        fs::write(package_v1.join("files/modified_obsolete.py"), "modified\n").unwrap();
        let installer = Installer {
            paths: &paths,
            store: &store,
        };
        install(
            &installer,
            &package_old,
            &release_old,
            &actions_old,
            &app,
            "example",
            &[],
        )
        .unwrap();
        let user_scripts = app.paths.user_scripts.as_ref().unwrap();
        fs::write(
            user_scripts.join("example/modified_obsolete.py"),
            "local change\n",
        )
        .unwrap();

        let package_v2 = temp.path().join("package-v2");
        let (package_new, release_new, actions_new) =
            write_package(&package_v2, "example", "1.0.1", "v2\n", None);
        install(
            &installer,
            &package_new,
            &release_new,
            &actions_new,
            &app,
            "example",
            &[],
        )
        .unwrap();
        assert!(!user_scripts.join("example/clean_obsolete.py").exists());
        assert!(user_scripts.join("example/modified_obsolete.py").exists());
        assert_eq!(
            fs::read_to_string(user_scripts.join("example/modified_obsolete.py")).unwrap(),
            "local change\n"
        );
        let recorded = store.target("example", "maya-2024-test").unwrap();
        assert_eq!(recorded.plugin_version, "1.0.1");
        assert!(recorded.files.iter().any(|file| {
            file.path.ends_with("modified_obsolete.py") && file.preserved_modified && file.obsolete
        }));
    }

    #[test]
    fn patch_is_inserted_once_and_updated_not_duplicated() {
        let first = compute_patch(
            "print('start')\n",
            "demo",
            "#",
            "import demo\nsettings = True",
            "\n",
            None,
        )
        .unwrap();
        let text = String::from_utf8(first.bytes.clone()).unwrap();
        assert_eq!(text.matches(">>> NexusTools:demo >>>").count(), 1);
        let second = compute_patch(
            &text,
            "demo",
            "#",
            "import demo\nsettings = False",
            "\n",
            Some(&first.block_sha256),
        )
        .unwrap();
        let text2 = String::from_utf8(second.bytes).unwrap();
        assert_eq!(text2.matches(">>> NexusTools:demo >>>").count(), 1);
        assert!(text2.contains("settings = False"));
        assert!(text2.contains("print('start')"));
    }

    #[test]
    fn patch_refuses_local_modification_and_preserves_crlf() {
        let first = compute_patch(
            "line1\r\nline2\r\n",
            "demo",
            "#",
            "print('x')\nprint('y')",
            "\r\n",
            None,
        )
        .unwrap();
        let text = String::from_utf8(first.bytes).unwrap();
        assert!(text.contains("\r\n"));
        let modified = text.replace("print('x')", "print('tampered')");
        let error = compute_patch(
            &modified,
            "demo",
            "#",
            "print('new')",
            "\r\n",
            Some(&first.block_sha256),
        )
        .unwrap_err();
        assert_eq!(error.code, "file_modified");
    }

    #[test]
    fn patch_removal_preserves_surrounding_content() {
        let computed = compute_patch("before\n", "demo", "//", "plugin code", "\n", None).unwrap();
        let text = String::from_utf8(computed.bytes).unwrap();
        match remove_patch_block(&text, "demo", "//").unwrap() {
            PatchRemoval::Removed {
                content,
                block_sha256,
            } => {
                assert_eq!(block_sha256, computed.block_sha256);
                assert!(content.contains("before"));
                assert!(!content.contains("NexusTools:demo"));
            }
            _ => panic!("expected removal"),
        }
    }

    #[test]
    fn comment_syntax_matches_file_types() {
        assert_eq!(comment_prefix(Path::new("startup.py")).unwrap(), "#");
        assert_eq!(comment_prefix(Path::new("shelf.mel")).unwrap(), "//");
        assert_eq!(
            comment_prefix(Path::new("data.json")).unwrap_err().code,
            "unsupported_patch_file"
        );
    }
}
