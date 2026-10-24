# Architecture

NexusTools separates discovery, cache management, application detection, release/package
handling, installation, and local state. No UI code performs network or filesystem writes directly.

```
+--------------------- ui/ (HTML/CSS/JS) ----------------------+
| catalog cards, details, tabs, update selection, settings      |
| invoke commands + transient install progress events           |
+-----------------------------+--------------------------------+
                              |
+-----------------------------v--------------------------------+
| src-tauri/src/commands.rs                                    |
| Tauri command boundary, operation serialization, events      |
+------+--------------------------+-----------------------------+
       |                          |
+------v----------+     +---------v--------------------------+
| loader.rs       |     | operations.rs                      |
| catalog refresh |     | release selection, download,       |
| GitHub metadata |     | extract, preflight, multi-target    |
| cache activation|     | install/uninstall reporting         |
+------+----------+     +---------+--------------------------+
       |                          |
+------v----------+     +---------v--------------------------+
| catalog.rs      |     | package.rs                         |
| http.rs         |     | safe ZIP extraction + structure    |
| cache.rs        |     | manifest inspection                |
| snapshot.rs     |     +---------+--------------------------+
| release.rs      |               |
+------+----------+     +---------v--------------------------+
       |                | installer.rs                       |
       |                | plans, ownership, backups,         |
       |                | journals, rollback, recovery,      |
       |                | uninstall                          |
       |                +---------+--------------------------+
       |                          |
+------v--------------------------v--------------------------+
| store.rs  (LocalAppData records/settings)                  |
| apps.rs   (detection and path resolution)                   |
| manifest.rs, version.rs, util.rs, error.rs                  |
+-------------------------------------------------------------+
```

## Boundaries

- `catalog.rs`: parse/validate v1 catalog entries, keep one bad entry from hiding others.
- `http.rs`: HTTPS, timeouts, bounded retries, conditional requests, rate-limit errors, safe
  redirects, streamed progress/cancellation, and checksum sidecars.
- `loader.rs`: commit-pinned metadata, per-file identity reuse, complete snapshots, background
  refresh, stale-cache fallback and issue reporting.
- `snapshot.rs` / `cache.rs`: content-addressed file validation, immutable snapshot directories,
  atomic pointer activation, previous-snapshot fallback, and cache metadata.
- `release.rs`: release pagination data, semantic versions, stable/prerelease split, uploaded-ZIP
  selection, explicit version rules.
- `apps.rs`: Maya/ZBrush discovery and documented path conventions.
- `manifest.rs`: metadata and installation contract validation, target matching, substitutions.
- `package.rs`: archive extraction defenses, package-root detection, bundled manifest authority.
- `installer.rs`: all file modifications, transactions, ownership, managed blocks, update cleanup,
  crash recovery, and uninstall.
- `store.rs`: installed-state records and manual path settings. Cache data is a separate tree.
- `view.rs`: read-only DTO construction for the interface.
- `commands.rs`: thin command layer, cancellation registry, single operation at a time, progress and
  state events.

## Threading

Tauri commands are async. Network and cache refresh are asynchronous. File installation runs under
an operation lock so ownership checks and state commits are not interleaved. Individual target
records remain independent. Cancellation is delivered through an atomic token registered per job.

## Failure philosophy

- Prefer refusing to guess over applying ambiguous instructions.
- Never expose partial metadata, assets, or release instructions.
- Success in one application version never implies success in another.
- Never roll back a target that completed because a different target failed.
- Preserve modified user content and report it.
- Retain the last complete metadata snapshot until a replacement is fully validated.
