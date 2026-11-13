param([string]$Exe = "target/release/opnoverlay.exe")
$ErrorActionPreference = "Stop"
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -property installationPath
$dumpbin = Get-ChildItem "$vs\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" | Select-Object -First 1
$deps = & $dumpbin.FullName /dependents $Exe | Where-Object { $_ -match '\.dll' } | ForEach-Object { $_.Trim() }
$deps
$runtime = $deps | Where-Object { $_ -match '^(vcruntime|msvcp|api-ms-win-crt-)' }
if ($runtime) { throw "$Exe needs the Visual C++ runtime: $($runtime -join ', ')" }
