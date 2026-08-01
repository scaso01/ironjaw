/**
 * Schedules Page - Cron scheduler management
 */
const SchedulesPage = (() => {
  function render(container) {
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Schedules</h2>
      <div id="schedules-list"></div>

      <div class="card mt-4">
        <div class="card-header"><span class="card-title">New Schedule</span></div>
        <div class="form-group">
          <label class="form-label">Name</label>
          <input id="sched-name" class="input" placeholder="Schedule name" type="text">
        </div>
        <div class="form-group">
          <label class="form-label">Cron Expression</label>
          <input id="sched-cron" class="input mono" placeholder="*/5 * * * *" type="text">
        </div>
        <div class="form-group">
          <label class="form-label">Action Type</label>
          <select id="sched-action-type" class="input">
            <option value="chat">Chat</option>
            <option value="tool">Tool</option>
            <option value="llm">LLM</option>
            <option value="workflow">Workflow</option>
          </select>
        </div>
        <div class="form-group">
          <label class="form-label">Action Data (JSON)</label>
          <textarea id="sched-action" class="textarea mono" rows="3">{}</textarea>
        </div>
        <button id="sched-create" class="btn btn-primary">Create</button>
      </div>
    `;

    document.getElementById('sched-create').addEventListener('click', _create);
    _load();
  }

  async function _load() {
    const list = document.getElementById('schedules-list');
    list.innerHTML = '<div style="text-align:center;padding:20px;"><div class="spinner"></div></div>';

    try {
      const result = await Gateway.sendRequest('schedule.list', {});
      const schedules = result.schedules || result || [];

      if (schedules.length === 0) {
        list.innerHTML = '<div class="empty-state">No schedules configured</div>';
        return;
      }

      // Schedule response contract: id, name, cron_expr, enabled, last_run, next_run
      list.innerHTML = '<div class="table-wrap"><table><thead><tr><th>Name</th><th>Cron</th><th>Enabled</th><th>Last Run</th><th>Next Run</th><th>Actions</th></tr></thead><tbody>' +
        schedules.map(s => `
          <tr>
            <td>${Utils.esc(s.name)}</td>
            <td class="mono">${Utils.esc(s.cron_expr)}</td>
            <td>
              <label class="toggle">
                <input type="checkbox" ${s.enabled ? 'checked' : ''} class="sched-toggle" data-schedule-id="${Utils.esc(s.id)}">
                <span class="toggle-slider"></span>
              </label>
            </td>
            <td class="text-sm text-muted">${Utils.safeTimestamp(s.last_run) ? new Date(s.last_run).toLocaleString() : 'Never'}</td>
            <td class="text-sm text-muted">${Utils.safeTimestamp(s.next_run) ? new Date(s.next_run).toLocaleString() : '\u2014'}</td>
            <td><button class="btn btn-sm btn-danger sched-delete" data-schedule-id="${Utils.esc(s.id)}">Delete</button></td>
          </tr>
        `).join('') +
        '</tbody></table></div>';

      list.querySelectorAll('.sched-toggle').forEach(cb => {
        cb.addEventListener('change', () => SchedulesPage.toggle(cb.dataset.scheduleId, cb.checked));
      });
      list.querySelectorAll('.sched-delete').forEach(btn => {
        btn.addEventListener('click', () => SchedulesPage.remove(btn.dataset.scheduleId));
      });
    } catch (e) {
      list.innerHTML = '<div class="empty-state">Failed to load schedules</div>';
    }
  }

  async function _create() {
    const name = document.getElementById('sched-name').value.trim();
    const cron = document.getElementById('sched-cron').value.trim();
    const actionType = document.getElementById('sched-action-type').value;
    const actionStr = document.getElementById('sched-action').value.trim();

    if (!name || !cron) return;

    let actionData;
    try { actionData = JSON.parse(actionStr); } catch { alert('Invalid JSON for action data'); return; }

    try {
      await Gateway.sendRequest('schedule.create', {
        name,
        cron_expr: cron,
        action_type: actionType,
        action_data: actionData,
      });
      document.getElementById('sched-name').value = '';
      document.getElementById('sched-cron').value = '';
      document.getElementById('sched-action').value = '{}';
      _load();
    } catch (e) {
      alert('Failed: ' + (e.message || e));
    }
  }

  async function toggle(id, enabled) {
    try {
      await Gateway.sendRequest('schedule.toggle', { schedule_id: id, enabled });
    } catch (e) {
      alert('Failed: ' + (e.message || e));
      _load();
    }
  }

  async function remove(id) {
    if (!confirm('Delete this schedule?')) return;
    try {
      await Gateway.sendRequest('schedule.delete', { schedule_id: id });
      _load();
    } catch (e) {
      alert('Failed: ' + (e.message || e));
    }
  }

  return { render, toggle, remove };
})();
