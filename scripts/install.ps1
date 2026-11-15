# Installs the portable release build to a stable per-user location so that
# shortcuts, the tray icon, and the config directory stay consistent between
# builds. Does not create shortcuts or startup entries.

$ErrorActionPreference = "Stop"

function Fail([string]$Message) {
  [Console]::Error.WriteLine("error: $Message")
  exit 1
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$source = Join-Path $repoRoot "src-tauri\target\release\window-switcher.exe"
$installDir = Join-Path $env:LOCALAPPDATA "Programs\Window Switcher"
$target = Join-Path $installDir "Window Switcher.exe"

if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
  Fail "Release build not found: `"$source`". Run `"npm run release`" (or `"npm run tauri build`") first."
}

Get-Process -Name "window-switcher", "Window Switcher" -ErrorAction SilentlyContinue |
  Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300

New-Item -ItemType Directory -Path $installDir -Force | Out-Null

try {
  Copy-Item -LiteralPath $source -Destination $target -Force
} catch {
  Fail "Could not copy `"$source`" to `"$target`". Make sure Window Switcher is not running. $($_.Exception.Message)"
}

Write-Output "Installed: $target"
