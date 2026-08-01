/**
 * Chat Page - Session list + message thread with streaming
 */
const ChatPage = (() => {
  let sessions = [];
  let activeSessionId = null;
  let streaming = false;
  let streamBuffer = '';
  let streamEl = null;
  let destroyed = false;
  let contextPct = 0;
  let stallTimer = null;
  let searchDebounce = null;

  function render(container) {
    destroyed = false;
    container.innerHTML = `
      <div class="chat-layout">
        <div class="session-panel">
          <div class="session-panel-header">
            <span style="font-weight:600;font-size:14px;">Sessions</span>
            <button id="new-session-btn" class="btn btn-sm btn-primary">+ New</button>
          </div>
          <ul id="session-list" class="session-list">
            <li class="empty-state text-sm">Loading...</li>
          </ul>
        </div>
        <div class="chat-main">
          <div id="chat-activity-bar" class="activity-bar hidden">
            <span class="spinner spinner-sm"></span>
            <span id="chat-activity-text">Running...</span>
          </div>
          <div id="chat-search-panel" class="chat-search-panel hidden">
            <input id="chat-search-input" class="search-input" type="text" placeholder="Search messages..." />
            <ul id="chat-search-results" class="chat-search-results"></ul>
          </div>
          <div id="chat-messages" class="chat-messages">
            <div class="empty-state">
              <div class="empty-state-title">Select or create a session</div>
            </div>
          </div>
          <div class="chat-footer">
            <div id="chat-context-bar" class="context-bar" style="display:none;padding:0 8px;margin-bottom:4px"></div>
            <div class="quick-actions">
              <button class="chip-action" data-text="Summarize">Summarize</button>
              <button class="chip-action" data-text="Explain">Explain</button>
              <button class="chip-action" data-text="Code Review">Code Review</button>
              <button class="chip-action" data-text="Translate">Translate</button>
            </div>
            <div class="chat-input-bar">
              <textarea id="chat-input" class="textarea" placeholder="Type a message..." rows="1"></textarea>
              <select id="chat-thinking" class="select-sm">
                <option value="">Thinking: Off</option>
                <option value="low">Low</option>
                <option value="medium">Medium</option>
                <option value="high">High</option>
              </select>
              <button id="chat-search-toggle" class="btn btn-sm" title="Search messages">Search</button>
              <button id="chat-send" class="btn btn-primary">Send</button>
              <button id="chat-abort" class="btn btn-danger hidden">Abort</button>
            </div>
          </div>
        </div>
      </div>
    `;

    document.getElementById('new-session-btn').addEventListener('click', _createSession);
    document.getElementById('chat-send').addEventListener('click', _sendMessage);
    document.getElementById('chat-abort').addEventListener('click', _abortStream);

    const input = document.getElementById('chat-input');
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        _sendMessage();
      }
    });

    // Auto-resize textarea
    input.addEventListener('input', () => {
      input.style.height = 'auto';
      input.style.height = Math.min(input.scrollHeight, 120) + 'px';
    });

    // A8: Quick action chips
    container.querySelectorAll('.chip-action').forEach(btn => {
      btn.addEventListener('click', () => {
        const text = btn.dataset.text;
        if (text && activeSessionId) {
          document.getElementById('chat-input').value = text;
          _sendMessage();
        }
      });
    });

    // A3: Search toggle
    document.getElementById('chat-search-toggle').addEventListener('click', _toggleSearch);
    document.getElementById('chat-search-input').addEventListener('input', _onSearchInput);

    // A6: Activity status bar
    Gateway.onEvent('tool.start', _onToolStart);
    Gateway.onEvent('tool.done', _onToolDone);

    Gateway.onEvent('chat', _onChatEvent);
    _loadSessions();
  }

  function destroy() {
    destroyed = true;
    Gateway.offEvent('chat', _onChatEvent);
    Gateway.offEvent('tool.start', _onToolStart);
    Gateway.offEvent('tool.done', _onToolDone);
    if (stallTimer) { clearTimeout(stallTimer); stallTimer = null; }
    if (searchDebounce) { clearTimeout(searchDebounce); searchDebounce = null; }
  }

  async function _loadSessions() {
    try {
      const result = await Gateway.sendRequest('session.list', {});
      sessions = result.sessions || result || [];
      _renderSessionList();
    } catch (e) {
      console.error('Failed to load sessions:', e);
    }
  }

  function _renderSessionList() {
    const list = document.getElementById('session-list');
    if (!list) return;

    if (sessions.length === 0) {
      list.innerHTML = '<li class="empty-state text-sm">No sessions yet</li>';
      return;
    }

    // Session response contract: id, title, updated_at, created_at, context_pct
    list.innerHTML = sessions.map(s => {
      const pct = s.context_pct || 0;
      return `
      <li class="session-item ${s.id === activeSessionId ? 'active' : ''}" data-id="${s.id}">
        <div class="session-item-title">${Utils.esc(s.title || 'Untitled')}</div>
        <div class="session-item-time">${Utils.timeAgo(s.updated_at || s.created_at)}</div>
        ${pct > 0 ? `<div class="context-bar" style="margin-top:2px">
          <div class="context-pct"><div class="context-pct-fill" style="width:${pct}%;background:${pct > 75 ? 'var(--danger)' : pct > 50 ? 'var(--warning)' : 'var(--accent)'}"></div></div>
          <span class="text-xs">${pct}%</span>
        </div>` : ''}
      </li>`;
    }).join('');

    list.querySelectorAll('.session-item').forEach(el => {
      el.addEventListener('click', () => _switchSession(el.dataset.id));
      el.addEventListener('contextmenu', (e) => {
        e.preventDefault();
        _sessionContextMenu(e, el.dataset.id);
      });
    });
  }

  async function _createSession() {
    try {
      const result = await Gateway.sendRequest('session.create', {});
      const newSession = result.session || result;
      sessions.unshift(newSession);
      _renderSessionList();
      _switchSession(newSession.id);
    } catch (e) {
      console.error('Failed to create session:', e);
    }
  }

  async function _switchSession(id) {
    activeSessionId = id;
    _renderSessionList();

    const messagesEl = document.getElementById('chat-messages');
    messagesEl.innerHTML = '<div style="text-align:center;padding:20px;"><div class="spinner"></div></div>';

    try {
      // Notify server of active session (updates ctx.current_session_id for delete guard)
      Gateway.sendRequest('session.switch', { session_id: id }).catch(() => {});
      const result = await Gateway.sendRequest('chat.history', { session_id: id });
      const messages = result.messages || result || [];
      _renderMessages(messages);
      // Fetch context percentage for this session
      try {
        const info = await Gateway.sendRequest('session.get', { session_id: id });
        _updateContextBar(info.context_pct || 0);
      } catch (_) {}
    } catch (e) {
      messagesEl.innerHTML = '<div class="empty-state">Failed to load history</div>';
    }
  }

  function _renderMessages(messages) {
    const el = document.getElementById('chat-messages');
    if (!el) return;

    if (messages.length === 0) {
      el.innerHTML = '<div class="empty-state">No messages yet. Start a conversation.</div>';
      return;
    }

    el.innerHTML = '<div class="message-list">' + messages.map(m => {
      const role = m.role || 'user';
      const content = m.content || '';

      // Denial messages get distinctive styling
      if (role === 'system' && content.includes('[TOOL DENIED]')) {
        return `<div class="message message-denial">
          <div class="pre-wrap">${Utils.esc(content)}</div>
        </div>`;
      }

      if (role === 'tool') {
        const classifierNote = m.classifier_auto
          ? '<span class="text-xs text-muted"> auto</span>'
          : '';
        return `<div class="message message-tool collapsed tool-collapsible">
          <div>${Utils.esc(content)}${classifierNote}</div>
        </div>`;
      }
      const cls = role === 'user' ? 'message-user' : 'message-assistant';
      return `<div class="message ${cls}">
        <div class="pre-wrap">${Utils.esc(content)}</div>
        <div class="message-time">${Utils.timeAgo(m.created_at)}</div>
      </div>`;
    }).join('') + '</div>';

    el.querySelectorAll('.tool-collapsible').forEach(div => {
      div.addEventListener('click', () => div.classList.toggle('collapsed'));
    });

    el.scrollTop = el.scrollHeight;
  }

  async function _sendMessage() {
    const input = document.getElementById('chat-input');
    const text = input.value.trim();
    if (!text || streaming) return;
    if (!activeSessionId) return;

    input.value = '';
    input.style.height = 'auto';

    // Append user message
    _appendMessage('user', text);

    // Show abort button
    streaming = true;
    streamBuffer = '';
    document.getElementById('chat-send').classList.add('hidden');
    document.getElementById('chat-abort').classList.remove('hidden');

    // Create streaming placeholder
    const messagesEl = document.getElementById('chat-messages');
    const listEl = messagesEl.querySelector('.message-list') || messagesEl;
    streamEl = document.createElement('div');
    streamEl.className = 'message message-assistant';
    streamEl.innerHTML = '<div class="pre-wrap"><span class="spinner"></span></div>';
    listEl.appendChild(streamEl);
    messagesEl.scrollTop = messagesEl.scrollHeight;

    // A1: Include thinking level if set
    const thinkingLevel = document.getElementById('chat-thinking').value;
    const params = { session_id: activeSessionId, message: text };
    if (thinkingLevel) params.thinkingLevel = thinkingLevel;

    // A7: Start stall warning timer
    _resetStallTimer();

    // Fire-and-forget: streaming responses arrive via 'chat' events, not as a res frame.
    // The server does not send a res frame for chat.send — it streams event frames instead.
    // The sendRequest promise will timeout after 30s, but that is expected — do NOT
    // call _finishStream on timeout because the LLM may still be streaming tokens.
    // The stream ends only when _onChatEvent receives state === "final".
    Gateway.sendRequest('chat.send', params).catch(() => {
      // Timeout is expected — do not end the stream here.
      // If the LLM truly failed, no "final" event will arrive and the user can click Abort.
    });
  }

  function _onChatEvent(data) {
    if (destroyed || !streamEl) return;

    if (data.state === 'delta' && data.message && data.message.content) {
      streamBuffer += data.message.content;
      streamEl.querySelector('.pre-wrap').textContent = streamBuffer;
      const messagesEl = document.getElementById('chat-messages');
      if (messagesEl) messagesEl.scrollTop = messagesEl.scrollHeight;
      // A7: Reset stall timer on each delta
      _resetStallTimer();
    }

    if (data.state === 'final') {
      if (data.message && data.message.content && !streamBuffer) {
        // If we missed deltas, show the final content
        streamBuffer = data.message.content;
        streamEl.querySelector('.pre-wrap').textContent = streamBuffer;
      }
      _updateContextBar(data.contextPct || 0);
      _finishStream();
    }
  }

  function _finishStream() {
    streaming = false;
    streamEl = null;
    streamBuffer = '';
    if (stallTimer) { clearTimeout(stallTimer); stallTimer = null; }
    const sendBtn = document.getElementById('chat-send');
    const abortBtn = document.getElementById('chat-abort');
    if (sendBtn) sendBtn.classList.remove('hidden');
    if (abortBtn) { abortBtn.classList.add('hidden'); abortBtn.classList.remove('stall-warning'); }
    const stallText = document.getElementById('chat-stall-text');
    if (stallText) stallText.remove();
  }

  function _updateContextBar(pct) {
    contextPct = pct;
    const el = document.getElementById('chat-context-bar');
    if (!el) return;
    if (pct <= 0) { el.style.display = 'none'; return; }
    el.style.display = 'flex';
    el.innerHTML = `<div class="context-pct"><div class="context-pct-fill" style="width:${pct}%;background:${pct > 75 ? 'var(--danger)' : pct > 50 ? 'var(--warning)' : 'var(--accent)'}"></div></div><span class="text-xs">${pct}%</span>`;
  }

  async function _abortStream() {
    try {
      await Gateway.sendRequest('chat.abort', { session_id: activeSessionId });
    } catch (e) {
      // Ignore
    }
    _finishStream();
  }

  function _appendMessage(role, content) {
    const messagesEl = document.getElementById('chat-messages');
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

  function _sessionContextMenu(event, sessionId) {
    // Remove existing menu
    const existing = document.querySelector('.context-menu');
    if (existing) existing.remove();

    const menu = document.createElement('div');
    menu.className = 'context-menu';
    menu.style.cssText = `position:fixed;left:${event.clientX}px;top:${event.clientY}px;
      background:var(--bg-card);border:1px solid var(--border);border-radius:var(--radius);
      padding:4px 0;z-index:200;min-width:120px;`;

    menu.innerHTML = `
      <div class="ctx-item" data-action="rename" style="padding:6px 12px;cursor:pointer;font-size:13px;">Rename</div>
      <div class="ctx-item" data-action="delete" style="padding:6px 12px;cursor:pointer;font-size:13px;color:var(--danger);">Delete</div>
    `;

    menu.querySelectorAll('.ctx-item').forEach(item => {
      item.addEventListener('mouseenter', () => item.style.background = 'var(--bg-tertiary)');
      item.addEventListener('mouseleave', () => item.style.background = '');
      item.addEventListener('click', async () => {
        menu.remove();
        if (item.dataset.action === 'rename') {
          const name = prompt('New session name:');
          if (name) {
            await Gateway.sendRequest('session.rename', { session_id: sessionId, title: name });
            _loadSessions();
          }
        } else if (item.dataset.action === 'delete') {
          if (confirm('Delete this session?')) {
            try {
              await Gateway.sendRequest('session.delete', { session_id: sessionId });
              if (activeSessionId === sessionId) {
                activeSessionId = null;
                document.getElementById('chat-messages').innerHTML =
                  '<div class="empty-state"><div class="empty-state-title">Select or create a session</div></div>';
              }
              _loadSessions();
            } catch (err) {
              alert('Failed to delete session: ' + (err.message || err));
            }
          }
        }
      });
    });

    document.body.appendChild(menu);
    const cleanup = () => { menu.remove(); document.removeEventListener('click', cleanup); };
    setTimeout(() => document.addEventListener('click', cleanup), 0);
  }

  // A7: Stall warning
  function _resetStallTimer() {
    if (stallTimer) clearTimeout(stallTimer);
    stallTimer = setTimeout(() => {
      if (!streaming) return;
      const abortBtn = document.getElementById('chat-abort');
      if (abortBtn) abortBtn.classList.add('stall-warning');
      // Add stall text if not present
      if (!document.getElementById('chat-stall-text')) {
        const footer = document.querySelector('.chat-footer');
        if (footer) {
          const warn = document.createElement('div');
          warn.id = 'chat-stall-text';
          warn.className = 'text-xs';
          warn.style.cssText = 'color:var(--danger);text-align:center;padding:4px 0;';
          warn.textContent = 'Response may be stalled';
          footer.prepend(warn);
        }
      }
    }, 15000);
  }

  // A6: Activity status bar
  function _onToolStart(data) {
    if (destroyed) return;
    const bar = document.getElementById('chat-activity-bar');
    const text = document.getElementById('chat-activity-text');
    if (bar) bar.classList.remove('hidden');
    if (text) text.textContent = `Running: ${data.tool_name || 'tool'}...`;
  }

  function _onToolDone() {
    if (destroyed) return;
    const bar = document.getElementById('chat-activity-bar');
    if (bar) bar.classList.add('hidden');
  }

  // A3: Search panel
  function _toggleSearch() {
    const panel = document.getElementById('chat-search-panel');
    if (!panel) return;
    panel.classList.toggle('hidden');
    if (!panel.classList.contains('hidden')) {
      document.getElementById('chat-search-input').focus();
    }
  }

  function _onSearchInput(e) {
    const query = e.target.value.trim();
    if (searchDebounce) clearTimeout(searchDebounce);
    if (!query || !activeSessionId) {
      document.getElementById('chat-search-results').innerHTML = '';
      return;
    }
    searchDebounce = setTimeout(async () => {
      try {
        const result = await Gateway.sendRequest('chat.search', { query, session_id: activeSessionId });
        const matches = result.matches || result || [];
        _renderSearchResults(matches);
      } catch (err) {
        document.getElementById('chat-search-results').innerHTML =
          '<li class="text-xs text-muted" style="padding:8px;">Search unavailable</li>';
      }
    }, 300);
  }

  function _renderSearchResults(matches) {
    const list = document.getElementById('chat-search-results');
    if (!list) return;
    if (matches.length === 0) {
      list.innerHTML = '<li class="text-xs text-muted" style="padding:8px;">No results</li>';
      return;
    }
    list.innerHTML = matches.map((m, i) => {
      const snippet = (m.content || '').substring(0, 80);
      return `<li class="chat-search-result" data-index="${m.index || i}" style="padding:6px 8px;cursor:pointer;font-size:12px;border-bottom:1px solid var(--border);">
        <span class="chip chip-category" style="margin-right:4px;">${m.role || 'msg'}</span>${Utils.esc(snippet)}
      </li>`;
    }).join('');
    list.querySelectorAll('.chat-search-result').forEach(el => {
      el.addEventListener('click', () => {
        const idx = parseInt(el.dataset.index, 10);
        const messages = document.querySelectorAll('.message');
        if (messages[idx]) {
          messages[idx].scrollIntoView({ behavior: 'smooth', block: 'center' });
          messages[idx].style.outline = '2px solid var(--accent)';
          setTimeout(() => { messages[idx].style.outline = ''; }, 2000);
        }
      });
    });
  }

  return { render, destroy };
})();
