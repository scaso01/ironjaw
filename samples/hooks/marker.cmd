@echo off
REM B2 sample subprocess hook — appends a marker line on every invocation.
REM
REM Triggered by the event declared in marker.toml (default: SessionStart).
REM HookContext is delivered on stdin as JSON; we echo a single line into
REM the marker file with a UTC-ish timestamp and ignore the payload here.
REM
REM Exit 0 + empty stdout = HookAction::Continue
REM (see crates/ironjaw-ws/src/hooks/subprocess.rs `parse_hook_action_from_json`).

setlocal
set "MARKER_FILE=%TEMP%\ironjaw-hook-marker.log"
for /f "tokens=2 delims==" %%I in ('wmic os get localdatetime /value ^| find "="') do set DT=%%I
echo [%DT%] marker.cmd fired >> "%MARKER_FILE%"
endlocal
exit /b 0
