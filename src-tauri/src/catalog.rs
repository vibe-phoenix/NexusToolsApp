use crate::error::{AppError, AppResult};
use crate::util::{repo_path_segments, split_repository, validate_plugin_id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub const DEFAULT_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/vibe-phoenix/NexusToolsApp/main/for%20Installer/catalog.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub id: String,
    pub repository: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub manifest_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogIssue {
    pub plugin_id: Option<String>,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ParsedCatalog {
    pub entries: Vec<CatalogEntry>,
    pub issues: Vec<CatalogIssue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogHeader {
    schema_version: u32,
    plugins: Vec<Value>,
}

/// Parse a v1 catalog. A bad plugin entry is reported individually and never
/// prevents validation of its siblings. Top-level JSON/schema failures reject
/// the catalog as a whole.
pub fn parse_catalog(bytes: &[u8]) -> AppResult<ParsedCatalog> {
    if bytes.len() > crate::util::MAX_CATALOG_BYTES {
        return Err(AppError::new(
            "catalog_too_large",
            "The plugin catalog exceeds the permitted size.",
        ));
    }
    let header: CatalogHeader = serde_json::from_slice(bytes).map_err(|error| {
        AppError::new(
            "invalid_catalog",
            format!(
                "The plugin catalog is not valid JSON or does not match schemaVersion 1: {error}"
            ),
        )
    })?;
    if header.schema_version != 1 {
        return Err(AppError::new(
            "unsupported_catalog_schema",
            format!(
                "The plugin catalog uses unsupported schemaVersion {} (NexusTools supports 1).",
                header.schema_version
            ),
        ));
    }

    let mut raw_entries: Vec<(Option<CatalogEntry>, Option<CatalogIssue>)> = Vec::new();
    for value in header.plugins {
        match serde_json::from_value::<CatalogEntry>(value.clone()) {
            Ok(entry) => raw_entries.push((Some(entry), None)),
            Err(error) => {
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                raw_entries.push((
                    None,
                    Some(CatalogIssue {
                        plugin_id: id,
                        code: "invalid_catalog_entry".to_string(),
                        message: format!("A catalog entry is invalid: {error}"),
                        details: vec![],
                    }),
                ));
            }
        }
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    for (entry, _) in &raw_entries {
        if let Some(entry) = entry {
            *counts.entry(entry.id.clone()).or_default() += 1;
        }
    }

    let mut entries = Vec::new();
    let mut issues = Vec::new();
    for (entry, parse_issue) in raw_entries {
        let Some(entry) = entry else {
            if let Some(issue) = parse_issue {
                issues.push(issue);
            }
            continue;
        };
        if counts.get(&entry.id).copied().unwrap_or(0) > 1 {
            issues.push(CatalogIssue {
                plugin_id: Some(entry.id.clone()),
                code: "duplicate_plugin_id".to_string(),
                message: format!("Multiple catalog entries use the id '{}'.", entry.id),
                details: vec![],
            });
            continue;
        }
        match validate_entry(&entry) {
            Ok(()) => entries.push(entry),
            Err(error) => issues.push(CatalogIssue {
                plugin_id: Some(entry.id.clone()),
                code: error.code,
                message: error.message,
                details: error.details,
            }),
        }
    }
    Ok(ParsedCatalog { entries, issues })
}

fn validate_entry(entry: &CatalogEntry) -> AppResult<()> {
    validate_plugin_id(&entry.id)?;
    split_repository(&entry.repository)?;
    if entry.git_ref.trim().is_empty() || entry.git_ref.len() > 250 {
        return Err(AppError::new(
            "invalid_ref",
            format!("Catalog entry '{}' has an invalid ref.", entry.id),
        ));
    }
    if entry
        .git_ref
        .chars()
        .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(AppError::new(
            "invalid_ref",
            format!("Catalog entry '{}' has an invalid ref.", entry.id),
        ));
    }
    repo_path_segments(&entry.manifest_path)?;
    if !entry.manifest_path.to_ascii_lowercase().ends_with(".json") {
        return Err(AppError::new(
            "invalid_manifest_path",
            format!(
                "Catalog entry '{}' manifestPath must point to a JSON file.",
                entry.id
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_catalog_with_spaced_casing_path() {
        let catalog = br#"{
          "schemaVersion": 1,
          "plugins": [
            {"id":"modtools","repository":"vibe-phoenix/ModTools","ref":"main","manifestPath":"for installer/plugin.json"},
            {"id":"second","repository":"example/Second","ref":"v2","manifestPath":"For Installer/Plugin.Json"}
          ]
        }"#;
        let parsed = parse_catalog(catalog).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.issues.len(), 0);
        assert_eq!(parsed.entries[0].manifest_path, "for installer/plugin.json");
        assert_eq!(parsed.entries[1].manifest_path, "For Installer/Plugin.Json");
    }

    #[test]
    fn one_invalid_entry_does_not_hide_other_plugins() {
        let catalog = br#"{
          "schemaVersion": 1,
          "plugins": [
            {"id":"good","repository":"owner/good","ref":"main","manifestPath":"for installer/plugin.json"},
            {"id":"../bad","repository":"owner/bad","ref":"main","manifestPath":"plugin.json"},
            {"id":"missing"}
          ]
        }"#;
        let parsed = parse_catalog(catalog).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].id, "good");
        assert_eq!(parsed.issues.len(), 2);
        assert!(parsed.issues.iter().any(|issue| !issue.code.is_empty()));
    }

    #[test]
    fn duplicate_ids_are_rejected_but_others_load() {
        let catalog = br#"{
          "schemaVersion": 1,
          "plugins": [
            {"id":"same","repository":"owner/one","ref":"main","manifestPath":"plugin.json"},
            {"id":"same","repository":"owner/two","ref":"main","manifestPath":"plugin.json"},
            {"id":"different","repository":"owner/three","ref":"main","manifestPath":"plugin.json"}
          ]
        }"#;
        let parsed = parse_catalog(catalog).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].id, "different");
        assert!(parsed
            .issues
            .iter()
            .any(|issue| issue.code == "duplicate_plugin_id"));
    }

    #[test]
    fn unsupported_schema_rejects_catalog() {
        let error = parse_catalog(br#"{"schemaVersion":2,"plugins":[]}"#).unwrap_err();
        assert_eq!(error.code, "unsupported_catalog_schema");
    }

    #[test]
    fn top_level_json_error_is_actionable() {
        let error = parse_catalog(br#"{"schemaVersion":1,"plugins":[}"#).unwrap_err();
        assert_eq!(error.code, "invalid_catalog");
        assert!(error.message.contains("catalog"));
    }
}
