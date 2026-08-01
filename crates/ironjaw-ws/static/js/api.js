/**
 * Ironjaw Dashboard - REST API Client
 *
 * Wraps all /api/dashboard/* endpoints with error handling.
 * Uses the same origin as the page (relative to window.location.origin).
 */
const API = (() => {
    const base = window.location.origin;

    async function get(path) {
        try {
            const res = await fetch(`${base}${path}`);
            if (!res.ok) throw new Error(`HTTP ${res.status}`);
            return await res.json();
        } catch (e) {
            console.error(`API GET ${path}:`, e);
            return null;
        }
    }

    async function post(path, body) {
        try {
            const res = await fetch(`${base}${path}`, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(body),
            });
            if (!res.ok) throw new Error(`HTTP ${res.status}`);
            return await res.json();
        } catch (e) {
            console.error(`API POST ${path}:`, e);
            return null;
        }
    }

    return {
        // Dashboard data endpoints
        overview:  () => get('/api/dashboard/overview'),
        sessions:  () => get('/api/dashboard/sessions'),
        tools:     () => get('/api/dashboard/tools'),
        metrics:   () => get('/api/dashboard/metrics'),
        brain:     () => get('/api/dashboard/brain'),
        devices:   () => get('/api/dashboard/devices'),
        health:    () => get('/api/dashboard/health'),

        // Raw accessors for custom endpoints
        get,
        post,
    };
})();
