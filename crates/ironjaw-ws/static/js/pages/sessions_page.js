/**
 * Ironjaw Dashboard - Sessions Page
 *
 * Lists all chat sessions with name, message count, and timestamps.
 * Filter tabs: All | Active | Pinned | Archived
 * Pin/Archive actions per session card.
 *
 * Data source: GET /api/dashboard/sessions -> Vec<SessionSummary>
 * Fields per session: id, name (nullable), message_count,
 *                     created_at, updated_at, pinned, archived, active
 */
const SessionsPage = (() => {
    let searchTerm = '';
    let activeTab = 'all';
    let sessions = [];

    function render(container) {
        container.innerHTML = `
            <h1 class="page-title">Sessions</h1>
            <div class="flex items-center gap-2" style="margin-bottom:12px;">
                <button class="filter-btn active" data-tab="all">All</button>
                <button class="filter-btn" data-tab="active">Active</button>
                <button class="filter-btn" data-tab="pinned">Pinned</button>
                <button class="filter-btn" data-tab="archived">Archived</button>
            </div>
            <div class="search-bar">
                <input type="text" class="search-input" id="session-search"
                       placeholder="Filter sessions..." autocomplete="off">
            </div>
            <div id="sessions-list">
                ${Utils.spinner()}
            </div>
        `;

        // Tab listeners
        container.querySelectorAll('.filter-btn[data-tab]').forEach(btn => {
            btn.addEventListener('click', () => {
                activeTab = btn.dataset.tab;
                container.querySelectorAll('.filter-btn[data-tab]').forEach(b => b.classList.remove('active'));
                btn.classList.add('active');
                _renderFiltered();
            });
        });

        const searchInput = document.getElementById('session-search');
        if (searchInput) {
            searchInput.addEventListener('input', (e) => {
                searchTerm = e.target.value.toLowerCase();
                _renderFiltered();
            });
        }

        refresh();
    }

    async function refresh() {
        const result = await API.sessions();
        sessions = result || [];
        _renderFiltered();
    }

    function _renderFiltered() {
        const list = document.getElementById('sessions-list');
        if (!list) return;

        if (!sessions || sessions.length === 0) {
            list.innerHTML = Utils.emptyState('No sessions found');
            return;
        }

        // Apply tab filter
        let filtered = sessions;
        if (activeTab === 'active') {
            filtered = sessions.filter(s => s.active);
        } else if (activeTab === 'pinned') {
            filtered = sessions.filter(s => s.pinned);
        } else if (activeTab === 'archived') {
            filtered = sessions.filter(s => s.archived);
        }

        // Apply search filter
        if (searchTerm) {
            filtered = filtered.filter(s => {
                const name = (s.name || s.id || '').toLowerCase();
                return name.includes(searchTerm);
            });
        }

        if (filtered.length === 0) {
            list.innerHTML = Utils.emptyState('No sessions match the current filter');
            return;
        }

        // Sort: pinned first, then by updated_at descending
        filtered.sort((a, b) => {
            if (a.pinned && !b.pinned) return -1;
            if (!a.pinned && b.pinned) return 1;
            const da = Utils.safeTimestamp(b.updated_at);
            const db = Utils.safeTimestamp(a.updated_at);
            if (!da && !db) return 0;
            if (!da) return -1;
            if (!db) return 1;
            return new Date(da).getTime() - new Date(db).getTime();
        });

        list.innerHTML = filtered.map(s => _renderSession(s)).join('');

        // Pin/Archive button listeners
        list.querySelectorAll('[data-pin]').forEach(btn => {
            btn.addEventListener('click', (e) => {
                e.stopPropagation();
                const id = btn.dataset.pin;
                const session = sessions.find(s => s.id === id);
                const newVal = !(session && session.pinned);
                _pinSession(id, newVal);
            });
        });

        list.querySelectorAll('[data-archive]').forEach(btn => {
            btn.addEventListener('click', (e) => {
                e.stopPropagation();
                const id = btn.dataset.archive;
                const session = sessions.find(s => s.id === id);
                const newVal = !(session && session.archived);
                _archiveSession(id, newVal);
            });
        });
    }

    function destroy() {
        searchTerm = '';
        activeTab = 'all';
        sessions = [];
    }

    function _renderSession(s) {
        const title = s.name || s.id || 'Untitled';
        const msgs = s.message_count || 0;
        const created = Utils.timeAgo(s.created_at);
        const updated = Utils.timeAgo(s.updated_at);
        const isPinned = s.pinned;
        const isArchived = s.archived;

        return `
            <div class="session-card">
                <div class="session-header">
                    <span class="session-title">
                        ${isPinned ? '<span title="Pinned" style="margin-right:4px;">&#128204;</span>' : ''}
                        ${Utils.esc(title)}
                    </span>
                    <div class="flex items-center gap-2">
                        <button class="btn btn-sm" data-pin="${Utils.esc(s.id || '')}" title="${isPinned ? 'Unpin' : 'Pin'}">${isPinned ? 'Unpin' : 'Pin'}</button>
                        <button class="btn btn-sm" data-archive="${Utils.esc(s.id || '')}" title="${isArchived ? 'Unarchive' : 'Archive'}">${isArchived ? 'Unarchive' : 'Archive'}</button>
                        <span class="text-sm text-muted">${msgs} message${msgs !== 1 ? 's' : ''}</span>
                    </div>
                </div>
                <div class="session-meta">
                    <span>ID: <span class="mono">${Utils.esc(s.id || '')}</span></span>
                    <span>Created ${created}</span>
                    <span>Updated ${updated}</span>
                </div>
            </div>
        `;
    }

    async function _pinSession(id, pinned) {
        try {
            await Gateway.sendRequest('session.pin', { session_id: id, pinned });
            // Update local state
            const session = sessions.find(s => s.id === id);
            if (session) session.pinned = pinned;
            _renderFiltered();
        } catch (e) {
            alert('Failed to pin session: ' + (e.message || e));
        }
    }

    async function _archiveSession(id, archived) {
        try {
            await Gateway.sendRequest('session.archive', { session_id: id, archived });
            // Update local state
            const session = sessions.find(s => s.id === id);
            if (session) session.archived = archived;
            _renderFiltered();
        } catch (e) {
            alert('Failed to archive session: ' + (e.message || e));
        }
    }

    return { render, refresh, destroy };
})();
