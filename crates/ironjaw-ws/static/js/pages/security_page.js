/**
 * Ironjaw Dashboard - Security Page
 *
 * Permission rules management with add/remove, classifier stats,
 * and rule tester.
 *
 * Data sources:
 *   security.rules.list -> Vec<PermissionRule>
 *   security.rules.add -> Result
 *   security.rules.remove -> Result
 *   security.classifier.stats -> ClassifierStats
 */
const SecurityPage = (() => {
    let rules = [];

    function render(container) {
        container.innerHTML = `
            <h1 class="page-title">Security</h1>

            <div class="section-title">Permission Rules</div>
            <div class="card">
                <div class="table-wrap">
                    <table>
                        <thead>
                            <tr>
                                <th>Pattern</th>
                                <th>Action</th>
                                <th>Scope</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody id="security-rules-tbody">
                            <tr><td colspan="4">${Utils.spinner()}</td></tr>
                        </tbody>
                    </table>
                </div>
                <div id="security-add-form" style="margin-top:12px;padding-top:12px;border-top:1px solid var(--border);">
                    <div class="flex gap-2 items-center flex-wrap">
                        <input id="rule-pattern" class="input" placeholder="Pattern (e.g. tools.execute:shell)" style="flex:2;min-width:200px;">
                        <select id="rule-action" class="select-sm" style="min-width:80px;">
                            <option value="allow">Allow</option>
                            <option value="deny">Deny</option>
                        </select>
                        <select id="rule-scope" class="select-sm" style="min-width:100px;">
                            <option value="user">User</option>
                            <option value="project">Project</option>
                            <option value="local">Local</option>
                            <option value="managed">Managed</option>
                        </select>
                        <button id="rule-add-btn" class="btn btn-primary btn-sm">Add Rule</button>
                    </div>
                    <div id="rule-add-result" class="mt-2"></div>
                </div>
            </div>

            <div class="section-title">Classifier Stats</div>
            <div class="stats-grid" id="security-classifier-stats">
                <div class="card" id="sec-approved">
                    <div class="card-header"><span class="card-title">Auto-Approved</span></div>
                    <div class="card-value" style="color:var(--green);">--</div>
                </div>
                <div class="card" id="sec-denied">
                    <div class="card-header"><span class="card-title">Denied</span></div>
                    <div class="card-value" style="color:var(--red);">--</div>
                </div>
                <div class="card" id="sec-escalated">
                    <div class="card-header"><span class="card-title">Escalated</span></div>
                    <div class="card-value" style="color:var(--amber);">--</div>
                </div>
            </div>

            <div class="section-title">Rule Tester</div>
            <div class="card" id="security-tester">
                <div class="flex gap-2 items-center flex-wrap">
                    <input id="test-command" class="input" placeholder="Test command (e.g. tools.execute)" style="flex:1;min-width:200px;">
                    <button id="test-btn" class="btn btn-sm">Test</button>
                </div>
                <div id="test-result" class="mt-2"></div>
            </div>
        `;

        document.getElementById('rule-add-btn').addEventListener('click', _addRule);
        document.getElementById('test-btn').addEventListener('click', _testRule);
        document.getElementById('rule-pattern').addEventListener('keydown', (e) => {
            if (e.key === 'Enter') _addRule();
        });
        document.getElementById('test-command').addEventListener('keydown', (e) => {
            if (e.key === 'Enter') _testRule();
        });

        _loadRules();
        _loadClassifierStats();
    }

    function destroy() {
        rules = [];
    }

    async function _loadRules() {
        const tbody = document.getElementById('security-rules-tbody');
        if (!tbody) return;

        try {
            const result = await Gateway.sendRequest('security.rules.list', {});
            rules = result.rules || [];
            _renderRules(tbody);
        } catch (e) {
            tbody.innerHTML = '<tr><td colspan="4" class="text-sm text-muted">Failed to load rules</td></tr>';
        }
    }

    function _renderRules(tbody) {
        if (rules.length === 0) {
            tbody.innerHTML = '<tr><td colspan="4" class="empty-state">No permission rules configured</td></tr>';
            return;
        }

        tbody.innerHTML = rules.map((r, idx) => {
            const actionBadge = r.action === 'allow'
                ? '<span class="badge badge-success">Allow</span>'
                : '<span class="badge badge-danger">Deny</span>';
            const scopeChip = _scopeChip(r.scope || 'user');

            return `<tr>
                <td class="mono text-sm">${Utils.esc(r.pattern || '')}</td>
                <td>${actionBadge}</td>
                <td>${scopeChip}</td>
                <td><button class="btn btn-sm btn-danger rule-delete-btn" data-rule-idx="${idx}">Delete</button></td>
            </tr>`;
        }).join('');

        tbody.querySelectorAll('.rule-delete-btn').forEach(btn => {
            btn.addEventListener('click', () => _removeRule(parseInt(btn.dataset.ruleIdx, 10)));
        });
    }

    function _scopeChip(scope) {
        const cls = {
            'managed': 'chip-purple',
            'user': 'chip-category',
            'project': 'chip-tier-notify',
            'local': 'chip-tier-auto',
        }[scope] || 'chip-category';
        return `<span class="chip ${cls}">${Utils.esc(scope)}</span>`;
    }

    async function _addRule() {
        const pattern = document.getElementById('rule-pattern').value.trim();
        const action = document.getElementById('rule-action').value;
        const scope = document.getElementById('rule-scope').value;
        const resultEl = document.getElementById('rule-add-result');

        if (!pattern) return;

        try {
            await Gateway.sendRequest('security.rules.add', { pattern, action, scope });
            document.getElementById('rule-pattern').value = '';
            resultEl.innerHTML = '<div class="text-sm" style="color:var(--green);">Rule added.</div>';
            setTimeout(() => { resultEl.innerHTML = ''; }, 2000);
            _loadRules();
        } catch (e) {
            resultEl.innerHTML = `<div class="text-sm" style="color:var(--danger);">${Utils.esc(e.message || 'Failed to add rule')}</div>`;
        }
    }

    async function _removeRule(idx) {
        const rule = rules[idx];
        if (!rule) return;
        if (!confirm('Remove rule "' + rule.pattern + '"?')) return;

        try {
            await Gateway.sendRequest('security.rules.remove', { pattern: rule.pattern, scope: rule.scope });
            _loadRules();
        } catch (e) {
            alert('Failed to remove rule: ' + (e.message || e));
        }
    }

    async function _loadClassifierStats() {
        try {
            const data = await Gateway.sendRequest('security.classifier.stats', {});
            const approved = document.querySelector('#sec-approved .card-value');
            const denied = document.querySelector('#sec-denied .card-value');
            const escalated = document.querySelector('#sec-escalated .card-value');
            if (approved) approved.textContent = data.auto_approved || 0;
            if (denied) denied.textContent = data.denied || 0;
            if (escalated) escalated.textContent = data.escalated || 0;
        } catch (e) {
            // Classifier stats unavailable
        }
    }

    async function _testRule() {
        const command = document.getElementById('test-command').value.trim();
        const resultEl = document.getElementById('test-result');
        if (!command) return;

        resultEl.innerHTML = Utils.spinner();

        // Test locally by matching against rules
        let matched = false;
        let matchResult = 'No matching rule found (default: Confirm tier)';
        for (const rule of rules) {
            const pattern = rule.pattern || '';
            if (command === pattern || command.startsWith(pattern.replace('*', ''))) {
                matched = true;
                const actionClass = rule.action === 'allow' ? 'color:var(--green)' : 'color:var(--red)';
                matchResult = `<span style="${actionClass}"><strong>${rule.action.toUpperCase()}</strong></span> by rule: <span class="mono">${Utils.esc(pattern)}</span> (${Utils.esc(rule.scope || 'user')})`;
                break;
            }
        }

        resultEl.innerHTML = `<div class="text-sm" style="margin-top:8px;">${matchResult}</div>`;
    }

    return { render, destroy };
})();
