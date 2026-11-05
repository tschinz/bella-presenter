# One-line installer for bella-presenter on Windows, from the latest GitHub release.
#
#   irm https://raw.githubusercontent.com/tschinz/bella-presenter/main/install.ps1 | iex
#
# Downloads the portable zip (x64), extracts it to %LOCALAPPDATA%\Programs\bella-presenter,
# and creates a Start Menu shortcut.
$ErrorActionPreference = 'Stop'

$repo = 'tschinz/bella-presenter'
$url  = "https://github.com/$repo/releases/latest/download/bella-presenter-windows-x64.zip"
$dest = Join-Path $env:LOCALAPPDATA 'Programs\bella-presenter'
$zip  = Join-Path $env:TEMP 'bella-presenter.zip'

Write-Host "Downloading bella-presenter for Windows (x64)…"
Invoke-WebRequest -Uri $url -OutFile $zip

Write-Host "Installing to $dest…"
if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
Expand-Archive -Path $zip -DestinationPath $dest -Force
Remove-Item $zip

# The zip contains a top-level 'bella-presenter' folder (bella-presenter.exe + pdfium.dll).
$exe = Join-Path $dest 'bella-presenter\bella-presenter.exe'

$startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
$shortcut  = (New-Object -ComObject WScript.Shell).CreateShortcut((Join-Path $startMenu 'bella-presenter.lnk'))
$shortcut.TargetPath = $exe
$shortcut.Save()

# Register as an "Open with" choice for .pdf (per-user, no admin; does not change the default app).
$app = 'HKCU:\Software\Classes\Applications\bella-presenter.exe'
New-Item -Force -Path "$app\shell\open\command" -Value "`"$exe`" `"%1`"" | Out-Null
New-Item -Force -Path 'HKCU:\Software\Classes\.pdf\OpenWithList\bella-presenter.exe' | Out-Null

Write-Host "✓ Installed. Launch 'bella-presenter' from the Start Menu, or run:"
Write-Host "    $exe"
