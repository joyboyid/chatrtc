# Install bandit-x from GitHub Releases (Windows PowerShell).
#
# Usage:
#   .\scripts\install.ps1
#   .\scripts\install.ps1 -Version v0.1.0
#
# Env:
#   $env:BANDIT_X_REPO  default: joyboyid/bandit-X

param(
    [string]$Version = "latest"
)

$ErrorActionPreference = "Stop"
$Repo = if ($env:BANDIT_X_REPO) { $env:BANDIT_X_REPO } else { "joyboyid/bandit-X" }
$InstallDir = "$env:LOCALAPPDATA\Programs\bandit-x"
$Target = "x86_64-pc-windows-msvc"

function Get-LatestTag {
    $resp = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
    return $resp.tag_name
}

if ($Version -eq "latest") {
    $Tag = Get-LatestTag
} else {
    $Tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
}

$Ver = $Tag.TrimStart("v")
$Asset = "bandit-x-$Ver-$Target.zip"
$Url = "https://github.com/$Repo/releases/download/$Tag/$Asset"

Write-Host "→ Installing bandit-x $Tag for $Target"

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$Tmp = Join-Path $env:TEMP "bandit-x-install"
Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $Tmp | Out-Null

$ZipPath = Join-Path $Tmp $Asset
Invoke-WebRequest -Uri $Url -OutFile $ZipPath
Expand-Archive -Path $ZipPath -DestinationPath $Tmp -Force

$BinSrc = Join-Path $Tmp "bandit-x-$Ver-$Target\bandit-x.exe"
$BinDst = Join-Path $InstallDir "bandit-x.exe"
Copy-Item $BinSrc $BinDst -Force

# Add to user PATH if not present
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
    $env:Path = "$env:Path;$InstallDir"
}

Write-Host "✓ Installed to $BinDst"
Write-Host ""
Write-Host "Buka terminal baru (Windows Terminal), lalu jalankan:"
Write-Host "  bandit-x --server"
Write-Host "  bandit-x --client -a SERVER:8080 -u username"