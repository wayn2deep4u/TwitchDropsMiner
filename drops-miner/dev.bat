@echo off
rem Runs Drops Miner straight from the source, without installing it.
cd /d "%~dp0"
cargo tauri --version >nul 2>nul || cargo install tauri-cli --version "^2" --locked
cargo tauri dev
