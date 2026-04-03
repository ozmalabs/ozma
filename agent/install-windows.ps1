# Ozma Agent — Windows Install Script
# Run in PowerShell (as Administrator for multi-seat user isolation):
#   irm https://raw.githubusercontent.com/ozmalabs/ozma/main/agent/install-windows.ps1 | iex
#
# Or manually:
#   powershell -ExecutionPolicy Bypass -File install-windows.ps1

$ErrorActionPreference = "Stop"

Write-Host "`n  OZMA AGENT INSTALLER" -ForegroundColor Green
Write-Host "  ═══════════════════`n" -ForegroundColor DarkGreen

# ── Check Python ─────────────────────────────────────────────────────
$python = $null
foreach ($cmd in @("python3", "python", "py -3")) {
    try {
        $ver = & $cmd.Split()[0] $cmd.Split()[1..99] --version 2>$null
        if ($ver -match "3\.(1[1-9]|[2-9]\d)") {
            $python = $cmd
            Write-Host "  Python: $ver" -ForegroundColor Cyan
            break
        }
    } catch {}
}

if (-not $python) {
    Write-Host "  Python 3.11+ not found. Installing..." -ForegroundColor Yellow
    # Download and install Python
    $pyUrl = "https://www.python.org/ftp/python/3.13.1/python-3.13.1-amd64.exe"
    $pyInstaller = "$env:TEMP\python-installer.exe"
    Invoke-WebRequest -Uri $pyUrl -OutFile $pyInstaller
    Start-Process -Wait -FilePath $pyInstaller -ArgumentList "/quiet InstallAllUsers=1 PrependPath=1"
    Remove-Item $pyInstaller
    $python = "python"
    Write-Host "  Python installed" -ForegroundColor Green
}

# ── Install ozma-agent ───────────────────────────────────────────────
Write-Host "`n  Installing ozma-agent..." -ForegroundColor Cyan

# Install from PyPI (when published) or from local/git
$agentDir = Split-Path -Parent $MyInvocation.MyCommand.Path
if (Test-Path "$agentDir\setup.py") {
    # Local install (development)
    & $python.Split()[0] $python.Split()[1..99] -m pip install -e "$agentDir[windows,multiseat]" --quiet
} else {
    # From PyPI
    & $python.Split()[0] $python.Split()[1..99] -m pip install "ozma-agent[windows,multiseat]" --quiet
}

Write-Host "  ozma-agent installed" -ForegroundColor Green

# ── Install optional deps ────────────────────────────────────────────
Write-Host "`n  Installing optional components..." -ForegroundColor Cyan

# ffmpeg (needed for screen capture encoding)
$ffmpeg = Get-Command ffmpeg -ErrorAction SilentlyContinue
if (-not $ffmpeg) {
    Write-Host "  Installing ffmpeg..." -ForegroundColor Yellow
    # Use winget if available, otherwise download
    try {
        winget install Gyan.FFmpeg --silent --accept-package-agreements 2>$null
    } catch {
        Write-Host "  Please install ffmpeg manually: https://ffmpeg.org/download.html" -ForegroundColor Yellow
    }
}

# ── Create startup shortcut ──────────────────────────────────────────
$startupDir = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Startup"
$shortcutPath = "$startupDir\Ozma Agent.lnk"

$createShortcut = Read-Host "`n  Start ozma-agent on login? (Y/n)"
if ($createShortcut -ne "n") {
    $WshShell = New-Object -ComObject WScript.Shell
    $shortcut = $WshShell.CreateShortcut($shortcutPath)
    $shortcut.TargetPath = (Get-Command python).Source
    $shortcut.Arguments = "-m agent.ozma_desktop_agent"
    $shortcut.WorkingDirectory = $agentDir
    $shortcut.WindowStyle = 7  # Minimized
    $shortcut.Description = "Ozma Agent"
    $shortcut.Save()
    Write-Host "  Startup shortcut created" -ForegroundColor Green
}

# ── Print instructions ───────────────────────────────────────────────
Write-Host "`n  ════════════════════════════════════════" -ForegroundColor DarkGreen
Write-Host "  INSTALLED SUCCESSFULLY" -ForegroundColor Green
Write-Host ""
Write-Host "  Start the agent:" -ForegroundColor White
Write-Host "    ozma-agent" -ForegroundColor Cyan
Write-Host ""
Write-Host "  Start with multi-seat (2 seats):" -ForegroundColor White
Write-Host "    ozma-agent --seats 2" -ForegroundColor Cyan
Write-Host ""
Write-Host "  Connect to a controller:" -ForegroundColor White
Write-Host "    ozma-agent --controller http://your-controller:7380" -ForegroundColor Cyan
Write-Host ""
Write-Host "  Management API:" -ForegroundColor White
Write-Host "    http://localhost:7399/api/v1/seats" -ForegroundColor Cyan
Write-Host "  ════════════════════════════════════════`n" -ForegroundColor DarkGreen
