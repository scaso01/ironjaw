/**
 * Shared utility functions for the Ironjaw Dashboard.
 * Loaded before all page scripts via index.html.
 */
const Utils = (() => {
  /**
   * HTML-escape a string to prevent XSS when inserting into innerHTML.
   * Handles non-string inputs (numbers, null, undefined) safely.
   */
  function esc(str) {
    const d = document.createElement('div');
    d.textContent = String(str || '');
    return d.innerHTML;
  }

  /**
   * Format a timestamp as a human-readable relative time string.
   * Returns 'just now', 'Nm ago', 'Nh ago', or a locale date string.
   */
  function timeAgo(ts) {
    if (!ts) return '';
    const d = new Date(ts);
    const diff = (Date.now() - d.getTime()) / 1000;
    if (diff < 60) return 'just now';
    if (diff < 3600) return Math.floor(diff / 60) + 'm ago';
    if (diff < 86400) return Math.floor(diff / 3600) + 'h ago';
    return d.toLocaleDateString();
  }

  /**
   * Return the standard centered spinner HTML used for loading states.
   */
  function spinner() {
    return '<div style="text-align:center;padding:20px;"><div class="spinner"></div></div>';
  }

  /**
   * Return an empty-state message block.
   * @param {string} message - The message to display.
   */
  function emptyState(message) {
    return '<div class="empty-state">' + esc(message) + '</div>';
  }

  /**
   * Return null if a timestamp is null/undefined/0/falsy, otherwise the value.
   */
  function safeTimestamp(ts) {
    return (ts === null || ts === undefined || ts === 0 || !ts) ? null : ts;
  }

  /**
   * Format a timestamp for display. Returns fallback if ts is null/undefined/0.
   */
  function formatTimestamp(ts, fallback) {
    if (fallback === undefined) fallback = '\u2014';
    if (ts === null || ts === undefined || ts === 0 || !ts) return fallback;
    return new Date(ts).toLocaleString();
  }

  return { esc, timeAgo, spinner, emptyState, safeTimestamp, formatTimestamp };
})();
