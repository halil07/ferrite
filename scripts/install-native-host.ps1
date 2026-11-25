param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[a-p]{32}$')]
    [string]$ExtensionId,

    [Parameter(Mandatory = $false)]
    [string]$BinaryPath = (Join-Path $PSScriptRoot "..\target\release\ferrite.exe"),

    [Parameter(Mandatory = $false)]
    [ValidateSet("chrome", "edge", "brave", "chromium")]
    [string]$Browser = "chrome"
)

$ErrorActionPreference = "Stop"

$resolvedBinary = (Resolve-Path -LiteralPath $BinaryPath).Path
$hostName = "com.ferrite.e2e"
$installDir = Join-Path $env:LOCALAPPDATA "FerriteE2E"
$manifestPath = Join-Path $installDir "$hostName.json"

New-Item -ItemType Directory -Force -Path $installDir | Out-Null

$manifest = [ordered]@{
    name            = $hostName
    description     = "Ferrite E2E Native Messaging Host (CDP + Tokio Automation Daemon)"
    path            = $resolvedBinary
    type            = "stdio"
    allowed_origins = @("chrome-extension://$ExtensionId/")
}

$json = $manifest | ConvertTo-Json -Depth 4
[System.IO.File]::WriteAllText($manifestPath, $json, [System.Text.UTF8Encoding]::new($false))

$regBase = switch ($Browser) {
    "chrome"   { "HKCU:\Software\Google\Chrome\NativeMessagingHosts" }
    "edge"     { "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts" }
    "brave"    { "HKCU:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts" }
    "chromium" { "HKCU:\Software\Chromium\NativeMessagingHosts" }
}

$regKey = Join-Path $regBase $hostName
New-Item -Path $regKey -Force | Out-Null
Set-ItemProperty -Path $regKey -Name "(default)" -Value $manifestPath

Write-Host "Installed Ferrite Native Messaging manifest at: $manifestPath"
Write-Host "Registered $Browser HKCU NativeMessagingHosts key: $regKey"
