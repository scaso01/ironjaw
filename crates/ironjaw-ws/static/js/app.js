/**
 * Ironjaw Command Center - App Router & Init
 *
 * Hash-based SPA router with 4 pages: overview, sessions, tools, health.
 * Auto-refreshes the current page every 30 seconds.
 * Connects the Gateway WebSocket for live status updates.
 */
const App = (() => {
    const pages = {
        'overview': OverviewPage,
        'sessions': SessionsPage,
        'tools': ToolsPage,
        'health': HealthPage,
        'chat': ChatPage,
        'models': typeof ModelsPage !== 'undefined' ? ModelsPage : null,
        'cc_sessions': typeof CcSessionsPage !== 'undefined' ? CcSessionsPage : null,
        'brain': typeof BrainPage !== 'undefined' ? BrainPage : null,
        'chronicle': typeof ChroniclePage !== 'undefined' ? ChroniclePage : null,
        'settings': typeof SettingsPage !== 'undefined' ? SettingsPage : null,
        'schedules': typeof SchedulesPage !== 'undefined' ? SchedulesPage : null,
        'metrics': typeof MetricsPage !== 'undefined' ? MetricsPage : null,
        'devices': typeof DevicesPage !== 'undefined' ? DevicesPage : null,
        'infrastructure': typeof InfrastructurePage !== 'undefined' ? InfrastructurePage : null,
        'graph': typeof GraphPage !== 'undefined' ? GraphPage : null,
        'vault': typeof VaultPage !== 'undefined' ? VaultPage : null,
        'plugins': typeof PluginsPage !== 'undefined' ? PluginsPage : null,
        'browser': typeof BrowserPage !== 'undefined' ? BrowserPage : null,
        'security': typeof SecurityPage !== 'undefined' ? SecurityPage : null,
        'login': typeof LoginPage !== 'undefined' ? LoginPage : null,
    };

    let currentPage = null;
    let refreshInterval = null;

    function init() {
        // Connect Gateway WebSocket for live status
        _connectGateway();

        // Gateway status listener -- update sidebar indicator
        Gateway.addStatusListener((status) => {
            const dot = document.querySelector('#ws-status .status-dot');
            const text = document.querySelector('#ws-status .status-text');

            switch (status) {
                case 'connecting':
                    if (dot) dot.classList.remove('connected');
                    if (text) text.textContent = 'Connecting...';
                    break;
                case 'connected':
                    if (dot) dot.classList.add('connected');
                    if (text) text.textContent = 'Connected';
                    break;
                case 'disconnected':
                    if (dot) dot.classList.remove('connected');
                    if (text) text.textContent = 'Disconnected';
                    break;
                case 'auth_failed':
                    if (dot) dot.classList.remove('connected');
                    if (text) text.textContent = 'Auth Failed';
                    // Auto-redirect to login page (unless already there)
                    if (currentPage !== 'login') navigate('login');
                    break;
            }
        });

        // Listen for live session events to auto-refresh relevant pages
        Gateway.onEvent('session.status', () => {
            if (currentPage === 'sessions') SessionsPage.refresh();
            if (currentPage === 'overview') OverviewPage.refresh();
        });

        // Global permission request handler
        Gateway.onEvent('permission.request', (data) => {
            _showPermissionModal(data);
        });

        // Hash-based routing
        window.addEventListener('hashchange', _onRoute);
        _onRoute();
    }

    function _connectGateway() {
        const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
        const wsUrl = `${proto}//${location.host}/ws`;
        // Use stored token or default 'dashboard' token
        const token = localStorage.getItem('ironjaw_token') || 'dashboard';
        Gateway.connect(wsUrl, token);
    }

    function _onRoute() {
        const hash = window.location.hash.replace('#/', '') || 'overview';
        renderPage(hash);
    }

    function renderPage(name) {
        const page = pages[name];

        // Update sidebar active state for every navigation, including "coming soon" pages
        document.querySelectorAll('.nav-link').forEach(link => {
            link.classList.toggle('active', link.getAttribute('data-page') === name);
        });

        if (!page) {
            if (name in pages) {
                // Page registered but JS not loaded yet -- show placeholder
                currentPage = name;
                const content = document.getElementById('content');
                content.innerHTML = `<div class="empty-state"><div class="empty-state-title">${name} page coming soon</div></div>`;
                return;
            }
            // Unknown page, redirect to overview
            navigate('overview');
            return;
        }

        // Destroy previous page if it has cleanup
        if (currentPage && pages[currentPage] && pages[currentPage].destroy) {
            pages[currentPage].destroy();
        }

        currentPage = name;

        // Render page into content area
        const content = document.getElementById('content');
        content.innerHTML = '';
        page.render(content);

        // Auto-refresh every 30 seconds
        if (refreshInterval) clearInterval(refreshInterval);
        refreshInterval = setInterval(() => {
            const handler = pages[currentPage];
            if (handler && handler.refresh) handler.refresh();
        }, 30000);
    }

    function navigate(page) {
        window.location.hash = '#/' + page;
    }

    function _showPermissionModal(data) {
        // Remove existing modal
        const existing = document.getElementById('permission-modal');
        if (existing) existing.remove();

        const overlay = document.createElement('div');
        overlay.id = 'permission-modal';
        overlay.className = 'permission-overlay';

        const method = data.method || 'unknown';
        const params = data.params ? JSON.stringify(data.params, null, 2) : '{}';
        const requestId = data.request_id || data.id || '';
        let countdown = 30;

        overlay.innerHTML = `
            <div class="permission-modal">
                <div class="permission-header">Permission Request</div>
                <div class="permission-method">${method}</div>
                <pre class="permission-params">${params}</pre>
                <div class="permission-countdown">Auto-deny in <span id="perm-countdown">${countdown}</span>s</div>
                <div class="permission-actions">
                    <button class="btn btn-primary" id="perm-allow">Allow</button>
                    <button class="btn btn-danger" id="perm-deny">Deny</button>
                </div>
            </div>
        `;

        document.body.appendChild(overlay);

        const countdownEl = document.getElementById('perm-countdown');
        const timer = setInterval(() => {
            countdown--;
            if (countdownEl) countdownEl.textContent = countdown;
            if (countdown <= 0) {
                clearInterval(timer);
                _respondPermission(requestId, false);
                overlay.remove();
            }
        }, 1000);

        document.getElementById('perm-allow').addEventListener('click', () => {
            clearInterval(timer);
            _respondPermission(requestId, true);
            overlay.remove();
        });

        document.getElementById('perm-deny').addEventListener('click', () => {
            clearInterval(timer);
            _respondPermission(requestId, false);
            overlay.remove();
        });
    }

    function _respondPermission(requestId, approved) {
        Gateway.sendCommandResponse(requestId, approved, { approved });
    }

    function reconnect() {
        _connectGateway();
    }

    return { init, navigate, renderPage, reconnect };
})();
