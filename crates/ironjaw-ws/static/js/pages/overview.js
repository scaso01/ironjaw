/**
 * Ironjaw Dashboard - Overview Page
 *
 * Shows gateway-level stats: uptime, sessions, messages, devices,
 * tools, brain memories, and LLM provider count.
 *
 * Data source: GET /api/dashboard/overview -> DashboardOverview
 * Fields: uptime_secs, active_sessions, total_messages,
 *         connected_devices, memory_count, active_tools,
 *         llm_provider_count, version
 */
const OverviewPage = (() => {
    function render(container) {
        container.innerHTML = `
            <h1 class="page-title">Overview</h1>
            <div class="stats-grid" id="overview-stats">
                ${_statCard('stat-uptime', 'Uptime', '--')}
                ${_statCard('stat-sessions', 'Active Sessions', '--')}
                ${_statCard('stat-messages', 'Total Messages', '--')}
                ${_statCard('stat-devices', 'Connected Devices', '--')}
                ${_statCard('stat-tools', 'Active Tools', '--')}
                ${_statCard('stat-brain', 'Brain Memories', '--')}
            </div>
            <div class="section-title">System Info</div>
            <div class="card" id="overview-info">
                ${Utils.spinner()}
            </div>
        `;
        refresh();
    }

    async function refresh() {
        const data = await API.overview();
        if (!data) return;

        _set('stat-uptime', _formatUptime(data.uptime_secs || 0));
        _set('stat-sessions', data.active_sessions || 0);
        _set('stat-messages', data.total_messages || 0);
        _set('stat-devices', data.connected_devices || 0);
        _set('stat-tools', data.active_tools || 0);
        _set('stat-brain', data.memory_count || 0);

        // Update version in sidebar
        const versionLabel = document.getElementById('version-label');
        if (versionLabel && data.version) {
            versionLabel.textContent = 'v' + Utils.esc(data.version);
        }

        // System info card
        const infoEl = document.getElementById('overview-info');
        if (infoEl) {
            infoEl.innerHTML = `
                <table>
                    <tr><th style="width:180px">Field</th><th>Value</th></tr>
                    <tr><td>Version</td><td class="mono">${Utils.esc(data.version)}</td></tr>
                    <tr><td>LLM Providers</td><td>${data.llm_provider_count || 0}</td></tr>
                    <tr><td>Active Tools</td><td>${data.active_tools || 0}</td></tr>
                    <tr><td>Active Sessions</td><td>${data.active_sessions || 0}</td></tr>
                    <tr><td>Connected Devices</td><td>${data.connected_devices || 0}</td></tr>
                    <tr><td>Uptime</td><td>${_formatUptime(data.uptime_secs || 0)}</td></tr>
                </table>
            `;
        }
    }

    function destroy() {
        // No event listeners to clean up
    }

    function _statCard(id, title, value) {
        return `
            <div class="card">
                <div class="card-header">
                    <span class="card-title">${Utils.esc(title)}</span>
                </div>
                <div class="card-value" id="${id}">${Utils.esc(String(value))}</div>
            </div>
        `;
    }

    function _set(id, value) {
        const el = document.getElementById(id);
        if (el) el.textContent = String(value);
    }

    function _formatUptime(secs) {
        if (secs <= 0) return '0m';
        const days = Math.floor(secs / 86400);
        const hours = Math.floor((secs % 86400) / 3600);
        const mins = Math.floor((secs % 3600) / 60);
        if (days > 0) return days + 'd ' + hours + 'h';
        if (hours > 0) return hours + 'h ' + mins + 'm';
        return mins + 'm';
    }

    return { render, refresh, destroy };
})();
