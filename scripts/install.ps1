<#
.SYNOPSIS
  Install (or remove) `aisub` — a global launcher for usage_monitor.py — on Windows.

.DESCRIPTION
  Drops an `aisub.cmd` shim in a bin dir (default ~\.local\bin) that forwards to
  this repo's usage_monitor.py, and puts that dir on your user PATH. After install:
    aisub          -> python usage_monitor.py        (web dashboard)
    aisub --json   -> python usage_monitor.py --json
    aisub --text   -> ... and every other flag passes through.

.EXAMPLE
  .\install.ps1
  .\install.ps1 -Uninstall
  .\install.ps1 -BinDir 'C:\tools\bin'
#>
[CmdletBinding()]
param(
    [switch]$Uninstall,
    [string]$BinDir = (Join-Path $HOME '.local\bin')
)

$ErrorActionPreference = 'Stop'
$Cmd = 'aisub'
$Launcher = Join-Path $BinDir "$Cmd.cmd"

if ($Uninstall) {
    if (Test-Path $Launcher) {
        Remove-Item $Launcher -Force
        Write-Host "Removed $Launcher"
    } else {
        Write-Host "Nothing to remove at $Launcher"
    }
    return
}

# usage_monitor.py lives in src/ (one level up from scripts/).
$TargetPy = Join-Path $PSScriptRoot '..\src\usage_monitor.py'
if (-not (Test-Path $TargetPy)) {
    Write-Error "usage_monitor.py not found next to install.ps1 ($TargetPy)"
}

# Find a Python interpreter to bake into the shim.
$Python = $null
foreach ($cand in @('python', 'python3', 'py')) {
    if (Get-Command $cand -ErrorAction SilentlyContinue) { $Python = $cand; break }
}
if (-not $Python) {
    Write-Error "no python found on PATH (install Python 3 from python.org)"
}
# The `py` launcher needs -3 to force Python 3.
$PyInvoke = if ($Python -eq 'py') { 'py -3' } else { $Python }

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

# A .cmd shim runs from cmd, PowerShell, and Explorer alike. ASCII = no BOM,
# which a .cmd file chokes on. %* forwards every argument through.
@"
@echo off
$PyInvoke "$TargetPy" %*
"@ | Set-Content -Path $Launcher -Encoding ASCII

Write-Host "Installed $Cmd -> $TargetPy"
Write-Host "  at $Launcher (via $PyInvoke)"

# Ensure the bin dir is on the persisted user PATH, adding it if missing.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$onPath = ($userPath -split ';') -contains $BinDir
if (-not $onPath) {
    $newPath = if ([string]::IsNullOrEmpty($userPath)) { $BinDir } else { "$userPath;$BinDir" }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host ""
    Write-Host "Added $BinDir to your user PATH. Open a NEW terminal for it to take effect."
}

Write-Host ""
Write-Host "Try it (new terminal):  $Cmd --text"
