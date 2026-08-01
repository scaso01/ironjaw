@echo off
REM Ironjaw WebSocket Gateway — wrapper script for Task Scheduler
REM Runs the Ironjaw binary with config, logging stderr to file.
REM Use ironjaw-wrapper-hidden.vbs to run without console window.

cd /D "%~dp0\.."

set IRONJAW_CONFIG=config\ironjaw.toml
set RUST_LOG=info

target\release\ironjaw.exe %IRONJAW_CONFIG% 2>> logs\ironjaw-stderr.log
