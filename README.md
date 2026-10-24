# NexusTools

NexusTools is a Windows plugin manager for Maya and ZBrush, built with Rust, Tauri 2, and a
static HTML/CSS/JS interface. The interface follows the supplied `NexusTools.html` design: black
surfaces, thin outlines, the blue accent, the three-column plugin grid, detail tabs, update
selection, settings and dialogs.

The application reads its plugin catalog from:

```
https://raw.githubusercontent.com/vibe-phoenix/NexusToolsApp/main/for%20Installer/catalog.json
```

The repository is the source of available plugins. There is no plugin name or repository list
hardcoded in production code.

## What it does

- Loads a validated, persistent per-user cache immediately at startup, then checks GitHub in the
  background.
- Resolves each catalog `ref` to a commit and reads `plugin.json` plus the exact declared banner,
  README, and FEATURES assets from that commit.
- Shows a plugin only after the complete metadata + asset snapshot is available.
- Discovers Maya and ZBrush installations, including custom drives, redirected user folders, and
  documented configuration overrides.
- Discovers stable and prerelease GitHub Releases with pagination, semantic version ordering, and
  unambiguous uploaded-ZIP selection.
- Downloads a package once per operation, verifies an available GitHub asset digest or sidecar
  checksum, extracts safely, and installs the release's own bundled `plugin.json` into every
  selected application installation.
- Tracks ownership per target, backs up files, refuses to overwrite unrelated or locally modified
  files, and records transaction journals for crash recovery.
- Updates multiple targets independently and uninstalls entirely from local records, even offline.
- Provides manual application-path overrides for unusual or undetected installations.

Full catalog, manifest, package, release, path, and cache rules are documented in
[docs/plugin-contract.md](docs/plugin-contract.md). Internal boundaries are described in
[docs/architecture.md](docs/architecture.md).

## Requirements

- Windows 10 or Windows 11 (x64)
- Administrator approval for the machine-wide installer (the app itself is installed under Program Files)
- WebView2 Runtime (installed on current Windows systems)
- Maya and/or ZBrush for actual plugin installation
- For building: Rust stable with the MSVC toolchain, Visual Studio C++ Build Tools, and Node.js

NexusTools intentionally keeps its machine-wide Program Files installation separate from the
per-user `%LOCALAPPDATA%\NexusTools` data parent.

## Build

```powershell
npm install
npm run dev
```

Release build and Windows installer:

```powershell
npm run build
```

Cargo-only build (no installer):

```powershell
cargo build --release --manifest-path src-tauri/Cargo.toml
```

The executable icon and window/taskbar icon are generated from the supplied `SculptTools.png`.
The icon sources are in `src-tauri/icons/`.

## Tests

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --lib
```

The suite covers catalog validation and isolation, path/URL encoding, complete-before-visible
cache loading, corrupt-cache fallback, semantic release selection and ZIP ambiguity, package
structure and archive traversal/symlink rejection, bundled-manifest installation, ownership
conflicts, multiple targets, rollback, interrupted-transaction recovery, managed blocks, and
uninstall preservation.

## Storage

All NexusTools data is under the current user's LocalAppData:

```
%LOCALAPPDATA%\NexusTools\
  webview\                  WebView2 cache/localStorage, kept out of Program Files
  cache\                    validated plugin metadata/assets and catalog
  installed-state.json      local ownership and installation records
  settings.json             manual application path overrides
  transactions\             incomplete-operation recovery journals
  backups\                  pre-change file backups
  staging\                  temporary package downloads/extractions
  plugin-data\              isolated per-plugin, per-target data
```

Clearing the metadata cache cannot remove installed records, settings, or user files.

## GitHub API limits

Public GitHub API rate limits apply. If needed, launch NexusTools with a read-only token in
`NEXUSTOOLS_GITHUB_TOKEN` (or `GITHUB_TOKEN`). The token is read from the environment at runtime and
is never saved or bundled.

## Application detection references

- Maya file path variables and user data locations:
  <https://help.autodesk.com/view/MAYACRE/ENU/?guid=GUID-228CCA33-4AFE-4380-8C3D-18D23F7EAC72>
- Maya user preferences can also be redirected with `MAYA_APP_DIR`; `MAYA_SCRIPT_PATH` and
  `MAYA_MODULE_PATH` are honored as user path overrides, with manual overrides available when a
  studio setup cannot be resolved unambiguously.
- Maxon ZBrush plugin installation (including the 2026.1+ user-assets location,
  `%APPDATA%\Maxon\ZBrush_<id>\ZStartup\ZPlugs64`, and `ZBRUSH_USER_ASSETS_DIR`):
  <https://support.maxon.net/hc/en-us/articles/7945763344412-Plugin-Installation>
- Maxon ZBrush user content and folder structure:
  <https://help.maxon.net/zbr/en-us/Content/html/user-guide/customizing-zbrush/user-content/user-content.html>

NexusTools does not assume every installation is in the default folder. It checks registry
uninstall entries, vendor registry keys, standard folders on every local disk, environment
overrides, and user-supplied paths. If it cannot determine a required destination, installation
for that target stops with an actionable error rather than guessing.

## Current catalog note

At the time of implementation, the published `vibe-phoenix/ModTools` manifest declares
`assets.banner` as `ModTools_banner.png`, but the repository contains `ModToolBanner.jpg`; the
repository also has no published releases. NexusTools deliberately does not guess a different
banner filename and does not expose a partial plugin, so the live catalog currently reports the
plugin as unavailable with an actionable problem. Once the manifest and releases are corrected,
the same installer will show and install it without an application update.

## License

MIT © 2026 Vibe Phoenix. See [LICENSE](LICENSE).
