[CmdletBinding()]
param(
    [switch]$Autostart,
    [switch]$Uninstall,
    [string]$GStreamerRoot = $env:GSTREAMER_1_0_ROOT_MSVC_X86_64,
    [string]$Binary = (Join-Path $PSScriptRoot '..\target\release\replay-rs.exe')
)
$ErrorActionPreference = 'Stop'
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\replay-rs'
$menuLink = Join-Path ([Environment]::GetFolderPath('Programs')) 'replay-rs.lnk'
$startupLink = Join-Path ([Environment]::GetFolderPath('Startup')) 'replay-rs.lnk'
if ($Uninstall) {
    foreach ($path in @($menuLink, $startupLink)) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
    }
    foreach ($name in @('replay-rs.exe', 'run-windows.ps1', 'gstreamer-path.txt')) {
        $path = Join-Path $installDir $name
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
    }
    Write-Host 'Application removed. Settings, clips and GStreamer are preserved.'
    return
}
if (-not $GStreamerRoot) { $GStreamerRoot = 'C:\gstreamer\1.0\msvc_x86_64' }
$GStreamerRoot = (Resolve-Path -LiteralPath $GStreamerRoot).Path
if (-not (Test-Path -LiteralPath (Join-Path $GStreamerRoot 'bin\gstreamer-1.0-0.dll'))) {
    throw 'Install GStreamer MSVC x86_64 Runtime (Complete), or specify -GStreamerRoot.'
}
if (-not (Test-Path -LiteralPath $Binary)) {
    throw 'Release binary not found. Run cargo build --release --locked, or specify -Binary.'
}
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
Copy-Item -LiteralPath $Binary -Destination (Join-Path $installDir 'replay-rs.exe') -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'run-windows.ps1') -Destination $installDir -Force
Set-Content -LiteralPath (Join-Path $installDir 'gstreamer-path.txt') -Value $GStreamerRoot -Encoding UTF8
$shell = New-Object -ComObject WScript.Shell
function New-ReplayShortcut([string]$Path, [string]$ExtraArgs) {
    $shortcut = $shell.CreateShortcut($Path)
    $shortcut.TargetPath = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $launcher = Join-Path $installDir 'run-windows.ps1'
    $shortcut.Arguments = "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$launcher`" $ExtraArgs"
    $shortcut.WorkingDirectory = $installDir
    $shortcut.Description = 'Replay buffer screen recorder'
    $shortcut.Save()
}
New-ReplayShortcut $menuLink ''
if ($Autostart) { New-ReplayShortcut $startupLink '--headless' }
Write-Host "Installed: $installDir"
Write-Host 'Start replay-rs from the Start menu. No administrator access required for this installer.'
