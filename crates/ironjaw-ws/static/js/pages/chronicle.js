/**
 * Chronicle Page - CC Chronicle session browser with tabs
 */
const ChroniclePage = (() => {
  let activeTab = 'sessions';

  function render(container) {
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Chronicle</h2>
      <div class="tabs">
        <button class="tab active" data-tab="sessions">Sessions</button>
        <button class="tab" data-tab="search">Search</button>
        <button class="tab" data-tab="topics">Topics</button>
        <button class="tab" data-tab="stats">Stats</button>
      </div>
      <div id="chronicle-body"></div>
    `;

    container.querySelectorAll('.tab').forEach(tab => {
      tab.addEventListener('click', () => {
        container.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
        tab.classList.add('active');
        activeTab = tab.dataset.tab;
        _renderTab();
      });
    });

    _renderTab();
  }

  function _renderTab() {
    switch (activeTab) {
      case 'sessions': _loadSessions(); break;
      case 'search': _renderSearch(); break;
      case 'topics': _loadTopics(); break;
      case 'stats': _loadStats(); break;
    }
  }

  async function _loadSessions() {
    const body = document.getElementById('chronicle-body');
    body.innerHTML = '<div style="text-align:center;padding:20px;"><div class="spinner"></div></div>';

    try {
      const result = await Gateway.sendRequest('chronicle.sessions', {});
      const sessions = result.sessions || result || [];

      if (sessions.length === 0) {
        body.innerHTML = '<div class="empty-state">No chronicle sessions</div>';
        return;
      }

      body.innerHTML = sessions.map(s => {
        // Chronicle API returns nested {session: {...}, summary: {...}}
        const title = (s.summary && s.summary.title) || s.title || 'Untitled';
        const machine = (s.session && s.session.machine) || s.machine || '';
        const date = (s.session && s.session.started_at) || s.date || s.created_at;
        const summaryText = (s.summary && s.summary.summary) || (typeof s.summary === 'string' ? s.summary : '');
        return `
        <div class="card">
          <div class="card-header">
            <div>
              <span class="card-title">${Utils.esc(title)}</span>
              <span class="text-sm text-muted" style="margin-left:8px;">${Utils.esc(machine)}</span>
            </div>
            <span class="text-sm text-muted">${Utils.safeTimestamp(date) ? new Date(date).toLocaleDateString() : '\u2014'}</span>
          </div>
          ${summaryText ? `<div class="text-sm">${Utils.esc(summaryText)}</div>` : ''}
        </div>
      `;
      }).join('');
    } catch (e) {
      body.innerHTML = '<div class="empty-state">Failed to load chronicle sessions</div>';
    }
  }

  function _renderSearch() {
    const body = document.getElementById('chronicle-body');
    body.innerHTML = `
      <div class="flex gap-2 mb-4">
        <input id="chron-query" class="input" placeholder="Search sessions..." type="text">
        <button id="chron-search-btn" class="btn btn-primary">Search</button>
      </div>
      <div id="chron-results"></div>
    `;

    document.getElementById('chron-search-btn').addEventListener('click', _doSearch);
    document.getElementById('chron-query').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') _doSearch();
    });
  }

  async function _doSearch() {
    const query = document.getElementById('chron-query').value.trim();
    if (!query) return;

    const results = document.getElementById('chron-results');
    results.innerHTML = '<div class="spinner"></div>';

    try {
      const result = await Gateway.sendRequest('chronicle.search', { query });
      const items = result.results || result.sessions || result || [];

      if (items.length === 0) {
        results.innerHTML = '<div class="text-muted text-sm">No results</div>';
        return;
      }

      results.innerHTML = items.map(s => {
        const title = (s.summary && s.summary.title) || s.title || 'Untitled';
        const machine = (s.session && s.session.machine) || s.machine || '';
        const date = (s.session && s.session.started_at) || s.date || '';
        return `
        <div class="card">
          <div class="card-title">${Utils.esc(title)}</div>
          <div class="text-sm text-muted">${Utils.esc(machine)} | ${date}</div>
          ${s.snippet ? `<div class="text-sm mt-2 pre-wrap">${Utils.esc(s.snippet)}</div>` : ''}
        </div>
      `;
      }).join('');
    } catch (e) {
      results.innerHTML = '<div style="color:var(--danger);" class="text-sm">Search failed</div>';
    }
  }

  async function _loadTopics() {
    const body = document.getElementById('chronicle-body');
    body.innerHTML = '<div style="text-align:center;padding:20px;"><div class="spinner"></div></div>';

    try {
      const result = await Gateway.sendRequest('chronicle.topics', {});
      const topics = result.topics || result || [];

      if (topics.length === 0) {
        body.innerHTML = '<div class="empty-state">No topics available</div>';
        return;
      }

      body.innerHTML = '<div class="flex" style="flex-wrap:wrap;gap:8px;">' +
        topics.map(t => `
          <div class="card" style="flex:0 0 auto;min-width:200px;">
            <div class="card-title">${Utils.esc(t.name || t.topic)}</div>
            <div class="text-sm text-muted">${t.count || 0} sessions</div>
          </div>
        `).join('') + '</div>';
    } catch (e) {
      body.innerHTML = '<div class="empty-state">Failed to load topics</div>';
    }
  }

  async function _loadStats() {
    const body = document.getElementById('chronicle-body');
    body.innerHTML = '<div style="text-align:center;padding:20px;"><div class="spinner"></div></div>';

    try {
      const result = await Gateway.sendRequest('chronicle.stats', {});

      body.innerHTML = `
        <div class="card">
          <table>
            <tr><th style="width:160px;">Total Sessions</th><td>${result.total_sessions || result.total || 0}</td></tr>
            <tr><th>workstation Sessions</th><td>${result.workstation_sessions || 0}</td></tr>
            <tr><th>Remote Sessions</th><td>${result.remote_sessions || 0}</td></tr>
            <tr><th>Avg Length</th><td>${result.avg_length || result.avg_messages || 'N/A'}</td></tr>
            <tr><th>Total Messages</th><td>${result.total_messages || 0}</td></tr>
          </table>
        </div>
      `;
    } catch (e) {
      body.innerHTML = '<div class="empty-state">Failed to load stats</div>';
    }
  }

  return { render };
})();
