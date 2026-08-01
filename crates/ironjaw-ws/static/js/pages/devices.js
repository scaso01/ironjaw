/**
 * Devices Page - Connected device list
 */
const DevicesPage = (() => {
  function render(container) {
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Devices</h2>
      <div id="devices-list"></div>
    `;
    _load();
  }

  async function _load() {
    const list = document.getElementById('devices-list');
    list.innerHTML = '<div style="text-align:center;padding:20px;"><div class="spinner"></div></div>';

    try {
      const result = await Gateway.sendRequest('device.list', {});
      const devices = result.devices || result || [];

      if (devices.length === 0) {
        list.innerHTML = '<div class="empty-state">No devices connected</div>';
        return;
      }

      // Device response contract: id, name, device_type, capabilities, connected_at, last_seen
      list.innerHTML = devices.map(d => `
        <div class="card">
          <div class="card-header">
            <div>
              <span class="card-title">${Utils.esc(d.name || 'Unknown')}</span>
            </div>
            <span class="text-sm text-muted">${Utils.esc(d.device_type || '')}</span>
          </div>
          <table class="text-sm">
            <tr><th style="width:120px;">Device ID</th><td class="mono">${Utils.esc(d.id || '')}</td></tr>
            <tr><th>Type</th><td>${Utils.esc(d.device_type || 'N/A')}</td></tr>
            <tr><th>Capabilities</th><td>${Utils.esc((d.capabilities || []).join(', ') || 'None')}</td></tr>
            <tr><th>Connected</th><td>${Utils.safeTimestamp(d.connected_at) ? new Date(d.connected_at).toLocaleString() : '\u2014'}</td></tr>
            <tr><th>Last Seen</th><td>${Utils.safeTimestamp(d.last_seen) ? new Date(d.last_seen).toLocaleString() : '\u2014'}</td></tr>
          </table>
        </div>
      `).join('');
    } catch (e) {
      list.innerHTML = '<div class="empty-state">Failed to load devices</div>';
    }
  }

  return { render };
})();
