/**
 * Settings Page - Theme, connection, AI settings, export, forking, logout
 */
const SettingsPage = (() => {
  function render(container) {
    const theme = localStorage.getItem('ironjaw_theme') || 'dark';
    const connected = Gateway.isConnected();
    const thinkingLevel = localStorage.getItem('ironjaw_thinking_level') || '';
    const forkingEnabled = localStorage.getItem('ironjaw_forking_enabled') === 'true';

    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Settings</h2>

      <div class="card">
        <div class="card-header"><span class="card-title">Appearance</span></div>
        <div class="flex items-center justify-between">
          <span>Theme</span>
          <div class="flex items-center gap-2">
            <span class="text-sm text-muted">${theme === 'dark' ? 'Dark' : 'Light'}</span>
            <label class="toggle">
              <input type="checkbox" id="theme-toggle" ${theme === 'light' ? 'checked' : ''}>
              <span class="toggle-slider"></span>
            </label>
          </div>
        </div>
      </div>

      <div class="card">
        <div class="card-header"><span class="card-title">AI Settings</span></div>
        <div class="flex items-center justify-between">
          <span>Default Thinking Level</span>
          <select id="thinking-level" class="select-sm">
            <option value="" ${thinkingLevel === '' ? 'selected' : ''}>Off</option>
            <option value="low" ${thinkingLevel === 'low' ? 'selected' : ''}>Low</option>
            <option value="medium" ${thinkingLevel === 'medium' ? 'selected' : ''}>Medium</option>
            <option value="high" ${thinkingLevel === 'high' ? 'selected' : ''}>High</option>
          </select>
        </div>
        <div class="flex items-center justify-between" style="margin-top:12px;">
          <span>Conversation Forking</span>
          <label class="toggle">
            <input type="checkbox" id="forking-toggle" ${forkingEnabled ? 'checked' : ''}>
            <span class="toggle-slider"></span>
          </label>
        </div>
      </div>

      <div class="card">
        <div class="card-header"><span class="card-title">Connection</span></div>
        <table>
          <tr><th style="width:120px;">Status</th><td><span class="badge ${connected ? 'badge-success' : 'badge-danger'}">${connected ? 'Connected' : 'Disconnected'}</span></td></tr>
          <tr><th>Server</th><td class="mono">${Utils.esc(location.host)}</td></tr>
        </table>
      </div>

      <div class="card">
        <div class="card-header"><span class="card-title">Export &amp; Backup</span></div>
        <div style="display:flex;flex-direction:column;gap:8px;">
          <button id="export-conversations" class="btn">Export Conversations</button>
          <button id="export-brain" class="btn">Export Brain Memories</button>
          <button id="backup-create" class="btn btn-primary">Create Full Backup</button>
        </div>
      </div>

      <div class="card">
        <div class="card-header"><span class="card-title">Account</span></div>
        <button id="logout-btn" class="btn btn-danger">Logout</button>
      </div>
    `;

    // Theme toggle
    document.getElementById('theme-toggle').addEventListener('change', (e) => {
      const newTheme = e.target.checked ? 'light' : 'dark';
      localStorage.setItem('ironjaw_theme', newTheme);
      if (newTheme === 'light') {
        document.documentElement.setAttribute('data-theme', 'light');
      } else {
        document.documentElement.removeAttribute('data-theme');
      }
      render(container);
    });

    // Thinking level
    document.getElementById('thinking-level').addEventListener('change', (e) => {
      localStorage.setItem('ironjaw_thinking_level', e.target.value);
    });

    // Forking toggle
    document.getElementById('forking-toggle').addEventListener('change', (e) => {
      localStorage.setItem('ironjaw_forking_enabled', e.target.checked ? 'true' : 'false');
    });

    // Export conversations
    document.getElementById('export-conversations').addEventListener('click', async () => {
      try {
        const result = await Gateway.sendRequest('export.conversations', {});
        _downloadResult(result, 'ironjaw-conversations.json');
      } catch (e) {
        alert('Export failed: ' + (e.message || e));
      }
    });

    // Export brain
    document.getElementById('export-brain').addEventListener('click', async () => {
      try {
        const result = await Gateway.sendRequest('export.brain', {});
        _downloadResult(result, 'ironjaw-brain.json');
      } catch (e) {
        alert('Export failed: ' + (e.message || e));
      }
    });

    // Create backup
    document.getElementById('backup-create').addEventListener('click', async () => {
      const btn = document.getElementById('backup-create');
      btn.disabled = true;
      btn.textContent = 'Creating backup...';
      try {
        await Gateway.sendRequest('backup.create', {});
        btn.textContent = 'Backup created!';
        setTimeout(() => {
          btn.textContent = 'Create Full Backup';
          btn.disabled = false;
        }, 2000);
      } catch (e) {
        alert('Backup failed: ' + (e.message || e));
        btn.textContent = 'Create Full Backup';
        btn.disabled = false;
      }
    });

    // Logout
    document.getElementById('logout-btn').addEventListener('click', () => {
      localStorage.removeItem('ironjaw_token');
      Gateway.disconnect();
      App.navigate('login');
    });
  }

  function _downloadResult(data, filename) {
    const json = typeof data === 'string' ? data : JSON.stringify(data, null, 2);
    const blob = new Blob([json], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);
  }

  return { render };
})();
