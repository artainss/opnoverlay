param(
    [Parameter(Mandatory)] [string] $Msi,
    [Parameter(Mandatory)] [string] $UpgradeMsi
)
$ErrorActionPreference = 'Stop'

$installDir = Join-Path $env:LOCALAPPDATA 'Programs\opnoverlay'
$startMenu = Join-Path ([Environment]::GetFolderPath('Programs')) 'opnoverlay.lnk'
$desktop = Join-Path ([Environment]::GetFolderPath('Desktop')) 'opnoverlay.lnk'
$exe = Join-Path $installDir 'opnoverlay.exe'

function Msiexec([string[]] $arguments) {
    $log = Join-Path $env:TEMP 'opnoverlay-msi.log'
    $p = Start-Process msiexec.exe -ArgumentList ($arguments + @('/qn', '/l*v', "`"$log`"")) -Wait -PassThru
    if ($p.ExitCode -ne 0) {
        Get-Content $log -Tail 40
        throw "msiexec $arguments failed with exit code $($p.ExitCode)"
    }
}

function Check([bool] $ok, [string] $what) {
    if (-not $ok) { throw "FAILED: $what" }
    "ok: $what"
}

function ShortcutTarget([string] $path) {
    (New-Object -ComObject WScript.Shell).CreateShortcut($path).TargetPath
}

function Installed {
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    Get-ChildItem "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Installer\UserData\$sid\Products" -ErrorAction SilentlyContinue |
        ForEach-Object { Get-Item (Join-Path $_.PSPath 'InstallProperties') -ErrorAction SilentlyContinue } |
        Where-Object { $_.GetValue('DisplayName') -eq 'opnoverlay' }
}

function CheckRemoved {
    Check (-not (Test-Path $installDir)) 'install folder removed'
    Check (-not (Test-Path $startMenu)) 'Start menu shortcut removed'
    Check (-not (Test-Path $desktop)) 'no desktop shortcut left'
    Check (-not (Installed)) 'no entry left in Installed apps'
    Check (-not (Test-Path 'HKCU:\Software\opnoverlay\Installer')) 'installer registry key removed'
}

Check (-not (Installed)) 'opnoverlay is not installed before the test'

Msiexec @('/i', "`"$Msi`"")
foreach ($file in 'opnoverlay.exe', 'PresentMon.exe', 'PRESENTMON-LICENSE.txt', 'LICENSE.txt', 'THIRD-PARTY-LICENSES.html') {
    Check (Test-Path (Join-Path $installDir $file)) "$file installed"
}
Check ((ShortcutTarget $startMenu) -eq $exe) 'Start menu shortcut "opnoverlay" points to opnoverlay.exe'
Check (-not (Test-Path $desktop)) 'no desktop shortcut by default'
$entry = Installed
Check ($null -ne $entry) 'listed in Installed apps as "opnoverlay"'
Check ($entry.GetValue('InstallLocation') -eq "$installDir\") 'Installed apps entry has the install folder'
$icon = Get-ChildItem 'HKCU:\Software\Microsoft\Installer\Products' |
    Where-Object { $_.GetValue('ProductName') -eq 'opnoverlay' } | ForEach-Object { $_.GetValue('ProductIcon') }
Check ([bool] $icon -and (Test-Path $icon)) 'Installed apps entry has the opnoverlay icon'
Msiexec @('/x', "`"$Msi`"")
CheckRemoved

Msiexec @('/i', "`"$Msi`"", 'DESKTOPSHORTCUT=1')
Check ((ShortcutTarget $desktop) -eq $exe) 'desktop shortcut "opnoverlay" points to opnoverlay.exe'
$before = (Installed).GetValue('DisplayVersion')
Msiexec @('/i', "`"$UpgradeMsi`"")
$after = Installed
Check (@($after).Count -eq 1 -and $after.GetValue('DisplayVersion') -ne $before) "upgraded in place ($before -> $($after.GetValue('DisplayVersion')))"
Check ((ShortcutTarget $desktop) -eq $exe) 'desktop shortcut kept by the upgrade'
Check ((ShortcutTarget $startMenu) -eq $exe) 'Start menu shortcut kept by the upgrade'
Msiexec @('/x', "`"$UpgradeMsi`"")
CheckRemoved
