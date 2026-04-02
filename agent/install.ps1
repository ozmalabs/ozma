# Ozma Agent — One-line installer for Windows
#
# Usage (PowerShell):
#   irm https://ozma.dev/install-agent.ps1 | iex
#   irm https://ozma.dev/install-agent.ps1 -OutFile install.ps1; .\install.ps1 -Controller https://ozma.hrdwrbob.net
#
# What it does:
#   1. Checks for Python 3.11+
#   2. uv pip installs ozma-agent
#   3. Registers as background service (Task Scheduler)
#   4. The machine appears in your dashboard

param(
    [string]$Controller = "",
    [string]$Name = $env:COMPUTERNAME
)

Write-Host "Ozma Agent Installer" -ForegroundColor White
Write-Host ""

# Check Python
$python = $null
foreach ($cmd in @("python3", "python", "py -3")) {
    try {
        $ver = & $cmd.Split()[0] @($cmd.Split() | Select-Object -Skip 1) -c "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')" 2>$null
        $parts = $ver.Split(".")
        if ([int]$parts[0] -ge 3 -and [int]$parts[1] -ge 11) {
            $python = $cmd
            break
        }
    } catch {}
}

if (-not $python) {
    Write-Host "Python 3.11+ required but not found." -ForegroundColor Red
    Write-Host ""
    Write-Host "Install from: https://python.org/downloads"
    Write-Host "Make sure to check 'Add to PATH' during installation."
    exit 1
}

$pyVer = & $python.Split()[0] --version 2>&1
Write-Host "Python: $pyVer" -ForegroundColor Green

# Install
Write-Host ""
Write-Host "Installing ozma-agent..." -ForegroundColor White
uv pip install --quiet --upgrade ozma-agent 2>$null
if ($LASTEXITCODE -ne 0) {
    Write-Host "uv pip install failed. Trying from source..." -ForegroundColor Yellow
    uv pip install --quiet --upgrade "git+https://github.com/ozmalabs/ozma.git#subdirectory=agent"
}

# Find the binary
$agentBin = Get-Command ozma-agent -ErrorAction SilentlyContinue
if (-not $agentBin) {
    $agentBin = "$python -m cli"
}

Write-Host "Agent: $($agentBin.Source ?? $agentBin)" -ForegroundColor Green

# Prompt for controller URL
if (-not $Controller) {
    Write-Host ""
    $Controller = Read-Host "Enter your controller URL (e.g., https://ozma.hrdwrbob.net)"
    if (-not $Controller) {
        Write-Host "No controller URL. Set it later:" -ForegroundColor Yellow
        Write-Host "  ozma-agent config --set controller https://your-controller"
        exit 0
    }
}

# Install as service
Write-Host ""
Write-Host "Installing as background service..." -ForegroundColor White
Write-Host "  Machine name: $Name"
Write-Host "  Controller:   $Controller"
Write-Host ""

ozma-agent install --name $Name --controller $Controller

if ($LASTEXITCODE -eq 0) {
    Write-Host ""
    Write-Host "Done!" -ForegroundColor Green
    Write-Host ""
    Write-Host "  Your machine is now in the ozma mesh."
    Write-Host "  Open your dashboard: $Controller"
    Write-Host ""
    Write-Host "  Commands:"
    Write-Host "    ozma-agent status      Check if running"
    Write-Host "    ozma-agent logs        View logs"
    Write-Host "    ozma-agent uninstall   Remove"
} else {
    Write-Host "Service install failed. Try:" -ForegroundColor Red
    Write-Host "  ozma-agent run --name $Name --controller $Controller"
}
