$ErrorActionPreference = 'Stop'

$Repo = Split-Path -Parent $PSScriptRoot
$Target = 'x86_64-pc-windows-msvc'
$Dist = Join-Path $PSScriptRoot 'dist'

Push-Location $Repo
try {
    cargo build --release --target $Target -p ozma-desktop
    New-Item -ItemType Directory -Force -Path $Dist | Out-Null
    Copy-Item "target/$Target/release/ozma-desktop.exe" $Dist
    Copy-Item "$PSScriptRoot/assets/ozma.ico" $Dist
    Write-Host "Built Ozma tray shell: $Dist"
    Write-Host 'Set OZMA_CONTROLLER_URL to the controller base URL, then run ozma-desktop.exe.'
} finally {
    Pop-Location
}
