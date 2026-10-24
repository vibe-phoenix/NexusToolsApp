use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// A deliberately small semantic-version implementation for plugin and release
/// labels. It accepts 1-4 numeric components ("1.0" is equivalent to "1.0.0"),
/// a leading v/V on version labels, prerelease tags, and build metadata. Build
/// metadata is ignored for equality and ordering, as SemVer requires.
#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct Version {
    components: [u64; 4],
    prerelease: Vec<String>,
    build: Option<String>,
    display: String,
}

impl Version {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("version is empty".to_string());
        }
        let trimmed = if let Some(rest) = strip_version_prefix(trimmed) {
            rest
        } else {
            trimmed
        };
        if trimmed.chars().any(char::is_whitespace) {
            return Err(format!("version contains whitespace: {raw}"));
        }
        let (without_build, build) = match trimmed.split_once('+') {
            Some((left, right)) => {
                if right.is_empty()
                    || !right
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
                {
                    return Err(format!("invalid build metadata in version: {raw}"));
                }
                (left, Some(right.to_string()))
            }
            None => (trimmed, None),
        };
        let (core, prerelease_raw) = match without_build.split_once('-') {
            Some((left, right)) => (left, Some(right)),
            None => (without_build, None),
        };
        let mut values = core.split('.');
        let mut components = [0u64; 4];
        let mut count = 0;
        let mut display_components = Vec::new();
        for component in values.by_ref() {
            if count == 4 {
                return Err(format!(
                    "version has more than four numeric components: {raw}"
                ));
            }
            if component.is_empty() || !component.chars().all(|c| c.is_ascii_digit()) {
                return Err(format!("invalid numeric version component: {raw}"));
            }
            if component.len() > 1 && component.starts_with('0') {
                return Err(format!(
                    "version components may not have leading zeros: {raw}"
                ));
            }
            let value: u64 = component
                .parse()
                .map_err(|_| format!("invalid numeric version component: {raw}"))?;
            components[count] = value;
            display_components.push(component.to_string());
            count += 1;
        }
        if count == 0 {
            return Err(format!("version has no numeric components: {raw}"));
        }
        let mut prerelease = Vec::new();
        if let Some(pre) = prerelease_raw {
            if pre.is_empty() {
                return Err(format!("empty prerelease in version: {raw}"));
            }
            for identifier in pre.split('.') {
                if identifier.is_empty()
                    || !identifier
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-')
                {
                    return Err(format!("invalid prerelease identifier in version: {raw}"));
                }
                if identifier.len() > 1
                    && identifier.chars().all(|c| c.is_ascii_digit())
                    && identifier.starts_with('0')
                {
                    return Err(format!(
                        "numeric prerelease identifiers may not have leading zeros: {raw}"
                    ));
                }
                prerelease.push(identifier.to_string());
            }
        }
        let mut display = display_components.join(".");
        if !prerelease.is_empty() {
            display.push('-');
            display.push_str(&prerelease.join("."));
        }
        if let Some(build) = &build {
            display.push('+');
            display.push_str(build);
        }
        Ok(Self {
            components,
            prerelease,
            build,
            display,
        })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.prerelease.is_empty()
    }

    pub fn canonical_without_build(&self) -> String {
        let mut output = self
            .components
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(".");
        if !self.prerelease.is_empty() {
            output.push('-');
            output.push_str(&self.prerelease.join("."));
        }
        output
    }

    /// Release tags may start with `v`; release display names may have a
    /// product prefix. This helper performs that documented normalization.
    pub fn from_label(label: &str) -> Option<Self> {
        if let Ok(version) = Self::parse(label) {
            return Some(version);
        }
        let trimmed = label.trim();
        for token in trimmed.split_whitespace().rev() {
            if let Ok(version) = Self::parse(token) {
                return Some(version);
            }
            if let Some(rest) = strip_version_prefix(token) {
                if let Ok(version) = Self::parse(rest) {
                    return Some(version);
                }
            }
        }
        None
    }
}

fn strip_version_prefix(value: &str) -> Option<&str> {
    let mut chars = value.chars();
    match chars.next() {
        Some('v') | Some('V') if chars.next().is_some_and(|c| c.is_ascii_digit()) => {
            Some(&value[1..])
        }
        _ => None,
    }
}

impl Version {
    fn compare_prerelease(&self, other: &Self) -> Ordering {
        match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                for (left, right) in self.prerelease.iter().zip(other.prerelease.iter()) {
                    let left_numeric = left.parse::<u64>().ok();
                    let right_numeric = right.parse::<u64>().ok();
                    let ordering = match (left_numeric, right_numeric) {
                        (Some(a), Some(b)) => a.cmp(&b),
                        (Some(_), None) => Ordering::Less,
                        (None, Some(_)) => Ordering::Greater,
                        (None, None) => left.cmp(right),
                    };
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                self.prerelease.len().cmp(&other.prerelease.len())
            }
        }
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.components == other.components && self.prerelease == other.prerelease
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.components
            .cmp(&other.components)
            .then_with(|| self.compare_prerelease(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display)
    }
}

impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Compare application version labels ("2024", "2026.1") without rejecting
/// application-specific labels. Numeric components compare numerically and
/// missing components are zero, so 2026 and 2026.0 match.
pub fn app_versions_match(required: &str, detected: &str) -> bool {
    if required.trim().eq_ignore_ascii_case(detected.trim()) {
        return true;
    }
    match (Version::parse(required), Version::parse(detected)) {
        (Ok(a), Ok(b)) => a == b,
        _ => required.trim().eq_ignore_ascii_case(detected.trim()),
    }
}

pub fn versions_match(left: &str, right: &str) -> bool {
    match (Version::from_label(left), Version::from_label(right)) {
        (Some(a), Some(b)) => a == b,
        _ => left.trim().eq_ignore_ascii_case(right.trim()),
    }
}

/// Extract a 4-digit Maya/ZBrush year or a dotted Maxon version from a folder
/// name or product name.
pub fn extract_app_version(label: &str) -> Option<String> {
    let candidate = label
        .split(|c: char| c.is_whitespace() || c == '_')
        .find(|part| {
            let cleaned = part.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
            cleaned.chars().next().is_some_and(|c| c.is_ascii_digit())
                && cleaned.chars().all(|c| c.is_ascii_digit() || c == '.')
                && cleaned.split('.').count() <= 4
        })?;
    let cleaned = candidate.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
    let cleaned = cleaned.trim_matches('.');
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_leading_v_and_pads_components() {
        let tag = Version::from_label("v1.0").unwrap();
        let manifest = Version::parse("1.0.0").unwrap();
        assert_eq!(tag, manifest);
        assert_eq!(manifest.to_string(), "1.0.0");
        assert_eq!(
            Version::from_label("SculptPie 1.0.2").unwrap().to_string(),
            "1.0.2"
        );
    }

    #[test]
    fn semantic_order_not_alphabetical() {
        let mut versions = ["1.0.10", "1.0.2", "1.0.0", "1.0.1", "2.0.0"]
            .iter()
            .map(|v| Version::parse(v).unwrap())
            .collect::<Vec<_>>();
        versions.sort();
        assert_eq!(
            versions.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["1.0.0", "1.0.1", "1.0.2", "1.0.10", "2.0.0"]
        );
    }

    #[test]
    fn prereleases_sort_before_stable_and_are_not_equal() {
        let beta = Version::parse("1.2.0-beta.2").unwrap();
        let alpha = Version::parse("1.2.0-alpha.1").unwrap();
        let stable = Version::parse("1.2.0").unwrap();
        assert!(alpha < beta);
        assert!(beta < stable);
        assert_ne!(stable, beta);
        assert!(!stable.is_prerelease());
        assert!(beta.is_prerelease());
    }

    #[test]
    fn app_versions_compare_numerically() {
        assert!(app_versions_match("2024", "2024"));
        assert!(app_versions_match("2026", "2026.0"));
        assert!(!app_versions_match("2026", "2026.1"));
        assert_eq!(extract_app_version("Maya2025"), Some("2025".to_string()));
        assert_eq!(
            extract_app_version("Maxon ZBrush 2026.1"),
            Some("2026.1".to_string())
        );
    }

    #[test]
    fn rejects_invalid_versions() {
        assert!(Version::parse("").is_err());
        assert!(Version::parse("1.0.0.0.0").is_err());
        assert!(Version::parse("1.0.0-").is_err());
        assert!(Version::parse("release").is_err());
        assert!(Version::parse("01.2.3").is_err());
    }
}
