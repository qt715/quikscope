# Run as the signed-in test user in a disposable Windows test environment.
# The caller owns V:\qk-pkg and must not use this on a workstation installation.
param([Parameter(Mandatory=$true)][string]$Installer)
$ErrorActionPreference = 'Stop'
$install = Join-Path $env:LOCALAPPDATA 'Programs\quikscope'
$work = 'V:\qk-pkg'
if (Test-Path $install) { throw "Refusing to replace existing installation: $install" }
$task = 'quikscope-packaging-smoke'
$exe = Join-Path $install 'quikscope.exe'
function Invoke-Quikscope {
    param([string[]]$CliArgs)
    # The Windows-subsystem executable is asynchronous to PowerShell's call
    # operator; explicitly wait before asserting its effects.
    $command = Start-Process -FilePath $exe -ArgumentList $CliArgs -Wait -PassThru
    if ($command.ExitCode -ne 0) { throw "quikscope $CliArgs failed: $($command.ExitCode)" }
}
$config = Join-Path $env:APPDATA 'quikscope'
if (Test-Path $config) { throw "Refusing to change existing user config: $config" }
New-Item -ItemType Directory -Force $work, $config | Out-Null
$settings = @"
save_dir = 'V:/qk-pkg/shots'
copy_to_clipboard = false
notify = false
[hotkeys]
region = ''
screen = ''
window = ''
open_last = ''
"@
[IO.File]::WriteAllText((Join-Path $config 'config.toml'), $settings, (New-Object Text.UTF8Encoding($false)))
try {
    # /S exercises the default current-user installation without elevation.
    $p = Start-Process -FilePath $Installer -ArgumentList '/S' -Wait -PassThru
    if ($p.ExitCode -ne 0 -or !(Test-Path $exe)) { throw 'Silent install failed' }
    Invoke-Quikscope -CliArgs @('--version')
    $arp = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope'
    if (!$arp.DisplayVersion -or !(Test-Path $arp.DisplayIcon)) { throw 'Missing Installed Apps metadata' }
    if (!(Test-Path (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\quikscope.lnk'))) { throw 'Missing Start menu shortcut' }
    $action = New-ScheduledTaskAction -Execute $exe -Argument 'daemon'
    $principal = New-ScheduledTaskPrincipal -UserId ([System.Security.Principal.WindowsIdentity]::GetCurrent().Name) -LogonType Interactive -RunLevel Limited
    Register-ScheduledTask -TaskName $task -Action $action -Principal $principal -Force | Out-Null
    Start-ScheduledTask -TaskName $task
    Start-Sleep -Seconds 3
    Invoke-Quikscope -CliArgs @('capture', 'screen', '--save', '--no-notify')
    if (!(Get-ChildItem "$work\shots\*.png")) { throw 'CLI screen capture failed' }
    # Reinstall over the running daemon: replacement must stop it cleanly.
    $p = Start-Process -FilePath $Installer -ArgumentList '/S' -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw 'Upgrade-over-install failed' }
    Invoke-Quikscope -CliArgs @('autostart', 'on')
    Invoke-Quikscope -CliArgs @('autostart', 'status')
    $p = Start-Process -FilePath (Join-Path $install 'Uninstall.exe') -ArgumentList '/S' -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw 'Uninstaller failed' }
    # NSIS runs its uninstaller from a temporary copy; wait for that copy too.
    for ($i = 0; $i -lt 100 -and (Test-Path $install); $i++) { Start-Sleep -Milliseconds 100 }
    if (Test-Path $install) { throw 'Install directory remains after uninstall' }
    if (Test-Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\quikscope') { throw 'Installed Apps entry remains' }
    if (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name quikscope -ErrorAction SilentlyContinue) { throw 'Login startup remains after uninstall' }
    if (Test-Path 'HKCU:\Software\quikscope') { throw 'Installer registry key remains' }
    if (Test-Path (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\quikscope.lnk')) { throw 'Start menu shortcut remains' }
    if (Get-Process quikscope -ErrorAction SilentlyContinue) { throw 'Daemon remains after uninstall' }
    $cache = Join-Path $env:LOCALAPPDATA 'quikscope'
    foreach ($name in @('last.raw', 'last.raw.*.part', 'daemon.log', 'ipc.sock')) {
        if (Test-Path (Join-Path $cache $name)) { throw "App cache remains after uninstall: $name" }
    }
    if ((Test-Path $cache) -and @(Get-ChildItem -Force $cache).Count -eq 0) { throw 'Empty cache directory remains' }
    if (!(Test-Path (Join-Path $config 'config.toml'))) { throw 'Uninstall removed user configuration' }
    Write-Output 'PASS: install, metadata, daemon, CLI capture, upgrade, autostart and uninstall'
} finally {
    Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
    # Test configuration is intentionally retained, matching uninstall policy.
}
