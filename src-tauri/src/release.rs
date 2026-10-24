use crate::error::{AppError, AppResult};
use crate::version::Version;
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubAsset {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub digest: Option<String>,
    pub browser_download_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubRelease {
    pub id: i64,
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleasePackage {
    pub name: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub checksum_source: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseView {
    pub id: i64,
    pub tag: String,
    pub name: String,
    pub version: Option<String>,
    pub prerelease: bool,
    pub published_at: Option<String>,
    pub notes: String,
    pub package: Option<ReleasePackage>,
    pub matches_manifest_version: bool,
    pub install_error: Option<String>,
    pub recommended: bool,
}

#[derive(Debug, Clone)]
pub struct VersionedRelease {
    pub release: GitHubRelease,
    pub version: Version,
}

/// The version is read from the tag first. Release display names such as
/// "SculptPie 1.0.2" are a fallback when a tag is not itself a version.
pub fn release_version(release: &GitHubRelease) -> Option<Version> {
    Version::from_label(&release.tag_name)
        .or_else(|| release.name.as_deref().and_then(Version::from_label))
}

pub fn display_release_name(release: &GitHubRelease) -> String {
    release
        .name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| release.tag_name.clone())
}

/// Published, non-draft releases that can be identified by a semantic version.
/// Drafts are excluded entirely. Stable and prerelease lists remain separate.
pub fn versioned_releases(releases: &[GitHubRelease]) -> (Vec<VersionedRelease>, Vec<String>) {
    let mut stable = Vec::new();
    let mut warnings = Vec::new();
    for release in releases.iter().filter(|release| !release.draft) {
        match release_version(release) {
            Some(version) => {
                let item = VersionedRelease {
                    release: release.clone(),
                    version,
                };
                if release.prerelease {
                    // Kept separately by callers; list remains deterministic.
                }
                stable.push(item);
            }
            None => warnings.push(format!(
                "Release '{}' has no identifiable semantic version tag or name.",
                display_release_name(release)
            )),
        }
    }
    stable.sort_by(|a, b| {
        b.version
            .cmp(&a.version)
            .then_with(|| b.release.id.cmp(&a.release.id))
    });
    (stable, warnings)
}

pub fn stable_releases(releases: &[GitHubRelease]) -> Vec<VersionedRelease> {
    versioned_releases(releases)
        .0
        .into_iter()
        .filter(|release| !release.release.prerelease)
        .collect()
}

pub fn prerelease_versions(releases: &[GitHubRelease]) -> Vec<VersionedRelease> {
    versioned_releases(releases)
        .0
        .into_iter()
        .filter(|release| release.release.prerelease)
        .collect()
}

fn normalized_token(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Exact, documented package-selection rule:
/// 1. If there is exactly one ZIP attachment, use it.
/// 2. If there are several, keep only names containing the plugin id (ignoring
///    separators and case). If exactly one remains, use it.
/// 3. If several still remain, keep those that also contain the release
///    version. If exactly one remains, use it.
/// 4. Anything else is ambiguous and reported; NexusTools never guesses.
pub fn select_zip_asset(
    plugin_id: &str,
    version: Option<&Version>,
    assets: &[GitHubAsset],
) -> Result<Option<GitHubAsset>, String> {
    let zips: Vec<&GitHubAsset> = assets
        .iter()
        .filter(|asset| asset.name.to_ascii_lowercase().ends_with(".zip"))
        .collect();
    if zips.is_empty() {
        return Ok(None);
    }
    if zips.len() == 1 {
        return Ok(Some(zips[0].clone()));
    }

    let id_token = normalized_token(plugin_id);
    let mut candidates: Vec<&GitHubAsset> = zips
        .iter()
        .copied()
        .filter(|asset| normalized_token(&asset.name).contains(&id_token))
        .collect();
    if candidates.len() == 1 {
        return Ok(Some(candidates[0].clone()));
    }
    if candidates.is_empty() {
        return Err(format!(
            "The release has {} ZIP attachments and none identifies plugin '{}'. NexusTools will not guess which package to install.",
            zips.len(),
            plugin_id
        ));
    }

    if let Some(version) = version {
        let version_token = normalized_token(&version.canonical_without_build());
        let version_filtered: Vec<&GitHubAsset> = candidates
            .iter()
            .copied()
            .filter(|asset| normalized_token(&asset.name).contains(&version_token))
            .collect();
        if version_filtered.len() == 1 {
            return Ok(Some(version_filtered[0].clone()));
        }
        candidates = version_filtered;
    }

    Err(format!(
        "The release has {} ZIP attachments that cannot be narrowed to one compatible package. Rename the intended package to include the plugin id, or publish exactly one ZIP for this release.",
        candidates.len()
    ))
}

fn digest_sha256(asset: &GitHubAsset) -> Option<String> {
    asset.digest.as_deref().and_then(|digest| {
        let digest = digest.trim();
        let value = digest
            .strip_prefix("sha256:")
            .or_else(|| digest.strip_prefix("sha256="))
            .or_else(|| digest.strip_prefix("SHA256:"))
            .unwrap_or(digest);
        if value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(value.to_ascii_lowercase())
        } else {
            None
        }
    })
}

pub fn asset_sha256(asset: &GitHubAsset) -> Option<String> {
    digest_sha256(asset)
}

fn package_for_release(
    plugin_id: &str,
    release: &GitHubRelease,
    version: Option<&Version>,
) -> Result<Option<ReleasePackage>, String> {
    let asset = select_zip_asset(plugin_id, version, &release.assets)?;
    Ok(asset.map(|asset| ReleasePackage {
        name: asset.name.clone(),
        size: asset.size,
        sha256: digest_sha256(&asset),
        checksum_source: digest_sha256(&asset).map(|_| "GitHub asset digest".to_string()),
    }))
}

pub fn release_view(
    plugin_id: &str,
    release: &GitHubRelease,
    manifest_version: &str,
) -> ReleaseView {
    let version = release_version(release);
    let matches = version
        .as_ref()
        .map(|version| {
            Version::from_label(manifest_version)
                .map(|manifest| version == &manifest)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let (package, install_error) = match package_for_release(plugin_id, release, version.as_ref()) {
        Ok(Some(package)) => (Some(package), None),
        Ok(None) => (
            None,
            Some(
                "This release has no uploaded installation ZIP. NexusTools does not use GitHub's automatically generated source archives."
                    .to_string(),
            ),
        ),
        Err(error) => (None, Some(error)),
    };
    // A past release may legitimately differ from the current branch manifest;
    // installability is determined by the release's own bundled manifest. The
    // mismatch is surfaced separately and only the automatic Install/Update
    // action requires a match.
    ReleaseView {
        id: release.id,
        tag: release.tag_name.clone(),
        name: display_release_name(release),
        version: version.map(|version| version.to_string()),
        prerelease: release.prerelease,
        published_at: release
            .published_at
            .clone()
            .or_else(|| release.created_at.clone()),
        notes: release.body.clone().unwrap_or_default(),
        package,
        matches_manifest_version: matches,
        install_error,
        recommended: false,
    }
}

pub fn build_release_views(
    plugin_id: &str,
    manifest_version: &str,
    releases: &[GitHubRelease],
) -> Vec<ReleaseView> {
    let (releases, _) = versioned_releases(releases);
    let mut views: Vec<ReleaseView> = releases
        .iter()
        .map(|versioned| release_view(plugin_id, &versioned.release, manifest_version))
        .collect();
    if let Some(recommended) =
        recommended_release_index(plugin_id, manifest_version, releases.as_slice())
    {
        if let Some(view) = views.get_mut(recommended) {
            view.recommended = true;
        }
    }
    // Stable releases first, then prereleases. Each group is already sorted
    // newest-first. This keeps prereleases from ever taking the "Latest" slot.
    views.sort_by(|a, b| {
        let ordering = a
            .prerelease
            .cmp(&b.prerelease)
            .then_with(|| version_cmp(&b.version, &a.version));
        ordering.then_with(|| b.id.cmp(&a.id))
    });
    views
}

fn version_cmp(a: &Option<String>, b: &Option<String>) -> Ordering {
    match (
        a.as_deref().and_then(|v| Version::parse(v).ok()),
        b.as_deref().and_then(|v| Version::parse(v).ok()),
    ) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => Ordering::Equal,
    }
}

/// Index in `views` (same order as `versioned_releases`) of the stable release
/// that exactly matches the branch manifest version and has a usable package.
/// This is the only release auto-selected by the Install/Update action.
fn recommended_release_index(
    plugin_id: &str,
    manifest_version: &str,
    releases: &[VersionedRelease],
) -> Option<usize> {
    let manifest = Version::from_label(manifest_version)?;
    let mut recommendation: Option<(usize, &VersionedRelease)> = None;
    // `views` filters nothing, but versioned_releases excludes drafts and keeps
    // a deterministic order. We search the filtered view order instead below.
    for (index, release) in releases.iter().enumerate() {
        if release.release.prerelease || release.version != manifest {
            continue;
        }
        if !matches!(
            package_for_release(plugin_id, &release.release, Some(&release.version)),
            Ok(Some(_))
        ) {
            continue;
        }
        match recommendation {
            Some((_, current)) if current.version >= release.version => {}
            _ => recommendation = Some((index, release)),
        }
    }
    recommendation.map(|(index, _)| index)
}

pub fn find_release_for_install(
    plugin_id: &str,
    manifest_version: &str,
    releases: &[GitHubRelease],
    requested_version: Option<&str>,
    release_id: Option<i64>,
    prerelease_was_explicit: bool,
) -> AppResult<(GitHubRelease, Version)> {
    let (versioned, warnings) = versioned_releases(releases);
    let selected = if let Some(release_id) = release_id {
        let found = versioned
            .iter()
            .find(|item| item.release.id == release_id)
            .cloned();
        if found.as_ref().is_some_and(|item| item.release.prerelease) && !prerelease_was_explicit {
            return Err(AppError::new(
                "prerelease_not_selected",
                "That release is a prerelease. Choose it explicitly from Past versions.",
            ));
        }
        found
    } else if let Some(requested_version) = requested_version {
        let requested = Version::from_label(requested_version).ok_or_else(|| {
            AppError::new(
                "invalid_version",
                format!("'{requested_version}' is not a valid release version."),
            )
        })?;
        let mut matches: Vec<VersionedRelease> = versioned
            .iter()
            .filter(|item| item.version == requested)
            .cloned()
            .collect();
        if matches.iter().all(|item| item.release.prerelease) && !prerelease_was_explicit {
            return Err(AppError::new(
                "prerelease_not_selected",
                "Only prerelease builds match that version. Choose the prerelease explicitly in Past versions.",
            ));
        }
        matches.sort_by(|a, b| {
            a.release
                .prerelease
                .cmp(&b.release.prerelease)
                .then_with(|| b.release.published_at.cmp(&a.release.published_at))
        });
        // Prefer a stable release when both stable and prerelease share a version.
        matches.into_iter().next()
    } else {
        let manifest = Version::from_label(manifest_version).ok_or_else(|| {
            AppError::new(
                "invalid_version",
                format!("The plugin manifest version is invalid: {manifest_version}"),
            )
        })?;
        versioned
            .iter()
            .find(|item| !item.release.prerelease && item.version == manifest)
            .cloned()
    };

    let selected = selected.ok_or_else(|| {
        let detail = if warnings.is_empty() {
            "No matching published release was found.".to_string()
        } else {
            warnings.join(" ")
        };
        AppError::new(
            "release_not_found",
            format!(
                "No installable stable release matches the plugin manifest version {manifest_version}. {detail} NexusTools will not silently install a different version."
            ),
        )
    })?;
    if let Some(requested) = requested_version {
        let requested = Version::from_label(requested).ok_or_else(|| {
            AppError::new(
                "invalid_version",
                format!("'{requested}' is not a valid release version."),
            )
        })?;
        if requested != selected.version {
            return Err(AppError::new(
                "version_mismatch",
                format!(
                    "The selected release's version {} does not match the requested version {}.",
                    selected.version, requested
                ),
            ));
        }
    }
    if !requested_version.is_some() && !selected.release.prerelease {
        // Default selection must match the branch manifest.
        let manifest = Version::from_label(manifest_version).unwrap();
        if selected.version != manifest {
            return Err(AppError::new(
                "version_mismatch",
                format!(
                    "The selected release version {} does not match the plugin manifest version {}.",
                    selected.version, manifest_version
                ),
            ));
        }
    }
    let package = package_for_release(plugin_id, &selected.release, Some(&selected.version))
        .map_err(|message| AppError::new("ambiguous_release_package", message))?;
    if package.is_none() {
        return Err(AppError::new(
            "missing_release_package",
            "The selected release has no uploaded installation ZIP. Generated source archives are not used.",
        ));
    }
    Ok((selected.release, selected.version))
}

pub fn parse_published_at(value: &Option<String>) -> Option<DateTime<FixedOffset>> {
    value
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> GitHubAsset {
        GitHubAsset {
            name: name.to_string(),
            size: 100,
            content_type: Some("application/zip".to_string()),
            digest: None,
            browser_download_url: format!("https://github.com/example/releases/download/v1/{name}"),
        }
    }

    fn release(
        id: i64,
        tag: &str,
        name: Option<&str>,
        prerelease: bool,
        assets: Vec<GitHubAsset>,
    ) -> GitHubRelease {
        GitHubRelease {
            id,
            tag_name: tag.to_string(),
            name: name.map(ToString::to_string),
            draft: false,
            prerelease,
            published_at: Some("2026-10-22T12:00:00Z".to_string()),
            created_at: None,
            body: Some("Notes".to_string()),
            assets,
        }
    }

    #[test]
    fn exactly_one_zip_is_selected_even_without_plugin_name() {
        let result = select_zip_asset(
            "modtools",
            Some(&Version::parse("1.0.0").unwrap()),
            &[asset("release.zip")],
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.name, "release.zip");
    }

    #[test]
    fn multiple_zips_require_documented_disambiguation() {
        let version = Version::parse("1.0.2").unwrap();
        let selected = select_zip_asset(
            "sculpt-pie",
            Some(&version),
            &[
                asset("SculptPie_1.0.2_windows.zip"),
                asset("helper-tools.zip"),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected.name, "SculptPie_1.0.2_windows.zip");

        let error = select_zip_asset(
            "sculpt-pie",
            Some(&version),
            &[asset("package-a.zip"), asset("package-b.zip")],
        )
        .unwrap_err();
        assert!(error.contains("will not guess"));
    }

    #[test]
    fn no_zip_is_reported_as_unavailable() {
        let release = release(1, "v1.0.0", None, false, vec![]);
        let view = release_view("modtools", &release, "1.0.0");
        assert!(view.package.is_none());
        assert!(view
            .install_error
            .as_deref()
            .unwrap()
            .contains("no uploaded installation ZIP"));
    }

    #[test]
    fn semantic_order_and_prerelease_separation() {
        let releases = vec![
            release(1, "v1.0.9", None, false, vec![asset("plugin.zip")]),
            release(2, "v1.0.10", None, false, vec![asset("plugin.zip")]),
            release(3, "v1.1.0-beta.1", None, true, vec![asset("plugin.zip")]),
        ];
        let (versioned, _) = versioned_releases(&releases);
        assert_eq!(
            versioned
                .iter()
                .map(|v| v.version.to_string())
                .collect::<Vec<_>>(),
            vec!["1.1.0-beta.1", "1.0.10", "1.0.9"]
        );
        assert_eq!(stable_releases(&releases).len(), 2);
        assert_eq!(prerelease_versions(&releases).len(), 1);
        let views = build_release_views("modtools", "1.0.10", &releases);
        assert_eq!(views[0].version.as_deref(), Some("1.0.10"));
        assert!(views[0].recommended);
        assert!(!views.iter().find(|v| v.prerelease).unwrap().recommended);
    }

    #[test]
    fn tag_v_and_two_component_manifest_match_release() {
        let releases = vec![release(
            9,
            "v1.0.0",
            Some("ModTools 1.0.0"),
            false,
            vec![asset("modtools.zip")],
        )];
        let (release, version) =
            find_release_for_install("modtools", "1.0", &releases, None, None, false).unwrap();
        assert_eq!(release.id, 9);
        assert_eq!(version.to_string(), "1.0.0");
    }

    #[test]
    fn manifest_version_without_release_is_not_replaced_by_older_release() {
        let releases = vec![release(
            1,
            "v1.0.0",
            None,
            false,
            vec![asset("modtools.zip")],
        )];
        let error = find_release_for_install("modtools", "1.0.2", &releases, None, None, false)
            .unwrap_err();
        assert_eq!(error.code, "release_not_found");
    }

    #[test]
    fn prerelease_requires_explicit_selection() {
        let releases = vec![release(
            1,
            "v2.0.0-beta.1",
            None,
            true,
            vec![asset("modtools.zip")],
        )];
        let error = find_release_for_install(
            "modtools",
            "1.0.0",
            &releases,
            Some("2.0.0-beta.1"),
            None,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, "prerelease_not_selected");
        let (release, _) = find_release_for_install(
            "modtools",
            "1.0.0",
            &releases,
            Some("2.0.0-beta.1"),
            None,
            true,
        )
        .unwrap();
        assert_eq!(release.id, 1);
    }
}
