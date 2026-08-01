/**
 * Plugins Page - Plugin Management
 *
 * Lists plugins with enable/disable toggles.
 * Uses plugins.list and plugins.toggle.
 */
const PluginsPage = (() => {
  let _plugins = [];

  function render(container) {
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Plugins</h2>
      <div id="plugins-list">${Utils.spinner()}</div>
    `;
    _loadPlugins();
  }

  function destroy() {
    _plugins = [];
  }

  async function _loadPlugins() {
    const list = document.getElementById('plugins-list');
    if (!list) return;

    try {
      const result = await Gateway.sendRequest('plugins.list', {});
      _plugins = result.plugins || result || [];

      if (_plugins.length === 0) {
        list.innerHTML = Utils.emptyState('No plugins installed');
        return;
      }

      _renderPlugins(list);
    } catch (e) {
      list.innerHTML = `<div class="empty-state">${Utils.esc(e.message || 'Failed to load plugins')}</div>`;
    }
  }

  function _renderPlugins(list) {
    list.innerHTML = _plugins.map((p, i) => {
      const borderColor = p.enabled ? 'var(--success)' : 'var(--border)';
      return `
        <div class="card" style="border-left:3px solid ${borderColor}">
          <div class="flex justify-between items-center" style="padding:12px;">
            <div>
              <strong>${Utils.esc(p.name)}</strong>
              ${p.version ? `<span class="text-sm text-muted" style="margin-left:8px;">v${Utils.esc(p.version)}</span>` : ''}
              <div class="text-sm text-muted" style="margin-top:4px;">${Utils.esc(p.description || 'No description')}</div>
            </div>
            <label class="toggle">
              <input type="checkbox" ${p.enabled ? 'checked' : ''} class="plugin-toggle" data-plugin-name="${Utils.esc(p.name)}">
              <span class="toggle-slider"></span>
            </label>
          </div>
        </div>
      `;
    }).join('');

    list.querySelectorAll('.plugin-toggle').forEach(cb => {
      cb.addEventListener('change', () => _togglePlugin(cb.dataset.pluginName, cb.checked));
    });
  }

  async function _togglePlugin(name, enabled) {
    try {
      await Gateway.sendRequest('plugins.toggle', { name, enabled });
      _loadPlugins();
    } catch (e) {
      alert('Failed to toggle plugin: ' + (e.message || e));
      _loadPlugins();
    }
  }

  return { render, destroy };
})();
