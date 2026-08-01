/**
 * Infrastructure Page - Service health and status monitoring
 */
const InfrastructurePage = (() => {
  let refreshTimer = null;

  function render(container) {
    destroyed = false;
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Infrastructure</h2>
      <div id="infra-body"></div>
    `;
    _load();
    refreshTimer = setInterval(_load, 15000);
  }

  let destroyed = false;

  function destroy() {
    destroyed = true;
    if (refreshTimer) {
      clearInterval(refreshTimer);
      refreshTimer = null;
    }
  }

  async function _load() {
    const body = document.getElementById('infra-body');
    if (!body) { destroy(); return; }

    // Fetch both infra.status and the REST health endpoint in parallel
    let infraData = null;
    let restHealth = null;
    let restOverview = null;

    try {
      const results = await Promise.allSettled([
        Gateway.sendRequest('infra.status', {}),
        fetch('/api/dashboard/health').then(r => r.json()),
        fetch('/api/dashboard/overview').then(r => r.json()),
      ]);

      if (results[0].status === 'fulfilled') infraData = results[0].value;
      if (results[1].status === 'fulfilled') restHealth = results[1].value;
      if (results[2].status === 'fulfilled') restOverview = results[2].value;
    } catch (e) {
      // partial failure is fine
    }

    if (!infraData && !restHealth && !restOverview) {
      body.innerHTML = '<div class="empty-state">Failed to load infrastructure status</div>';
      return;
    }

    let html = '';

    // Overview card from REST API
    if (restOverview) {
      const uptimeSecs = restOverview.uptime_secs || 0;
      const hours = Math.floor(uptimeSecs / 3600);
      const mins = Math.floor((uptimeSecs % 3600) / 60);
      html += `
        <div class="card">
          <div class="card-header"><span class="card-title">Gateway Overview</span></div>
          <div class="stats-grid">
            ${_statBox('Version', restOverview.version || 'N/A')}
            ${_statBox('Uptime', uptimeSecs > 0 ? hours + 'h ' + mins + 'm' : 'N/A')}
            ${_statBox('Sessions', restOverview.active_sessions || 0)}
            ${_statBox('Messages', restOverview.total_messages || 0)}
            ${_statBox('Devices', restOverview.connected_devices || 0)}
            ${_statBox('Memories', restOverview.memory_count || 0)}
            ${_statBox('Tools', restOverview.active_tools || 0)}
            ${_statBox('LLM Providers', restOverview.llm_provider_count || 0)}
          </div>
        </div>
      `;
    }

    // Health subsystems from REST API
    if (restHealth) {
      const checks = restHealth.checks || {};
      html += `
        <div class="card">
          <div class="card-header"><span class="card-title">Subsystem Health</span></div>
          <div class="table-wrap">
            <table>
              <thead><tr><th>Subsystem</th><th>Status</th></tr></thead>
              <tbody>
                ${_healthRow('Database', checks.database)}
                ${_healthRow('LLM', checks.llm)}
                ${_healthRow('Brain', checks.brain)}
              </tbody>
            </table>
          </div>
        </div>
      `;
    }

    // Service status from WebSocket (infra.status)
    if (infraData) {
      const services = infraData.services || infraData || {};

      if (Array.isArray(services)) {
        // If services is an array of {name, status, url, ...}
        html += `
          <div class="card">
            <div class="card-header"><span class="card-title">Services</span></div>
            <div class="table-wrap">
              <table>
                <thead><tr><th>Service</th><th>Status</th><th>URL</th><th>Details</th></tr></thead>
                <tbody>${services.map(s => {
                  const isUp = (s.status || '').toLowerCase() === 'up' ||
                               (s.status || '').toLowerCase() === 'ok' ||
                               (s.status || '').toLowerCase() === 'running' ||
                               s.healthy === true;
                  return `<tr>
                    <td><strong>${Utils.esc(s.name || 'Unknown')}</strong></td>
                    <td><span class="badge ${isUp ? 'badge-success' : 'badge-danger'}">${Utils.esc(s.status || 'unknown')}</span></td>
                    <td class="mono text-sm">${Utils.esc(s.url || s.endpoint || '')}</td>
                    <td class="text-sm text-muted">${Utils.esc(s.details || s.version || '')}</td>
                  </tr>`;
                }).join('')}</tbody>
              </table>
            </div>
          </div>
        `;
      } else if (typeof services === 'object') {
        // If services is an object keyed by service name
        const entries = Object.entries(services);
        if (entries.length > 0) {
          html += `
            <div class="card">
              <div class="card-header"><span class="card-title">Services</span></div>
              <div class="table-wrap">
                <table>
                  <thead><tr><th>Service</th><th>Status</th><th>Details</th></tr></thead>
                  <tbody>${entries.map(([name, info]) => {
                    const status = typeof info === 'string' ? info : (info.status || 'unknown');
                    const isUp = status.toLowerCase() === 'up' ||
                                 status.toLowerCase() === 'ok' ||
                                 status.toLowerCase() === 'running' ||
                                 (typeof info === 'object' && info.healthy === true);
                    const details = typeof info === 'object' ? (info.details || info.version || info.error || '') : '';
                    return `<tr>
                      <td><strong>${Utils.esc(name)}</strong></td>
                      <td><span class="badge ${isUp ? 'badge-success' : 'badge-danger'}">${Utils.esc(status)}</span></td>
                      <td class="text-sm text-muted">${Utils.esc(details)}</td>
                    </tr>`;
                  }).join('')}</tbody>
                </table>
              </div>
            </div>
          `;
        }
      }

      // Render any extra fields from infra.status (uptime, version, etc.)
      if (infraData.uptime_secs != null || infraData.version) {
        const uptimeSecs = infraData.uptime_secs || 0;
        const hours = Math.floor(uptimeSecs / 3600);
        const mins = Math.floor((uptimeSecs % 3600) / 60);
        html += `
          <div class="card">
            <div class="card-header"><span class="card-title">Ironjaw Instance</span></div>
            <table>
              ${infraData.version ? `<tr><th style="width:140px;">Version</th><td>${Utils.esc(infraData.version)}</td></tr>` : ''}
              ${uptimeSecs > 0 ? `<tr><th style="width:140px;">Uptime</th><td>${hours}h ${mins}m</td></tr>` : ''}
            </table>
          </div>
        `;
      }
    }

    // If we got nothing useful, show the raw data for debugging
    if (!html) {
      html = '<div class="empty-state">No infrastructure data available</div>';
    }

    html += '<div class="text-sm text-muted mt-2">Auto-refreshes every 15s</div>';
    body.innerHTML = html;
  }

  function _statBox(label, value) {
    return `
      <div class="stat-box">
        <div class="stat-value">${Utils.esc(String(value))}</div>
        <div class="stat-label">${Utils.esc(label)}</div>
      </div>
    `;
  }

  function _healthRow(name, healthy) {
    const isOk = healthy === true;
    return `<tr>
      <td>${Utils.esc(name)}</td>
      <td><span class="badge ${isOk ? 'badge-success' : 'badge-danger'}">${isOk ? 'Healthy' : 'Unhealthy'}</span></td>
    </tr>`;
  }

  return { render, destroy };
})();
