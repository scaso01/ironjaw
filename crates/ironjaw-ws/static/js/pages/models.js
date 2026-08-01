/**
 * Models Page - LLM model info, filtering, switching with live events
 */
const ModelsPage = (() => {
  let models = [];
  let currentModelId = null;
  let activeTab = 'all';
  let switchPollTimer = null;
  let switchStartTime = null;
  let switchTargetModel = null;

  const RECENT_KEY = 'ironjaw_recent_models';
  const MAX_RECENT = 5;
  const SWITCH_TIMEOUT_S = 60;
  const POLL_INTERVAL_MS = 2000;

  function render(container) {
    _cleanup();
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Models</h2>
      <div id="model-status" class="card">
        <div class="card-header"><span class="card-title">Current Model</span></div>
        <div id="model-status-body" style="text-align:center;padding:12px;"><div class="spinner"></div></div>
      </div>
      <div id="model-switch-feedback" style="display:none;"></div>
      <div id="recent-models-section" style="margin-bottom:12px;"></div>
      <div id="model-list-section" class="card">
        <div class="card-header">
          <span class="card-title">Available Models</span>
        </div>
        <div class="flex items-center gap-2" style="padding:8px 12px 0;">
          <button class="filter-btn active" data-tab="all">All</button>
          <button class="filter-btn" data-tab="local">Local</button>
          <button class="filter-btn" data-tab="cloud">Cloud</button>
        </div>
        <div id="model-list-body" style="text-align:center;padding:12px;"><div class="spinner"></div></div>
      </div>
    `;

    container.querySelectorAll('.filter-btn[data-tab]').forEach(btn => {
      btn.addEventListener('click', () => {
        activeTab = btn.dataset.tab;
        container.querySelectorAll('.filter-btn[data-tab]').forEach(b => b.classList.remove('active'));
        btn.classList.add('active');
        _renderModelList();
      });
    });

    _loadStatus();
    _loadModels();
    _renderRecentModels();

    Gateway.onEvent('model.changed', _onModelChanged);
  }

  function _cleanup() {
    if (switchPollTimer) {
      clearInterval(switchPollTimer);
      switchPollTimer = null;
    }
    switchStartTime = null;
    switchTargetModel = null;
  }

  // --- Recent models (localStorage) ---

  function _getRecent() {
    try {
      return JSON.parse(localStorage.getItem(RECENT_KEY) || '[]');
    } catch { return []; }
  }

  function _addRecent(name) {
    let recent = _getRecent().filter(m => m !== name);
    recent.unshift(name);
    if (recent.length > MAX_RECENT) recent = recent.slice(0, MAX_RECENT);
    localStorage.setItem(RECENT_KEY, JSON.stringify(recent));
    _renderRecentModels();
  }

  function _renderRecentModels() {
    const section = document.getElementById('recent-models-section');
    if (!section) return;
    const recent = _getRecent();
    if (recent.length === 0) { section.innerHTML = ''; return; }
    section.innerHTML = `
      <div style="display:flex;flex-wrap:wrap;gap:6px;align-items:center;">
        <span class="text-sm text-muted" style="margin-right:4px;">Recent:</span>
        ${recent.map(m => `<button class="btn btn-sm" style="font-size:11px;padding:2px 8px;" data-recent-model="${Utils.esc(m)}">${Utils.esc(m)}</button>`).join('')}
      </div>
    `;
    section.querySelectorAll('[data-recent-model]').forEach(btn => {
      btn.addEventListener('click', () => switchModel(btn.dataset.recentModel, true));
    });
  }

  // --- Status loading ---

  async function _loadStatus() {
    const body = document.getElementById('model-status-body');
    if (!body) return;
    try {
      const result = await Gateway.sendRequest('model.status', {});
      currentModelId = result.id || result.name || null;
      body.innerHTML = `
        <table>
          <tr><th style="width:140px;">Model</th><td>${Utils.esc(result.name || 'Unknown')}</td></tr>
          <tr><th>Status</th><td>${Utils.esc(result.status || 'N/A')}</td></tr>
          <tr><th>Provider</th><td>${Utils.esc(result.provider || 'N/A')}</td></tr>
        </table>
      `;
    } catch (e) {
      body.innerHTML = '<div class="text-muted text-sm">Failed to load model status</div>';
    }
  }

  async function _loadModels() {
    const body = document.getElementById('model-list-body');
    if (!body) return;
    try {
      const result = await Gateway.sendRequest('model.list', {});
      models = result.models || result || [];
      if (models.length === 0) {
        body.innerHTML = '<div class="text-muted text-sm">No models available</div>';
        return;
      }
      _renderModelList();
    } catch (e) {
      body.innerHTML = '<div class="text-muted text-sm">Failed to load model list</div>';
    }
  }

  function _isLocal(m) {
    const p = (m.provider || '').toLowerCase();
    return p.includes('llama') || p.includes('local');
  }

  function _isCloud(m) {
    const p = (m.provider || '').toLowerCase();
    return p.includes('openai') || p.includes('anthropic');
  }

  function _renderModelList() {
    const body = document.getElementById('model-list-body');
    if (!body) return;

    let filtered = models;
    if (activeTab === 'local') {
      filtered = models.filter(_isLocal);
    } else if (activeTab === 'cloud') {
      filtered = models.filter(_isCloud);
    }

    if (filtered.length === 0) {
      body.innerHTML = '<div class="text-muted text-sm">No models in this category</div>';
      return;
    }

    const isSwitching = switchTargetModel !== null;

    body.innerHTML = `<table>
      <thead><tr><th>Name</th><th>Provider</th><th>Details</th><th>Action</th></tr></thead>
      <tbody>${filtered.map(m => {
        const modelId = m.id || m;
        const isActive = currentModelId && (modelId === currentModelId);
        const details = [];
        if (m.size) details.push(Utils.esc(m.size));
        if (m.modified_at) details.push(Utils.timeAgo(m.modified_at));
        return `
          <tr>
            <td>
              ${Utils.esc(modelId)}
              ${isActive ? ' <span class="badge badge-success">ACTIVE</span>' : ''}
            </td>
            <td>${Utils.esc(m.provider || '')}</td>
            <td class="text-sm text-muted">${details.join(' | ') || '-'}</td>
            <td>${isActive
              ? '<span class="text-sm text-muted">Current</span>'
              : isSwitching
                ? '<span class="text-sm text-muted">Switching...</span>'
                : `<button class="btn btn-sm" data-switch-model="${Utils.esc(modelId)}">Switch</button>`
            }</td>
          </tr>
        `;
      }).join('')}</tbody>
    </table>`;

    body.querySelectorAll('[data-switch-model]').forEach(btn => {
      btn.addEventListener('click', () => {
        const name = btn.dataset.switchModel;
        const isKnown = _getRecent().includes(name);
        if (isKnown || confirm('Switch to model "' + name + '"?')) {
          switchModel(name, isKnown);
        }
      });
    });
  }

  // --- Switch with polling + elapsed timer ---

  async function switchModel(name, skipConfirm) {
    _addRecent(name);
    const fb = document.getElementById('model-switch-feedback');
    if (fb) {
      fb.style.display = 'block';
      fb.innerHTML = `<div class="card" style="border-left:3px solid var(--accent);padding:10px 14px;">
        <span id="switch-progress">Switching to <strong>${Utils.esc(name)}</strong>...</span>
      </div>`;
    }

    switchTargetModel = name;
    switchStartTime = Date.now();
    _renderModelList(); // disable buttons

    try {
      await Gateway.sendRequest('model.switch', { model: name, restart: true });
      _startSwitchPoll(name);
    } catch (e) {
      _showSwitchError(name, e.message || String(e));
    }
  }

  function _startSwitchPoll(targetModel) {
    if (switchPollTimer) clearInterval(switchPollTimer);

    switchPollTimer = setInterval(async () => {
      const elapsed = Math.round((Date.now() - switchStartTime) / 1000);

      if (elapsed >= SWITCH_TIMEOUT_S) {
        _showSwitchError(targetModel, 'Timed out after ' + SWITCH_TIMEOUT_S + 's');
        return;
      }

      const prog = document.getElementById('switch-progress');
      if (prog) {
        prog.innerHTML = `Switching to <strong>${Utils.esc(targetModel)}</strong>... (${elapsed}s / ${SWITCH_TIMEOUT_S}s)`;
      }

      try {
        const status = await Gateway.sendRequest('model.status', {});
        if (status.status === 'ready' && status.id === targetModel) {
          _showSwitchSuccess(targetModel);
        }
      } catch { /* ignore poll errors */ }
    }, POLL_INTERVAL_MS);
  }

  function _showSwitchSuccess(model) {
    _cleanup();
    currentModelId = model;
    const fb = document.getElementById('model-switch-feedback');
    if (fb) {
      fb.innerHTML = `<div class="card" style="border-left:3px solid #22c55e;padding:10px 14px;color:#22c55e;">
        Verified: <strong>${Utils.esc(model)}</strong> loaded successfully
      </div>`;
      setTimeout(() => { fb.style.display = 'none'; }, 5000);
    }
    _loadStatus();
    _loadModels();
  }

  function _showSwitchError(model, msg) {
    _cleanup();
    const fb = document.getElementById('model-switch-feedback');
    if (fb) {
      fb.innerHTML = `<div class="card" style="border-left:3px solid #ef4444;padding:10px 14px;">
        <strong style="color:#ef4444;">Switch failed</strong>: ${Utils.esc(model)} &mdash; ${Utils.esc(msg)}
        <br><button class="btn btn-sm" style="margin-top:6px;" onclick="ModelsPage.switchModel('${Utils.esc(model)}')">Retry</button>
        <button class="btn btn-sm" style="margin-top:6px;margin-left:4px;" onclick="document.getElementById('model-switch-feedback').style.display='none'">Dismiss</button>
      </div>`;
    }
    _renderModelList(); // re-enable buttons
  }

  // --- WS event: model.changed ---

  function _onModelChanged(data) {
    const newModel = data && data.current;
    if (newModel) {
      // If we were waiting for this switch, show success
      if (switchTargetModel && newModel === switchTargetModel) {
        _showSwitchSuccess(newModel);
        return;
      }
      // External change — refresh
      currentModelId = newModel;
      _loadStatus();
      _loadModels();
    }
  }

  return { render, switchModel };
})();
