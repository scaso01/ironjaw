/**
 * CC Sessions Page - Claude Code session browser with dual-pane layout
 * Uses cc.* methods (Protocol v4) for subprocess management
 */
const CcSessionsPage = (() => {
  let sessions = [];
  let filtered = [];
  let activeSessionId = null;
  let attached = false;
  let streaming = false;
  let streamBuffer = '';
  let streamEl = null;
  let destroyed = false;
  let searchTerm = '';
  let streamHandler = null;

  let activeTab = 'sessions';

  function render(container) {
    destroyed = false;
    activeSessionId = null;
    attached = false;
    streaming = false;
    streamBuffer = '';
    streamEl = null;
    searchTerm = '';
    activeTab = 'sessions';

    container.innerHTML = `
      <div class="chat-layout">
        <div class="session-panel">
          <div class="session-panel-header">
            <span style="font-weight:600;font-size:14px;">CC Sessions</span>
            <button id="cc-new-btn" class="btn btn-sm btn-primary">+ New</button>
          </div>
          <div class="filter-bar" style="padding:4px 8px;margin-bottom:0;">
            <button class="filter-btn active" data-tab="sessions">Sessions</button>
            <button class="filter-btn" data-tab="mailbox">Mailbox <span class="badge badge-danger" id="mailbox-count" style="display:none;"></span></button>
          </div>
          <div style="padding:4px 8px;">
            <input id="cc-search" class="search-input" placeholder="Filter sessions..." type="text" style="width:100%;">
          </div>
          <div id="cc-tab-sessions">
            <ul id="cc-session-list" class="session-list">
              <li class="empty-state text-sm">Loading...</li>
            </ul>
          </div>
          <div id="cc-tab-mailbox" class="hidden">
            <div id="cc-mailbox-list" style="padding:4px 8px;overflow-y:auto;flex:1;">
              ${Utils.spinner()}
            </div>
          </div>
        </div>
        <div class="chat-main">
          <div id="cc-messages" class="chat-messages">
            <div class="empty-state">
              <div class="empty-state-title">Select a CC session to attach</div>
            </div>
          </div>
          <div class="chat-footer">
            <div id="cc-context-bar" class="context-bar" style="display:none;padding:0 8px;margin-bottom:4px"></div>
            <div class="chat-input-bar">
              <textarea id="cc-input" class="textarea" placeholder="Type a message..." rows="1"></textarea>
              <button id="cc-send" class="btn btn-primary">Send</button>
              <button id="cc-abort" class="btn btn-danger hidden">Abort</button>
              <button id="cc-detach" class="btn btn-sm hidden" style="margin-left:4px;">Detach</button>
            </div>
          </div>
        </div>
      </div>
      <div id="cc-create-dialog" class="hidden" style="position:fixed;inset:0;background:rgba(0,0,0,0.5);z-index:300;display:flex;align-items:center;justify-content:center;">
        <div class="card" style="width:400px;max-width:90vw;">
          <div class="card-header"><span class="card-title">New CC Session</span></div>
          <div style="padding:12px;display:flex;flex-direction:column;gap:12px;">
            <div>
              <label class="text-sm" style="display:block;margin-bottom:4px;">Project Path</label>
              <input id="cc-create-project" class="input" placeholder="/path/to/project" style="width:100%;">
            </div>
            <div>
              <label class="text-sm" style="display:block;margin-bottom:4px;">Working Directory</label>
              <input id="cc-create-cwd" class="input" placeholder="/path/to/cwd (optional)" style="width:100%;">
            </div>
            <div>
              <label class="text-sm" style="display:block;margin-bottom:4px;">Model</label>
              <input id="cc-create-model" class="input" placeholder="default" style="width:100%;">
            </div>
            <div class="flex gap-2" style="justify-content:flex-end;">
              <button id="cc-create-cancel" class="btn">Cancel</button>
              <button id="cc-create-submit" class="btn btn-primary">Create</button>
            </div>
          </div>
        </div>
      </div>
    `;

    // Event listeners
    document.getElementById('cc-new-btn').addEventListener('click', _showCreateDialog);
    document.getElementById('cc-send').addEventListener('click', _sendMessage);
    document.getElementById('cc-abort').addEventListener('click', _abortChat);
    document.getElementById('cc-detach').addEventListener('click', _detachSession);
    document.getElementById('cc-create-cancel').addEventListener('click', _hideCreateDialog);
    document.getElementById('cc-create-submit').addEventListener('click', _createSession);

    // Tab switching
    container.querySelectorAll('.filter-btn[data-tab]').forEach(btn => {
      btn.addEventListener('click', () => {
        container.querySelectorAll('.filter-btn[data-tab]').forEach(b => b.classList.remove('active'));
        btn.classList.add('active');
        activeTab = btn.dataset.tab;
        document.getElementById('cc-tab-sessions').classList.toggle('hidden', activeTab !== 'sessions');
        document.getElementById('cc-tab-mailbox').classList.toggle('hidden', activeTab !== 'mailbox');
        if (activeTab === 'mailbox') _loadMailbox();
      });
    });

    document.getElementById('cc-search').addEventListener('input', (e) => {
      searchTerm = e.target.value.toLowerCase();
      _applyFilter();
      _renderSessionList();
    });

    const input = document.getElementById('cc-input');
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        _sendMessage();
      }
    });
    input.addEventListener('input', () => {
      input.style.height = 'auto';
      input.style.height = Math.min(input.scrollHeight, 120) + 'px';
    });

    // Register stream handler
    streamHandler = _onStreamEvent;
    Gateway.onEvent("cc.chat", streamHandler);

    _loadSessions();
  }

  function destroy() {
    destroyed = true;
    if (streamHandler) {
      Gateway.offEvent("cc.chat", streamHandler);
      streamHandler = null;
    }
    if (attached && activeSessionId) {
      Gateway.sendRequest('cc.detach', { session_id: activeSessionId }).catch(() => {});
    }
  }

  // ── Session List ──────────────────────────────────────────────────

  async function _loadSessions() {
    const list = document.getElementById('cc-session-list');
    if (!list) return;
    list.innerHTML = '<li class="empty-state text-sm">Loading...</li>';

    try {
      const result = await Gateway.sendRequest('cc.sessions', {});
      sessions = result.sessions || result || [];
      // Sort: active first, then by lastActivity descending
      sessions.sort((a, b) => {
        if (a.active && !b.active) return -1;
        if (!a.active && b.active) return 1;
        const aRaw = Utils.safeTimestamp(a.lastActivity) || Utils.safeTimestamp(a.updatedAt);
        const bRaw = Utils.safeTimestamp(b.lastActivity) || Utils.safeTimestamp(b.updatedAt);
        if (!aRaw && !bRaw) return 0;
        if (!aRaw) return 1;
        if (!bRaw) return -1;
        return new Date(bRaw).getTime() - new Date(aRaw).getTime();
      });
      _applyFilter();
      _renderSessionList();
    } catch (e) {
      list.innerHTML = '<li class="empty-state text-sm">Failed to load CC sessions</li>';
    }
  }

  function _applyFilter() {
    if (!searchTerm) {
      filtered = sessions;
    } else {
      filtered = sessions.filter(s =>
        (s.title || '').toLowerCase().includes(searchTerm) ||
        (s.project || '').toLowerCase().includes(searchTerm) ||
        (s.machine || '').toLowerCase().includes(searchTerm)
      );
    }
  }

  function _renderSessionList() {
    const list = document.getElementById('cc-session-list');
    if (!list) return;

    if (filtered.length === 0) {
      list.innerHTML = '<li class="empty-state text-sm">No sessions found</li>';
      return;
    }

    list.innerHTML = filtered.map(s => {
      const id = s.sessionId || s.id || '';
      const isActive = s.active;
      const pct = s.contextPct || 0;
      const forkPrefix = s.parent_id ? '<span style="color:var(--purple);margin-right:4px;">&#9095;</span>' : '';
      const forkIndent = s.parent_id ? 'padding-left:24px;' : '';
      const worktreeBadge = s.spawn_mode === 'Worktree'
        ? '<span class="chip chip-purple" style="font-size:10px;margin-left:6px;">WT</span>' : '';
      const forkCount = (s.fork_count || 0) > 0
        ? ` <span class="badge badge-info">${s.fork_count} forks</span>` : '';
      return `
      <li class="session-item ${id === activeSessionId ? 'active' : ''}" data-id="${Utils.esc(id)}" style="${forkIndent}">
        <div class="flex items-center gap-2">
          ${isActive ? '<span class="badge badge-success" style="width:8px;height:8px;min-width:8px;border-radius:50%;padding:0;"></span>' : ''}
          <div class="session-item-title" style="flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">${forkPrefix}${Utils.esc(s.title || 'Untitled')}${worktreeBadge}</div>
          ${forkCount}
        </div>
        <div class="text-xs text-muted" style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">${Utils.esc(s.project || '')}</div>
        <div class="flex items-center justify-between" style="margin-top:2px;">
          ${s.machine ? `<span class="chip text-xs">${Utils.esc(s.machine)}</span>` : '<span></span>'}
          <span class="text-xs text-muted">${Utils.timeAgo(s.lastActivity || s.updatedAt)}</span>
        </div>
        ${pct > 0 ? `<div class="context-bar" style="margin-top:2px">
          <div class="context-pct"><div class="context-pct-fill" style="width:${pct}%;background:${pct > 75 ? 'var(--danger)' : pct > 50 ? 'var(--warning)' : 'var(--accent)'}"></div></div>
          <span class="text-xs">${pct}%</span>
        </div>` : ''}
      </li>`;
    }).join('');

    list.querySelectorAll('.session-item').forEach(el => {
      el.addEventListener('click', () => _selectSession(el.dataset.id));
    });
  }

  // ── Session Attachment ────────────────────────────────────────────

  async function _selectSession(id) {
    // Detach from previous session if attached
    if (attached && activeSessionId && activeSessionId !== id) {
      try {
        await Gateway.sendRequest('cc.detach', { sessionId: activeSessionId });
      } catch (_) {}
    }

    activeSessionId = id;
    attached = false;
    _renderSessionList();

    const messagesEl = document.getElementById('cc-messages');
    messagesEl.innerHTML = '<div style="text-align:center;padding:20px;"><div class="spinner"></div></div>';

    // Load history
    try {
      const result = await Gateway.sendRequest('cc.history', { sessionId: id });
      const messages = result.messages || result || [];
      _renderMessages(messages);
    } catch (e) {
      messagesEl.innerHTML = '<div class="empty-state">Failed to load session history</div>';
      return;
    }

    // Update context bar from session metadata
    try {
      const meta = await Gateway.sendRequest('cc.session.get', { sessionId: id });
      _updateContextBar(meta.contextPct || 0);
    } catch (_) {}

    // Attach to the session for live streaming
    try {
      await Gateway.sendRequest('cc.attach', { sessionId: id });
      attached = true;
      document.getElementById('cc-detach').classList.remove('hidden');
    } catch (_) {
      // Non-active sessions may not support attach -- that's fine
    }
  }

  function _renderMessages(messages) {
    const el = document.getElementById('cc-messages');
    if (!el) return;

    if (messages.length === 0) {
      el.innerHTML = '<div class="empty-state">No messages in this session</div>';
      return;
    }

    el.innerHTML = '<div class="message-list">' + messages.map(m => {
      const role = m.role || 'user';
      if (role === 'tool') {
        return `<div class="message message-tool collapsed tool-collapsible">
          <div>${Utils.esc(m.content || '')}</div>
        </div>`;
      }
      const cls = role === 'user' ? 'message-user' : 'message-assistant';
      return `<div class="message ${cls}">
        <div class="pre-wrap">${Utils.esc(m.content || '')}</div>
      </div>`;
    }).join('') + '</div>';

    el.querySelectorAll('.tool-collapsible').forEach(div => {
      div.addEventListener('click', () => div.classList.toggle('collapsed'));
    });

    el.scrollTop = el.scrollHeight;
  }

  // ── Chat / Streaming ─────────────────────────────────────────────

  async function _sendMessage() {
    const input = document.getElementById('cc-input');
    const text = input.value.trim();
    if (!text || streaming || !activeSessionId) return;

    input.value = '';
    input.style.height = 'auto';

    // Append user message
    _appendMessage('user', text);

    // Show abort, hide send
    streaming = true;
    streamBuffer = '';
    document.getElementById('cc-send').classList.add('hidden');
    document.getElementById('cc-abort').classList.remove('hidden');

    // Create streaming placeholder
    const messagesEl = document.getElementById('cc-messages');
    const listEl = messagesEl.querySelector('.message-list') || messagesEl;
    streamEl = document.createElement('div');
    streamEl.className = 'message message-assistant';
    streamEl.innerHTML = '<div class="pre-wrap"><span class="spinner"></span></div>';
    listEl.appendChild(streamEl);
    messagesEl.scrollTop = messagesEl.scrollHeight;

    // Send via cc.chat -- response streams via onStream
    Gateway.sendRequest('cc.chat', {
      session_id: activeSessionId,
      message: text,
    }).catch(() => {
      // Timeout expected for long-running CC operations
    });
  }

  function _onStreamEvent(data) {
    if (destroyed || !streamEl || !streaming) return;

    if (data.state === 'delta' && data.message && data.message.content) {
      streamBuffer += data.message.content;
      streamEl.querySelector('.pre-wrap').textContent = streamBuffer;
      const messagesEl = document.getElementById('cc-messages');
      if (messagesEl) messagesEl.scrollTop = messagesEl.scrollHeight;
    }

    if (data.state === 'final') {
      if (data.usage) {
        _updateContextBar(data.usage.contextPct || 0);
      }
      _finishStream();
    }
  }

  function _finishStream() {
    streaming = false;
    streamEl = null;
    streamBuffer = '';
    const sendBtn = document.getElementById('cc-send');
    const abortBtn = document.getElementById('cc-abort');
    if (sendBtn) sendBtn.classList.remove('hidden');
    if (abortBtn) abortBtn.classList.add('hidden');
  }

  async function _abortChat() {
    try {
      await Gateway.sendRequest('cc.chat.abort', { session_id: activeSessionId });
    } catch (_) {}
    _finishStream();
  }

  async function _detachSession() {
    if (!activeSessionId) return;
    try {
      await Gateway.sendRequest('cc.detach', { session_id: activeSessionId });
    } catch (_) {}
    attached = false;
    document.getElementById('cc-detach').classList.add('hidden');
  }

  function _appendMessage(role, content) {
    const messagesEl = document.getElementById('cc-messages');
    if (!messagesEl) return;

    let listEl = messagesEl.querySelector('.message-list');
    if (!listEl) {
      messagesEl.innerHTML = '<div class="message-list"></div>';
      listEl = messagesEl.querySelector('.message-list');
    }

    const cls = role === 'user' ? 'message-user' : 'message-assistant';
    const div = document.createElement('div');
    div.className = `message ${cls}`;
    div.innerHTML = `<div class="pre-wrap">${Utils.esc(content)}</div>`;
    listEl.appendChild(div);
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  function _updateContextBar(pct) {
    const el = document.getElementById('cc-context-bar');
    if (!el) return;
    if (pct <= 0) { el.style.display = 'none'; return; }
    el.style.display = 'flex';
    el.innerHTML = `<div class="context-pct"><div class="context-pct-fill" style="width:${pct}%;background:${pct > 75 ? 'var(--danger)' : pct > 50 ? 'var(--warning)' : 'var(--accent)'}"></div></div><span class="text-xs">${pct}%</span>`;
  }

  // ── Create Session Dialog ─────────────────────────────────────────

  function _showCreateDialog() {
    const dialog = document.getElementById('cc-create-dialog');
    dialog.classList.remove('hidden');
    dialog.style.display = 'flex';
    document.getElementById('cc-create-project').value = '';
    document.getElementById('cc-create-cwd').value = '';
    document.getElementById('cc-create-model').value = '';
    document.getElementById('cc-create-project').focus();
  }

  function _hideCreateDialog() {
    const dialog = document.getElementById('cc-create-dialog');
    dialog.classList.add('hidden');
    dialog.style.display = 'none';
  }

  async function _createSession() {
    const project = document.getElementById('cc-create-project').value.trim();
    const cwd = document.getElementById('cc-create-cwd').value.trim();
    const model = document.getElementById('cc-create-model').value.trim();

    if (!project) {
      alert('Project path is required');
      return;
    }

    _hideCreateDialog();

    try {
      const params = { project };
      if (cwd) params.cwd = cwd;
      if (model) params.model = model;
      const result = await Gateway.sendRequest('cc.session.create', params);
      // Reload sessions and select the new one
      await _loadSessions();
      const newId = result.session_id || result.sessionId || result.id;
      if (newId) {
        _selectSession(newId);
      }
    } catch (e) {
      alert('Failed to create CC session: ' + (e.message || e));
    }
  }

  // ── Mailbox ─────────────────────────────────────────────────────

  async function _loadMailbox() {
    const el = document.getElementById('cc-mailbox-list');
    if (!el) return;
    el.innerHTML = Utils.spinner();

    try {
      const result = await Gateway.sendRequest('agent.mailbox.peek', { limit: 50 });
      const messages = result.messages || [];

      // Update badge count
      const badge = document.getElementById('mailbox-count');
      if (badge) {
        const pending = messages.filter(m => !m.claimed_by).length;
        if (pending > 0) {
          badge.textContent = pending;
          badge.style.display = 'inline-block';
        } else {
          badge.style.display = 'none';
        }
      }

      if (messages.length === 0) {
        el.innerHTML = '<div class="empty-state text-sm">No mailbox messages</div>';
        return;
      }

      el.innerHTML = messages.map(msg => _renderMailboxMessage(msg)).join('');

      // Attach claim button listeners
      el.querySelectorAll('.mailbox-claim-btn').forEach(btn => {
        btn.addEventListener('click', async (e) => {
          e.stopPropagation();
          const msgId = btn.dataset.msgId;
          btn.disabled = true;
          btn.textContent = '...';
          try {
            await Gateway.sendRequest('agent.mailbox.claim', { message_id: msgId });
            _loadMailbox();
          } catch (err) {
            alert('Failed to claim: ' + (err.message || err));
            btn.disabled = false;
            btn.textContent = 'Claim';
          }
        });
      });
    } catch (e) {
      el.innerHTML = '<div class="text-sm" style="color:var(--danger);padding:8px;">Failed to load mailbox</div>';
    }
  }

  function _renderMailboxMessage(msg) {
    const priorityClass = { High: 'badge-danger', Normal: 'badge-info', Low: 'badge-warning' }[msg.priority] || 'badge-info';
    const claimStatus = msg.claimed_by ? `<span class="text-xs text-muted">Claimed by ${Utils.esc(msg.claimed_by)}</span>` : '';
    const claimBtn = !msg.claimed_by
      ? `<button class="btn btn-sm btn-primary mailbox-claim-btn" data-msg-id="${Utils.esc(msg.id)}">Claim</button>` : '';

    return `
    <div class="card" style="margin-bottom:8px;padding:12px;">
      <div style="display:flex;align-items:center;gap:8px;">
        <span class="badge ${priorityClass}">${Utils.esc(msg.priority || 'Normal')}</span>
        <span class="text-sm"><strong>${Utils.esc(msg.from || 'unknown')}</strong>: ${Utils.esc(msg.subject || msg.content || '')}</span>
        <span style="margin-left:auto;display:flex;align-items:center;gap:6px;">
          ${claimStatus}
          ${claimBtn}
        </span>
      </div>
    </div>`;
  }

  return { render, destroy };
})();
