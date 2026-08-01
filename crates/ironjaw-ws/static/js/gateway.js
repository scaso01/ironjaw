/**
 * Ironjaw Protocol v4 WebSocket Client
 */
const Gateway = (() => {
  let ws = null;
  let authenticated = false;
  let reconnectTimer = null;
  let reconnectDelay = 1000;
  const MAX_RECONNECT = 30000;
  const pendingRequests = new Map();
  const eventListeners = new Map();
  let streamHandler = null;
  const statusListeners = [];
  let token = null;
  let url = null;

  function generateId() {
    return crypto.randomUUID ? crypto.randomUUID() :
      'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, c => {
        const r = Math.random() * 16 | 0;
        return (c === 'x' ? r : (r & 0x3 | 0x8)).toString(16);
      });
  }

  function connect(wsUrl, authToken) {
    url = wsUrl;
    token = authToken;
    authenticated = false;
    _clearReconnect();

    if (ws) {
      ws.onclose = null;
      ws.close();
    }

    _setStatus('connecting');
    ws = new WebSocket(wsUrl);

    ws.onopen = () => {
      reconnectDelay = 1000;
      _sendAuth();
    };

    ws.onmessage = (evt) => {
      let frame;
      try { frame = JSON.parse(evt.data); } catch { return; }
      _handleFrame(frame);
    };

    ws.onclose = () => {
      authenticated = false;
      _setStatus('disconnected');
      _scheduleReconnect();
    };

    ws.onerror = () => {
      // onclose will fire after onerror
    };
  }

  function disconnect() {
    _clearReconnect();
    if (ws) {
      ws.onclose = null;
      ws.close();
      ws = null;
    }
    authenticated = false;
    _setStatus('disconnected');
  }

  function _sendAuth() {
    const frame = {
      type: 'req',
      method: 'auth.connect',
      params: { token, client: 'dashboard', version: '1.0' },
      id: generateId(),
    };
    ws.send(JSON.stringify(frame));
    // We treat auth as a pending request
    pendingRequests.set(frame.id, {
      resolve: (data) => {
        authenticated = true;
        _setStatus('connected');
      },
      reject: (err) => {
        _setStatus('auth_failed');
      },
    });
  }

  function sendRequest(method, params = {}) {
    return new Promise((resolve, reject) => {
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        return reject(new Error('Not connected'));
      }
      const id = generateId();
      const frame = { type: 'req', method, params, id };

      // Timeout after 30s
      const timer = setTimeout(() => {
        if (pendingRequests.has(id)) {
          pendingRequests.delete(id);
          reject(new Error(`Request timeout: ${method}`));
        }
      }, 30000);

      pendingRequests.set(id, {
        resolve: (data) => { clearTimeout(timer); resolve(data); },
        reject: (err) => { clearTimeout(timer); reject(err); },
        timer,
      });
      ws.send(JSON.stringify(frame));
    });
  }

  function sendCommandResponse(id, ok, data) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const frame = { type: 'cmd.res', id, ok };
    if (data) frame.payload = data;
    ws.send(JSON.stringify(frame));
  }

  function _handleFrame(frame) {
    switch (frame.type) {
      case 'res': {
        const pending = pendingRequests.get(frame.id);
        if (pending) {
          pendingRequests.delete(frame.id);
          if (frame.ok === false || frame.error) {
            pending.reject(frame.error || { message: 'Unknown error' });
          } else {
            pending.resolve(frame.payload || frame.data || frame.result || {});
          }
        }
        break;
      }
      case 'err': {
        const pending = pendingRequests.get(frame.id);
        if (pending) {
          pendingRequests.delete(frame.id);
          pending.reject(frame.error || { message: 'Error' });
        }
        break;
      }
      case 'stream': {
        if (streamHandler) streamHandler(frame);
        break;
      }
      case 'event':
      case 'evt': {
        const eventName = frame.event;
        const listeners = eventListeners.get(eventName);
        if (listeners) {
          listeners.forEach(cb => cb(frame.payload || frame.data || {}));
        }
        break;
      }
      case 'cmd': {
        // Server sending a command (e.g., permission.request)
        const listeners = eventListeners.get('cmd');
        if (listeners) {
          listeners.forEach(cb => cb(frame));
        }
        break;
      }
    }
  }

  function onEvent(type, callback) {
    if (!eventListeners.has(type)) {
      eventListeners.set(type, []);
    }
    eventListeners.get(type).push(callback);
  }

  function offEvent(type, callback) {
    const listeners = eventListeners.get(type);
    if (listeners) {
      const idx = listeners.indexOf(callback);
      if (idx !== -1) listeners.splice(idx, 1);
    }
  }

  function onStream(callback) {
    streamHandler = callback;
  }

  function offStream() {
    streamHandler = null;
  }

  function addStatusListener(callback) {
    if (callback && statusListeners.indexOf(callback) === -1) {
      statusListeners.push(callback);
    }
  }

  function removeStatusListener(callback) {
    const idx = statusListeners.indexOf(callback);
    if (idx !== -1) statusListeners.splice(idx, 1);
  }

  function onStatus(callback) {
    // Backward compat: null is ignored, non-null pushes to listener array
    if (callback) addStatusListener(callback);
  }

  function _setStatus(status) {
    // Iterate a copy so listeners can remove themselves during notification
    const snapshot = statusListeners.slice();
    for (const cb of snapshot) {
      cb(status);
    }
  }

  function _scheduleReconnect() {
    if (reconnectTimer) return;
    if (!token || !url) return;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect(url, token);
    }, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, MAX_RECONNECT);
  }

  function _clearReconnect() {
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
  }

  function isConnected() {
    return authenticated && ws && ws.readyState === WebSocket.OPEN;
  }

  return {
    connect,
    disconnect,
    sendRequest,
    sendCommandResponse,
    onEvent,
    offEvent,
    onStream,
    offStream,
    onStatus,
    addStatusListener,
    removeStatusListener,
    isConnected,
  };
})();
