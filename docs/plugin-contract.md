# NexusTools plugin and installer contract

This document records the documented rules implemented by NexusTools.

## 1. Catalog

`catalog.json` is read from the raw GitHub endpoint, not from a GitHub HTML page:

```
https://raw.githubusercontent.com/vibe-phoenix/NexusToolsApp/main/for%20Installer/catalog.json
```

Schema:

```json
{
  "schemaVersion": 1,
  "plugins": [
    {
      "id": "example-plugin",
      "repository": "owner/repository",
      "ref": "main",
      "manifestPath": "for installer/plugin.json"
    }
  ]
}
```

- Paths in JSON are stored with normal spaces and forward slashes.
- Each path segment is percent-encoded only when a URL is built:
  `for installer/plugin.json` becomes `for%20installer/plugin.json`.
- Directory separators, capitalization, and filename spelling are preserved.
- `id` must be safe and unique. One invalid entry is reported without preventing other entries
  from loading. A wrong top-level schema fails the catalog.
- Adding a compatible entry makes it available on the next successful refresh. No application
  update is needed.
- `ref` is resolved to a full commit before any plugin files are read.

## 2. Branch metadata snapshot

For each catalog entry:

1. Resolve `ref` to a commit.
2. Read the exact `manifestPath` at that commit.
3. Reject a manifest whose `id` differs from the catalog id.
4. Read `assets.banner`, `content.readme`, and `content.features` relative to the directory
   containing the manifest.
5. Validate the image type, UTF-8 Markdown, sizes, and required fields.
6. Make the plugin visible only after the metadata and all required assets form a complete,
   atomically activated snapshot.

Required metadata fields: `id`, `name`, `description`, `overview`, `version`, `assets.banner`,
`content.readme`, and `content.features`.

The Markdown is untrusted. The desktop UI escapes it and parses only a small safe subset; raw HTML
and scripts are never executed, and remote images/links are not loaded in the application process.

## 3. Release selection

- Only GitHub Releases with `draft: false` are considered.
- Prereleases remain visible in a separate group and are never auto-selected.
- Versions are parsed from the tag first, with release display names such as
  `SculptPie 1.0.2` as a fallback.
- Tag normalization: leading `v`/`V` is ignored, one to four numeric components are accepted, and
  missing components are treated as zero (`1.0` equals `1.0.0`). Build metadata is ignored for
  ordering. Semantic version ordering is used, not alphabetic ordering.
- The main Install/Update action requires a stable release whose normalized version equals the
  catalog manifest version. If none exists, NexusTools does not substitute an older release.
- Past versions can be installed explicitly from the release history.
- Pagination is followed until no more pages are returned.

### Uploaded ZIP rule

Only assets attached to the release are considered. GitHub's generated source archives are never
used.

1. If exactly one `.zip` attachment exists, use it.
2. If multiple ZIPs exist, retain filenames containing the plugin id (case/separators ignored). If
   exactly one remains, use it.
3. If several remain, retain filenames containing the normalized release version. If exactly one
   remains, use it.
4. Otherwise installation is unavailable with an ambiguity message.

A release with no suitable ZIP is unavailable for installation with an explanation.

### Version checks

The ZIP's bundled `plugin.json` must have the catalog plugin id and a version matching the selected
release under the same tag normalization. A mismatch stops installation. The bundled manifest is
authoritative for the selected release; branch installation instructions are never applied to an
older release.

### Package integrity

If the GitHub release asset provides a SHA-256 digest, it is verified after download. If no digest
exists and exactly one `<zip>.sha256` asset exists, that checksum is fetched and verified. The
archive is not extracted until verification succeeds. The installed record stores the computed
package hash, whether a checksum was available, the selected release, and the asset identity.

## 4. Package structure

The uploaded ZIP must contain, at its root or inside exactly one enclosing directory:

```
files/
  plugin files...
for installer/
  plugin.json
```

Ambiguous roots, multiple candidate roots, missing `files/`, or a missing manifest are rejected.
`sourceDirectory` in the bundled manifest is resolved relative to the extracted package root.

Archive protections:

- no absolute or drive-prefixed paths
- no `..` traversal
- no backslash ambiguity, alternate data streams, reserved Windows device names, trailing spaces
  or dots
- no symbolic links or junctions
- case-insensitive duplicate path detection
- bounded entry count, compressed download size, and total extracted size
- extraction stays inside a private staging folder

## 5. Installation manifest (schemaVersion 1)

The release's bundled manifest provides:

```json
{
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
        {
          "action": "copy",
          "source": ".",
          "destination": "${paths.userScripts}/example_plugin"
        }
      ]
    }
  ]
}
```

Target matching:

- `app` must be `maya` or `zbrush`.
- `os` must be `windows`.
- The detected application version must match a `supportedVersions` entry (numeric,
  component-padded; `2026` matches `2026.0`, not `2026.1`).
- More than one matching target for the same app/version is rejected as ambiguous.
- Separate targets are required when instructions differ by application version.

### Actions

Supported actions only. Unknown actions, unknown action fields, and unsupported replacement rules
are rejected before any write.

`copy`
: Requires `source` and `destination`. Source is relative to `sourceDirectory`; `.` copies the
  directory contents. File copies write a file; directory copies merge files/directories without
  deleting unrelated destination content.

`mkdir`
: Requires `destination`. Creates a directory if absent.

`writeFile`
: Requires `destination` and UTF-8 `content`. Supports these documented content placeholders:

  - `${plugin.id}`
  - `${plugin.name}`
  - `${plugin.version}`
  - `${target.version}`

`patchText`
: Requires `destination`, `marker`, and `content`. Inserts or updates one uniquely marked block
  and preserves all unrelated text and the existing newline style. Markers use the correct comment
  syntax for the target extension (`#` for Python/text/TOML, `//` for MEL/JS/CSS, `--` for Lua).
  Duplicate or malformed blocks are conflicts. Reinstalling updates the block rather than
  duplicating it. Supported file types: `.mel`, `.py`, `.js`, `.jsx`, `.ts`, `.tsx`, `.css`,
  `.scss`, `.lua`, `.txt`, `.cfg`, `.conf`, `.toml`, `.properties`, `.ini`. JSON is not supported
  because it has no standard comments.

Optional target field:

```json
"requiresAppClosed": true
```

NexusTools always requires ZBrush to be closed. This field can add the same requirement for a Maya
target.

Optional replacement rule on `copy`/`writeFile`:

```json
"replace": "owned"
```

`owned` is the default and the only supported rule. Other values are rejected explicitly. Files are
only replaced if they are recorded as owned by the same plugin target, are unchanged since they were
installed, and are backed up first. Unowned files are conflicts.

No action executes shell commands or scripts from the manifest.

### Destination paths

Supported placeholders:

- `${paths.userScripts}`
- `${paths.userModules}`
- `${paths.userPrefs}`
- `${paths.zplugins}`
- `${paths.pluginData}`
- `${target.version}`
- `${plugin.id}`
- `${plugin.version}`

${paths.pluginData} resolves to a plugin- and target-specific folder underneath
`%LOCALAPPDATA%\NexusTools\plugin-data`. Unresolved placeholders, relative destinations, and
destinations outside approved roots stop installation. A manifest that requests an absolute path
outside the managed paths is not installed until the exact required approval roots are shown to and
approved by the user. No other write roots are allowed.

## 6. Application detection

### Maya

- Versions and roots are discovered from Autodesk registry entries, Windows uninstall entries,
  `MAYA_LOCATION`, and Autodesk directories on local disks. Multiple roots are retained.
- The user application root comes from `MAYA_APP_DIR` or the known Windows Documents folder
  (respecting redirection such as OneDrive). The version-specific Maya folder is selected when
  unambiguous. A missing folder uses the documented default `...\maya\<version>` and can be created.
- `MAYA_SCRIPT_PATH` and `MAYA_MODULE_PATH` are honored as path overrides, taking the first absolute
  entry, because they are the documented configuration override mechanism. Preferences live under
  the resolved Mayan user folder. Manual overrides take priority.
- If multiple version-prefixed Maya user folders make preferences ambiguous, the target requires a
  manual override instead of guessing.

### ZBrush

- Installations are discovered from Maxon/Pixologic uninstall entries, vendor folders, standard
  application folders on local disks, and manual overrides.
- ZBrush 2026.1+ uses
  `%APPDATA%\Maxon\ZBrush_<id>\ZStartup\ZPlugs64`, unless `ZBRUSH_USER_ASSETS_DIR` overrides the
  assets root. If several hash folders exist, NexusTools refuses to guess which one belongs to the
  installation and requires a manual override.
- ZBrush 2026.0.1 and earlier use `<install root>\ZStartup\ZPlugs64` (or the legacy `ZPlugs`
  folder when that is the path present).
- ZBrush must be closed before installation.

Installations whose effective plugin folders are identical are collapsed into one target; distinct
plugin locations remain distinct targets. Each target has an independent installed-version record.

## 7. Transactions, ownership, and updates

Before any target is changed:

- the complete bundled manifest is validated
- compatibility and all paths are resolved
- source files, conflicts, ownership, read-only state, parent paths, symlink/junction components,
  free disk space, and write permissions are checked
- the application-running requirement is checked
- all selected targets are preflighted before the first target is modified

For each target:

1. Files that will be replaced or removed are backed up.
2. A transaction journal is written to LocalAppData.
3. Files and managed blocks are applied atomically where possible.
4. The per-target installed-state record is atomically saved.
5. The journal is committed and removed.
6. On failure, the target's files and state are rolled back without rolling back other successful
   targets.
7. On the next launch, unfinished journals are detected and recovered before the UI is shown.

Ownership rules:

- Files installed by another plugin or target are conflicts.
- Unowned existing files are not silently overwritten.
- Plugin-owned files are replaced only if their on-disk hash still matches the installation record.
- Modified files, modified managed blocks, and user content are preserved and reported.
- Obsolete plugin-owned files are removed only after the same checks pass.
- Backups are referenced from the target's installed record.

## 8. Installed state and uninstall

Each target record contains the plugin id and version, target identity and application version,
repository, selected release, asset/checksum identity, installed file paths and hashes, managed text
blocks, created directories, backup reference, operation id, status, and the bundled manifest
snapshot.

Uninstall uses only local records. It removes unchanged plugin-owned files and unmodified managed
blocks, backs up removals, leaves modified or unrelated content, and removes created directories only
when empty. If some content was preserved, the target is kept with status `partial-uninstall` so the
user can resolve conflicts and try again. Uninstall works without the repository or catalog.

## 9. Cache behavior

The persistent cache is separate from installed state and settings:

- plugin.json, banner, README.md, FEATURES.md
- repository, ref, resolved commit and tree identity
- HTTP ETag/Last-Modified validators per file, plus commit/catalog/release validators
- SHA-256 and GitHub blob hash per file
- release metadata and fetch diagnostics

Named snapshots are assembled in a staging directory. Only after every file is present and validated
is a new snapshot activated by an atomic pointer. The previous snapshot is kept as a fallback.
Conditional HTTP requests, commit/tree hashes, and per-file blob hashes avoid downloading unchanged
files. If the ref still resolves to the cached commit, files are reused without downloading. A
release-only change reuses all cached plugin files and atomically rewrites only the snapshot
metadata.

Corrupt or incomplete current snapshots are never shown. NexusTools falls back to the previous
complete snapshot when possible and re-fetches corrupt files on the next refresh. Complete cached
plugins remain available offline; cached information is clearly labelled in the UI.
