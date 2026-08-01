/**
 * Graph Page - Knowledge Graph Visualization
 *
 * Renders entities and relations from brain.recall as a force-directed SVG graph.
 * Self-contained: no external libraries.
 */
const GraphPage = (() => {
  let _nodes = [];
  let _edges = [];
  let _svg = null;
  let _dragNode = null;
  let _dragOffset = { x: 0, y: 0 };
  let _scale = 1;
  let _pan = { x: 0, y: 0 };
  let _animFrame = null;

  const TYPE_COLORS = {
    person: '#4fc3f7',
    place: '#81c784',
    thing: '#ffb74d',
    concept: '#ce93d8',
    event: '#ef5350',
    tool: '#26a69a',
    project: '#42a5f5',
    default: '#90a4ae',
  };

  function render(container) {
    container.innerHTML = `
      <h2 style="font-size:18px;font-weight:600;margin-bottom:16px;">Knowledge Graph</h2>
      <div class="card">
        <div class="card-header">
          <span class="card-title">Entity Network</span>
          <div class="flex gap-2">
            <input id="graph-query" class="input" placeholder="Search entities..." type="text" style="width:200px">
            <button id="graph-refresh" class="btn btn-sm">Refresh</button>
          </div>
        </div>
        <div id="graph-canvas" style="width:100%;height:500px;position:relative;overflow:hidden;background:var(--bg-primary);border:1px solid var(--border);border-radius:var(--radius)"></div>
      </div>
      <div id="graph-details" class="card" style="display:none">
        <div class="card-header"><span class="card-title">Entity Details</span></div>
        <div id="graph-detail-body"></div>
      </div>
    `;

    document.getElementById('graph-refresh').addEventListener('click', _loadGraph);
    document.getElementById('graph-query').addEventListener('input', _filterNodes);
    _loadGraph();
  }

  function destroy() {
    if (_animFrame) cancelAnimationFrame(_animFrame);
    _animFrame = null;
    _nodes = [];
    _edges = [];
    _svg = null;
    _scale = 1;
    _pan = { x: 0, y: 0 };
  }

  async function _loadGraph() {
    const canvas = document.getElementById('graph-canvas');
    if (!canvas) return;
    canvas.innerHTML = '<div style="text-align:center;padding:40px;"><div class="spinner"></div></div>';

    try {
      const result = await Gateway.sendRequest('brain.recall', {
        query: '*',
        graph: true,
        limit: 100,
      });

      const memories = result.memories || result || [];
      _parseGraph(memories);
      _forceLayout(_nodes, _edges, 150);
      _renderSvg(canvas);
    } catch (e) {
      canvas.innerHTML = `<div class="empty-state">${Utils.esc(e.message || 'Failed to load graph')}</div>`;
    }
  }

  function _parseGraph(memories) {
    const nodeMap = new Map();
    const edgeSet = new Set();
    _nodes = [];
    _edges = [];

    for (const m of memories) {
      const entities = m.entities || [];
      const relations = m.relations || [];

      for (const e of entities) {
        const name = e.name || e;
        const type = e.type || 'default';
        if (!nodeMap.has(name)) {
          nodeMap.set(name, {
            id: name,
            type,
            x: Math.random() * 600 + 50,
            y: Math.random() * 400 + 50,
            vx: 0,
            vy: 0,
            content: m.content || m.text || '',
          });
        }
      }

      for (const r of relations) {
        const from = r.from || r.subject || r[0];
        const to = r.to || r.object || r[2];
        const label = r.relation || r.predicate || r[1] || '';
        const key = `${from}|${to}|${label}`;
        if (!edgeSet.has(key) && from && to) {
          edgeSet.add(key);
          // Ensure both endpoints exist as nodes
          if (!nodeMap.has(from)) {
            nodeMap.set(from, {
              id: from, type: 'default',
              x: Math.random() * 600 + 50, y: Math.random() * 400 + 50,
              vx: 0, vy: 0, content: '',
            });
          }
          if (!nodeMap.has(to)) {
            nodeMap.set(to, {
              id: to, type: 'default',
              x: Math.random() * 600 + 50, y: Math.random() * 400 + 50,
              vx: 0, vy: 0, content: '',
            });
          }
          _edges.push({ from, to, label });
        }
      }
    }

    _nodes = Array.from(nodeMap.values());

    // If no entities/relations parsed, create nodes from memory content
    if (_nodes.length === 0 && memories.length > 0) {
      for (let i = 0; i < memories.length; i++) {
        const m = memories[i];
        const text = m.content || m.text || `Memory ${i}`;
        const label = text.length > 30 ? text.slice(0, 30) + '...' : text;
        _nodes.push({
          id: `mem-${i}`,
          type: 'concept',
          x: Math.random() * 600 + 50,
          y: Math.random() * 400 + 50,
          vx: 0, vy: 0,
          content: text,
          label,
        });
      }
    }
  }

  function _forceLayout(nodes, edges, iterations) {
    if (nodes.length === 0) return;

    const repulsion = 5000;
    const attraction = 0.01;
    const damping = 0.9;
    const centerX = 350;
    const centerY = 250;

    for (let iter = 0; iter < iterations; iter++) {
      // Repulsion between all pairs
      for (let i = 0; i < nodes.length; i++) {
        for (let j = i + 1; j < nodes.length; j++) {
          const dx = nodes[j].x - nodes[i].x;
          const dy = nodes[j].y - nodes[i].y;
          const dist = Math.sqrt(dx * dx + dy * dy) || 1;
          const force = repulsion / (dist * dist);
          const fx = (dx / dist) * force;
          const fy = (dy / dist) * force;
          nodes[i].vx -= fx;
          nodes[i].vy -= fy;
          nodes[j].vx += fx;
          nodes[j].vy += fy;
        }
      }

      // Attraction along edges
      for (const edge of edges) {
        const a = nodes.find(n => n.id === edge.from);
        const b = nodes.find(n => n.id === edge.to);
        if (!a || !b) continue;
        const dx = b.x - a.x;
        const dy = b.y - a.y;
        const dist = Math.sqrt(dx * dx + dy * dy) || 1;
        const force = dist * attraction;
        const fx = (dx / dist) * force;
        const fy = (dy / dist) * force;
        a.vx += fx;
        a.vy += fy;
        b.vx -= fx;
        b.vy -= fy;
      }

      // Center gravity
      for (const node of nodes) {
        node.vx += (centerX - node.x) * 0.001;
        node.vy += (centerY - node.y) * 0.001;
      }

      // Apply velocities with damping
      for (const node of nodes) {
        node.vx *= damping;
        node.vy *= damping;
        node.x += node.vx;
        node.y += node.vy;
        // Clamp to bounds
        node.x = Math.max(30, Math.min(670, node.x));
        node.y = Math.max(30, Math.min(470, node.y));
      }
    }
  }

  function _renderSvg(canvas) {
    canvas.innerHTML = '';
    if (_nodes.length === 0) {
      canvas.innerHTML = '<div class="empty-state">No graph data available</div>';
      return;
    }

    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('width', '100%');
    svg.setAttribute('height', '100%');
    svg.setAttribute('viewBox', '0 0 700 500');
    svg.style.cursor = 'grab';
    _svg = svg;

    const g = document.createElementNS('http://www.w3.org/2000/svg', 'g');
    g.setAttribute('id', 'graph-group');
    svg.appendChild(g);

    // Draw edges
    for (const edge of _edges) {
      const a = _nodes.find(n => n.id === edge.from);
      const b = _nodes.find(n => n.id === edge.to);
      if (!a || !b) continue;

      const line = document.createElementNS('http://www.w3.org/2000/svg', 'line');
      line.setAttribute('x1', a.x);
      line.setAttribute('y1', a.y);
      line.setAttribute('x2', b.x);
      line.setAttribute('y2', b.y);
      line.setAttribute('stroke', 'var(--border)');
      line.setAttribute('stroke-width', '1.5');
      line.setAttribute('data-from', edge.from);
      line.setAttribute('data-to', edge.to);
      g.appendChild(line);

      if (edge.label) {
        const midX = (a.x + b.x) / 2;
        const midY = (a.y + b.y) / 2;
        const text = document.createElementNS('http://www.w3.org/2000/svg', 'text');
        text.setAttribute('x', midX);
        text.setAttribute('y', midY - 4);
        text.setAttribute('text-anchor', 'middle');
        text.setAttribute('fill', 'var(--text-secondary)');
        text.setAttribute('font-size', '9');
        text.textContent = edge.label;
        g.appendChild(text);
      }
    }

    // Draw nodes
    for (const node of _nodes) {
      const color = TYPE_COLORS[node.type] || TYPE_COLORS.default;
      const nodeG = document.createElementNS('http://www.w3.org/2000/svg', 'g');
      nodeG.setAttribute('data-node-id', node.id);
      nodeG.style.cursor = 'pointer';

      const circle = document.createElementNS('http://www.w3.org/2000/svg', 'circle');
      circle.setAttribute('cx', node.x);
      circle.setAttribute('cy', node.y);
      circle.setAttribute('r', '14');
      circle.setAttribute('fill', color);
      circle.setAttribute('stroke', '#fff');
      circle.setAttribute('stroke-width', '2');
      nodeG.appendChild(circle);

      const label = document.createElementNS('http://www.w3.org/2000/svg', 'text');
      const displayLabel = node.label || node.id;
      const truncated = displayLabel.length > 16 ? displayLabel.slice(0, 16) + '..' : displayLabel;
      label.setAttribute('x', node.x);
      label.setAttribute('y', node.y + 26);
      label.setAttribute('text-anchor', 'middle');
      label.setAttribute('fill', 'var(--text-primary)');
      label.setAttribute('font-size', '10');
      label.textContent = truncated;
      nodeG.appendChild(label);

      // Click to show details
      nodeG.addEventListener('click', () => _showDetails(node));

      // Drag
      nodeG.addEventListener('mousedown', (e) => {
        e.preventDefault();
        _dragNode = node;
        _dragOffset.x = e.clientX - node.x * _scale;
        _dragOffset.y = e.clientY - node.y * _scale;
        svg.style.cursor = 'grabbing';
      });

      g.appendChild(nodeG);
    }

    // Mouse move / up for drag
    svg.addEventListener('mousemove', (e) => {
      if (!_dragNode) return;
      _dragNode.x = (e.clientX - _dragOffset.x) / _scale;
      _dragNode.y = (e.clientY - _dragOffset.y) / _scale;
      _updateNodePosition(_dragNode);
    });

    svg.addEventListener('mouseup', () => {
      _dragNode = null;
      svg.style.cursor = 'grab';
    });

    svg.addEventListener('mouseleave', () => {
      _dragNode = null;
      svg.style.cursor = 'grab';
    });

    // Zoom
    svg.addEventListener('wheel', (e) => {
      e.preventDefault();
      const delta = e.deltaY > 0 ? 0.9 : 1.1;
      _scale = Math.max(0.3, Math.min(3, _scale * delta));
      g.setAttribute('transform', `scale(${_scale})`);
    });

    canvas.appendChild(svg);
  }

  function _updateNodePosition(node) {
    if (!_svg) return;
    const g = _svg.querySelector('#graph-group');
    if (!g) return;

    // Update circle and label
    const nodeG = g.querySelector(`[data-node-id="${CSS.escape(node.id)}"]`);
    if (nodeG) {
      const circle = nodeG.querySelector('circle');
      const text = nodeG.querySelector('text');
      if (circle) {
        circle.setAttribute('cx', node.x);
        circle.setAttribute('cy', node.y);
      }
      if (text) {
        text.setAttribute('x', node.x);
        text.setAttribute('y', node.y + 26);
      }
    }

    // Update connected edges
    const lines = g.querySelectorAll('line');
    for (const line of lines) {
      if (line.getAttribute('data-from') === node.id) {
        line.setAttribute('x1', node.x);
        line.setAttribute('y1', node.y);
      }
      if (line.getAttribute('data-to') === node.id) {
        line.setAttribute('x2', node.x);
        line.setAttribute('y2', node.y);
      }
    }
  }

  function _showDetails(node) {
    const panel = document.getElementById('graph-details');
    const body = document.getElementById('graph-detail-body');
    if (!panel || !body) return;

    const color = TYPE_COLORS[node.type] || TYPE_COLORS.default;

    // Find connected edges
    const connections = _edges.filter(e => e.from === node.id || e.to === node.id);

    body.innerHTML = `
      <div style="padding:12px;">
        <div class="flex items-center gap-2 mb-2">
          <span style="display:inline-block;width:12px;height:12px;border-radius:50%;background:${color}"></span>
          <strong>${Utils.esc(node.label || node.id)}</strong>
          <span class="badge text-sm">${Utils.esc(node.type)}</span>
        </div>
        ${node.content ? `<div class="pre-wrap text-sm" style="margin-bottom:8px;">${Utils.esc(node.content)}</div>` : ''}
        ${connections.length > 0 ? `
          <div class="text-sm text-muted" style="margin-top:8px;">
            <strong>Connections (${connections.length}):</strong>
            <ul style="margin:4px 0 0 16px;">
              ${connections.map(c => {
                const other = c.from === node.id ? c.to : c.from;
                const dir = c.from === node.id ? '&rarr;' : '&larr;';
                return `<li>${dir} <span class="mono">${Utils.esc(other)}</span>${c.label ? ` (${Utils.esc(c.label)})` : ''}</li>`;
              }).join('')}
            </ul>
          </div>
        ` : '<div class="text-sm text-muted">No connections</div>'}
      </div>
    `;
    panel.style.display = '';
  }

  function _filterNodes() {
    const query = (document.getElementById('graph-query').value || '').toLowerCase();
    if (!_svg) return;
    const g = _svg.querySelector('#graph-group');
    if (!g) return;

    for (const node of _nodes) {
      const nodeG = g.querySelector(`[data-node-id="${CSS.escape(node.id)}"]`);
      if (!nodeG) continue;
      const match = !query || node.id.toLowerCase().includes(query) ||
        (node.label && node.label.toLowerCase().includes(query)) ||
        node.type.toLowerCase().includes(query);
      nodeG.style.opacity = match ? '1' : '0.15';
    }
  }

  return { render, destroy };
})();
