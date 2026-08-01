/**
 * Vault Page - Secrets Vault Management
 *
 * Search-by-key interface for encrypted secrets.
 * Uses secrets.get and secrets.set (both Confirm tier).
 */
const VaultPage = (() => {
  function render(container) {
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Secrets Vault</h2>

      <div class="card" style="border-color:var(--warning)">
        <div style="color:var(--warning);font-size:13px;padding:8px 12px;">
          Secrets operations require Confirm-tier approval. Values are encrypted at rest.
        </div>
      </div>

      <div class="card">
        <div class="card-header"><span class="card-title">Retrieve Secret</span></div>
        <div class="flex gap-2" style="padding:0 12px 12px;">
          <input id="vault-get-key" class="input" placeholder="Secret key..." type="text" style="flex:1">
          <button id="vault-get-btn" class="btn btn-primary">Get</button>
        </div>
        <div id="vault-get-result" style="padding:0 12px 12px;display:none"></div>
      </div>

      <div class="card">
        <div class="card-header"><span class="card-title">Store Secret</span></div>
        <div style="padding:0 12px 12px;">
          <div class="form-group">
            <label class="form-label">Key</label>
            <input id="vault-set-key" class="input" placeholder="Key" type="text">
          </div>
          <div class="form-group">
            <label class="form-label">Value</label>
            <input id="vault-set-value" class="input" placeholder="Value" type="password">
          </div>
          <button id="vault-set-btn" class="btn btn-primary">Save</button>
          <div id="vault-set-result" style="margin-top:8px"></div>
        </div>
      </div>
    `;

    document.getElementById('vault-get-btn').addEventListener('click', _getSecret);
    document.getElementById('vault-get-key').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') _getSecret();
    });
    document.getElementById('vault-set-btn').addEventListener('click', _setSecret);
  }

  async function _getSecret() {
    const key = document.getElementById('vault-get-key').value.trim();
    if (!key) return;

    const result = document.getElementById('vault-get-result');
    result.style.display = '';
    result.innerHTML = '<div class="spinner"></div>';

    try {
      const data = await Gateway.sendRequest('secrets.get', { key });
      const value = data.value || '';

      result.innerHTML = `
        <div class="card" style="margin:0;">
          <div class="flex justify-between items-center" style="padding:8px 12px;">
            <div>
              <span class="text-sm text-muted">Key:</span>
              <span class="mono">${Utils.esc(key)}</span>
            </div>
            <div class="flex gap-2">
              <button id="vault-reveal-btn" class="btn btn-sm">Reveal</button>
              <button id="vault-delete-btn" class="btn btn-sm btn-danger">Delete</button>
            </div>
          </div>
          <div style="padding:4px 12px 8px;">
            <span class="text-sm">Value: </span>
            <span id="vault-value-display" class="mono text-sm">${'*'.repeat(Math.min(value.length || 8, 20))}</span>
          </div>
        </div>
      `;

      let revealed = false;
      document.getElementById('vault-reveal-btn').addEventListener('click', () => {
        const display = document.getElementById('vault-value-display');
        if (!display) return;
        revealed = !revealed;
        display.textContent = revealed ? value : '*'.repeat(Math.min(value.length || 8, 20));
        document.getElementById('vault-reveal-btn').textContent = revealed ? 'Hide' : 'Reveal';
      });

      document.getElementById('vault-delete-btn').addEventListener('click', () => _deleteSecret(key));
    } catch (e) {
      result.innerHTML = `<div style="color:var(--danger);" class="text-sm">${Utils.esc(e.message || 'Failed to retrieve secret')}</div>`;
    }
  }

  async function _setSecret() {
    const key = document.getElementById('vault-set-key').value.trim();
    const value = document.getElementById('vault-set-value').value;
    if (!key || !value) return;

    const result = document.getElementById('vault-set-result');
    result.innerHTML = '<div class="spinner"></div>';

    try {
      await Gateway.sendRequest('secrets.set', { key, value });
      result.innerHTML = '<div style="color:var(--success);" class="text-sm">Secret stored.</div>';
      document.getElementById('vault-set-key').value = '';
      document.getElementById('vault-set-value').value = '';
      setTimeout(() => { result.innerHTML = ''; }, 3000);
    } catch (e) {
      result.innerHTML = `<div style="color:var(--danger);" class="text-sm">${Utils.esc(e.message || 'Failed to store secret')}</div>`;
    }
  }

  async function _deleteSecret(key) {
    if (!confirm('Delete secret "' + key + '"?')) return;

    try {
      await Gateway.sendRequest('secrets.set', { key, value: '' });
      const result = document.getElementById('vault-get-result');
      if (result) {
        result.innerHTML = '<div style="color:var(--success);" class="text-sm">Secret deleted.</div>';
        setTimeout(() => { result.style.display = 'none'; result.innerHTML = ''; }, 3000);
      }
    } catch (e) {
      alert('Failed to delete: ' + (e.message || e));
    }
  }

  return { render };
})();
