# Pulse - social-preview generator.
#
# Renders assets/social-preview.svg to assets/social-preview.png (1280x640),
# the GitHub repo "social preview" image (Settings -> General -> Social
# preview). Repeatable: re-run after editing social-preview.svg; commit the
# generated PNG. Same toolchain as gen-icons.ps1 (resvg; no ImageMagick).

[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot

$resvg = (Get-Command resvg -ErrorAction SilentlyContinue)
if (-not $resvg) { throw "resvg not found. Install with: cargo install resvg" }

$svg = Join-Path $here 'social-preview.svg'
$png = Join-Path $here 'social-preview.png'
& resvg -w 1280 -h 640 $svg $png | Out-Null
if (-not (Test-Path $png)) { throw "resvg failed for social-preview" }
Write-Host "Generated: assets\social-preview.png (1280x640)"
