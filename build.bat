@echo off
:: ─────────────────────────────────────────────────────────────────────────────
:: sRemote build script — builds broker.exe and daemon.exe
:: Run from the repo root.  Requires Rust (cargo) to be installed.
:: ─────────────────────────────────────────────────────────────────────────────

echo [sRemote] Building Session Broker (broker.exe)...
cargo build --release -p broker
if errorlevel 1 ( echo [ERROR] broker build failed & exit /b 1 )

echo [sRemote] Building Privileged Capture Daemon (daemon.exe)...
cargo build --release -p daemon
if errorlevel 1 ( echo [ERROR] daemon build failed & exit /b 1 )

echo [sRemote] Copying .env to release folder...
copy /Y .env target\release\.env >nul
if errorlevel 1 ( echo [WARNING] Could not copy .env to target\release )

echo [sRemote] Copying console UI to release folder...
xcopy /E /I /Y console target\release\console >nul
if errorlevel 1 ( echo [WARNING] Could not copy console UI to target\release )

echo.
echo ════════════════════════════════════════════════════════════════
echo  Build complete!  Binaries in:
echo    target\release\broker.exe
echo    target\release\daemon.exe
echo.
echo  IMPORTANT: copy openh264.dll next to daemon.exe before running.
echo  Download:  http://ciscobinary.openh264.org/openh264-2.4.0-win64.dll.bz2
echo  Extract the .dll, rename it openh264.dll, place it in target\release\
echo ════════════════════════════════════════════════════════════════
