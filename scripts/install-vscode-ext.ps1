$ErrorActionPreference = "Stop"

$srcDir = (Resolve-Path (Join-Path $PSScriptRoot "..\vscode-ferrite")).Path
$pkg = Get-Content -Raw (Join-Path $srcDir "package.json") | ConvertFrom-Json
$extFolderName = "$($pkg.publisher).$($pkg.name)-$($pkg.version)"

$vscodeExtRoot = Join-Path $env:USERPROFILE ".vscode\extensions"
New-Item -ItemType Directory -Force -Path $vscodeExtRoot | Out-Null

$destDir = Join-Path $vscodeExtRoot $extFolderName
if (Test-Path $destDir) {
    Remove-Item -Recurse -Force $destDir
}

Copy-Item -Recurse -Force -Path $srcDir -Destination $destDir
Write-Host "Installed VS Code Ferrite extension to: $destDir"
Write-Host "Reload VS Code (Ctrl+Shift+P -> Developer: Reload Window) to activate .ferrite syntax highlighting."
