@echo off
setlocal
cd /d "%~dp0"
title Bonkr Twitch Miner (dev)
call :main
echo.
echo Press any key to close this window.
pause >nul
exit /b

:main
where cargo >nul 2>nul
if errorlevel 1 goto norust
cargo tauri --version >nul 2>nul
if not errorlevel 1 goto run
echo Installing the Tauri tools (first time only, 5 to 10 minutes)...
cargo install tauri-cli --version "^2" --locked
if errorlevel 1 exit /b 1
:run
cargo tauri dev
exit /b

:norust
echo Rust isn't installed, or this window can't find it yet. See build.bat or README.md.
exit /b 1
