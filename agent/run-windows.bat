@echo off
REM Ozma Agent — Quick Start (no install needed)
REM Just double-click this file or run from command prompt.

echo.
echo   OZMA AGENT
echo   ==========
echo.

REM Find Python
where python >nul 2>&1
if %errorlevel% neq 0 (
    echo   ERROR: Python 3.11+ required. Install from python.org
    pause
    exit /b 1
)

REM Install deps if needed
python -c "import aiohttp" 2>nul
if %errorlevel% neq 0 (
    echo   Installing dependencies...
    pip install aiohttp zeroconf dxcam numpy Pillow aiortc --quiet
)

REM Run the agent
echo   Starting ozma-agent...
echo   Press Ctrl+C to stop.
echo.

cd /d "%~dp0"
python -m ozma_desktop_agent %*
