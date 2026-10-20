$ErrorActionPreference = "Stop"

cargo build --release

$dist = Join-Path $PSScriptRoot "dist"
if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
New-Item -ItemType Directory -Path $dist | Out-Null
Copy-Item (Join-Path $PSScriptRoot "target\release\portable_file_time.exe") (Join-Path $dist "360.exe")
Copy-Item (Join-Path $PSScriptRoot "README.md") (Join-Path $dist "README.md")

$zip = Join-Path $PSScriptRoot "360-windows.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path (Join-Path $dist "*") -DestinationPath $zip
Write-Host "Created $zip"
