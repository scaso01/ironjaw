/**
 * Tools Page - List and execute registered tools
 */
const ToolsPage = (() => {
  function render(container) {
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Tools</h2>
      <div id="tools-list"></div>
    `;
    _loadTools();
  }

  async function _loadTools() {
    const list = document.getElementById('tools-list');
    list.innerHTML = '<div style="text-align:center;padding:20px;"><div class="spinner"></div></div>';

    try {
      const result = await Gateway.sendRequest('tools.list', {});
      const tools = result.tools || result || [];
      if (tools.length === 0) {
        list.innerHTML = '<div class="empty-state">No tools registered</div>';
        return;
      }

      list.innerHTML = tools.map((t, i) => {
        const tierClass = _tierBadgeClass(t.tier);
        return `
          <div class="card">
            <div class="card-header">
              <div>
                <span class="card-title">${Utils.esc(t.name)}</span>
                <span class="badge ${tierClass}" style="margin-left:8px;">${Utils.esc(_tierLabel(t.tier))}</span>
              </div>
              <div class="flex gap-2">
                <button class="btn btn-sm tool-schema-btn" data-tool-idx="${i}">Schema</button>
                <button class="btn btn-sm btn-primary tool-exec-btn" data-tool-name="${Utils.esc(t.name)}">Execute</button>
              </div>
            </div>
            <div class="text-sm text-muted">${Utils.esc(t.description || '')}</div>
            <div id="tool-schema-${i}" class="hidden mt-2">
              <pre class="mono text-sm" style="background:var(--bg-primary);padding:8px;border-radius:var(--radius);overflow-x:auto;">${Utils.esc(JSON.stringify(t.schema || t.parameters || {}, null, 2))}</pre>
            </div>
          </div>
        `;
      }).join('');

      list.querySelectorAll('.tool-schema-btn').forEach(btn => {
        btn.addEventListener('click', () => ToolsPage.toggleSchema(parseInt(btn.dataset.toolIdx, 10)));
      });
      list.querySelectorAll('.tool-exec-btn').forEach(btn => {
        btn.addEventListener('click', () => ToolsPage.openExec(btn.dataset.toolName));
      });
    } catch (e) {
      list.innerHTML = '<div class="empty-state">Failed to load tools</div>';
    }
  }

  function toggleSchema(idx) {
    const el = document.getElementById('tool-schema-' + idx);
    if (el) el.classList.toggle('hidden');
  }

  function openExec(name) {
    // Create modal
    const overlay = document.createElement('div');
    overlay.className = 'modal-overlay';
    overlay.innerHTML = `
      <div class="modal">
        <div class="modal-title">Execute: ${Utils.esc(name)}</div>
        <div class="form-group">
          <label class="form-label">Parameters (JSON)</label>
          <textarea id="exec-params" class="textarea mono" rows="6">{}</textarea>
        </div>
        <div id="exec-result" class="hidden mt-2"></div>
        <div class="modal-actions">
          <button class="btn" id="exec-cancel">Cancel</button>
          <button class="btn btn-primary" id="exec-run">Run</button>
        </div>
      </div>
    `;

    document.body.appendChild(overlay);

    overlay.querySelector('#exec-cancel').addEventListener('click', () => overlay.remove());
    overlay.addEventListener('click', (e) => { if (e.target === overlay) overlay.remove(); });

    overlay.querySelector('#exec-run').addEventListener('click', async () => {
      const paramsEl = document.getElementById('exec-params');
      const resultEl = document.getElementById('exec-result');
      let params;
      try {
        params = JSON.parse(paramsEl.value);
      } catch {
        resultEl.innerHTML = '<div style="color:var(--danger);">Invalid JSON</div>';
        resultEl.classList.remove('hidden');
        return;
      }

      resultEl.innerHTML = '<div class="spinner"></div>';
      resultEl.classList.remove('hidden');

      try {
        const result = await Gateway.sendRequest('tools.execute', { tool: name, params });
        resultEl.innerHTML = `<pre class="mono text-sm" style="background:var(--bg-primary);padding:8px;border-radius:var(--radius);overflow-x:auto;max-height:300px;">${Utils.esc(JSON.stringify(result, null, 2))}</pre>`;
      } catch (e) {
        resultEl.innerHTML = `<div style="color:var(--danger);">${Utils.esc(e.message || JSON.stringify(e))}</div>`;
      }
    });
  }

  // Map numeric tier (0-3) to display label
  function _tierLabel(tier) {
    switch (tier) {
      case 0: return 'Auto';
      case 1: return 'Notify';
      case 2: return 'Confirm';
      case 3: return 'MultiConfirm';
      default:
        // Fallback for string tiers
        if (typeof tier === 'string') return tier;
        return 'Auto';
    }
  }

  function _tierBadgeClass(tier) {
    // Handle both numeric (0-3) and string tier values
    const numTier = typeof tier === 'number' ? tier : -1;
    switch (numTier) {
      case 0: return 'badge-auto';
      case 1: return 'badge-notify';
      case 2:
      case 3: return 'badge-confirm';
      default:
        // Fallback for string tiers
        switch (String(tier || '').toLowerCase()) {
          case 'auto': return 'badge-auto';
          case 'notify': return 'badge-notify';
          case 'confirm':
          case 'multiconfirm': return 'badge-confirm';
          default: return 'badge-auto';
        }
    }
  }

  return { render, toggleSchema, openExec };
})();
