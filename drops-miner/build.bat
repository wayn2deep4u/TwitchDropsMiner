@echo off
rem Builds the Drops Miner installer. Needs Rust (https://rustup.rs, the MSVC option).
cd /d "%~dp0"
where cargo >nul 2>nul || (
  echo Rust isn't installed. Get it from https://rustup.rs ^(choose the MSVC option^), then run this again.
  pause
  exit /b 1
)
cargo tauri --version >nul 2>nul || (
  echo Installing the Tauri CLI ^(first time only, a few minutes^)...
  cargo install tauri-cli --version "^2" --locked || goto :fail
)
echo Testing the engine...
cargo test -p drops-miner-core || goto :fail
echo Building Drops Miner...
cargo tauri build --bundles nsis || goto :fail
echo.
echo Done. The installer is in target\release\bundle\nsis
start "" "%~dp0target\release\bundle\nsis"
pause
exit /b 0
:fail
echo.
echo The build stopped with an error (see above).
pause
exit /b 1
