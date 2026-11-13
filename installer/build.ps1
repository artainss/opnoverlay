param(
    [Parameter(Mandatory)] [string] $BinDir,
    [Parameter(Mandatory)] [string] $Out,
    [string] $Version
)
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot
if (-not $Version) {
    $Version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
}
$icon = Get-ChildItem (Join-Path $root 'target\release\build\opnoverlay-*\out\icon.ico') |
    Sort-Object LastWriteTime | Select-Object -Last 1
if (-not $icon) { throw 'icon.ico not found: run cargo build --release first' }

wix build (Join-Path $PSScriptRoot 'opnoverlay.wxs') -arch x64 -ext WixToolset.UI.wixext `
    -d "Version=$Version" -d "BinDir=$BinDir" -d "Icon=$($icon.FullName)" -d "PresentMonDir=$(Join-Path $root 'third-party\presentmon')" -pdbtype none -o $Out
if ($LASTEXITCODE) { throw "wix build failed with exit code $LASTEXITCODE" }
