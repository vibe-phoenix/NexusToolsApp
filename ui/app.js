'use strict';

/*
 * NexusTools frontend.
 *
 * Content shown here comes from GitHub repositories. Markdown is treated as
 * text: it is escaped before a deliberately small formatter adds safe tags.
 * Remote images and raw HTML are never executed or loaded.
 */

const tauri = window.__TAURI__;
const invoke = tauri && tauri.core ? tauri.core.invoke.bind(tauri.core) : null;
const listen = tauri && tauri.event ? tauri.event.listen.bind(tauri.event) : null;

const APP_LABELS = { maya: 'Maya', zbrush: 'ZBrush', substance: 'Substance' };
const KEY_TIP = 'nexustools.hideUpdateTip';

const state = {
  data: null,
  section: 'Plugins',
  selected: null,
  tab: 0,
  query: '',
  software: 'All',
  onlyInstalled: false,
  onlyUpdates: false,
  chosen: new Set(),
  jobs: new Map(),
  refreshing: false,
  batchRunning: false,
  targetDialog: null,
  pending: null,
  toastTimer: null,
  hideTip: false,
  startupLoaded: false,
};

try {
  state.hideTip = localStorage.getItem(KEY_TIP) === '1';
} catch {
  state.hideTip = false;
}

const $ = (id) => document.getElementById(id);
const main = $('main');

function esc(value) {
  return String(value == null ? '' : value).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}

function appLabel(app) {
  const key = String(app || '').toLowerCase();
  return APP_LABELS[key] || (key ? key.charAt(0).toUpperCase() + key.slice(1) : 'Unknown');
}

function formatDate(value) {
  if (!value) return 'Unknown date';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return 'Unknown date';
  return date.toLocaleDateString('en-GB', { day: 'numeric', month: 'long', year: 'numeric' });
}

function formatDateTime(value) {
  if (!value) return 'not yet';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return 'not yet';
  return date.toLocaleString('en-GB', {
    day: '2-digit', month: 'short', year: 'numeric', hour: '2-digit', minute: '2-digit',
  });
}

function formatBytes(value) {
  const bytes = Number(value) || 0;
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB'];
  let amount = bytes;
  let unit = -1;
  do {
    amount /= 1024;
    unit += 1;
  } while (amount >= 1024 && unit < units.length - 1);
  return `${amount.toFixed(amount >= 10 ? 0 : 1)} ${units[unit]}`;
}

function newJobId() {
  if (window.crypto && typeof window.crypto.randomUUID === 'function') {
    return window.crypto.randomUUID().replace(/-/g, '');
  }
  return `job${Date.now()}${Math.random().toString(16).slice(2)}`;
}

function notify(message, timeout = 4800) {
  const toast = $('toast');
  toast.textContent = String(message);
  toast.hidden = false;
  clearTimeout(state.toastTimer);
  state.toastTimer = setTimeout(() => { toast.hidden = true; }, timeout);
}

function announce(message) {
  $('announcer').textContent = String(message);
}

function errorText(error) {
  if (!error) return 'Unknown error.';
  if (typeof error === 'string') return error;
  return error.message || error.code || 'Unknown error.';
}

function showError(error, title = 'NexusTools reported a problem') {
  const details = Array.isArray(error && error.details) ? error.details : [];
  const items = [{
    title,
    code: error && error.code ? error.code : 'error',
    message: errorText(error),
    details,
    source: error && error.retryable ? 'This may be temporary. Try again.' : 'No changes were made unless the message says otherwise.',
  }];
  showProblems('NexusTools problem', items);
}

function showProblems(title, items) {
  const body = $('problemsBody');
  body.innerHTML = (items || []).map((item) => `
    <article class="problem-item">
      <div class="problem-head">
        <div><h3>${esc(item.title || 'Problem')}</h3><p>${esc(item.message || '')}</p></div>
        <span class="problem-code">${esc(item.code || '')}</span>
      </div>
      ${Array.isArray(item.details) && item.details.length
        ? `<ul>${item.details.map((detail) => `<li>${esc(detail)}</li>`).join('')}</ul>` : ''}
      ${item.source ? `<div class="problem-source">${esc(item.source)}</div>` : ''}
    </article>
  `).join('') || '<div class="app-item-empty">No problems to report.</div>';
  $('problemsTitle').textContent = title;
  const dialog = $('problemsDialog');
  if (!dialog.open) dialog.showModal();
}

/* ---------------------------------------------------------------------------
 * Safe Markdown rendering
 * ------------------------------------------------------------------------- */

function safeInline(raw) {
  let value = esc(String(raw == null ? '' : raw));
  const codeTokens = [];
  value = value.replace(/`([^`\n]+)`/g, (_, code) => {
    const token = `\u0000${codeTokens.length}\u0000`;
    codeTokens.push(`<code>${code}</code>`);
    return token;
  });
  value = value.replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (_, alt) => `<span class="md-link">[image: ${alt}]</span>`);
  value = value.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, label, url) => {
    const clean = url.replace(/&amp;/g, '&');
    if (/^https?:\/\//i.test(clean)) {
      return `<span class="md-link" title="${esc(clean)}">${label} (${url})</span>`;
    }
    return label;
  });
  value = value.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  value = value.replace(/(^|[\s(])\*([^*\n]+)\*/g, '$1<em>$2</em>');
  value = value.replace(/\u0000(\d+)\u0000/g, (_, index) => codeTokens[Number(index)] || '');
  return value;
}

function renderMarkdown(source) {
  const raw = String(source == null ? '' : source).replace(/\r\n/g, '\n');
  if (!raw.trim()) return '<p class="muted">No content was provided.</p>';
  const lines = raw.split('\n');
  const output = [];
  let paragraph = [];
  let inFence = false;
  let fence = [];
  let listType = null;
  let list = [];

  const flushParagraph = () => {
    if (!paragraph.length) return;
    output.push(`<p>${safeInline(paragraph.join(' '))}</p>`);
    paragraph = [];
  };
  const flushList = () => {
    if (!list.length) { listType = null; return; }
    const tag = listType === 'ol' ? 'ol' : 'ul';
    output.push(`<${tag}>${list.map((item) => `<li>${safeInline(item)}</li>`).join('')}</${tag}>`);
    list = [];
    listType = null;
  };
  const flushFence = () => {
    output.push(`<pre><code>${esc(fence.join('\n'))}</code></pre>`);
    fence = [];
  };

  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (inFence) {
      if (/^\s*```/.test(line)) {
        inFence = false;
        flushFence();
      } else {
        fence.push(line);
      }
      index += 1;
      continue;
    }
    if (/^\s*```/.test(line)) {
      flushParagraph();
      flushList();
      inFence = true;
      index += 1;
      continue;
    }
    if (/^\s*$/.test(line)) {
      flushParagraph();
      flushList();
      index += 1;
      continue;
    }
    const heading = line.match(/^(#{1,4})\s+(.+)$/);
    if (heading) {
      flushParagraph();
      flushList();
      const level = heading[1].length;
      output.push(`<h${level}>${safeInline(heading[2])}</h${level}>`);
      index += 1;
      continue;
    }
    if (/^\s*([-*_])(?:\s*\1){2,}\s*$/.test(line)) {
      flushParagraph();
      flushList();
      output.push('<hr>');
      index += 1;
      continue;
    }
    const listItem = line.match(/^\s*(?:([-*+])|(\d+)[.)])\s+(.+)$/);
    if (listItem) {
      flushParagraph();
      const nextType = listItem[2] ? 'ol' : 'ul';
      if (listType && listType !== nextType) flushList();
      listType = nextType;
      list.push(listItem[3]);
      index += 1;
      continue;
    }
    if (/^\s*>\s?/.test(line)) {
      flushParagraph();
      flushList();
      const quote = [];
      while (index < lines.length && /^\s*>\s?/.test(lines[index])) {
        quote.push(lines[index].replace(/^\s*>\s?/, ''));
        index += 1;
      }
      output.push(`<blockquote>${safeInline(quote.join(' '))}</blockquote>`);
      continue;
    }
    if (/^\s*\|.*\|\s*$/.test(line)) {
      flushParagraph();
      flushList();
      output.push(`<p>${safeInline(line)}</p>`);
      index += 1;
      continue;
    }
    paragraph.push(line.trim());
    index += 1;
  }
  if (inFence) flushFence();
  flushParagraph();
  flushList();
  return output.join('\n');
}

/* ---------------------------------------------------------------------------
 * Data helpers
 * ------------------------------------------------------------------------- */

function allPlugins() {
  return (state.data && state.data.plugins) || [];
}

function pluginById(id) {
  return allPlugins().find((plugin) => plugin.id === id) || null;
}

function releaseById(plugin, id) {
  return (plugin.releases || []).find((release) => String(release.id) === String(id)) || null;
}

function recommendedRelease(plugin) {
  if (!plugin || plugin.recommendedReleaseId == null) return null;
  return releaseById(plugin, plugin.recommendedReleaseId);
}

function softwareOptions() {
  const values = new Map();
  for (const plugin of allPlugins()) {
    for (const item of plugin.applications || []) {
      values.set(item.app, appLabel(item.app));
    }
  }
  return [{ id: 'All', label: 'All' }, ...[...values.entries()]
    .sort((a, b) => a[1].localeCompare(b[1]))
    .map(([id, label]) => ({ id, label }))];
}

function normaliseAppVersion(value) {
  const parts = String(value || '').split('.').map((part) => Number.parseInt(part, 10));
  if (parts.some((part) => Number.isNaN(part))) return String(value || '').toLowerCase();
  while (parts.length > 1 && parts[parts.length - 1] === 0) parts.pop();
  return parts.join('.');
}

function looseVersionKey(value) {
  const text = String(value || '').trim().replace(/^v/i, '');
  const [core, pre] = text.split('+')[0].split('-', 2);
  const parts = core.split('.').map((part) => Number.parseInt(part, 10));
  if (parts.some((part) => Number.isNaN(part))) return text.toLowerCase();
  while (parts.length > 1 && parts[parts.length - 1] === 0) parts.pop();
  return `${parts.join('.')}${pre ? `-${pre.toLowerCase()}` : ''}`;
}

function appVersionMatches(required, detected) {
  return normaliseAppVersion(required) === normaliseAppVersion(detected);
}

function pluginSupportsApplication(plugin, app, strictVersion) {
  const applications = plugin.applications || [];
  if (!applications.length) return true;
  return applications.some((entry) => {
    if (entry.app !== app.app) return false;
    if (!strictVersion) return true;
    return (entry.supportedVersions || []).some((version) => appVersionMatches(version, app.version));
  });
}

function compatibleApps(plugin, strictVersion = true) {
  const apps = (state.data && state.data.apps) || [];
  return apps.filter((app) => pluginSupportsApplication(plugin, app, strictVersion));
}

function installedTargetMap(plugin) {
  return new Map((plugin.installedTargets || []).map((target) => [target.targetId, target]));
}

function supportedSummary(plugin) {
  const groups = new Map();
  for (const entry of plugin.applications || []) {
    if (!groups.has(entry.app)) groups.set(entry.app, new Set());
    for (const version of entry.supportedVersions || []) groups.get(entry.app).add(version);
  }
  if (!groups.size) return 'Not declared in plugin.json';
  return [...groups.entries()]
    .map(([app, versions]) => `${appLabel(app)} ${[...versions].join(', ')}`)
    .join(' · ');
}

function statusSummary(plugin) {
  if (!plugin.installedTargets || !plugin.installedTargets.length) return 'Not installed';
  return plugin.installedTargets.map((target) => `${target.appName} ${target.appVersion} · ${target.pluginVersion}`).join('; ');
}

function visiblePlugins() {
  const query = state.query.toLowerCase();
  return allPlugins().filter((plugin) => {
    if (state.onlyInstalled && !plugin.installed) return false;
    if (state.onlyUpdates && !(plugin.updateAvailable && !state.jobs.has(plugin.id))) return false;
    if (state.software !== 'All' && !(plugin.applications || []).some((app) => app.app === state.software)) return false;
    const haystack = `${plugin.name} ${plugin.description} ${(plugin.applications || []).map((a) => appLabel(a.app)).join(' ')}`.toLowerCase();
    return haystack.includes(query);
  });
}

function catalogNotice() {
  const catalog = state.data && state.data.catalog;
  const issues = ((state.data && state.data.issues) || []).length;
  const recovery = ((state.data && state.data.recoveryWarnings) || []).length;
  let copy = '';
  if (state.refreshing) {
    copy = '<span>Refreshing plugin catalog… Cached plugins remain available.</span>';
  } else if (catalog && catalog.error) {
    copy = `<span>Catalog refresh failed. Showing the last complete cached data. ${esc(catalog.error.message)}</span>`;
  } else if (catalog && catalog.cachedPluginCount > 0) {
    copy = `<span>${catalog.cachedPluginCount} plugin${catalog.cachedPluginCount === 1 ? ' is' : 's are'} shown from cached metadata. Online refresh is ${catalog.lastCheckedAt ? 'needed' : 'pending'}.</span>`;
  } else if (catalog) {
    const checked = catalog.lastCheckedAt ? `Last checked ${formatDateTime(catalog.lastCheckedAt)}` : 'Not checked online yet';
    copy = `<span>${checked} · ${catalog.pluginCount} plugin${catalog.pluginCount === 1 ? '' : 's'} available · Catalog updates automatically.</span>`;
  } else {
    copy = '<span>Loading the plugin catalog…</span>';
  }
  const refreshDisabled = state.refreshing || [...state.jobs.keys()].length > 0;
  const actions = [
    `<button type="button" data-refresh ${refreshDisabled ? 'disabled' : ''}>${state.refreshing ? 'Refreshing…' : 'Refresh'}</button>`,
    issues || recovery || (catalog && catalog.error)
      ? `<button type="button" data-problems>Problems${issues ? ` (${issues})` : ''}</button>`
      : '',
  ].join('');
  return `<div class="notice"><div class="notice-copy">${copy}</div><div class="notice-actions">${actions}</div></div>`;
}

function jobProgress(pluginId) {
  const job = state.jobs.get(pluginId);
  if (!job) return '';
  const phase = job.phase ? job.phase.replace(/-/g, ' ') : 'preparing';
  return `
    <div class="progress" data-job-progress="${esc(pluginId)}">
      <div class="progresslabel">${esc(phase)} · <span data-progress-label>${Number(job.percent) || 0}%</span> · <span data-progress-message>${esc(job.message || '')}</span></div>
      <progress data-progress="${esc(pluginId)}" value="${Number(job.percent) || 0}" max="100" aria-label="Installation progress"></progress>
      <div class="progress-actions">
        <button type="button" data-cancel-job="${esc(job.jobId)}" ${job.cancelRequested ? 'disabled' : ''}>${job.cancelRequested ? 'Cancelling…' : 'Cancel'}</button>
      </div>
    </div>`;
}

/* ---------------------------------------------------------------------------
 * List and detail rendering
 * ------------------------------------------------------------------------- */

function render() {
  if (!state.data) return;
  if (state.section !== 'Plugins') state.section = 'Plugins';
  document.querySelectorAll('[data-nav]').forEach((button) => {
    button.classList.toggle('active', button.dataset.nav === state.section);
    if (button.dataset.nav === state.section) button.setAttribute('aria-current', 'page');
    else button.removeAttribute('aria-current');
  });
  main.classList.toggle('pluginlisting', !state.selected);
  if (state.selected) {
    const plugin = pluginById(state.selected);
    if (plugin) {
      renderDetail(plugin);
      return;
    }
    state.selected = null;
  }
  renderList();
}

function renderList() {
  const options = softwareOptions();
  if (!options.some((option) => option.id === state.software)) state.software = 'All';
  const list = visiblePlugins();
  const updates = allPlugins().filter((plugin) => plugin.updateAvailable && !state.jobs.has(plugin.id));
  const selectedUpdates = updates.filter((plugin) => state.chosen.has(plugin.id));
  const activeOption = options.find((option) => option.id === state.software) || options[0];

  const filters = `<div class="filters">
    <div class="softwarelabel"><span id="softwareLabel">Software</span>
      <details class="softwaremenu" id="softwareMenu">
        <summary aria-labelledby="softwareLabel softwareValue"><span id="softwareValue">${esc(activeOption.label)}</span><span class="dropdownchevron" aria-hidden="true"></span></summary>
        <div class="softwareoptions" role="group" aria-label="Software options">
          ${options.map((option) => `<button type="button" data-software="${esc(option.id)}" aria-pressed="${option.id === state.software}">${esc(option.label)}<span aria-hidden="true">${option.id === state.software ? '✓' : ''}</span></button>`).join('')}
        </div>
      </details>
    </div>
    <button type="button" data-filter="installed" aria-pressed="${state.onlyInstalled}">Installed</button>
    <button type="button" data-filter="updates" aria-pressed="${state.onlyUpdates}">Updates</button>
  </div>`;

  const updatebar = state.onlyUpdates ? `<div class="updatebar">
      <p>${updates.length ? 'Select plugins by clicking their image or text, or update every installed plugin with an available update.' : 'All installed plugins are up to date.'}</p>
      <button class="primary" id="updateAll" ${updates.length && !state.batchRunning ? '' : 'disabled'}>${state.batchRunning ? 'Updating…' : 'Update all'}</button>
      <button class="primary" id="updateSelected" ${selectedUpdates.length && !state.batchRunning ? '' : 'disabled'}>Update selected${selectedUpdates.length ? ` (${selectedUpdates.length})` : ''}</button>
    </div>` : '';

  let content = '';
  if (list.length) {
    content = `<div class="grid">${list.map((plugin) => listCard(plugin)).join('')}</div>`;
  } else {
    const hasAny = allPlugins().length > 0;
    const issueCount = ((state.data && state.data.issues) || []).length;
    const catalogError = state.data && state.data.catalog && state.data.catalog.error;
    let title;
    let description;
    if (state.query || state.software !== 'All') {
      title = 'No matching plugins';
      description = 'Try another software filter or search term.';
    } else if (state.onlyUpdates) {
      title = 'No updates available';
      description = 'Your installed plugins are up to date.';
    } else if (state.onlyInstalled) {
      title = 'No installed plugins';
      description = hasAny ? 'Install a plugin to see it here.' : 'No plugins are available yet.';
    } else if (state.refreshing) {
      title = 'Loading the plugin catalog';
      description = 'Cached plugins remain available while the online catalog is checked.';
    } else if (catalogError) {
      title = 'The catalog could not be refreshed';
      description = errorText(catalogError);
    } else if (issueCount > 0) {
      title = 'Plugins could not be loaded';
      description = 'The catalog was reached, but no plugin passed validation. Open Problems to see the actionable error.';
    } else {
      title = 'No NexusTool plugins are available yet';
      description = 'The catalog returned no plugin entries. Use Refresh to try again, or review Problems for details.';
    }
    content = `<div class="empty"><h2>${esc(title)}</h2><p>${esc(description)}</p><div class="empty-actions">${state.query || state.software !== 'All' ? '<button type="button" data-empty>Show all plugins</button>' : ''}${state.refreshing ? '' : '<button type="button" data-refresh class="quiet">Refresh catalog</button>'}${issueCount || catalogError ? '<button type="button" data-problems class="quiet">View problems</button>' : ''}</div></div>`;
  }

  main.innerHTML = `
    <div class="pagehead pluginhead">
      <h1 tabindex="-1" id="pageTitle">Plugins</h1>
      ${filters}
    </div>
    ${updatebar}
    ${content}
    ${catalogNotice()}
  `;
}

function listCard(plugin) {
  const job = state.jobs.get(plugin.id);
  const busy = Boolean(job);
  const cached = plugin.cachedOnline ? '<span class="badge cached">Cached</span>' : '';
  const statusBadge = plugin.installed
    ? `<span class="updatebanner ${plugin.updateAvailable ? '' : 'installedbanner'}">${plugin.updateAvailable ? 'Update available' : 'Installed'}</span>`
    : '';
  const selection = state.onlyUpdates
    ? `<button class="preview" type="button" data-pick="${esc(plugin.id)}" aria-pressed="${state.chosen.has(plugin.id)}" aria-label="Select ${esc(plugin.name)} for update">
         <img src="${plugin.bannerDataUrl}" alt="${esc(plugin.name)} banner" width="400" height="300">
         ${plugin.installed ? `<span class="updatebanner ${plugin.updateAvailable ? '' : 'installedbanner'}">${plugin.updateAvailable ? 'Update available' : 'Installed'}</span>` : ''}
         <span class="selectionmark">${state.chosen.has(plugin.id) ? 'Selected' : 'Click to select'}</span>
       </button>`
    : `<button class="preview" type="button" data-open="${esc(plugin.id)}" aria-label="View ${esc(plugin.name)} details">
         <img src="${plugin.bannerDataUrl}" alt="${esc(plugin.name)} banner" width="400" height="300">
         ${statusBadge}
       </button>`;
  return `<article class="${state.onlyUpdates && state.chosen.has(plugin.id) ? 'chosen' : ''}" ${state.onlyUpdates && !busy ? `data-select-plugin="${esc(plugin.id)}"` : ''}>
    ${selection}
    <h2>${esc(plugin.name)}</h2>
    <p class="carddesc">${esc(plugin.description)}</p>
    ${cached ? `<div class="cardbottom">${cached}</div>` : ''}
    ${jobProgress(plugin.id)}
  </article>`;
}

function renderDetail(plugin) {
  const recommended = recommendedRelease(plugin);
  const job = state.jobs.get(plugin.id);
  const busy = Boolean(job);
  const supportedTargets = compatibleApps(plugin, true);
  const installedMap = installedTargetMap(plugin);
  const allCompatibleInstalledAndCurrent = supportedTargets.length > 0
    && supportedTargets.every((app) => installedMap.get(app.targetId) && installedMap.get(app.targetId).current);
  const someCompatibleUninstalled = supportedTargets.some((app) => !installedMap.has(app.targetId));
  const hasInstalledRecords = (plugin.installedTargets || []).length > 0;
  const noCompatibleTargets = supportedTargets.length === 0;

  let primaryLabel = 'Install plugin';
  let primaryDisabled = false;
  let primaryAction = 'install';
  let primaryReason = '';
  if (busy) {
    primaryLabel = 'Installing…';
    primaryDisabled = true;
    primaryReason = job.message || '';
  } else if (state.batchRunning) {
    primaryDisabled = true;
    primaryReason = 'A batch update is running.';
  } else if (plugin.updateAvailable) {
    primaryLabel = recommended ? `Update to ${plugin.version}` : 'Update unavailable';
    primaryDisabled = !recommended || noCompatibleTargets;
    primaryAction = 'update';
  } else if (hasInstalledRecords && allCompatibleInstalledAndCurrent) {
    primaryLabel = '✓ Installed';
    primaryDisabled = true;
    primaryAction = 'none';
  } else if (hasInstalledRecords && someCompatibleUninstalled) {
    primaryLabel = 'Install on another installation';
    primaryDisabled = !recommended;
    primaryAction = 'another';
  } else if (hasInstalledRecords) {
    primaryLabel = '✓ Installed';
    primaryDisabled = true;
    primaryAction = 'none';
  } else {
    primaryLabel = 'Install plugin';
    primaryDisabled = !recommended || noCompatibleTargets;
  }
  if (noCompatibleTargets && !hasInstalledRecords) {
    primaryLabel = 'No compatible application detected';
    primaryDisabled = true;
    primaryReason = 'Set or correct an application folder in Settings, then refresh applications.';
  } else if (plugin.installUnavailable && !plugin.installed) {
    primaryDisabled = true;
    primaryReason = plugin.installUnavailable;
  } else if (!recommended && (plugin.updateAvailable || primaryAction === 'another')) {
    primaryReason = plugin.installUnavailable || 'No installable release matches the current catalog version.';
  }

  const installedList = (plugin.installedTargets || []).map((target) => `
    <div class="installed-target">
      <span class="target-name">${esc(target.appName)} ${esc(target.appVersion)}</span>
      <span>v${esc(target.pluginVersion)}</span>
      ${target.current ? '<span class="badge latest">Current</span>' : '<span class="badge update">Update available</span>'}
      ${target.status === 'partial-uninstall' ? '<span class="badge prerelease">Partial</span>' : ''}
      <span class="target-actions"><button type="button" data-uninstall-target="${esc(plugin.id)}" data-target-id="${esc(target.targetId)}">Uninstall target</button></span>
    </div>`).join('');

  const warnings = [
    ...(plugin.warnings || []),
    ...(plugin.cachedOnline ? ['Showing the last complete cached snapshot; online refresh has not succeeded.'] : []),
    ...(plugin.releaseError && !(plugin.warnings || []).includes(plugin.releaseError) ? [plugin.releaseError] : []),
  ].filter((warning, index, all) => warning && all.indexOf(warning) === index);

  const installButton = `<button class="primary" data-install="${esc(plugin.id)}" data-mode="${primaryAction}" ${primaryDisabled ? 'disabled' : ''}>${esc(primaryLabel)}</button>`;
  const statusText = statusSummary(plugin);
  const tabNames = ['Features', 'Read me', 'Past versions'];
  const tabs = `<div class="tabs" role="tablist" aria-label="Plugin information">
    ${tabNames.map((name, index) => `<button class="tab" role="tab" id="tab${index}" aria-controls="panel" aria-selected="${index === state.tab}" tabindex="${index === state.tab ? 0 : -1}" data-tab="${index}">${name}</button>`).join('')}
  </div>
  <div class="tabcontent" id="panel" role="tabpanel" aria-labelledby="tab${state.tab}" tabindex="0">${renderTab(plugin, state.tab)}</div>`;

  main.innerHTML = `
    <button class="quiet back" type="button" id="back">← Back to plugins</button>
    <div class="details">
      <section class="overview" aria-label="Plugin overview">
        <div class="preview"><img src="${plugin.bannerDataUrl}" alt="${esc(plugin.name)} banner" width="400" height="300"></div>
        <h1 style="margin-top:24px" tabindex="-1" id="pageTitle">${esc(plugin.name)}</h1>
        <p>${esc(plugin.description)}</p>
        <p style="font-size:13px">${esc(plugin.overview)}</p>
        <dl>
          <dt>Supported application</dt><dd>${esc(supportedSummary(plugin))}</dd>
          <dt>Latest version</dt><dd>${esc(plugin.version)}${plugin.cachedOnline ? ' <span class="badge cached">Cached</span>' : ''}</dd>
          <dt>Installation status</dt><dd>${esc(statusText)}</dd>
        </dl>
        ${installButton}
        ${primaryReason ? `<p class="demolabel" style="margin-top:12px">${esc(primaryReason)}</p>` : ''}
        ${jobProgress(plugin.id)}
        ${installedList ? `<div class="installstatus">${installedList}${plugin.installedTargets.length > 1 ? '<button class="quiet" type="button" data-uninstall="' + esc(plugin.id) + '">Uninstall all targets</button>' : ''}</div>` : ''}
        <p class="demolabel" style="margin-top:12px">Packages are downloaded only when you install a version. NexusTools verifies the release package before writing files.</p>
        ${warnings.map((warning) => `<div class="warningline">${esc(warning)}</div>`).join('')}
      </section>
      <section aria-label="Plugin information">${tabs}</section>
    </div>
    ${catalogNotice()}
  `;
}

function renderTab(plugin, tab) {
  if (tab === 0) {
    return `<p class="demolabel">Rendered from the plugin's FEATURES.md. Untrusted content is shown as text only.</p><div class="md">${renderMarkdown(plugin.featuresMarkdown)}</div>`;
  }
  if (tab === 1) {
    return `<p class="demolabel">Rendered from the plugin's README.md. Untrusted content is shown as text only.</p><div class="md">${renderMarkdown(plugin.readmeMarkdown)}</div>`;
  }
  if (!(plugin.releases || []).length) {
    return `<div class="empty" style="margin-top:0"><h2>No published releases</h2><p>${esc(plugin.releaseError || 'This plugin repository has no releases to install.')}</p></div>`;
  }
  const stable = plugin.releases.filter((release) => !release.prerelease);
  const pre = plugin.releases.filter((release) => release.prerelease);
  let html = '<p class="demolabel">Versions come from published GitHub Releases. Prereleases are listed separately and are never selected automatically.</p>';
  if (stable.length) {
    html += `<div class="relgroup">Stable releases</div>${stable.map((release) => releaseCard(plugin, release)).join('')}`;
  } else {
    html += '<div class="relgroup">Stable releases</div><p class="muted">No stable releases are published.</p>';
  }
  if (pre.length) {
    html += `<div class="relgroup">Prereleases</div>${pre.map((release) => releaseCard(plugin, release)).join('')}`;
  }
  return html;
}

function releaseCard(plugin, release) {
  const currentOn = (plugin.installedTargets || []).filter(
    (target) => looseVersionKey(target.pluginVersion) === looseVersionKey(release.version),
  );
  const busy = state.jobs.has(plugin.id) || state.batchRunning;
  const disabled = busy || Boolean(release.installError) || !release.version;
  const installedBadge = currentOn.length
    ? `<span class="badge">Installed on ${currentOn.map((target) => `${esc(target.appName)} ${esc(target.appVersion)}`).join(', ')}</span>`
    : '';
  const packageLine = release.package
    ? `<p class="packageinfo">${esc(release.package.name)} · ${formatBytes(release.package.size)}${release.package.sha256 ? ` · SHA-256 ${esc(release.package.sha256.slice(0, 16))}…` : ''}</p>`
    : '';
  return `<article class="release">
    <div class="releasehead">
      <h3>${esc(release.name || `Version ${release.version || release.tag}`)}</h3>
      ${release.recommended ? '<span class="badge latest">Latest</span>' : ''}
      ${release.prerelease ? '<span class="badge prerelease">Prerelease</span>' : ''}
      ${!release.matchesManifestVersion ? '<span class="badge update">Not the catalog version</span>' : ''}
      ${installedBadge}
    </div>
    <div class="releasemeta"><time datetime="${esc(release.publishedAt || '')}">${formatDate(release.publishedAt)}</time><span>Tag ${esc(release.tag)}</span></div>
    <div class="md">${renderMarkdown(release.notes || 'No release notes were provided.')}</div>
    ${packageLine}
    ${release.installError ? `<p class="releaseerror">${esc(release.installError)}</p>` : ''}
    <div class="releaseactions">
      <button type="button" data-install-release="${esc(plugin.id)}" data-release-id="${esc(release.id)}" ${disabled ? 'disabled' : ''}>Install this version</button>
      ${release.prerelease ? '<span class="muted">Explicit prerelease selection</span>' : ''}
    </div>
  </article>`;
}

/* ---------------------------------------------------------------------------
 * Dialogs and installation flow
 * ------------------------------------------------------------------------- */

function ask(title, text, action, label) {
  $('confirmTitle').textContent = title;
  $('confirmText').textContent = text;
  $('acceptConfirm').textContent = label || 'Continue';
  state.pending = action;
  const dialog = $('confirmDialog');
  if (!dialog.open) dialog.showModal();
}

function showUpdateTip() {
  if (state.hideTip) return;
  const dialog = $('updateTip');
  if (!dialog.open) dialog.showModal();
}

function targetCandidates(plugin, release) {
  const strict = Boolean(release && release.matchesManifestVersion && !release.installError);
  const apps = compatibleApps(plugin, strict);
  return apps.map((app) => ({ app, strict }));
}

function openTargetDialog(plugin, release, options = {}) {
  const mode = options.mode || 'install';
  const candidates = targetCandidates(plugin, release);
  const installed = installedTargetMap(plugin);
  const defaults = new Set();
  if (Array.isArray(options.defaultTargetIds)) {
    options.defaultTargetIds.forEach((id) => defaults.add(String(id)));
  } else if (mode === 'update') {
    plugin.installedTargets.forEach((target) => { if (!target.current) defaults.add(target.targetId); });
  } else if (mode === 'version') {
    const installedTargets = plugin.installedTargets || [];
    if (installedTargets.length) installedTargets.forEach((target) => defaults.add(target.targetId));
    else candidates.forEach(({ app }) => defaults.add(app.targetId));
  } else if (mode === 'another') {
    candidates.forEach(({ app }) => {
      const record = installed.get(app.targetId);
      if (!record || !record.current) defaults.add(app.targetId);
    });
  } else {
    candidates.forEach(({ app }) => {
      const record = installed.get(app.targetId);
      if (!record || !record.current) defaults.add(app.targetId);
    });
  }
  if (!defaults.size) candidates.forEach(({ app }) => {
    const record = installed.get(app.targetId);
    if (!record) defaults.add(app.targetId);
  });
  state.targetDialog = {
    pluginId: plugin.id,
    releaseId: release.id,
    mode,
  };
  $('targetTitle').textContent = mode === 'update' ? 'Choose installations to update' : 'Choose installations';
  $('targetText').textContent = `Install ${plugin.name} ${release.version || ''} into the selected application installations. Each target is installed independently.${mode === 'version' ? ' Compatibility is verified against the selected release package.' : ''}`;
  const choices = $('targetChoices');
  if (!candidates.length) {
    choices.innerHTML = `<div class="target-empty">No compatible Maya or ZBrush installation was detected. Use Settings → Add or correct an application path, then refresh applications.</div>`;
  } else {
    choices.innerHTML = candidates.map(({ app, strict }) => {
      const record = installed.get(app.targetId);
      const supported = strict
        ? 'Declared compatible with this plugin version.'
        : 'Compatibility will be verified from the selected release package before installation.';
      const issues = (app.issues || []).map((issue) => `<div class="target-issues">${esc(issue)}</div>`).join('');
      const checked = defaults.has(app.targetId);
      return `<label class="target-choice ${checked ? 'selected' : ''} ${record ? 'installed' : ''}" data-target-choice="${esc(app.targetId)}">
        <input type="checkbox" data-target-checkbox="${esc(app.targetId)}" ${checked ? 'checked' : ''}>
        <span class="target-copy">
          <span class="target-title">${esc(app.appName)} ${esc(app.version)}${record ? ` · installed ${esc(record.pluginVersion)}` : ''}</span>
          <span class="target-meta">${esc(app.installRoot)}<br>${esc(supported)}${app.pathNotes && app.pathNotes.length ? `<br>${esc(app.pathNotes.join(' '))}` : ''}</span>
          ${issues}
        </span>
      </label>`;
    }).join('');
  }
  $('confirmTargets').textContent = mode === 'update' ? 'Update selected' : 'Install selected';
  $('confirmTargets').disabled = !candidates.length;
  $('cancelTargets').textContent = 'Cancel';
  const dialog = $('targetDialog');
  if (!dialog.open) dialog.showModal();
  updateTargetChoiceStyles();
}

function updateTargetChoiceStyles() {
  document.querySelectorAll('[data-target-choice]').forEach((choice) => {
    const checkbox = choice.querySelector('input[type=checkbox]');
    choice.classList.toggle('selected', Boolean(checkbox && checkbox.checked));
  });
}

async function openInstallApproval(plugin, release, targetIds, details) {
  const text = `The selected release needs to write outside NexusTools's managed application folders.\n\n${details.join('\n')}\n\nApprove these exact locations and install?`;
  ask('Approve external destinations?', text, async () => {
    await installWithApproval(plugin, release, targetIds, details, true);
  }, 'Approve and install');
}

async function installWithApproval(plugin, release, targetIds, approved, allowPrompt = true) {
  try {
    const report = await runInstall(plugin, release, targetIds, approved, { interactive: allowPrompt });
    await afterInstall(plugin, report);
    return report;
  } catch (error) {
    if (error && error.code === 'outside_destination_roots' && allowPrompt) {
      await openInstallApproval(plugin, release, targetIds, Array.isArray(error.details) ? error.details : []);
      return null;
    }
    if (error && error.code === 'cancelled') {
      notify('Installation was cancelled. Any partial target changes were rolled back.');
      return null;
    }
    throw error;
  }
}

async function runInstall(plugin, release, targetIds, approved, options = {}) {
  const jobId = newJobId();
  state.jobs.set(plugin.id, {
    jobId,
    percent: 0,
    phase: 'preparing',
    message: 'Preparing installation',
    cancelRequested: false,
    replacing: (plugin.installedTargets || []).some((target) => targetIds.includes(target.targetId)),
  });
  render();
  try {
    const report = await invoke('install_plugin', {
      request: {
        pluginId: plugin.id,
        version: release.version || plugin.version,
        releaseId: release.id,
        explicitPrerelease: Boolean(release.prerelease),
        targetIds,
        approvedOutsideDestinations: approved,
        jobId,
      },
    });
    state.jobs.delete(plugin.id);
    render();
    return report;
  } catch (error) {
    state.jobs.delete(plugin.id);
    render();
    if (options.interactive !== false && error && error.code !== 'outside_destination_roots' && error.code !== 'cancelled') {
      showError(error, `Could not install ${plugin.name}`);
    }
    throw error;
  }
}

async function afterInstall(plugin, report) {
  if (!report) return;
  try {
    await reloadBootstrap();
  } catch (error) {
    showError(error, 'Installation finished but the local view could not refresh');
  }
  if (report.allSucceeded) {
    const parts = report.targets.filter((target) => target.success)
      .map((target) => `${target.app} ${target.appVersion}`);
    notify(`${plugin.name} ${report.version} installed on ${parts.join(', ')}.${report.checksumVerified ? ' Package checksum verified.' : ''}`);
    announce(`${plugin.name} installation complete.`);
  } else {
    const failures = report.targets.filter((target) => !target.success);
    showProblems('Installation finished with problems', [
      ...failures.map((target) => ({
        title: `${target.app || 'Application'} ${target.appVersion || ''}`.trim() || target.targetId,
        code: target.error ? target.error.code : 'target_failed',
        message: target.error ? errorText(target.error) : 'This target was not installed.',
        details: target.error && target.error.details ? target.error.details : target.warnings || [],
        source: 'Successful targets were kept; failed targets were rolled back independently.',
      })),
      ...report.targets.filter((target) => target.success && target.warnings && target.warnings.length).map((target) => ({
        title: `${target.app} ${target.appVersion} installed with notes`,
        code: 'warning',
        message: target.warnings.join(' '),
        details: [],
        source: 'The plugin is installed for this target.',
      })),
    ]);
    notify('Installation finished with problems. Review the details.');
  }
}

function maybeConfirmReplacement(plugin, release, targetIds) {
  const installedMap = installedTargetMap(plugin);
  const replacing = targetIds
    .map((id) => installedMap.get(id))
    .filter(Boolean);
  if (!replacing.length) {
    installWithApproval(plugin, release, targetIds, []).catch((error) => showError(error));
    return;
  }
  const lines = replacing.map((target) => `${target.appName} ${target.appVersion}: ${target.pluginVersion} → ${release.version || plugin.version}`);
  ask('Replace installed version?', `${lines.join('\n')}\n\nEach selected installation is backed up and updated independently.`, () => {
    installWithApproval(plugin, release, targetIds, []).catch((error) => showError(error));
  }, 'Install');
}

function startMainInstall(plugin, mode) {
  const release = recommendedRelease(plugin);
  if (!release) {
    notify(plugin.installUnavailable || 'No installable release matches the current catalog version.');
    return;
  }
  openTargetDialog(plugin, release, { mode });
}

function startVersionInstall(plugin, releaseId) {
  const release = releaseById(plugin, releaseId);
  if (!release) return;
  if (release.installError) {
    notify(release.installError);
    return;
  }
  openTargetDialog(plugin, release, { mode: 'version' });
}

function confirmTargetSelection() {
  const info = state.targetDialog;
  if (!info) return;
  const plugin = pluginById(info.pluginId);
  const release = plugin ? releaseById(plugin, info.releaseId) : null;
  if (!plugin || !release) {
    $('targetDialog').close();
    state.targetDialog = null;
    return;
  }
  const ids = [...document.querySelectorAll('[data-target-checkbox]:checked')]
    .map((checkbox) => checkbox.dataset.targetCheckbox);
  if (!ids.length) {
    notify('Select at least one application installation.');
    return;
  }
  $('targetDialog').close();
  state.targetDialog = null;
  maybeConfirmReplacement(plugin, release, ids);
}

async function confirmUninstall(pluginId, targetIds, label) {
  const plugin = pluginById(pluginId)
    || ((state.data.installedPlugins || []).find((item) => item.id === pluginId));
  if (!plugin) return;
  const only = targetIds.length ? ` ${targetIds.length} selected installation${targetIds.length === 1 ? '' : 's'}` : ' all installed applications';
  ask('Uninstall plugin?', `Remove ${plugin.name || pluginId} from${only}.\n\nNexusTools removes only files and managed blocks recorded as plugin-owned. Modified files, user data, and unrelated content are preserved.`, async () => {
    try {
      const report = await invoke('uninstall_plugin', { request: { pluginId, targetIds } });
      await reloadBootstrap();
      if (report.allRemoved) {
        notify(`${plugin.name || pluginId} was uninstalled.`);
      } else {
        showProblems('Uninstall finished with notes', report.targets.map((target) => ({
          title: `${target.app || 'Target'} ${target.appVersion || ''}`.trim() || target.targetId,
          code: target.error ? target.error.code : (target.skipped.length ? 'preserved' : 'incomplete'),
          message: target.error ? errorText(target.error) : (target.fullyRemoved ? 'Removed.' : 'Some files or managed blocks were preserved.'),
          details: target.skipped,
          source: target.success ? `${target.removedFiles} files and ${target.removedBlocks} managed blocks removed.` : '',
        })));
        notify('Uninstall finished with notes. Review the details.');
      }
    } catch (error) {
      showError(error, 'Could not uninstall plugin');
    }
  }, label || 'Uninstall');
}

function batchUpdates(all) {
  if (state.batchRunning) return;
  const updates = allPlugins().filter((plugin) => plugin.updateAvailable && !state.jobs.has(plugin.id) && (all || state.chosen.has(plugin.id)));
  if (!updates.length) {
    notify('No plugins are available to update.');
    return;
  }
  const lines = updates.map((plugin) => {
    const versions = [...new Set((plugin.installedTargets || []).filter((target) => !target.current).map((target) => target.pluginVersion))];
    return `${plugin.name}: ${versions.join(', ') || 'installed'} → ${plugin.version}`;
  });
  ask(`Update ${updates.length} plugin${updates.length === 1 ? '' : 's'}?`, `${lines.join('\n')}\n\nOnly locally recorded targets with an available update are updated.`, async () => {
    state.batchRunning = true;
    render();
    const failures = [];
    for (const plugin of updates) {
      const release = recommendedRelease(plugin);
      const targetIds = (plugin.installedTargets || [])
        .filter((target) => !target.current)
        .map((target) => target.targetId);
      if (!release || !targetIds.length) {
        failures.push({ title: plugin.name, code: 'update_unavailable', message: release ? 'No outdated local target was found.' : (plugin.installUnavailable || 'No installable release matches this version.'), details: [], source: '' });
        continue;
      }
      try {
        const report = await runInstall(plugin, release, targetIds, [], { interactive: false });
        for (const target of report.targets.filter((item) => !item.success)) {
          failures.push({
            title: `${plugin.name} · ${target.app || 'Target'} ${target.appVersion || ''}`.trim(),
            code: target.error ? target.error.code : 'target_failed',
            message: target.error ? errorText(target.error) : 'This target was not updated.',
            details: target.error && target.error.details ? target.error.details : [],
            source: 'Other plugin updates continue independently.',
          });
        }
      } catch (error) {
        failures.push({
          title: plugin.name,
          code: error && error.code ? error.code : 'update_failed',
          message: errorText(error),
          details: error && error.details ? error.details : [],
          source: 'Other plugin updates continue independently.',
        });
      }
      render();
    }
    state.batchRunning = false;
    try {
      await reloadBootstrap();
    } catch (error) {
      showError(error, 'Could not refresh the local plugin state');
    }
    if (failures.length) {
      showProblems('Some plugin updates failed', failures);
      notify('Updates finished with problems.');
    } else {
      state.chosen.clear();
      notify(`${updates.length} plugin${updates.length === 1 ? '' : 's'} updated.`);
    }
    render();
  }, 'Update plugins');
}

/* ---------------------------------------------------------------------------
 * Settings
 * ------------------------------------------------------------------------- */

function openSettings() {
  if (!state.data) return;
  renderSettings();
  const dialog = $('settingsDialog');
  if (!dialog.open) dialog.showModal();
}

function renderSettings() {
  if (!state.data) return;
  const catalog = state.data.catalog || {};
  const appOverrides = (state.data.settings && state.data.settings.appOverrides) || [];
  let catalogText = catalog.catalogUrl ? `${catalog.catalogUrl}` : 'Catalog URL unavailable';
  catalogText += `<br>${catalog.lastCheckedAt ? `Last checked ${formatDateTime(catalog.lastCheckedAt)}` : 'No online check has finished yet.'}`;
  catalogText += ` · ${catalog.pluginCount || 0} plugin${catalog.pluginCount === 1 ? '' : 's'} loaded`;
  if (catalog.usingCached) catalogText += ' · some information is cached';
  if (catalog.error) catalogText += `<br><span style="color:#d1b26c">${esc(catalog.error.message)}</span>`;
  $('catalogInfo').innerHTML = catalogText;
  $('storageInfo').innerHTML = `Data: ${esc(state.data.dataRoot || '')}<br>Metadata cache: ${esc(state.data.cacheRoot || '')}<br>NexusTools ${esc(state.data.version || '')}`;
  $('refreshCatalog').disabled = state.refreshing || state.jobs.size > 0;
  $('clearCache').disabled = state.refreshing || state.jobs.size > 0;
  $('refreshApps').disabled = state.refreshing || state.jobs.size > 0;

  const apps = state.data.apps || [];
  $('detectedInfo').textContent = apps.length
    ? `${apps.length} application installation${apps.length === 1 ? '' : 's'} detected.`
    : 'No Maya or ZBrush installations have been detected yet.';
  const appsContainer = $('detectedApps');
  if (!apps.length) {
    appsContainer.innerHTML = '<div class="app-item-empty">No applications detected. Add an installation folder below if one is installed in a custom location.</div>';
  } else {
    appsContainer.innerHTML = apps.map((app) => {
      const paths = [];
      if (app.paths.userScripts) paths.push(`Scripts: ${app.paths.userScripts}`);
      if (app.paths.userModules) paths.push(`Modules: ${app.paths.userModules}`);
      if (app.paths.userPrefs) paths.push(`Preferences: ${app.paths.userPrefs}`);
      if (app.paths.zplugins) paths.push(`Plugins: ${app.paths.zplugins}`);
      const badges = [
        `<span class="badge">${esc(app.source || 'Detected')}</span>`,
        app.manual ? '<span class="badge latest">Manual</span>' : '',
        app.running && app.requiresAppClosed ? '<span class="badge prerelease">Running</span>' : '',
      ].join('');
      const issues = (app.issues || []).map((issue) => `<div class="app-issue">${esc(issue)}</div>`).join('');
      const notes = (app.pathNotes || []).map((note) => `<div>${esc(note)}</div>`).join('');
      return `<div class="app-card">
        <div class="app-main">
          <div class="app-title">${esc(app.appName)} ${esc(app.version)}</div>
          <div class="app-path">${esc(app.installRoot)}</div>
          ${paths.map((line) => `<div class="app-path">${esc(line)}</div>`).join('')}
          ${notes ? `<div class="app-path">${notes}</div>` : ''}
          <div class="app-badges">${badges}</div>
          ${issues}
        </div>
        <div class="app-actions">
          ${app.manual ? `<button type="button" data-remove-override="${esc(app.app)}" data-install-root="${esc(app.installRoot)}">Remove override</button>` : ''}
        </div>
      </div>`;
    }).join('');
  }

  const installed = state.data.installedPlugins || [];
  const installedContainer = $('installedSection');
  if (!installed.length) {
    installedContainer.innerHTML = '<div class="app-item-empty">No plugins are installed.</div>';
  } else {
    installedContainer.innerHTML = installed.map((plugin) => `
      <div class="app-card">
        <div class="app-main">
          <div class="app-title">${esc(plugin.name)}</div>
          <div class="app-path">${plugin.targets.map((target) => `${esc(target.appName)} ${esc(target.appVersion)} · v${esc(target.pluginVersion)} · ${esc(target.status)}`).join('<br>')}</div>
        </div>
        <div class="app-actions">
          ${plugin.targets.map((target) => `<button type="button" data-settings-uninstall="${esc(plugin.id)}" data-target-id="${esc(target.targetId)}">Remove ${esc(target.appName)} ${esc(target.appVersion)}</button>`).join('')}
        </div>
      </div>`).join('');
  }
  updateFormPathVisibility();
  const overrides = appOverrides.length;
  $('appFormStatus').textContent = overrides ? `${overrides} manual path override${overrides === 1 ? '' : 's'} saved.` : '';
}

function updateFormPathVisibility() {
  const kind = $('appKind').value;
  document.querySelectorAll('[data-path]').forEach((label) => {
    label.hidden = label.dataset.path !== kind;
  });
}

function readAppForm() {
  const app = $('appKind').value;
  const value = {
    app,
    version: $('appVersion').value.trim(),
    installRoot: $('appRoot').value.trim(),
  };
  if (app === 'maya') {
    if ($('pathScripts').value.trim()) value.userScripts = $('pathScripts').value.trim();
    if ($('pathModules').value.trim()) value.userModules = $('pathModules').value.trim();
    if ($('pathPrefs').value.trim()) value.userPrefs = $('pathPrefs').value.trim();
  } else {
    if ($('pathZplugs').value.trim()) value.zplugins = $('pathZplugs').value.trim();
  }
  return value;
}

function resetAppForm() {
  $('appForm').reset();
  $('appVersion').value = '';
  $('appRoot').value = '';
  $('pathScripts').value = '';
  $('pathModules').value = '';
  $('pathPrefs').value = '';
  $('pathZplugs').value = '';
  $('appFormStatus').textContent = '';
  updateFormPathVisibility();
}

async function saveAppOverrideForm(event) {
  event?.preventDefault();
  const value = readAppForm();
  if (!value.app || !value.version || !value.installRoot) {
    $('appFormStatus').textContent = 'Application, version and installation folder are required.';
    return;
  }
  $('appFormStatus').textContent = 'Saving application path…';
  try {
    const apps = await invoke('save_app_override', { appOverride: value });
    if (state.data) state.data.apps = apps;
    renderSettings();
    render();
    notify('Application path saved.');
    resetAppForm();
  } catch (error) {
    $('appFormStatus').textContent = errorText(error);
  }
}

async function removeOverride(app, installRoot) {
  try {
    const apps = await invoke('remove_app_override', { app, installRoot });
    if (state.data) state.data.apps = apps;
    renderSettings();
    render();
    notify('Manual path override removed.');
  } catch (error) {
    showError(error, 'Could not remove path override');
  }
}

async function refreshApplications(showToast = true) {
  try {
    const apps = await invoke('refresh_applications');
    if (state.data) state.data.apps = apps;
    renderSettings();
    render();
    if (showToast) notify('Application detection refreshed.');
  } catch (error) {
    showError(error, 'Could not refresh applications');
  }
}

async function refreshCatalog(manual = true) {
  if (state.refreshing) return;
  if (state.jobs.size > 0) {
    notify('Wait for the current installation to finish before refreshing the catalog.');
    return;
  }
  state.refreshing = true;
  render();
  if ($('settingsDialog').open) renderSettings();
  try {
    const data = await invoke('refresh_catalog');
    applyBootstrap(data);
    if (manual) {
      if (data.catalog && data.catalog.error) notify('Catalog refresh failed; cached plugins remain available.');
      else notify('Plugin catalog refreshed.');
    }
    if (data.catalog && data.catalog.error && !data.plugins.length) {
      render();
    }
  } catch (error) {
    notify(errorText(error));
  } finally {
    state.refreshing = false;
    render();
    if ($('settingsDialog').open) renderSettings();
  }
}

async function reloadBootstrap() {
  const data = await invoke('bootstrap');
  applyBootstrap(data);
}

function applyBootstrap(data) {
  state.data = data;
  state.startupLoaded = true;
  const pluginIds = new Set(data.plugins.map((plugin) => plugin.id));
  if (state.selected && !pluginIds.has(state.selected)) state.selected = null;
  for (const id of [...state.chosen]) {
    const plugin = data.plugins.find((item) => item.id === id);
    if (!plugin || !plugin.updateAvailable) state.chosen.delete(id);
  }
  const validSoftware = new Set(softwareOptions().map((option) => option.id));
  if (!validSoftware.has(state.software)) state.software = 'All';
  if ($('settingsDialog').open) renderSettings();
  render();
}

/* ---------------------------------------------------------------------------
 * Startup and events
 * ------------------------------------------------------------------------- */

function loadingMarkup(message) {
  return `<div class="pagehead pluginhead"><h1 tabindex="-1" id="pageTitle">Plugins</h1></div>
    <div class="empty"><h2>NexusTools</h2><p>${esc(message)}</p></div>`;
}

async function bootstrapStartup() {
  if (!invoke) {
    main.classList.remove('pluginlisting');
    main.innerHTML = `<div class="empty"><h2>NexusTools could not connect</h2><p>This page must run inside the NexusTools desktop application.</p></div>`;
    return;
  }
  main.classList.add('pluginlisting');
  main.innerHTML = loadingMarkup('Loading cached plugin metadata…');
  try {
    const data = await invoke('bootstrap');
    applyBootstrap(data);
  } catch (error) {
    main.classList.remove('pluginlisting');
    main.innerHTML = `<div class="empty"><h2>NexusTools could not start</h2><p>${esc(errorText(error))}</p><div class="empty-actions"><button type="button" data-retry-bootstrap>Retry</button></div></div>`;
    return;
  }
  refreshCatalog(false);
}

function installStaticListeners() {
  document.querySelectorAll('[data-nav]').forEach((button) => {
    button.addEventListener('click', () => {
      state.section = 'Plugins';
      state.selected = null;
      render();
    });
  });
  $('search').addEventListener('input', (event) => {
    state.query = event.target.value;
    state.selected = null;
    state.section = 'Plugins';
    render();
  });
  $('settings').addEventListener('click', openSettings);
  $('closeSettings').addEventListener('click', () => $('settingsDialog').close());
  $('refreshCatalog').addEventListener('click', () => refreshCatalog(true));
  $('refreshApps').addEventListener('click', () => refreshApplications(true));
  $('clearCache').addEventListener('click', () => {
    const dialog = $('settingsDialog');
    dialog.close();
    ask('Clear metadata cache?', 'This removes cached plugin.json, banners, README.md and FEATURES.md files. Installed plugins, local installation records, settings and user files are not changed. Plugins will be downloaded again on the next refresh.', async () => {
      try {
        const data = await invoke('clear_metadata_cache');
        applyBootstrap(data);
        notify('Metadata cache cleared. Installed records and settings were preserved.');
      } catch (error) {
        showError(error, 'Could not clear metadata cache');
      }
    }, 'Clear cache');
  });
  $('openDataFolder').addEventListener('click', async () => {
    try { await invoke('open_data_folder'); } catch (error) { showError(error); }
  });
  $('appKind').addEventListener('change', updateFormPathVisibility);
  $('appForm').addEventListener('submit', saveAppOverrideForm);
  document.querySelectorAll('[data-browse]').forEach((button) => {
    button.addEventListener('click', async () => {
      const id = button.dataset.browse;
      const input = $(id);
      try {
        const path = await invoke('pick_directory', { initial: input.value.trim() || null });
        if (path) input.value = path;
      } catch (error) {
        showError(error, 'Could not open folder picker');
      }
    });
  });
  $('cancelConfirm').addEventListener('click', () => {
    state.pending = null;
    $('confirmDialog').close();
  });
  $('acceptConfirm').addEventListener('click', () => {
    const action = state.pending;
    state.pending = null;
    $('confirmDialog').close();
    if (action) Promise.resolve(action()).catch((error) => showError(error));
  });
  $('confirmDialog').addEventListener('cancel', () => { state.pending = null; });
  $('closeProblems').addEventListener('click', () => $('problemsDialog').close());
  $('closeTip').addEventListener('click', () => {
    state.hideTip = Boolean($('dontShowTip').checked);
    try { localStorage.setItem(KEY_TIP, state.hideTip ? '1' : '0'); } catch { /* session-only */ }
    $('updateTip').close();
  });
  $('cancelTargets').addEventListener('click', () => {
    state.targetDialog = null;
    $('targetDialog').close();
  });
  $('confirmTargets').addEventListener('click', confirmTargetSelection);
  $('targetChoices').addEventListener('change', updateTargetChoiceStyles);
  $('targetDialog').addEventListener('cancel', () => { state.targetDialog = null; });
  $('settingsDialog').addEventListener('click', handleSettingsClick);
  document.addEventListener('click', (event) => {
    const menu = $('softwareMenu');
    if (menu && !menu.contains(event.target)) menu.open = false;
  });
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') {
      const menu = $('softwareMenu');
      if (menu) menu.open = false;
    }
  });
  document.addEventListener('click', async (event) => {
    const button = event.target.closest('button, [data-select-plugin]');
    if (!button) return;
    if (button.matches('[data-retry-bootstrap]')) {
      bootstrapStartup();
    }
  });
  main.addEventListener('click', handleMainClick);
  main.addEventListener('keydown', handleMainKeydown);
}

async function handleSettingsClick(event) {
  const button = event.target.closest('button');
  if (!button) return;
  if (button.matches('[data-remove-override]')) {
    await removeOverride(button.dataset.removeOverride, button.dataset.installRoot);
    return;
  }
  if (button.matches('[data-settings-uninstall]')) {
    await confirmUninstall(button.dataset.settingsUninstall, [button.dataset.targetId], 'Uninstall target');
  }
}

async function handleMainClick(event) {
  const card = event.target.closest('[data-select-plugin]');
  if (card && !event.target.closest('button')) {
    const id = card.dataset.selectPlugin;
    if (state.chosen.has(id)) state.chosen.delete(id);
    else state.chosen.add(id);
    render();
    return;
  }
  const button = event.target.closest('button');
  if (!button) return;
  if (button.hasAttribute('disabled')) return;
  const settingsButton = event.target.closest('[data-remove-override], [data-settings-uninstall]');
  if (settingsButton) {
    if (settingsButton.matches('[data-remove-override]')) {
      await removeOverride(settingsButton.dataset.removeOverride, settingsButton.dataset.installRoot);
    } else {
      await confirmUninstall(settingsButton.dataset.settingsUninstall, [settingsButton.dataset.targetId], 'Uninstall target');
    }
    return;
  }
  if (button.dataset.software !== undefined) {
    state.software = button.dataset.software;
    render();
    document.querySelector('.softwaremenu summary')?.focus();
    return;
  }
  if (button.dataset.filter) {
    if (button.dataset.filter === 'installed') {
      state.onlyInstalled = !state.onlyInstalled;
    } else {
      state.onlyUpdates = !state.onlyUpdates;
      if (state.onlyUpdates) showUpdateTip();
      if (!state.onlyUpdates) state.chosen.clear();
    }
    render();
    return;
  }
  if (button.dataset.refresh !== undefined) {
    await refreshCatalog(true);
    return;
  }
  if (button.hasAttribute('data-problems')) {
    const items = ((state.data && state.data.issues) || []).map((issue) => ({
      title: issue.pluginId ? (pluginById(issue.pluginId)?.name || issue.pluginId) : 'Catalog',
      code: issue.code,
      message: issue.message,
      details: issue.details || [],
      source: issue.usingCached ? 'Cached information' : 'Online refresh',
    }));
    const catalog = state.data && state.data.catalog;
    if (catalog && catalog.error) {
      items.unshift({
        title: 'Catalog',
        code: catalog.error.code,
        message: catalog.error.message,
        details: catalog.error.details || [],
        source: catalog.usingCached ? 'Cached catalog' : '',
      });
    }
    const recovery = (state.data && state.data.recoveryWarnings) || [];
    if (recovery.length) {
      items.push({
        title: 'Startup recovery',
        code: 'recovery',
        message: 'NexusTools found interrupted operations.',
        details: recovery,
        source: 'See transaction details above.',
      });
    }
    showProblems('Catalog and plugin problems', items);
    return;
  }
  if (button.dataset.pick) {
    const id = button.dataset.pick;
    if (state.chosen.has(id)) state.chosen.delete(id);
    else state.chosen.add(id);
    render();
    return;
  }
  if (button.id === 'updateAll') {
    batchUpdates(true);
    return;
  }
  if (button.id === 'updateSelected') {
    batchUpdates(false);
    return;
  }
  if (button.dataset.open) {
    state.selected = button.dataset.open;
    state.tab = 0;
    render();
    $('pageTitle')?.focus();
    main.scrollTop = 0;
    return;
  }
  if (button.id === 'back') {
    state.selected = null;
    render();
    $('pageTitle')?.focus();
    main.scrollTop = 0;
    return;
  }
  if (button.dataset.tab !== undefined) {
    state.tab = Number(button.dataset.tab);
    render();
    $(`tab${state.tab}`)?.focus();
    return;
  }
  if (button.dataset.install) {
    const plugin = pluginById(button.dataset.install);
    if (plugin) startMainInstall(plugin, button.dataset.mode || 'install');
    return;
  }
  if (button.dataset.installRelease) {
    const plugin = pluginById(button.dataset.installRelease);
    if (plugin) startVersionInstall(plugin, button.dataset.releaseId);
    return;
  }
  if (button.dataset.uninstall) {
    await confirmUninstall(button.dataset.uninstall, [], 'Uninstall plugin');
    return;
  }
  if (button.dataset.uninstallTarget) {
    await confirmUninstall(button.dataset.uninstallTarget, [button.dataset.targetId], 'Uninstall target');
    return;
  }
  if (button.dataset.cancelJob) {
    const job = [...state.jobs.values()].find((item) => item.jobId === button.dataset.cancelJob);
    if (job) {
      job.cancelRequested = true;
      try { await invoke('cancel_job', { jobId: job.jobId }); } catch { /* already finished */ }
      const entry = [...state.jobs.entries()].find(([, item]) => item.jobId === button.dataset.cancelJob);
      if (entry) {
        entry[1].cancelRequested = true;
        render();
      }
    }
    return;
  }
  if (button.hasAttribute('data-empty')) {
    state.query = '';
    $('search').value = '';
    state.software = 'All';
    state.onlyInstalled = false;
    state.onlyUpdates = false;
    state.chosen.clear();
    render();
  }
}

async function handleMainKeydown(event) {
  const menu = event.target.closest('.softwaremenu');
  if (menu) {
    const summary = menu.querySelector('summary');
    if (event.key === 'Escape') {
      menu.open = false;
      summary.focus();
      event.preventDefault();
      return;
    }
    if (['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
      event.preventDefault();
      menu.open = true;
      const options = [...menu.querySelectorAll('[data-software]')];
      const current = options.indexOf(document.activeElement);
      let next = event.key === 'Home' ? 0 : event.key === 'End' ? options.length - 1
        : event.key === 'ArrowDown' ? (current + 1) % options.length
          : (current - 1 + options.length) % options.length;
      options[next].focus();
      return;
    }
  }
  if (event.target.matches('[role=tab]')) {
    let next = state.tab;
    if (event.key === 'ArrowRight') next = (state.tab + 1) % 3;
    else if (event.key === 'ArrowLeft') next = (state.tab + 2) % 3;
    else if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = 2;
    else return;
    event.preventDefault();
    state.tab = next;
    render();
    $(`tab${state.tab}`)?.focus();
  }
}

function installInstallListeners() {
  if (!listen) return;
  listen('install-progress', (event) => {
    const payload = event.payload || {};
    const job = state.jobs.get(payload.pluginId);
    if (!job || job.jobId !== payload.jobId) return;
    job.percent = Number(payload.percent) || 0;
    job.phase = payload.phase || job.phase;
    job.message = payload.message || job.message;
    document.querySelectorAll('[data-job-progress]').forEach((node) => {
      if (node.dataset.jobProgress !== payload.pluginId) return;
      const label = node.querySelector('[data-progress-label]');
      const message = node.querySelector('[data-progress-message]');
      const progress = node.querySelector('progress');
      if (label) label.textContent = `${job.percent}%`;
      if (message) message.textContent = job.message;
      if (progress) progress.value = job.percent;
    });
    announce(`${payload.message || 'Installing'} ${job.percent}%`);
  });
  listen('state-changed', () => {
    // The invoking command reloads the local bootstrap when it completes. Other
    // windows may have changed installed state; a later refresh picks it up.
  });
}

async function boot() {
  installStaticListeners();
  installInstallListeners();
  await bootstrapStartup();
}

boot();
