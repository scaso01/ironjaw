/**
 * Brain Page - RAG memory management with 5 tabs + stats strip
 */
const BrainPage = (() => {
  let _container;
  let _activeTab = 'memories';
  let _statsInterval;

  // ── Render ──────────────────────────────────────────────────────
  function render(container) {
    _container = container;
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Brain</h2>

      <!-- Stats strip -->
      <div class="card mb-4" id="brain-stats">
        <div class="flex gap-4 flex-wrap" id="brain-stats-inner">${Utils.spinner()}</div>
      </div>

      <!-- Tab bar -->
      <div class="filter-bar mb-4">
        <button class="filter-btn active" data-tab="memories">Memories</button>
        <button class="filter-btn" data-tab="context">Context Variables</button>
        <button class="filter-btn" data-tab="summaries">Summaries</button>
        <button class="filter-btn" data-tab="timeline">Timeline</button>
        <button class="filter-btn" data-tab="pending">Pending</button>
      </div>

      <!-- Tab content areas -->
      <div class="tab-content active" id="tab-memories"></div>
      <div class="tab-content" id="tab-context"></div>
      <div class="tab-content" id="tab-summaries"></div>
      <div class="tab-content" id="tab-timeline"></div>
      <div class="tab-content" id="tab-pending"></div>
    `;

    // Attach tab switching
    _container.querySelectorAll('.filter-btn[data-tab]').forEach(btn => {
      btn.addEventListener('click', () => {
        const tab = btn.dataset.tab;
        _container.querySelectorAll('.filter-btn[data-tab]').forEach(b => b.classList.remove('active'));
        btn.classList.add('active');
        _container.querySelectorAll('.tab-content').forEach(tc => tc.classList.remove('active'));
        const el = document.getElementById('tab-' + tab);
        if (el) el.classList.add('active');
        _activeTab = tab;
        _renderTab(tab);
      });
    });

    // Load stats + initial tab
    _loadStats();
    _statsInterval = setInterval(_loadStats, 30000);
    _renderTab(_activeTab);
  }

  function destroy() {
    if (_statsInterval) { clearInterval(_statsInterval); _statsInterval = null; }
  }

  // ── Stats strip ────────────────────────────────────────────────
  async function _loadStats() {
    try {
      const s = await Gateway.sendRequest('brain.status', {});
      const el = document.getElementById('brain-stats-inner');
      if (!el) return;

      const memCount = s.memory_count || 0;
      const tempActive = s.temporal_active || 0;
      const tempExpired = s.temporal_expired || 0;
      const ctxCount = (s.context_vars && s.context_vars.count) || 0;
      const ctxTokens = (s.context_vars && s.context_vars.total_tokens) || 0;

      el.innerHTML = [
        _statBox('Memories', memCount),
        _statBox('Temporal Active', tempActive),
        _statBox('Temporal Expired', tempExpired),
        _statBox('Context Vars', ctxCount),
        _statBox('Context Tokens', ctxTokens.toLocaleString()),
      ].join('');

      // Load dream status separately
      _loadDreamStatus();
    } catch (e) {
      const el = document.getElementById('brain-stats-inner');
      if (el) el.innerHTML = `<div class="text-sm" style="color:var(--danger);">${Utils.esc(e.message || 'Failed to load stats')}</div>`;
    }
  }

  function _statBox(label, value) {
    return `<div style="min-width:100px;"><div class="text-sm text-muted">${Utils.esc(label)}</div><div style="font-size:20px;font-weight:600;">${Utils.esc(String(value))}</div></div>`;
  }

  // ── Dream Status ───────────────────────────────────────────────
  async function _loadDreamStatus() {
    try {
      const dream = await Gateway.sendRequest('brain.dream.status', {});
      const el = document.getElementById('brain-stats-inner');
      if (!el) return;

      const dreamHtml = _renderDreamStatus(dream);
      // Append dream status to existing stats
      const dreamEl = document.createElement('div');
      dreamEl.id = 'brain-dream-stat';
      dreamEl.innerHTML = dreamHtml;
      // Remove previous dream stat if exists
      const prev = document.getElementById('brain-dream-stat');
      if (prev) prev.remove();
      el.appendChild(dreamEl);
    } catch (e) {
      // Dream status not available -- ignore silently
    }
  }

  function _renderDreamStatus(dream) {
    const gates = [
      { icon: '&#128339;', label: 'Time', met: dream.time_gate_met },
      { icon: '&#128172;', label: 'Sessions', met: dream.session_gate_met },
      { icon: '&#128274;', label: 'Unlocked', met: !dream.locked },
    ];
    const gateHtml = gates.map(g =>
      `<span title="${g.label}" style="color: ${g.met ? 'var(--green)' : 'var(--text-muted)'}">${g.icon}</span>`
    ).join(' ');

    const phases = ['Orient', 'Gather', 'Consolidate', 'Prune'];
    const phaseHtml = phases.map((p, i) =>
      `<span style="color: ${i === dream.current_phase ? 'var(--accent)' : 'var(--text-muted)'}">${p}</span>`
    ).join(' &gt; ');

    const active = dream.active;
    const pulseHtml = active ? '<span class="pulse-active"></span>' : '';
    const statusHtml = active ? phaseHtml : ('Last: ' + _timeAgo(dream.last_dream));

    return `
      <div style="min-width:160px;">
        <div class="text-sm text-muted">Dream ${pulseHtml}</div>
        <div style="font-size:14px;font-weight:500;margin:4px 0;">${statusHtml}</div>
        <div class="text-xs">${gateHtml}</div>
      </div>`;
  }

  function _timeAgo(timestamp) {
    if (!timestamp) return 'never';
    if (typeof Utils !== 'undefined' && Utils.timeAgo) return Utils.timeAgo(timestamp);
    const diff = Date.now() - new Date(timestamp).getTime();
    const hours = Math.floor(diff / 3600000);
    if (hours < 1) return 'just now';
    if (hours < 24) return hours + 'h ago';
    return Math.floor(hours / 24) + 'd ago';
  }

  // ── Tab router ─────────────────────────────────────────────────
  function _renderTab(tab) {
    switch (tab) {
      case 'memories': _renderMemories(); break;
      case 'context': _renderContext(); break;
      case 'summaries': _renderSummaries(); break;
      case 'timeline': _renderTimeline(); break;
      case 'pending': _renderPending(); break;
    }
  }

  // ── Shared memory card renderer ────────────────────────────────
  function _memoryCard(m) {
    const id = m.id != null ? m.id : '';
    const content = m.content || m.text || m.snippet || '';
    const memType = m.memory_type || '';
    const importance = m.importance != null ? Math.round(m.importance * 100) : null;
    const strength = m.strength != null ? Math.round(m.strength * 100) : null;
    const score = m.score || m.similarity || null;
    const validFrom = m.valid_from || '';
    const validTo = m.valid_to || '';
    const source = m.source || '';

    let metaHtml = '<div class="memory-meta">';
    if (memType) metaHtml += `<span class="chip chip-category">${Utils.esc(memType)}</span>`;
    if (importance != null) metaHtml += `<span class="text-xs">Importance: ${importance}%</span>`;
    if (strength != null) {
      metaHtml += `<span class="text-xs">Strength:</span><span class="strength-bar"><span class="strength-fill" style="width:${strength}%"></span></span>`;
    }
    if (score != null) metaHtml += `<span class="text-xs text-muted">Score: ${score.toFixed(3)}</span>`;
    if (source) metaHtml += `<span class="text-xs text-muted">Src: ${Utils.esc(source.substring(0, 20))}</span>`;
    metaHtml += '</div>';

    let validityHtml = '';
    if (validFrom || validTo) {
      const from = Utils.safeTimestamp(validFrom) ? new Date(validFrom).toLocaleString() : 'anytime';
      const to = Utils.safeTimestamp(validTo) ? new Date(validTo).toLocaleString() : 'ongoing';
      validityHtml = `<div class="memory-validity">Valid: ${Utils.esc(from)} &mdash; ${Utils.esc(to)}</div>`;
    }

    return `
      <div class="card mt-2">
        ${metaHtml}
        <div class="pre-wrap text-sm">${Utils.esc(content)}</div>
        ${validityHtml}
        ${id !== '' ? `<div style="margin-top:8px;text-align:right;"><button class="btn btn-sm btn-danger forget-btn" data-memory-id="${Utils.esc(id)}">Forget</button></div>` : ''}
      </div>`;
  }

  function _attachForgetButtons(container) {
    container.querySelectorAll('.forget-btn').forEach(btn => {
      btn.addEventListener('click', () => _forget(btn.dataset.memoryId));
    });
  }

  async function _forget(id) {
    if (!confirm('Forget this memory?')) return;
    try {
      const numericId = typeof id === 'string' ? parseInt(id, 10) : id;
      await Gateway.sendRequest('brain.forget', { memory_id: numericId });
      // Refresh current tab
      _renderTab(_activeTab);
      _loadStats();
    } catch (e) {
      alert('Failed to forget: ' + (e.message || e));
    }
  }

  // ── Tab 1: Memories ────────────────────────────────────────────
  function _renderMemories() {
    const el = document.getElementById('tab-memories');
    if (!el) return;
    el.innerHTML = `
      <div class="card">
        <div class="card-header"><span class="card-title">Search Memories</span></div>
        <div class="flex gap-2 items-center flex-wrap">
          <input id="brain-query" class="input" placeholder="Search memories..." type="text" style="flex:1;min-width:200px;">
          <select id="brain-detail" style="padding:6px 10px;border-radius:var(--radius);border:1px solid var(--border);background:var(--bg-primary);color:var(--text-secondary);font-size:13px;outline:none;">
            <option value="full">Full</option>
            <option value="compact">Compact</option>
            <option value="deep">Deep</option>
          </select>
          <button id="brain-search-btn" class="btn btn-primary">Search</button>
        </div>
        <div id="brain-results" class="mt-2"></div>
      </div>

      <div class="card">
        <div class="card-header"><span class="card-title">Add Memory</span></div>
        <div class="form-group">
          <textarea id="brain-content" class="textarea" placeholder="Enter memory content..." rows="4" style="width:100%;"></textarea>
        </div>
        <button id="brain-remember-btn" class="btn btn-primary">Remember</button>
        <div id="brain-add-result" class="mt-2"></div>
      </div>
    `;

    document.getElementById('brain-search-btn').addEventListener('click', _searchMemories);
    document.getElementById('brain-query').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') _searchMemories();
    });
    document.getElementById('brain-remember-btn').addEventListener('click', _remember);
  }

  async function _searchMemories() {
    const query = document.getElementById('brain-query').value.trim();
    if (!query) return;
    const detail = document.getElementById('brain-detail').value;
    const results = document.getElementById('brain-results');
    results.innerHTML = Utils.spinner();

    try {
      const result = await Gateway.sendRequest('brain.recall', { query, detail });
      const memories = result.memories || result || [];

      if (!Array.isArray(memories) || memories.length === 0) {
        results.innerHTML = Utils.emptyState('No results found');
        return;
      }

      results.innerHTML = memories.map(m => _memoryCard(m)).join('');
      _attachForgetButtons(results);
    } catch (e) {
      results.innerHTML = `<div style="color:var(--danger);" class="text-sm mt-2">${Utils.esc(e.message || 'Search failed')}</div>`;
    }
  }

  async function _remember() {
    const content = document.getElementById('brain-content').value.trim();
    if (!content) return;
    const resultEl = document.getElementById('brain-add-result');
    resultEl.innerHTML = Utils.spinner();

    try {
      await Gateway.sendRequest('brain.remember', { content });
      resultEl.innerHTML = '<div style="color:var(--green);" class="text-sm">Memory stored.</div>';
      document.getElementById('brain-content').value = '';
      _loadStats();
      setTimeout(() => { resultEl.innerHTML = ''; }, 3000);
    } catch (e) {
      resultEl.innerHTML = `<div style="color:var(--danger);" class="text-sm">${Utils.esc(e.message || 'Failed to store memory')}</div>`;
    }
  }

  // ── Tab 2: Context Variables ───────────────────────────────────
  function _renderContext() {
    const el = document.getElementById('tab-context');
    if (!el) return;
    el.innerHTML = `
      <div class="card">
        <div class="card-header"><span class="card-title">Context Variables</span></div>
        <div id="ctx-table-wrap" class="table-wrap">${Utils.spinner()}</div>
      </div>

      <div class="card">
        <div class="card-header"><span class="card-title">Add Context Variable</span></div>
        <div class="flex gap-2 mb-2">
          <input id="ctx-name" class="input" placeholder="Variable name..." type="text" style="width:200px;">
        </div>
        <div class="mb-2">
          <textarea id="ctx-content" class="textarea" placeholder="Variable content..." rows="4" style="width:100%;"></textarea>
        </div>
        <button id="ctx-add-btn" class="btn btn-primary">Add Variable</button>
        <div id="ctx-add-result" class="mt-2"></div>
      </div>
    `;

    document.getElementById('ctx-add-btn').addEventListener('click', _addContextVar);
    _loadContextVars();
  }

  async function _loadContextVars() {
    const wrap = document.getElementById('ctx-table-wrap');
    if (!wrap) return;
    wrap.innerHTML = Utils.spinner();

    try {
      const result = await Gateway.sendRequest('brain.context.list', {});
      const vars = result.variables || [];

      if (vars.length === 0) {
        wrap.innerHTML = Utils.emptyState('No context variables');
        return;
      }

      wrap.innerHTML = `
        <table>
          <thead><tr>
            <th>Name</th>
            <th>Tokens</th>
            <th>Loaded</th>
            <th>Actions</th>
          </tr></thead>
          <tbody>
            ${vars.map(v => {
              const name = v.name || '';
              const tokens = v.token_count || 0;
              const loaded = !!v.loaded;
              return `<tr>
                <td class="mono text-sm">${Utils.esc(name)}</td>
                <td>${tokens.toLocaleString()}</td>
                <td>
                  <label class="toggle-switch">
                    <input type="checkbox" ${loaded ? 'checked' : ''} data-ctx-name="${Utils.esc(name)}" class="ctx-toggle">
                    <span class="toggle-slider"></span>
                  </label>
                </td>
                <td>
                  <button class="btn btn-sm ctx-peek-btn" data-ctx-name="${Utils.esc(name)}">Peek</button>
                  <button class="btn btn-sm btn-danger ctx-delete-btn" data-ctx-name="${Utils.esc(name)}">Delete</button>
                </td>
              </tr>`;
            }).join('')}
          </tbody>
        </table>
      `;

      // Toggle listeners
      wrap.querySelectorAll('.ctx-toggle').forEach(input => {
        input.addEventListener('change', () => {
          _toggleContextVar(input.dataset.ctxName, input.checked);
        });
      });

      // Peek listeners
      wrap.querySelectorAll('.ctx-peek-btn').forEach(btn => {
        btn.addEventListener('click', () => _peekContextVar(btn.dataset.ctxName));
      });

      // Delete listeners
      wrap.querySelectorAll('.ctx-delete-btn').forEach(btn => {
        btn.addEventListener('click', () => _deleteContextVar(btn.dataset.ctxName));
      });

    } catch (e) {
      wrap.innerHTML = `<div style="color:var(--danger);" class="text-sm">${Utils.esc(e.message || 'Failed to load context variables')}</div>`;
    }
  }

  async function _toggleContextVar(name, loaded) {
    try {
      await Gateway.sendRequest('brain.context.load', { name, loaded });
      _loadStats();
    } catch (e) {
      alert('Failed to toggle: ' + (e.message || e));
      _loadContextVars(); // revert UI
    }
  }

  async function _peekContextVar(name) {
    // Show modal overlay with content
    try {
      const result = await Gateway.sendRequest('brain.context.peek', { name, offset: 0, limit: 2000 });
      const content = result.content || '';
      const totalLen = result.total_length || content.length;

      // Create modal
      const overlay = document.createElement('div');
      overlay.className = 'permission-overlay';
      overlay.innerHTML = `
        <div class="permission-modal" style="max-width:700px;max-height:80vh;display:flex;flex-direction:column;">
          <div class="permission-header">${Utils.esc(name)}</div>
          <div class="text-xs text-muted mb-2">${content.length} / ${totalLen} characters shown</div>
          <pre style="flex:1;overflow:auto;max-height:50vh;font-size:12px;">${Utils.esc(content)}</pre>
          <div class="permission-actions mt-4">
            <button class="btn" id="peek-close-btn">Close</button>
          </div>
        </div>
      `;
      document.body.appendChild(overlay);

      overlay.querySelector('#peek-close-btn').addEventListener('click', () => overlay.remove());
      overlay.addEventListener('click', (e) => { if (e.target === overlay) overlay.remove(); });
    } catch (e) {
      alert('Peek failed: ' + (e.message || e));
    }
  }

  async function _deleteContextVar(name) {
    if (!confirm('Delete context variable "' + name + '"?')) return;
    try {
      await Gateway.sendRequest('brain.context.delete', { name });
      _loadContextVars();
      _loadStats();
    } catch (e) {
      alert('Delete failed: ' + (e.message || e));
    }
  }

  async function _addContextVar() {
    const name = document.getElementById('ctx-name').value.trim();
    const content = document.getElementById('ctx-content').value.trim();
    if (!name || !content) return;
    const resultEl = document.getElementById('ctx-add-result');
    resultEl.innerHTML = Utils.spinner();

    try {
      await Gateway.sendRequest('brain.context.set', { name, content });
      resultEl.innerHTML = '<div style="color:var(--green);" class="text-sm">Context variable stored.</div>';
      document.getElementById('ctx-name').value = '';
      document.getElementById('ctx-content').value = '';
      _loadContextVars();
      _loadStats();
      setTimeout(() => { resultEl.innerHTML = ''; }, 3000);
    } catch (e) {
      resultEl.innerHTML = `<div style="color:var(--danger);" class="text-sm">${Utils.esc(e.message || 'Failed to add variable')}</div>`;
    }
  }

  // ── Tab 3: Summaries ───────────────────────────────────────────
  function _renderSummaries() {
    const el = document.getElementById('tab-summaries');
    if (!el) return;
    el.innerHTML = `
      <div class="card">
        <div class="card-header"><span class="card-title">Session Summaries</span></div>
        <div class="flex gap-2 items-center flex-wrap mb-2">
          <input id="summary-session-id" class="input" placeholder="Session ID (optional)..." type="text" style="flex:1;min-width:200px;">
          <button id="summary-search-btn" class="btn btn-primary">Search</button>
        </div>
        <div id="summary-results">${Utils.spinner()}</div>
      </div>
    `;

    document.getElementById('summary-search-btn').addEventListener('click', _searchSummaries);
    document.getElementById('summary-session-id').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') _searchSummaries();
    });
    // Auto-load on tab open
    _searchSummaries();
  }

  async function _searchSummaries() {
    const sessionId = document.getElementById('summary-session-id').value.trim() || undefined;
    const results = document.getElementById('summary-results');
    results.innerHTML = Utils.spinner();

    try {
      const params = { limit: 20 };
      if (sessionId) params.session_id = sessionId;
      const result = await Gateway.sendRequest('brain.summaries', params);
      const memories = result.memories || result || [];

      if (!Array.isArray(memories) || memories.length === 0) {
        results.innerHTML = Utils.emptyState('No summaries found');
        return;
      }

      results.innerHTML = memories.map(m => _memoryCard(m)).join('');
      _attachForgetButtons(results);
    } catch (e) {
      results.innerHTML = `<div style="color:var(--danger);" class="text-sm">${Utils.esc(e.message || 'Failed to load summaries')}</div>`;
    }
  }

  // ── Tab 4: Timeline ────────────────────────────────────────────
  function _renderTimeline() {
    const el = document.getElementById('tab-timeline');
    if (!el) return;

    // Default to now in datetime-local format
    const now = new Date();
    const localIso = new Date(now.getTime() - now.getTimezoneOffset() * 60000).toISOString().slice(0, 16);

    el.innerHTML = `
      <div class="card">
        <div class="card-header"><span class="card-title">Timeline Query</span></div>
        <div class="flex gap-2 items-center flex-wrap mb-2">
          <input id="timeline-at" type="datetime-local" class="input" value="${localIso}" style="min-width:220px;">
          <label class="text-sm text-muted">Limit:</label>
          <input id="timeline-limit" type="number" class="input" value="10" min="1" max="50" style="width:70px;">
          <button id="timeline-search-btn" class="btn btn-primary">Query</button>
        </div>
        <div id="timeline-results">${Utils.emptyState('Enter a date and click Query')}</div>
      </div>
    `;

    document.getElementById('timeline-search-btn').addEventListener('click', _searchTimeline);
  }

  async function _searchTimeline() {
    const atVal = document.getElementById('timeline-at').value;
    const limitVal = parseInt(document.getElementById('timeline-limit').value, 10) || 10;
    if (!atVal) return;

    const isoDate = new Date(atVal).toISOString();
    const results = document.getElementById('timeline-results');
    results.innerHTML = Utils.spinner();

    try {
      const result = await Gateway.sendRequest('brain.timeline', { at: isoDate, limit: limitVal });
      const memories = result.memories || result || [];

      if (!Array.isArray(memories) || memories.length === 0) {
        results.innerHTML = Utils.emptyState('No memories found at this time');
        return;
      }

      results.innerHTML = memories.map(m => _memoryCard(m)).join('');
      _attachForgetButtons(results);
    } catch (e) {
      results.innerHTML = `<div style="color:var(--danger);" class="text-sm">${Utils.esc(e.message || 'Timeline query failed')}</div>`;
    }
  }

  // ── Tab 5: Pending ──────────────────────────────────────────
  async function _renderPending() {
    const el = document.getElementById('tab-pending');
    if (!el) return;
    el.innerHTML = `<div style="padding:16px">${Utils.spinner()}</div>`;

    try {
      const result = await Gateway.sendRequest('brain.pending.list', { limit: 50 });
      const memories = result.memories || [];

      if (!Array.isArray(memories) || memories.length === 0) {
        el.innerHTML = Utils.emptyState('No pending memories awaiting review');
        return;
      }

      el.innerHTML = `<div style="padding:8px 0">${memories.map(m => _pendingCard(m)).join('')}</div>`;

      // Approve listeners
      el.querySelectorAll('.approve-pending-btn').forEach(btn => {
        btn.addEventListener('click', async () => {
          const id = parseInt(btn.dataset.pendingId, 10);
          btn.disabled = true;
          btn.textContent = '...';
          try {
            await Gateway.sendRequest('brain.pending.approve', { id });
            _renderPending();
            _loadStats();
          } catch (e) {
            alert('Failed to approve: ' + (e.message || e));
            btn.disabled = false;
            btn.textContent = 'Approve';
          }
        });
      });

      // Reject listeners
      el.querySelectorAll('.reject-pending-btn').forEach(btn => {
        btn.addEventListener('click', async () => {
          const id = parseInt(btn.dataset.pendingId, 10);
          if (!confirm('Reject this memory permanently?')) return;
          btn.disabled = true;
          btn.textContent = '...';
          try {
            await Gateway.sendRequest('brain.pending.reject', { id });
            _renderPending();
            _loadStats();
          } catch (e) {
            alert('Failed to reject: ' + (e.message || e));
            btn.disabled = false;
            btn.textContent = 'Reject';
          }
        });
      });
    } catch (e) {
      el.innerHTML = `<div style="color:var(--danger);padding:16px" class="text-sm">${Utils.esc(e.message || 'Failed to load pending memories')}</div>`;
    }
  }

  function _pendingCard(m) {
    const id = m.id != null ? m.id : '';
    const source = m.source || 'unknown';
    const memType = m.memory_type || 'fact';
    const content = m.content || '';
    const created = m.created_at ? new Date(m.created_at).toLocaleString() : '';
    const importance = m.importance != null ? Math.round(m.importance * 100) : null;
    const confidence = m.confidence != null ? Math.round(m.confidence * 100) : null;

    let metaHtml = '<div class="memory-meta">';
    metaHtml += `<span class="chip chip-category">${Utils.esc(memType)}</span>`;
    metaHtml += `<span class="text-xs text-muted">from: ${Utils.esc(source)}</span>`;
    if (importance != null) metaHtml += `<span class="text-xs">Imp: ${importance}%</span>`;
    if (confidence != null) metaHtml += `<span class="text-xs">Conf: ${confidence}%</span>`;
    if (created) metaHtml += `<span class="text-xs text-muted">${Utils.esc(created)}</span>`;
    metaHtml += '</div>';

    return `
      <div class="card mt-2">
        ${metaHtml}
        <div class="pre-wrap text-sm" style="margin:8px 0">${Utils.esc(content)}</div>
        <div style="display:flex;justify-content:flex-end;gap:8px">
          <button class="btn btn-sm btn-danger reject-pending-btn" data-pending-id="${Utils.esc(String(id))}">Reject</button>
          <button class="btn btn-sm btn-primary approve-pending-btn" data-pending-id="${Utils.esc(String(id))}">Approve</button>
        </div>
      </div>`;
  }

  // ── Public API ─────────────────────────────────────────────────
  return { render, destroy };
})();
