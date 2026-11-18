@echo off
setlocal
cd /d "%~dp0"
title Bonkr Twitch Miner build
call :main
echo.
echo Press any key to close this window.
pause >nul
exit /b

:main
echo Bonkr Twitch Miner build
echo =================
echo.
where cargo >nul 2>nul
if errorlevel 1 goto norust
echo [1/4] Rust found:
cargo --version
echo.

cargo tauri --version >nul 2>nul
if not errorlevel 1 goto havecli
echo [2/4] Installing the Tauri tools. First time only, this takes 5 to 10 minutes...
cargo install tauri-cli --version "^2" --locked
if errorlevel 1 goto failed
:havecli
echo [2/4] Tauri tools ready.
echo.

echo [3/4] Testing the engine...
cargo test -p drops-miner-core
if errorlevel 1 goto failed
echo.

echo [4/4] Building the app. The first build takes several minutes...
cargo tauri build --bundles nsis
if errorlevel 1 goto failed
echo.
echo DONE. The installer is in:
echo   %~dp0target\release\bundle\nsis
start "" "%~dp0target\release\bundle\nsis"
exit /b 0

:norust
echo Rust isn't installed, or this window can't find it yet.
echo.
echo  1. Get it from https://rustup.rs and run rustup-init.exe
echo  2. When it asks, choose the default install. It also installs the
echo     Visual Studio C++ build tools if you don't have them; let it.
echo  3. Close this window, then double-click build.bat again.
echo.
echo If you already installed Rust, sign out of Windows and back in (or restart)
echo so Windows picks up the new PATH, then try again.
exit /b 1

:failed
echo.
echo The build stopped with an error. The red or "error:" lines above say why.
echo Copy them (or take a screenshot) and send them to Claude.
exit /b 1
