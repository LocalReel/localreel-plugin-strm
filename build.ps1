# Builds the plugin and stages a package directory in pkg/.
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
cargo build --release
$binary = "target\release\strm_plugin.exe"
if (-not (Test-Path $binary)) { throw "missing $binary" }
New-Item -ItemType Directory -Force -Path 'pkg\bin' | Out-Null
Copy-Item 'manifest.json' 'pkg\manifest.json' -Force
Copy-Item $binary 'pkg\bin\plugin.exe' -Force
Write-Host 'plugin staged in pkg/'
