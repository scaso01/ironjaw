/**
 * Browser Page - Browser Automation Controls
 *
 * Navigate, screenshot, and extract via tools.execute with the browser tool.
 */
const BrowserPage = (() => {
  let _history = [];

  function render(container) {
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Browser Automation</h2>

      <div class="card">
        <div class="card-header"><span class="card-title">Navigation</span></div>
        <div class="flex gap-2" style="padding:0 12px 12px;">
          <input id="browser-url" class="input" placeholder="https://..." type="text" style="flex:1">
          <button id="browser-go" class="btn btn-primary">Go</button>
          <button id="browser-screenshot" class="btn">Screenshot</button>
          <button id="browser-extract" class="btn">Extract</button>
        </div>
      </div>

      <div class="flex gap-4" style="margin-top:16px;">
        <div style="flex:1">
          <div id="browser-result" class="card" style="display:none">
            <div class="card-header"><span class="card-title" id="browser-result-title">Result</span></div>
            <div id="browser-result-body" style="padding:12px;"></div>
          </div>
        </div>
        <div style="width:220px;flex-shrink:0;">
          <div class="card">
            <div class="card-header"><span class="card-title">History</span></div>
            <div id="browser-history" style="padding:8px 12px;">
              <div class="text-sm text-muted">No history yet</div>
            </div>
          </div>
        </div>
      </div>
    `;

    document.getElementById('browser-go').addEventListener('click', _navigate);
    document.getElementById('browser-url').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') _navigate();
    });
    document.getElementById('browser-screenshot').addEventListener('click', _screenshot);
    document.getElementById('browser-extract').addEventListener('click', _extract);
  }

  function destroy() {
    _history = [];
  }

  async function _navigate() {
    const url = document.getElementById('browser-url').value.trim();
    if (!url) return;

    _showLoading('Navigating...');

    try {
      const result = await Gateway.sendRequest('tools.execute', {
        tool: 'browser',
        params: { action: 'navigate', url },
      });

      _addHistory(url);
      _showResult('Navigation', `
        <div style="color:var(--success);" class="text-sm">
          Navigated to <span class="mono">${Utils.esc(url)}</span>
        </div>
        ${result.title ? `<div class="text-sm" style="margin-top:4px;">Title: ${Utils.esc(result.title)}</div>` : ''}
      `);
    } catch (e) {
      _showResult('Navigation Error', `
        <div style="color:var(--danger);" class="text-sm">${Utils.esc(e.message || 'Navigation failed')}</div>
      `);
    }
  }

  async function _screenshot() {
    _showLoading('Taking screenshot...');

    try {
      const result = await Gateway.sendRequest('tools.execute', {
        tool: 'browser',
        params: { action: 'screenshot' },
      });

      const imageData = result.data || result.image || result.screenshot || '';
      if (imageData) {
        const src = imageData.startsWith('data:') ? imageData : 'data:image/png;base64,' + imageData;
        _showResult('Screenshot', `
          <img src="${src}" style="max-width:100%;border:1px solid var(--border);border-radius:var(--radius);" alt="Screenshot">
        `);
      } else {
        _showResult('Screenshot', '<div class="text-sm text-muted">No image data returned</div>');
      }
    } catch (e) {
      _showResult('Screenshot Error', `
        <div style="color:var(--danger);" class="text-sm">${Utils.esc(e.message || 'Screenshot failed')}</div>
      `);
    }
  }

  async function _extract() {
    _showLoading('Extracting content...');

    try {
      const result = await Gateway.sendRequest('tools.execute', {
        tool: 'browser',
        params: { action: 'extract' },
      });

      const content = result.content || result.text || result.html || '';
      if (content) {
        _showResult('Extracted Content', `
          <pre style="white-space:pre-wrap;word-break:break-word;font-size:12px;max-height:400px;overflow:auto;background:var(--bg-secondary);padding:12px;border-radius:var(--radius);">${Utils.esc(content)}</pre>
        `);
      } else {
        _showResult('Extract', '<div class="text-sm text-muted">No content extracted</div>');
      }
    } catch (e) {
      _showResult('Extract Error', `
        <div style="color:var(--danger);" class="text-sm">${Utils.esc(e.message || 'Extraction failed')}</div>
      `);
    }
  }

  function _showLoading(label) {
    const panel = document.getElementById('browser-result');
    const title = document.getElementById('browser-result-title');
    const body = document.getElementById('browser-result-body');
    if (!panel || !body) return;
    panel.style.display = '';
    if (title) title.textContent = label;
    body.innerHTML = '<div class="spinner" style="margin:20px auto;"></div>';
  }

  function _showResult(titleText, html) {
    const panel = document.getElementById('browser-result');
    const title = document.getElementById('browser-result-title');
    const body = document.getElementById('browser-result-body');
    if (!panel || !body) return;
    panel.style.display = '';
    if (title) title.textContent = titleText;
    body.innerHTML = html;
  }

  function _addHistory(url) {
    _history.unshift({ url, time: Date.now() });
    if (_history.length > 20) _history.pop();
    _renderHistory();
  }

  function _renderHistory() {
    const el = document.getElementById('browser-history');
    if (!el) return;

    if (_history.length === 0) {
      el.innerHTML = '<div class="text-sm text-muted">No history yet</div>';
      return;
    }

    el.innerHTML = _history.map(h => `
      <div class="flex justify-between items-center" style="padding:4px 0;border-bottom:1px solid var(--border);">
        <a href="#" class="history-link mono text-sm" data-url="${Utils.esc(h.url)}" style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:160px;display:block;">${Utils.esc(h.url)}</a>
        <span class="text-sm text-muted">${Utils.timeAgo(h.time)}</span>
      </div>
    `).join('');

    el.querySelectorAll('.history-link').forEach(link => {
      link.addEventListener('click', (e) => {
        e.preventDefault();
        document.getElementById('browser-url').value = link.dataset.url;
        _navigate();
      });
    });
  }

  return { render, destroy };
})();
