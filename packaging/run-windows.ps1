# Installed launcher: add GStreamer DLLs only for this process, not the user's PATH.
$ErrorActionPreference = 'Stop'
$gstRoot = (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'gstreamer-path.txt') -Raw).Trim()
$env:PATH = (Join-Path $gstRoot 'bin') + ';' + $env:PATH
& (Join-Path $PSScriptRoot 'replay-rs.exe') @args
exit $LASTEXITCODE
