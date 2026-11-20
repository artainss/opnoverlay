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
$messages = cargo build --manifest-path (Join-Path $root 'Cargo.toml') --locked --release --message-format=json
if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
$icon = $messages | ForEach-Object { $_ | ConvertFrom-Json } |
    Where-Object { $_.reason -eq 'build-script-executed' -and (Split-Path -Leaf (Split-Path $_.out_dir)) -like 'opnoverlay-*' } |
    ForEach-Object { Get-Item -LiteralPath (Join-Path $_.out_dir 'icon.ico') -ErrorAction SilentlyContinue } |
    Select-Object -First 1
if (-not $icon) { throw 'Cargo reported no icon.ico for opnoverlay' }

wix build (Join-Path $PSScriptRoot 'opnoverlay.wxs') -arch x64 -ext WixToolset.UI.wixext `
    -d "Version=$Version" -d "BinDir=$BinDir" -d "Icon=$($icon.FullName)" -d "PresentMonDir=$(Join-Path $root 'third-party\presentmon')" -pdbtype none -o $Out
if ($LASTEXITCODE) { throw "wix build failed with exit code $LASTEXITCODE" }
