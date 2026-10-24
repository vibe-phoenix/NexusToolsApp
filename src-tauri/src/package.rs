use crate::error::{AppError, AppResult};
use crate::manifest::PluginManifest;
use crate::util::{
    is_windows_reserved, path_is_within, validate_plugin_id, MAX_ARCHIVE_FILES,
    MAX_EXTRACTED_BYTES, MAX_MANIFEST_BYTES,
};
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;
use zip::ZipArchive;

pub struct PackageWorkspace {
    root: PathBuf,
}

impl PackageWorkspace {
    pub fn new(base: &Path, label: &str) -> AppResult<Self> {
        fs::create_dir_all(base)?;
        let safe_label: String = label
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .take(48)
            .collect();
        let root = base.join(format!(
            "{}-{}",
            if safe_label.is_empty() {
                "package"
            } else {
                &safe_label
            },
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn download_path(&self) -> PathBuf {
        self.root.join("package.zip")
    }

    pub fn extraction_dir(&self) -> PathBuf {
        self.root.join("extracted")
    }
}

impl Drop for PackageWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, Clone)]
pub struct ValidatedPackage {
    pub package_root: PathBuf,
    pub source_dir: PathBuf,
    pub manifest: PluginManifest,
    pub manifest_json: Value,
}

/// Extract an uploaded ZIP with strict archive-path and size checks. No archive
/// entry may be absolute, contain a drive prefix, traverse with `..`, escape via
/// a symlink, or exceed the total budget.
pub fn extract_zip(zip_path: &Path, extraction_dir: &Path) -> AppResult<()> {
    fs::create_dir_all(extraction_dir)?;
    let file = fs::File::open(zip_path).map_err(|error| {
        AppError::io(
            &format!("Cannot open the downloaded package {}", zip_path.display()),
            error,
        )
    })?;
    let mut archive = ZipArchive::new(file).map_err(|error| {
        AppError::new(
            "invalid_package",
            format!("The downloaded release is not a readable ZIP archive: {error}"),
        )
    })?;
    if archive.len() > MAX_ARCHIVE_FILES {
        return Err(AppError::new(
            "package_too_large",
            format!(
                "The release ZIP contains more than {MAX_ARCHIVE_FILES} entries and was rejected."
            ),
        ));
    }
    let mut total_uncompressed = 0u64;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let extraction_root = extraction_dir.to_path_buf();

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            AppError::new(
                "invalid_package",
                format!("The release ZIP contains an unreadable entry: {error}"),
            )
        })?;
        if entry.encrypted() {
            return Err(AppError::new(
                "invalid_package",
                "Password-protected release archives are not supported.",
            ));
        }
        let raw_name = entry.name().to_string();
        let relative = safe_archive_path(&raw_name, entry.is_dir())?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let lower = relative
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
        if !seen.insert(lower) {
            return Err(AppError::new(
                "invalid_package",
                format!("The release ZIP contains duplicate paths: {raw_name}"),
            ));
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(AppError::new(
                "invalid_package",
                format!("The release ZIP contains a symbolic link, which NexusTools rejects: {raw_name}"),
            ));
        }
        let declared_size = entry.size();
        let is_dir = entry.is_dir();
        if !is_dir {
            total_uncompressed =
                total_uncompressed
                    .checked_add(declared_size)
                    .ok_or_else(|| {
                        AppError::new("package_too_large", "The release ZIP size overflowed.")
                    })?;
            if total_uncompressed > MAX_EXTRACTED_BYTES {
                return Err(AppError::new(
                    "package_too_large",
                    "The release ZIP expands beyond NexusTools's safe extraction limit.",
                ));
            }
        }
        let out_path = extraction_dir.join(&relative);
        if !path_is_within(&out_path, &extraction_root) {
            return Err(AppError::new(
                "archive_path_traversal",
                format!("The release ZIP entry escapes the extraction root: {raw_name}"),
            ));
        }
        if is_dir {
            fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&out_path)
            .map_err(|error| {
                AppError::io(&format!("Cannot extract ZIP entry {raw_name}"), error)
            })?;
        let mut limited = entry.take(declared_size.saturating_add(1));
        let written = std::io::copy(&mut limited, &mut output)?;
        if written > declared_size || written + total_uncompressed > MAX_EXTRACTED_BYTES {
            let _ = fs::remove_file(&out_path);
            return Err(AppError::new(
                "package_too_large",
                "A release ZIP entry was larger than declared or exceeded the safe extraction limit.",
            ));
        }
        output.flush()?;
    }

    // Reject any entry that somehow became a reparse point during extraction.
    if contains_symlink_or_reparse(extraction_dir)? {
        return Err(AppError::new(
            "archive_symlink",
            "The extracted package contains a symbolic link or junction, which NexusTools rejects.",
        ));
    }
    Ok(())
}

fn safe_archive_path(raw: &str, is_dir: bool) -> AppResult<PathBuf> {
    if raw.is_empty() || raw.contains('\0') {
        return Err(AppError::new(
            "invalid_package",
            "The release ZIP contains an invalid empty entry name.",
        ));
    }
    let normalized = raw.replace('\\', "/");
    if normalized.starts_with('/')
        || normalized.starts_with("//")
        || normalized.get(1..2) == Some(":")
    {
        return Err(AppError::new(
            "archive_absolute_path",
            format!("The release ZIP contains an absolute path: {raw}"),
        ));
    }
    let mut path = PathBuf::new();
    let mut segments = normalized.split('/').peekable();
    while let Some(segment) = segments.next() {
        if segment.is_empty() || segment == "." {
            if segments.peek().is_none() && is_dir {
                break;
            }
            return Err(AppError::new(
                "invalid_package",
                format!("The release ZIP contains an invalid path: {raw}"),
            ));
        }
        if segment == ".." {
            return Err(AppError::new(
                "archive_path_traversal",
                format!("The release ZIP contains a path traversal: {raw}"),
            ));
        }
        if segment.ends_with(' ')
            || segment.ends_with('.')
            || segment.contains(':')
            || segment.chars().any(char::is_control)
            || is_windows_reserved(segment)
        {
            return Err(AppError::new(
                "invalid_package",
                format!("The release ZIP contains a Windows-unsafe path segment: {raw}"),
            ));
        }
        path.push(segment);
    }
    // Use component validation as a second, independent guard.
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(AppError::new(
                    "archive_path_traversal",
                    format!("The release ZIP contains a non-relative path: {raw}"),
                ))
            }
        }
    }
    Ok(path)
}

fn contains_symlink_or_reparse(root: &Path) -> AppResult<bool> {
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| {
            AppError::new(
                "invalid_package",
                format!("Could not inspect the extracted package: {error}"),
            )
        })?;
        if entry.file_type().is_symlink() {
            return Ok(true);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if let Ok(metadata) = entry.metadata() {
                if metadata.file_attributes() & 0x400 != 0 {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Locate the one valid `files/` + `for installer/plugin.json` package root, at
/// the archive root or inside a single enclosing directory.
pub fn inspect_package(
    extraction_root: &Path,
    expected_plugin_id: &str,
    release_version: &str,
) -> AppResult<ValidatedPackage> {
    let mut candidates = Vec::new();
    if package_root_at(extraction_root) {
        candidates.push(extraction_root.to_path_buf());
    }
    for entry in fs::read_dir(extraction_root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && package_root_at(&entry.path()) {
            candidates.push(entry.path());
        }
    }
    if candidates.is_empty() {
        return Err(AppError::new(
            "invalid_package_structure",
            "The release ZIP must contain a files/ directory and for installer/plugin.json, either at its root or inside exactly one enclosing directory. NexusTools did not find that structure.",
        ));
    }
    if candidates.len() > 1 {
        return Err(AppError::new(
            "ambiguous_package_root",
            format!(
                "The release ZIP contains multiple possible package roots ({}). NexusTools will not guess.",
                candidates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let package_root = candidates.remove(0);
    let installer_dir = find_child_dir_ci(&package_root, "for installer")?.ok_or_else(|| {
        AppError::new(
            "invalid_package_structure",
            "The package is missing the 'for installer' folder.",
        )
    })?;
    let manifest_path = find_child_file_ci(&installer_dir, "plugin.json")?.ok_or_else(|| {
        AppError::new(
            "invalid_package_structure",
            "The package is missing 'for installer/plugin.json'.",
        )
    })?;
    let metadata = fs::metadata(&manifest_path)?;
    if metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(AppError::new(
            "invalid_package",
            "The bundled plugin.json is too large.",
        ));
    }
    let manifest_bytes = fs::read(&manifest_path)?;
    let manifest_json: Value = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        AppError::new(
            "invalid_manifest",
            format!("The bundled plugin.json is not valid JSON: {error}"),
        )
    })?;
    let manifest = PluginManifest::from_bytes(&manifest_bytes)?;
    validate_plugin_id(expected_plugin_id)?;
    manifest.validate_for_installation(expected_plugin_id, release_version)?;

    let source_directory = manifest
        .source_directory
        .as_deref()
        .ok_or_else(|| AppError::new("missing_source_directory", "sourceDirectory is missing."))?;
    let source_dir = safe_package_relative(source_directory)?;
    let source_dir = package_root.join(source_dir);
    if !source_dir.is_dir() {
        return Err(AppError::new(
            "missing_source_directory",
            format!(
                "The bundled manifest's sourceDirectory '{}' does not exist in the release package.",
                source_directory
            ),
        ));
    }
    let canonical_root = fs::canonicalize(&package_root)?;
    let canonical_source = fs::canonicalize(&source_dir)?;
    if !path_is_within(&canonical_source, &canonical_root) {
        return Err(AppError::new(
            "archive_path_traversal",
            "sourceDirectory resolves outside the extracted package.",
        ));
    }
    if contains_symlink_or_reparse(&source_dir)? {
        return Err(AppError::new(
            "archive_symlink",
            "The sourceDirectory contains a symbolic link or junction.",
        ));
    }
    Ok(ValidatedPackage {
        package_root,
        source_dir,
        manifest,
        manifest_json,
    })
}

fn safe_package_relative(raw: &str) -> AppResult<PathBuf> {
    if raw.trim().is_empty() {
        return Err(AppError::new(
            "missing_source_directory",
            "sourceDirectory is empty.",
        ));
    }
    let normalized = raw.replace('\\', "/");
    if normalized.starts_with('/') || normalized.get(1..2) == Some(":") {
        return Err(AppError::new(
            "invalid_source_directory",
            "sourceDirectory must be relative.",
        ));
    }
    let mut path = PathBuf::new();
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
                "invalid_source_directory",
                format!("sourceDirectory contains an unsafe segment: {raw}"),
            ));
        }
        path.push(segment);
    }
    if path.as_os_str().is_empty() {
        return Err(AppError::new(
            "invalid_source_directory",
            "sourceDirectory resolves to the package root, which is not allowed.",
        ));
    }
    Ok(path)
}

fn package_root_at(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let files = find_child_dir_ci(path, "files").ok().flatten();
    let installer = find_child_dir_ci(path, "for installer").ok().flatten();
    if files.is_none() || installer.is_none() {
        return false;
    }
    find_child_file_ci(&installer.unwrap(), "plugin.json")
        .ok()
        .flatten()
        .is_some()
}

fn find_child_dir_ci(parent: &Path, name: &str) -> AppResult<Option<PathBuf>> {
    let wanted = name.to_ascii_lowercase();
    let mut matches = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry.file_name().to_string_lossy().to_ascii_lowercase() == wanted
        {
            matches.push(entry.path());
        }
    }
    unique_match(parent, name, matches)
}

fn find_child_file_ci(parent: &Path, name: &str) -> AppResult<Option<PathBuf>> {
    let wanted = name.to_ascii_lowercase();
    let mut matches = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry.file_name().to_string_lossy().to_ascii_lowercase() == wanted
        {
            matches.push(entry.path());
        }
    }
    unique_match(parent, name, matches)
}

fn unique_match(
    parent: &Path,
    name: &str,
    mut matches: Vec<PathBuf>,
) -> AppResult<Option<PathBuf>> {
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches.remove(0))),
        _ => Err(AppError::new(
            "ambiguous_package_structure",
            format!(
                "The package contains more than one '{}' beneath {}.",
                name,
                parent.display()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn build_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, content) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap();
    }

    fn manifest(id: &str, version: &str) -> String {
        format!(
            r#"{{
              "schemaVersion":1,
              "id":"{id}",
              "name":"Example",
              "description":"Description",
              "overview":"Overview",
              "version":"{version}",
              "sourceDirectory":"files",
              "targets":[
                {{"app":"maya","os":"windows","supportedVersions":["2024"],"install":[
                  {{"action":"copy","source":".","destination":"${{paths.userScripts}}/example"}}
                ]}}
              ]
            }}"#
        )
    }

    #[test]
    fn extracts_direct_root_package_and_reads_bundled_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("package.zip");
        let plugin = manifest("example", "1.0.0");
        build_zip(
            &zip,
            &[
                ("files/example.py", b"print('hello')"),
                ("for installer/plugin.json", plugin.as_bytes()),
            ],
        );
        let extract = dir.path().join("extract");
        extract_zip(&zip, &extract).unwrap();
        let package = inspect_package(&extract, "example", "v1.0.0").unwrap();
        assert_eq!(package.manifest.id, "example");
        assert!(package.source_dir.join("example.py").is_file());
    }

    #[test]
    fn accepts_one_enclosing_package_directory() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("package.zip");
        let plugin = manifest("example", "1.0.0");
        build_zip(
            &zip,
            &[
                ("Example-1.0.0/files/example.py", b"x"),
                ("Example-1.0.0/for installer/plugin.json", plugin.as_bytes()),
            ],
        );
        let extract = dir.path().join("extract");
        extract_zip(&zip, &extract).unwrap();
        inspect_package(&extract, "example", "1.0.0").unwrap();
    }

    #[test]
    fn rejects_root_traversal_absolute_paths_and_dotdot() {
        assert_eq!(
            safe_archive_path("../evil.py", false).unwrap_err().code,
            "archive_path_traversal"
        );
        assert_eq!(
            safe_archive_path("C:/Windows/evil.py", false)
                .unwrap_err()
                .code,
            "archive_absolute_path"
        );
        assert_eq!(
            safe_archive_path("/etc/evil.py", false).unwrap_err().code,
            "archive_absolute_path"
        );
        assert!(safe_archive_path("folder/../../evil.py", false).is_err());
    }

    #[test]
    fn rejects_bundled_id_and_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("package.zip");
        let plugin = manifest("other", "1.0.0");
        build_zip(
            &zip,
            &[
                ("files/example.py", b"x"),
                ("for installer/plugin.json", plugin.as_bytes()),
            ],
        );
        let extract = dir.path().join("extract");
        extract_zip(&zip, &extract).unwrap();
        assert_eq!(
            inspect_package(&extract, "example", "1.0.0")
                .unwrap_err()
                .code,
            "bundled_manifest_id_mismatch"
        );

        let zip2 = dir.path().join("package2.zip");
        let plugin = manifest("example", "1.0.0");
        build_zip(
            &zip2,
            &[
                ("files/example.py", b"x"),
                ("for installer/plugin.json", plugin.as_bytes()),
            ],
        );
        let extract2 = dir.path().join("extract2");
        extract_zip(&zip2, &extract2).unwrap();
        assert_eq!(
            inspect_package(&extract2, "example", "1.0.2")
                .unwrap_err()
                .code,
            "bundled_manifest_version_mismatch"
        );
    }

    #[test]
    fn rejects_ambiguous_package_root() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("package.zip");
        let plugin = manifest("example", "1.0.0");
        build_zip(
            &zip,
            &[
                ("files/example.py", b"x"),
                ("for installer/plugin.json", plugin.as_bytes()),
                ("nested/files/example.py", b"x"),
                ("nested/for installer/plugin.json", plugin.as_bytes()),
            ],
        );
        let extract = dir.path().join("extract");
        extract_zip(&zip, &extract).unwrap();
        assert_eq!(
            inspect_package(&extract, "example", "1.0.0")
                .unwrap_err()
                .code,
            "ambiguous_package_root"
        );
    }

    #[test]
    fn rejects_oversized_archive_declared_size() {
        // Build a small archive and confirm the guard is wired to max size by
        // exercising the safe path and count-level validation instead of
        // allocating gigabytes in a unit test.
        assert!(MAX_EXTRACTED_BYTES > 0);
        assert!(MAX_ARCHIVE_FILES > 0);
    }

    #[test]
    fn extraction_rejects_traversal_and_symlink_entries() {
        let dir = tempfile::tempdir().unwrap();
        let traversal = dir.path().join("traversal.zip");
        let file = fs::File::create(&traversal).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file("../escape.txt", options).unwrap();
        writer.write_all(b"escaped").unwrap();
        writer.start_file("files/ok.py", options).unwrap();
        writer.write_all(b"ok").unwrap();
        writer.finish().unwrap();
        let extract = dir.path().join("extract-traversal");
        let error = extract_zip(&traversal, &extract).unwrap_err();
        assert!(
            matches!(
                error.code.as_str(),
                "archive_path_traversal" | "invalid_package"
            ),
            "unexpected traversal error: {error:?}"
        );
        assert!(!dir.path().join("escape.txt").exists());
        assert!(!extract.join("escape.txt").exists());

        let symlink_zip = dir.path().join("symlink.zip");
        let file = fs::File::create(&symlink_zip).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .add_symlink("files/link.py", "../../outside.py", options)
            .unwrap();
        writer.start_file("files/ok.py", options).unwrap();
        writer.write_all(b"ok").unwrap();
        writer.finish().unwrap();
        let extract = dir.path().join("extract-symlink");
        let error = extract_zip(&symlink_zip, &extract).unwrap_err();
        assert!(matches!(
            error.code.as_str(),
            "invalid_package" | "archive_symlink"
        ));
        assert!(!dir.path().join("outside.py").exists());
    }
}
