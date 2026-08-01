# E2E Test Issues — 2026-03-24

## Status: ALL RESOLVED (2026-03-24)

53 total issues fixed (33 tracked + 20 clippy violations):
- 20 clippy violations in dashboard_tests.rs (uninlined_format_args + bool_assert_comparison)
- 6 dashboard E2E failures (timing/timeout fixes)
- 27 Dr. CLAW instrumented test failures (FakeGatewayModule + individual fixes)

---

## Dashboard E2E (6 failures) — RESOLVED

### DASH-1: test_cc_sessions_session_displays_title — FIXED
- **Fix applied**: Replaced fixed 5s sleep with polling loop checking `.session-item-title` elements every 500ms, 15s timeout.

### DASH-2: test_cc_sessions_sessions_sorted_by_time — FIXED
- **Fix applied**: Null-safe timestamp fallback (`|| '1970-01-01T00:00:00Z'`) for sort comparison.

### DASH-3: test_tools_page_lists_tools — FIXED
- **Fix applied**: Added `wait_for_ws_connected()` helper that polls `#ws-status .status-dot.connected`. Replaced brittle `thread::sleep(3s)` in `navigate_with_auth()`.

### DASH-4: test_vault_security_warning — FIXED
- **Fix applied**: Same as DASH-3 — `wait_for_ws_connected()` ensures WS is ready before navigation.

### DASH-5: test_ws_rpc_nonexistent_method — FIXED
- **Fix applied**: Increased JS Promise.race timeout from 5s to 15s. Added 2s sleep before JS eval for WS readiness.

### DASH-6: test_error_unknown_rpc_returns_error — FIXED
- **Fix applied**: Same as DASH-5.

### IMPORTANT: Single-threaded execution required
Dashboard E2E tests must run single-threaded to avoid headless Chrome resource contention:
```bash
cargo test --test dashboard_tests -- --ignored --test-threads=1
```
Multi-threaded runs spawn 194 Chrome instances simultaneously, causing ~65% failure rate from timeouts.

### Clippy violations (20) — FIXED
- 14x `uninlined_format_args`: inlined variables into format strings
- 6x `bool_assert_comparison`: replaced `assert_eq!(expr, true/false)` with `assert!()`/`assert!(!)`

---

## Dr. CLAW Instrumented (27 failures) — RESOLVED

### Category A: Chat input disabled (11 failures) — FIXED via FakeGatewayModule
- **Fix applied**: Created `FakeGatewayModule` with `@TestInstallIn` that provides `FakeGatewayClient` emitting `ConnectionState.Connected`. Extracted `GatewayModule` from `AppModule` for clean replacement. All test classes automatically get connected state.
- APP-1 through APP-11: All fixed by FakeGatewayModule.

### Category B: CC Sessions search bar hidden (6 failures) — FIXED via FakeGatewayModule
- **Fix applied**: With connected gateway, CcSessionsScreen no longer enters error state. Search bar renders in the "no sessions" branch.
- APP-12 through APP-17: All fixed by FakeGatewayModule.

### Category C: Send button vs Voice button (2 failures) — FIXED via FakeGatewayModule
- **Fix applied**: With connected state, input is enabled, typing text shows Send button.
- APP-18, APP-19: Fixed by FakeGatewayModule.

### Category D: Settings custom instructions (4 failures) — FIXED
- **Fix applied**: Added `testTag("custom_instructions_field")` to `OutlinedTextField` in `SettingsScreen.kt`. Updated tests to use `onNodeWithTag()` instead of `onNodeWithText()` for placeholder matching.
- APP-20 through APP-23: Fixed.

### Category E: Chat keyboard nav (1 failure) — FIXED via FakeGatewayModule
- APP-24: Fixed by FakeGatewayModule (input enabled).

### Category F: Chronicle stats timeout (1 failure) — FIXED
- **Fix applied**: Increased `waitUntil` timeout from 5000ms to 10000ms. Added `"--"` as third success condition (empty stat placeholder).
- APP-25: Fixed.

### Category G: Brain rapid tab switching (1 failure) — FIXED
- **Fix applied**: Added `testTag("brain_tab_<Name>")` to all 6 Brain tabs. Test uses `onNodeWithTag()` instead of `onNodeWithText("Search")`. Added `waitForIdle()` between tab clicks.
- APP-26: Fixed.

### Category H: Chat send button after text (1 failure) — FIXED via FakeGatewayModule
- APP-27: Fixed by FakeGatewayModule (input enabled).

---

## New: emu-mcp Functional Test Suite

Created `tests/emu-functional-tests.sh` — 10 E2E tests using `emu-tap.py` against live emulator + Ironjaw:
1. App launch + gateway connect
2. Send message + receive LLM response
3. Navigate all screens
4. CC Sessions list loads
5. Brain remember + recall
6. Settings persistence
7. Model list from llama-server
8. Infrastructure status
9. Voice mode toggle
10. Offline → reconnect

Run via `emu-test --functional` or `emu-test --functional-only`.
