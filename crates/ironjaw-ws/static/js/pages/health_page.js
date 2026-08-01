/**
 * Ironjaw Dashboard - Health Page
 *
 * Shows subsystem health checks and metrics snapshot.
 *
 * Data sources:
 *   GET /api/dashboard/health  -> HealthCheck { status, checks: { database, llm, brain } }
 *   GET /api/dashboard/metrics -> MetricsSnapshot { requests_total, errors_total,
 *                                   active_connections, llm_tokens_used,
 *                                   avg_response_ms, uptime_secs }
 */
const HealthPage = (() => {
    function render(container) {
        container.innerHTML = `
            <h1 class="page-title">Health</h1>
            <div class="section-title">Subsystem Status</div>
            <div class="health-grid" id="health-checks">
                ${Utils.spinner()}
            </div>
            <div class="section-title">Metrics</div>
            <div class="stats-grid" id="health-metrics">
                ${_metricCard('metric-requests', 'Total Requests', '--')}
                ${_metricCard('metric-errors', 'Total Errors', '--')}
                ${_metricCard('metric-connections', 'Active Connections', '--')}
                ${_metricCard('metric-tokens', 'LLM Tokens Used', '--')}
                ${_metricCard('metric-response', 'Avg Response', '--')}
                ${_metricCard('metric-uptime', 'Uptime', '--')}
            </div>
            <div class="section-title">Operational</div>
            <div class="stats-grid" id="health-ops">
                <div class="card" id="health-compaction">${Utils.spinner()}</div>
                <div class="card" id="health-cache">${Utils.spinner()}</div>
                <div class="card" id="health-classifier">${Utils.spinner()}</div>
            </div>
            <div class="section-title">Raw Health Response</div>
            <div class="card" id="health-raw">
                ${Utils.spinner()}
            </div>
        `;
        refresh();
    }

    async function refresh() {
        // Fetch health, metrics, and operational data in parallel
        const [health, metrics] = await Promise.all([
            API.health(),
            API.metrics(),
        ]);

        _renderHealthChecks(health);
        _renderMetrics(metrics);
        _renderRaw(health);
        _loadOpsCards();
    }

    function destroy() {
        // No event listeners to clean up
    }

    function _renderHealthChecks(health) {
        const grid = document.getElementById('health-checks');
        if (!grid || !health) {
            if (grid) grid.innerHTML = Utils.emptyState('Health data unavailable');
            return;
        }

        const checks = health.checks || {};
        const items = [
            { label: 'Overall', ok: health.status === 'ok' },
            { label: 'Database', ok: !!checks.database },
            { label: 'LLM', ok: !!checks.llm },
            { label: 'Brain', ok: !!checks.brain },
        ];

        grid.innerHTML = items.map(item => `
            <div class="health-check-card">
                <span class="check-dot ${item.ok ? 'ok' : 'fail'}"></span>
                <div>
                    <div class="check-label">${Utils.esc(item.label)}</div>
                    <div class="check-status">${item.ok ? 'Healthy' : 'Unhealthy'}</div>
                </div>
            </div>
        `).join('');
    }

    function _renderMetrics(metrics) {
        if (!metrics) return;

        _set('metric-requests', _formatNumber(metrics.requests_total || 0));
        _set('metric-errors', _formatNumber(metrics.errors_total || 0));
        _set('metric-connections', metrics.active_connections || 0);
        _set('metric-tokens', _formatTokens(metrics.llm_tokens_used || 0));
        _set('metric-response', (metrics.avg_response_ms || 0).toFixed(1) + 'ms');
        _set('metric-uptime', _formatUptime(metrics.uptime_secs || 0));
    }

    function _renderRaw(health) {
        const raw = document.getElementById('health-raw');
        if (!raw) return;

        if (health) {
            raw.innerHTML = `<pre>${Utils.esc(JSON.stringify(health, null, 2))}</pre>`;
        } else {
            raw.innerHTML = Utils.emptyState('Could not fetch health data');
        }
    }

    function _metricCard(id, title, value) {
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

    function _formatNumber(n) {
        if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M';
        if (n >= 1e3) return (n / 1e3).toFixed(1) + 'K';
        return String(n);
    }

    function _formatTokens(n) {
        if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M';
        if (n >= 1e3) return (n / 1e3).toFixed(0) + 'K';
        return String(n);
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

    async function _loadOpsCards() {
        // Compaction status
        _loadCompaction();
        // Cache stats
        _loadCache();
        // Classifier stats
        _loadClassifier();
    }

    async function _loadCompaction() {
        const el = document.getElementById('health-compaction');
        if (!el) return;
        try {
            const data = await Gateway.sendRequest('context.status', {});
            const lastStrategy = data.last_strategy || 'none';
            const lastTime = data.last_compact ? new Date(data.last_compact).toLocaleString() : 'never';
            const tokensBefore = _formatTokens(data.tokens_before || 0);
            const tokensAfter = _formatTokens(data.tokens_after || 0);
            const cb = data.circuit_breaker || {};
            const cbState = cb.state || 'closed';
            const cbClass = cbState === 'closed' ? 'status-ok' : 'status-fail';

            el.innerHTML = `
                <div class="card-header"><span class="card-title">Context Compaction</span></div>
                <div style="margin-bottom:6px;">Last: <span class="mono">${Utils.esc(lastStrategy)}</span> at ${Utils.esc(lastTime)}</div>
                <div style="margin-bottom:6px;">Tokens: <span class="mono">${tokensBefore} &rarr; ${tokensAfter}</span></div>
                <div class="status-indicator ${cbClass}">Circuit breaker: ${Utils.esc(cbState)} (${cb.failures || 0} failures)</div>
            `;
        } catch (e) {
            el.innerHTML = `<div class="card-header"><span class="card-title">Context Compaction</span></div><div class="text-sm text-muted">Unavailable</div>`;
        }
    }

    async function _loadCache() {
        const el = document.getElementById('health-cache');
        if (!el) return;
        try {
            const data = await Gateway.sendRequest('cache.stats', {});
            const hitRate = data.hit_rate != null ? (data.hit_rate * 100).toFixed(1) + '%' : '--';
            const size = data.size_bytes != null ? _formatBytes(data.size_bytes) : (data.entries || 0) + ' entries';

            el.innerHTML = `
                <div class="card-header"><span class="card-title">Cache Stats</span></div>
                <div class="card-value">${hitRate}</div>
                <div class="card-detail">Hit Rate</div>
                <div style="margin-top:8px;" class="text-sm text-muted">Size: ${size}</div>
            `;
        } catch (e) {
            el.innerHTML = `<div class="card-header"><span class="card-title">Cache Stats</span></div><div class="text-sm text-muted">Unavailable</div>`;
        }
    }

    async function _loadClassifier() {
        const el = document.getElementById('health-classifier');
        if (!el) return;
        try {
            const data = await Gateway.sendRequest('security.classifier.stats', {});
            const approved = data.auto_approved || 0;
            const denied = data.denied || 0;
            const escalated = data.escalated || 0;

            el.innerHTML = `
                <div class="card-header"><span class="card-title">Permission Classifier</span></div>
                <div style="display:flex;gap:16px;margin-top:4px;">
                    <div><div class="text-sm text-muted">Auto-Approved</div><div style="font-size:20px;font-weight:600;color:var(--green);">${approved}</div></div>
                    <div><div class="text-sm text-muted">Denied</div><div style="font-size:20px;font-weight:600;color:var(--red);">${denied}</div></div>
                    <div><div class="text-sm text-muted">Escalated</div><div style="font-size:20px;font-weight:600;color:var(--amber);">${escalated}</div></div>
                </div>
            `;
        } catch (e) {
            el.innerHTML = `<div class="card-header"><span class="card-title">Permission Classifier</span></div><div class="text-sm text-muted">Unavailable</div>`;
        }
    }

    function _formatBytes(bytes) {
        if (bytes >= 1048576) return (bytes / 1048576).toFixed(1) + ' MB';
        if (bytes >= 1024) return (bytes / 1024).toFixed(1) + ' KB';
        return bytes + ' B';
    }

    return { render, refresh, destroy };
})();
