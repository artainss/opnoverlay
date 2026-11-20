param(
    [Parameter(Mandatory)] [string] $BinDir,
    [Parameter(Mandatory)] [string] $Out,
    [string] $Version
)
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot
if (-not $Version) {
    $Version = (Select-String -LiteralPath (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
}
$BinDir = (Resolve-Path -LiteralPath $BinDir).ProviderPath
$exe = (Resolve-Path -LiteralPath (Join-Path $BinDir 'opnoverlay.exe')).ProviderPath
$icon = Join-Path $BinDir 'opnoverlay.ico'
if (-not ('OpnoverlayInstallerIcon' -as [type])) {
    Add-Type -LiteralPath (Join-Path $PSScriptRoot 'IconResource.cs')
}
[OpnoverlayInstallerIcon]::Save($exe, $icon)

wix build (Join-Path $PSScriptRoot 'opnoverlay.wxs') -arch x64 -ext WixToolset.UI.wixext `
    -d "Version=$Version" -d "BinDir=$BinDir" -d "Icon=$icon" -d "PresentMonDir=$(Join-Path $root 'third-party\presentmon')" -pdbtype none -o $Out
if ($LASTEXITCODE) { throw "wix build failed with exit code $LASTEXITCODE" }
