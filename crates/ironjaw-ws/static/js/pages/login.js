/**
 * Login Page - Token authentication
 */
const LoginPage = (() => {
  function render(container) {
    container.innerHTML = `
      <div style="max-width:400px;margin:80px auto;text-align:center;">
        <h1 style="font-size:28px;font-weight:700;color:var(--accent);margin-bottom:8px;">Ironjaw</h1>
        <p class="text-muted mb-4">Enter your authentication token to connect.</p>
        <div id="login-error" class="hidden" style="color:var(--danger);margin-bottom:12px;font-size:13px;"></div>
        <div class="form-group">
          <input id="login-token" type="password" class="input" placeholder="Auth token" autocomplete="off">
        </div>
        <button id="login-btn" class="btn btn-primary w-full">Connect</button>
      </div>
    `;

    const input = document.getElementById('login-token');
    const btn = document.getElementById('login-btn');
    const errEl = document.getElementById('login-error');

    async function doLogin() {
      const token = input.value.trim();
      if (!token) return;

      btn.disabled = true;
      btn.textContent = 'Connecting...';
      errEl.classList.add('hidden');

      localStorage.setItem('ironjaw_token', token);

      // Stop any in-flight reconnect attempts with the old token
      Gateway.disconnect();

      let listening = false;
      const authListener = function(status) {
        // Ignore 'connecting' — only act on final auth result
        if (status === 'connecting') {
          listening = true;
          return;
        }
        if (!listening) return;
        if (status === 'connected') {
          Gateway.removeStatusListener(authListener);
          App.navigate('chat');
        } else if (status === 'auth_failed') {
          Gateway.removeStatusListener(authListener);
          localStorage.removeItem('ironjaw_token');
          errEl.textContent = 'Authentication failed. Check your token.';
          errEl.classList.remove('hidden');
          btn.disabled = false;
          btn.textContent = 'Connect';
        }
      };
      Gateway.addStatusListener(authListener);

      App.reconnect();
    }

    btn.addEventListener('click', doLogin);
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') doLogin();
    });

    input.focus();
  }

  return { render };
})();
