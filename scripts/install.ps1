<#
.SYNOPSIS
  Install (or remove) the native Rust aifuel executable on Windows.

.DESCRIPTION
  Builds the Rust binary and copies aifuel.exe to a bin dir (default
  ~\.local\bin), then puts that dir on your user PATH.

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
$Cmd = 'aifuel'
$Launcher = Join-Path $BinDir "$Cmd.exe"

if ($Uninstall) {
    if (Test-Path $Launcher) {
        Remove-Item $Launcher -Force
        Write-Host "Removed $Launcher"
    } else {
        Write-Host "Nothing to remove at $Launcher"
    }
    return
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo is required to build the Rust aifuel binary"
}

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$TargetBinary = Join-Path $RepoRoot 'target\release\aifuel.exe'
& cargo build --release --locked -p aifuel
if (-not (Test-Path $TargetBinary)) {
    Write-Error "Rust build did not produce $TargetBinary"
}

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
Copy-Item -Path $TargetBinary -Destination $Launcher -Force

Write-Host "Installed $Cmd -> $TargetBinary"
Write-Host "  at $Launcher"

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
