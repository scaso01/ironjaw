/**
 * Metrics Page - System metrics and circuit breakers
 */
const MetricsPage = (() => {
  let refreshTimer = null;

  function render(container) {
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Metrics</h2>
      <div id="metrics-body"></div>
    `;
    _load();
    refreshTimer = setInterval(_load, 30000);
  }

  function destroy() {
    if (refreshTimer) {
      clearInterval(refreshTimer);
      refreshTimer = null;
    }
  }

  async function _load() {
    const body = document.getElementById('metrics-body');
    if (!body) { destroy(); return; }

    try {
      const result = await Gateway.sendRequest('infra.metrics', {});

      const uptime = result.uptime_secs || result.uptime || 0;
      const hours = Math.floor(uptime / 3600);
      const mins = Math.floor((uptime % 3600) / 60);

      // Metrics response contract: uptime_secs, metrics.{active_connections, connections_total, ...}, devices_connected, circuit_breakers
      const m = result.metrics || {};

      body.innerHTML = `
        <div class="card">
          <div class="card-header"><span class="card-title">Server</span></div>
          <table>
            <tr><th style="width:160px;">Uptime</th><td>${hours}h ${mins}m</td></tr>
            <tr><th>Active Connections</th><td>${m.active_connections || 0}</td></tr>
            <tr><th>Total Connections</th><td>${m.connections_total || 0}</td></tr>
            <tr><th>Total Requests</th><td>${m.requests_total || 0}</td></tr>
            <tr><th>LLM Calls</th><td>${m.llm_calls_total || 0}</td></tr>
            <tr><th>Tool Calls</th><td>${m.tool_calls_total || 0}</td></tr>
            <tr><th>Total Errors</th><td>${m.errors_total || 0}</td></tr>
          </table>
        </div>

        ${_renderCircuitBreakers(result.circuit_breakers || {})}

        <div class="text-sm text-muted mt-2">Auto-refreshes every 30s</div>
      `;
    } catch (e) {
      body.innerHTML = '<div class="empty-state">Failed to load metrics</div>';
    }
  }

  function _renderCircuitBreakers(breakers) {
    if (!breakers || typeof breakers !== 'object') return '';

    // Circuit breakers come as an object: { "service_name": { "state": "Closed", "failures": 0 }, ... }
    const entries = Object.entries(breakers);
    if (entries.length === 0) return '';

    return `
      <div class="card">
        <div class="card-header"><span class="card-title">Circuit Breakers</span></div>
        <div class="table-wrap">
          <table>
            <thead><tr><th>Service</th><th>State</th><th>Failures</th></tr></thead>
            <tbody>${entries.map(([name, b]) => {
              const state = (b.state || 'Closed').toLowerCase();
              const badgeClass = state === 'closed' ? 'badge-success' :
                                 state === 'halfopen' || state === 'half_open' || state === 'half-open' ? 'badge-warning' : 'badge-danger';
              return `<tr>
                <td>${Utils.esc(name)}</td>
                <td><span class="badge ${badgeClass}">${state}</span></td>
                <td>${b.failures || 0}</td>
              </tr>`;
            }).join('')}</tbody>
          </table>
        </div>
      </div>
    `;
  }

  return { render, destroy };
})();
