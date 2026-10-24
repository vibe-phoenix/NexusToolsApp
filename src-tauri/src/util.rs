use crate::error::{AppError, AppResult};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

/// Characters escaped inside one repository path segment. `/` is never passed
/// to this set: callers split on separators so separators survive encoding.
const URL_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'^')
    .add(b'|')
    .add(b'[')
    .add(b']')
    .add(b'/')
    .add(b'\\');

pub const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_MARKDOWN_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_BANNER_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_RELEASE_JSON_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PACKAGE_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_EXTRACTED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_ARCHIVE_FILES: usize = 10_000;

/// Validate a repository-relative path in a catalog or manifest. The returned
/// segments preserve spelling and capitalization.
pub fn repo_path_segments(raw: &str) -> AppResult<Vec<String>> {
    if raw.is_empty() {
        return Err(AppError::invalid("A repository path is empty."));
    }
    if raw.len() > 1024 {
        return Err(AppError::invalid("A repository path is unreasonably long."));
    }
    if raw.contains('\\') {
        return Err(AppError::new(
            "invalid_path",
            format!("Repository paths must use forward slashes: {raw}"),
        ));
    }
    if raw.starts_with('/') || raw.get(1..2) == Some(":") {
        return Err(AppError::new(
            "invalid_path",
            format!("Absolute repository paths are not allowed: {raw}"),
        ));
    }
    let mut segments = Vec::new();
    for segment in raw.split('/') {
        if segment.is_empty() {
            return Err(AppError::new(
                "invalid_path",
                format!("Repository paths cannot contain empty segments: {raw}"),
            ));
        }
        if segment == "." || segment == ".." {
            return Err(AppError::new(
                "invalid_path",
                format!("Repository path traversal is not allowed: {raw}"),
            ));
        }
        if segment
            .chars()
            .any(|c| c.is_control() || c == ':' || c == '\0')
        {
            return Err(AppError::new(
                "invalid_path",
                format!("Repository path contains an invalid character: {raw}"),
            ));
        }
        if segment.ends_with(' ') || segment.ends_with('.') {
            return Err(AppError::new(
                "invalid_path",
                format!("Repository path segment has a trailing space or dot: {raw}"),
            ));
        }
        if is_windows_reserved(segment) {
            return Err(AppError::new(
                "invalid_path",
                format!("Repository path uses a reserved Windows name: {raw}"),
            ));
        }
        segments.push(segment.to_string());
    }
    Ok(segments)
}

pub fn is_windows_reserved(segment: &str) -> bool {
    let stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

pub fn encode_repo_path(raw: &str) -> AppResult<String> {
    let segments = repo_path_segments(raw)?;
    Ok(segments
        .iter()
        .map(|segment| utf8_percent_encode(segment, URL_SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/"))
}

/// Join a relative manifest asset path to its containing repository directory.
/// `..` is accepted here because asset paths are declared relative to the
/// manifest directory, but it may not escape the repository root.
pub fn resolve_manifest_relative(
    manifest_path: &str,
    relative: &str,
) -> AppResult<(String, String)> {
    let manifest_segments = repo_path_segments(manifest_path)?;
    let mut base: Vec<String> = manifest_segments[..manifest_segments.len() - 1].to_vec();
    if relative.is_empty() {
        return Err(AppError::new(
            "missing_asset_path",
            "A manifest asset path is empty.",
        ));
    }
    if relative.starts_with('/') || relative.starts_with('\\') || relative.get(1..2) == Some(":") {
        return Err(AppError::new(
            "invalid_asset_path",
            format!("Asset paths must be relative to the manifest: {relative}"),
        ));
    }
    let normalized = relative.replace('\\', "/");
    for segment in normalized.split('/') {
        match segment {
            "" | "." => {
                return Err(AppError::new(
                    "invalid_asset_path",
                    format!("Asset path contains an empty or dot segment: {relative}"),
                ))
            }
            ".." => {
                if base.pop().is_none() {
                    return Err(AppError::new(
                        "invalid_asset_path",
                        format!("Asset path escapes the repository: {relative}"),
                    ));
                }
            }
            seg => {
                if seg.ends_with(' ') || seg.ends_with('.') || is_windows_reserved(seg) {
                    return Err(AppError::new(
                        "invalid_asset_path",
                        format!("Asset path contains an invalid Windows path segment: {relative}"),
                    ));
                }
                base.push(seg.to_string());
            }
        }
    }
    let full = base.join("/");
    let manifest_dir = manifest_segments[..manifest_segments.len() - 1].join("/");
    Ok((full, manifest_dir))
}

pub fn raw_github_url(repository: &str, commit: &str, repo_path: &str) -> AppResult<String> {
    let (owner, name) = split_repository(repository)?;
    if !is_hex_commit(commit) {
        return Err(AppError::new(
            "invalid_commit",
            format!("The resolved repository commit is invalid: {commit}"),
        ));
    }
    let encoded = encode_repo_path(repo_path)?;
    Ok(format!(
        "https://raw.githubusercontent.com/{}/{}/{}/{}",
        url_encode_component(owner),
        url_encode_component(name),
        commit,
        encoded
    ))
}

pub fn api_repository_url(repository: &str) -> AppResult<String> {
    let (owner, name) = split_repository(repository)?;
    Ok(format!(
        "https://api.github.com/repos/{}/{}",
        url_encode_component(owner),
        url_encode_component(name)
    ))
}

pub fn split_repository(repository: &str) -> AppResult<(&str, &str)> {
    let mut parts = repository.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return Err(AppError::new(
            "invalid_repository",
            format!("Repository must be in owner/name form: {repository}"),
        ));
    }
    let valid = |s: &str| {
        !s.starts_with('.')
            && !s.ends_with('.')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !valid(owner) || !valid(name) {
        return Err(AppError::new(
            "invalid_repository",
            format!("Repository contains unsupported characters: {repository}"),
        ));
    }
    Ok((owner, name))
}

pub fn url_encode_component(value: &str) -> String {
    utf8_percent_encode(value, URL_SEGMENT).to_string()
}

pub fn is_hex_commit(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn sha256_file(path: &Path) -> AppResult<String> {
    let mut file = fs::File::open(path)
        .map_err(|e| AppError::io(&format!("Cannot open {}", path.display()), e))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Lexically normalize an absolute destination. Relative paths are rejected.
pub fn normalize_absolute_path(path: &str) -> AppResult<PathBuf> {
    let parsed = Path::new(path);
    if !parsed.is_absolute() {
        return Err(AppError::new(
            "unresolved_path",
            format!("An installation destination is not absolute: {path}"),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in parsed.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(AppError::new(
                        "unresolved_path",
                        format!("A destination escapes its filesystem root: {path}"),
                    ));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

pub fn path_is_within(path: &Path, root: &Path) -> bool {
    let mut path_components: Vec<String> = Vec::new();
    let mut root_components: Vec<String> = Vec::new();
    collect_normal_components(path, &mut path_components);
    collect_normal_components(root, &mut root_components);
    if root_components.is_empty() || path_components.len() < root_components.len() {
        return false;
    }
    for (a, b) in path_components.iter().zip(root_components.iter()) {
        if !path_component_eq(a, b) {
            return false;
        }
    }
    true
}

fn collect_normal_components(path: &Path, output: &mut Vec<String>) {
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                output.push(prefix.as_os_str().to_string_lossy().to_ascii_lowercase())
            }
            Component::RootDir => output.push(String::from("/")),
            Component::Normal(part) => output.push(part.to_string_lossy().to_ascii_lowercase()),
            Component::CurDir | Component::ParentDir => {}
        }
    }
}

fn path_component_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a == b
    } else {
        a == b
    }
}

pub fn paths_equal(a: &Path, b: &Path) -> bool {
    path_is_within(a, b) && path_is_within(b, a)
}

/// Reject destinations that resolve through an existing symlink or junction
/// below an approved root. This complements lexical containment checks.
pub fn contains_reparse_point(root: &Path, path: &Path) -> Option<PathBuf> {
    if !path_is_within(path, root) {
        return Some(path.to_path_buf());
    }
    let mut current = path.to_path_buf();
    while !paths_equal(&current, root) {
        if let Ok(metadata) = fs::symlink_metadata(&current) {
            if is_reparse_or_symlink(&metadata) {
                return Some(current);
            }
        }
        if !current.pop() {
            break;
        }
    }
    None
}

#[cfg(windows)]
fn is_reparse_or_symlink(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || (metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}

#[cfg(not(windows))]
fn is_reparse_or_symlink(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Atomically replace a file in the same directory. On Windows MoveFileExW is
/// used so a crash cannot leave a half-written JSON/cache pointer.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let parent = path.parent().ok_or_else(|| {
        AppError::new(
            "io_error",
            format!("Cannot write a file without a parent: {}", path.display()),
        )
    })?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("nexustools");
    let temp = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    let result = replace_file(&temp, path);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

pub fn atomic_write_json<T: serde::Serialize>(path: &Path, value: &T) -> AppResult<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| AppError::new("serialization", e.to_string()))?;
    atomic_write(path, &bytes)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> AppResult<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let success = unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if success == 0 {
        Err(AppError::io(
            &format!("Cannot atomically replace {}", to.display()),
            std::io::Error::last_os_error(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> AppResult<()> {
    fs::rename(from, to)
        .map_err(|e| AppError::io(&format!("Cannot atomically replace {}", to.display()), e))
}

pub fn read_limited(path: &Path, limit: usize) -> AppResult<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > limit as u64 {
        return Err(AppError::new(
            "file_too_large",
            format!("{} exceeds the {limit} byte limit.", path.display()),
        ));
    }
    Ok(fs::read(path)?)
}

/// Copy a file, creating parent directories. Used for cache reuse and staging.
pub fn copy_file(source: &Path, destination: &Path) -> AppResult<u64> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = fs::read(source)?;
    fs::write(destination, &bytes)?;
    Ok(bytes.len() as u64)
}

pub fn hardlink_or_copy(source: &Path, destination: &Path) -> AppResult<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::hard_link(source, destination).is_err() {
        fs::copy(source, destination)?;
    }
    Ok(())
}

pub fn sanitize_filename_component(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').trim_start_matches('.');
    if trimmed.is_empty() {
        "backup".to_string()
    } else {
        trimmed.chars().take(80).collect()
    }
}

pub fn validate_plugin_id(id: &str) -> AppResult<()> {
    let valid = !id.is_empty()
        && id.len() <= 100
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid || id.starts_with('.') || id.ends_with('.') {
        return Err(AppError::new(
            "invalid_plugin_id",
            format!("Plugin id is invalid: {id}"),
        ));
    }
    Ok(())
}

pub fn banner_content_type(path: &str, bytes: &[u8]) -> AppResult<&'static str> {
    let extension = Path::new(path)
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok("image/png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Ok("image/jpeg");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Ok("image/webp");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Ok("image/gif");
    }
    Err(AppError::new(
        "invalid_banner",
        format!("Banner is not a supported PNG, JPEG, WebP, or GIF image ({extension})."),
    ))
}

pub fn markdown_text(bytes: &[u8], name: &str) -> AppResult<String> {
    if bytes.len() > MAX_MARKDOWN_BYTES {
        return Err(AppError::new(
            "file_too_large",
            format!("{name} is larger than the permitted download size."),
        ));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        AppError::new(
            "invalid_markdown",
            format!("{name} is not valid UTF-8 text."),
        )
    })
}

pub fn encode_data_url(content_type: &str, bytes: &[u8]) -> String {
    use base64::Engine;
    format!(
        "data:{content_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// Expand the `%NAME%` syntax used by Windows settings without invoking a
/// shell. Unknown variables are left intact so validation reports them.
pub fn expand_windows_env(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '%' {
            if let Some(relative_end) = chars[index + 1..].iter().position(|c| *c == '%') {
                let end = index + 1 + relative_end;
                let name: String = chars[index + 1..end].iter().collect();
                if !name.is_empty() {
                    if let Ok(value) = std::env::var(&name) {
                        output.push_str(&value);
                        index = end + 1;
                        continue;
                    }
                }
            }
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_only_problematic_url_characters() {
        assert_eq!(
            encode_repo_path("for installer/plugin.json").unwrap(),
            "for%20installer/plugin.json"
        );
        assert_eq!(
            encode_repo_path("for installer/Mod Tools Banner (1).PNG").unwrap(),
            "for%20installer/Mod%20Tools%20Banner%20(1).PNG"
        );
        assert_eq!(
            encode_repo_path("dir/space file#1%20.png").unwrap(),
            "dir/space%20file%231%2520.png"
        );
    }

    #[test]
    fn path_capitalization_and_separators_are_preserved() {
        let url = raw_github_url(
            "vibe-phoenix/ModTools",
            "0123456789abcdef0123456789abcdef01234567",
            "for installer/plugin.json",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://raw.githubusercontent.com/vibe-phoenix/ModTools/0123456789abcdef0123456789abcdef01234567/for%20installer/plugin.json"
        );
    }

    #[test]
    fn rejects_repository_traversal_and_absolute_paths() {
        assert!(repo_path_segments("../plugin.json").is_err());
        assert!(repo_path_segments("folder/../../plugin.json").is_err());
        assert!(repo_path_segments("/etc/plugin.json").is_err());
        assert!(repo_path_segments("C:/plugin.json").is_err());
        assert!(repo_path_segments("folder//plugin.json").is_err());
        assert!(repo_path_segments("folder\\plugin.json").is_err());
        assert!(repo_path_segments("folder/CON.json").is_err());
    }

    #[test]
    fn manifest_relative_assets_may_go_up_but_not_out_of_repo() {
        let (joined, dir) =
            resolve_manifest_relative("for installer/plugin.json", "../assets/banner.png").unwrap();
        assert_eq!(joined, "assets/banner.png");
        assert_eq!(dir, "for installer");
        assert!(resolve_manifest_relative("plugin.json", "../banner.png").is_err());
    }

    #[test]
    fn path_containment_is_component_aware() {
        let root = Path::new(r"C:\Users\Test\Documents\maya");
        assert!(path_is_within(
            Path::new(r"C:\Users\Test\Documents\maya\2025\scripts\x.py"),
            root
        ));
        assert!(!path_is_within(
            Path::new(r"C:\Users\Test\Documents\maya-other\x.py"),
            root
        ));
        #[cfg(windows)]
        assert!(path_is_within(
            Path::new(r"c:\users\test\documents\MAYA\2025\x.py"),
            root
        ));
    }

    #[test]
    fn atomic_write_replaces_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("one.json");
        atomic_write(&file, b"one").unwrap();
        atomic_write(&file, b"two").unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "two");
    }

    #[test]
    fn expands_windows_environment_syntax() {
        std::env::set_var("NEXUSTOOLS_TEST_VAR", "hello world");
        assert_eq!(
            expand_windows_env("%NEXUSTOOLS_TEST_VAR%\\x"),
            "hello world\\x"
        );
        assert_eq!(expand_windows_env("%NOT_SET_HERE%\\x"), "%NOT_SET_HERE%\\x");
    }
}
