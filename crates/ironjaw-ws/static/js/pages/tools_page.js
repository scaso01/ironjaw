/**
 * Ironjaw Dashboard - Tools Page
 *
 * Lists all registered tools grouped by category, with tier badges.
 *
 * Data source: GET /api/dashboard/tools -> Vec<ToolSummary>
 * Fields per tool: name, description, tier, category
 */
const ToolsPage = (() => {
    let filterCategory = 'all';
    let searchQuery = '';
    let allTools = [];

    function render(container) {
        container.innerHTML = `
            <h1 class="page-title">Tools</h1>
            <div class="search-bar" style="margin-bottom: 12px;">
                <input type="text" class="search-input" id="tool-search" placeholder="Search tools by name or description..." />
            </div>
            <div class="filter-bar" id="tools-filter-bar">
                <button class="filter-btn active" data-filter="all">All</button>
            </div>
            <div class="card">
                <div class="table-wrap">
                    <table class="tools-table">
                        <thead>
                            <tr>
                                <th>Name</th>
                                <th>Description</th>
                                <th>Tier</th>
                                <th>Category</th>
                            </tr>
                        </thead>
                        <tbody id="tools-tbody">
                            <tr><td colspan="4">${Utils.spinner()}</td></tr>
                        </tbody>
                    </table>
                </div>
            </div>
        `;

        document.getElementById('tool-search').addEventListener('input', (e) => {
            searchQuery = e.target.value.toLowerCase();
            _renderTable(allTools, document.getElementById('tools-tbody'));
        });

        refresh();
    }

    async function refresh() {
        const tools = await API.tools();
        const tbody = document.getElementById('tools-tbody');
        const filterBar = document.getElementById('tools-filter-bar');
        if (!tbody) return;

        allTools = tools || [];

        if (!tools || tools.length === 0) {
            tbody.innerHTML = '<tr><td colspan="4" class="empty-state">No tools registered</td></tr>';
            return;
        }

        // Build category filter buttons
        if (filterBar) {
            const categories = ['all'].concat(
                [...new Set(tools.map(t => t.category || 'unknown'))].sort()
            );
            filterBar.innerHTML = categories.map(cat => {
                const active = cat === filterCategory ? ' active' : '';
                return `<button class="filter-btn${active}" data-filter="${Utils.esc(cat)}">${Utils.esc(cat)}</button>`;
            }).join('');

            filterBar.querySelectorAll('.filter-btn').forEach(btn => {
                btn.addEventListener('click', () => {
                    filterBar.querySelectorAll('.filter-btn').forEach(b => b.classList.remove('active'));
                    btn.classList.add('active');
                    filterCategory = btn.dataset.filter;
                    _renderTable(tools, tbody);
                });
            });
        }

        _renderTable(tools, tbody);
    }

    function destroy() {
        filterCategory = 'all';
        searchQuery = '';
        allTools = [];
    }

    function _renderTable(tools, tbody) {
        let filtered = tools;
        if (filterCategory !== 'all') {
            filtered = tools.filter(t => (t.category || 'unknown') === filterCategory);
        }
        if (searchQuery) {
            filtered = filtered.filter(t =>
                (t.name || '').toLowerCase().includes(searchQuery) ||
                (t.description || '').toLowerCase().includes(searchQuery)
            );
        }

        if (filtered.length === 0) {
            tbody.innerHTML = '<tr><td colspan="4" class="empty-state">No tools match</td></tr>';
            return;
        }

        tbody.innerHTML = '';
        filtered.forEach(t => {
            const tierClass = _tierChipClass(t.tier);
            const tr = document.createElement('tr');
            tr.className = 'tool-row';
            tr.innerHTML = `
                <td><span class="tool-chevron"></span><strong class="mono">${Utils.esc(t.name)}</strong></td>
                <td>${Utils.esc(t.description)}</td>
                <td><span class="chip ${tierClass}">${Utils.esc(t.tier)}</span></td>
                <td><span class="chip chip-category">${Utils.esc(t.category)}</span></td>
            `;
            tr.addEventListener('click', async () => {
                const next = tr.nextElementSibling;
                if (tr.classList.contains('expanded') && next && next.classList.contains('tool-schema-row')) {
                    tr.classList.remove('expanded');
                    next.remove();
                } else {
                    tr.classList.add('expanded');
                    const schemaRow = document.createElement('tr');
                    schemaRow.className = 'tool-schema-row';
                    schemaRow.innerHTML = `<td colspan="4"><pre class="mono" style="margin:0;font-size:12px;">${Utils.spinner()}</pre></td>`;
                    tr.after(schemaRow);
                    try {
                        const result = await Gateway.sendRequest('tools.search', { query: t.name, max_results: 1 });
                        const schema = (result.tools && result.tools[0] && result.tools[0].schema) || result.schema || null;
                        schemaRow.querySelector('pre').textContent = schema ? JSON.stringify(schema, null, 2) : 'No schema available';
                    } catch (e) {
                        schemaRow.querySelector('pre').textContent = 'Failed to load schema: ' + (e.message || e);
                    }
                }
            });
            tbody.appendChild(tr);
        });
    }

    function _tierChipClass(tier) {
        switch ((tier || '').toLowerCase()) {
            case 'auto': return 'chip-tier-auto';
            case 'notify': return 'chip-tier-notify';
            case 'confirm':
            case 'multiconfirm': return 'chip-tier-confirm';
            default: return 'chip-category';
        }
    }

    return { render, refresh, destroy };
})();
