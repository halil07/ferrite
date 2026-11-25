param(
    [Parameter(Mandatory = $false)]
    [string]$OutDir = (Join-Path $PSScriptRoot "..\dist")
)

$ErrorActionPreference = "Stop"

$extDir = (Resolve-Path (Join-Path $PSScriptRoot "..\extension")).Path
$manifest = Get-Content -Raw (Join-Path $extDir "manifest.json") | ConvertFrom-Json
$version = $manifest.version

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$zipPath = Join-Path (Resolve-Path $OutDir).Path "ferrite-e2e-extension-v$version.zip"

if (Test-Path $zipPath) {
    Remove-Item -Force $zipPath
}

Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::CreateFromDirectory($extDir, $zipPath)

Write-Host "Packaged Chrome Web Store extension zip at: $zipPath"
