@echo off
REM B2 sample subprocess hook — unconditionally Blocks on UserPromptSubmit.
REM
REM Used for Smoke Test 6: confirms that a Block action on UserPromptSubmit
REM short-circuits chat.send to a HOOK_BLOCKED frame, with no LLM call and
REM no user message persisted.
REM
REM Returns JSON on stdout per subprocess.rs `parse_hook_action_from_json`:
REM   {"action":"block","reason":"<text>"}

REM Drain stdin (HookContext payload) without using it.
findstr "." > nul

echo {"action":"block","reason":"sample block-prompt hook engaged"}
exit /b 0
