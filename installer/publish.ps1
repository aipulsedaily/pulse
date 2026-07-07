# Pulse - Velopack release publisher.
#
# CI-less local flow:
#   .\installer\publish.ps1                        # pack + upload to GitHub Releases
#   .\installer\publish.ps1 -Local C:\tcfeed       # pack into a local feed dir (staging proof)
#   .\installer\publish.ps1 -Local C:\tcfeed -SkipBuild   # reuse target\release binaries
#
# Facts this script encodes (do not drift):
# - packId is AIPulseDaily.Pulse - NEVER bare "Pulse": Velopack's
#   uninstaller deletes its install dir, and the bare id would co-locate with
#   the data dir (%LOCALAPPDATA%\Pulse) and wipe journals/state. The org
#   prefix also keeps us clear of the crowded "Pulse" package namespace.
# - Version single-source: Cargo.toml [package] version.
# - --noPortable: a portable build of THIS app is a foot-gun (it would still
#   prefer the bin\ daemon).
# - Icon/splash are passed only when the assets exist (they are committed;
#   the guard keeps the script working even if they're ever absent).
# - Velopack manages NO startup shortcut: autostart stays the HKCU Run key,
#   self-healed by the daemon.
#
# Prereqs: Rust toolchain; vpk (`dotnet tool install -g vpk`); for the GitHub
# path additionally a GH token (VPK_GITHUB_TOKEN / --token).

[CmdletBinding()]
param(
    # Local feed directory: skip GitHub download/upload, emit the release
    # (Setup.exe + full/delta .nupkg + releases.win.json) into this dir.
    [string]$Local,
    # GitHub repo URL for download (delta base) + upload. Defaults to the
    # repo in Cargo.toml `repository`.
    [string]$Repo = 'https://github.com/aipulsedaily/pulse',
    # Reuse existing target\release binaries instead of rebuilding.
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot   # repo root (this file lives in installer\)

# -- version from Cargo.toml (single source) -----------------------------
$cargo = Get-Content (Join-Path $root 'Cargo.toml')
$verLine = $cargo | Where-Object { $_ -match '^\s*version\s*=\s*"([^"]+)"' } | Select-Object -First 1
if (-not $verLine) { throw 'Could not read version from Cargo.toml' }
$version = [regex]::Match($verLine, '"([^"]+)"').Groups[1].Value
Write-Host "Packaging Pulse v$version"

# -- vpk present? --------------------------------------------------------
$vpk = Get-Command vpk -ErrorAction SilentlyContinue
if (-not $vpk) {
    throw "vpk not found. Install it with: dotnet tool install -g vpk"
}

# -- build ---------------------------------------------------------------
if (-not $SkipBuild) {
    Write-Host 'cargo build --release'
    Push-Location $root
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally { Pop-Location }
}

# -- stage the pack dir (exactly the two shipped binaries) ---------------
$publish = Join-Path $root 'target\publish'
if (Test-Path $publish) { Remove-Item -Recurse -Force $publish }
New-Item -ItemType Directory -Force $publish | Out-Null
foreach ($bin in 'pulse.exe', 'pulse-ctl.exe') {
    $src = Join-Path $root "target\release\$bin"
    if (-not (Test-Path $src)) { throw "missing $src - build first" }
    Copy-Item $src $publish
}

# -- delta base: pull the previous release (GitHub path only) ------------
$outDir = Join-Path $root 'target\releases'
if ($Local) {
    $outDir = $Local
    New-Item -ItemType Directory -Force $outDir | Out-Null
} else {
    New-Item -ItemType Directory -Force $outDir | Out-Null
    Write-Host "vpk download github ($Repo)"
    vpk download github --repoUrl $Repo --outputDir $outDir
    # A missing previous release is fine (first publish) - vpk reports it;
    # deltas simply won't be generated.
}

# -- pack ----------------------------------------------------------------
$packArgs = @(
    'pack',
    '--packId', 'AIPulseDaily.Pulse',
    '--packTitle', 'Pulse',
    '--packAuthors', 'AI Pulse Daily',
    '--packVersion', $version,
    '--packDir', $publish,
    '--mainExe', 'pulse.exe',
    '--outputDir', $outDir,
    '--noPortable',
    # Quiet install (update-plan Axis 2): Start-menu entry only - no Desktop
    # shortcut, and NEVER a Startup shortcut (it would launch the GUI, not
    # --daemon; autostart stays the daemon-healed HKCU Run key).
    '--shortcuts', 'StartMenuRoot'
)
$icon = Join-Path $root 'assets\icon.ico'
if (Test-Path $icon) { $packArgs += @('--icon', $icon) }
$splash = Join-Path $root 'assets\splash.png'
if (Test-Path $splash) { $packArgs += @('--splashImage', $splash) }

Write-Host "vpk $($packArgs -join ' ')"
vpk @packArgs
if ($LASTEXITCODE -ne 0) { throw "vpk pack failed ($LASTEXITCODE)" }

# -- upload (GitHub path only) -------------------------------------------
if ($Local) {
    Write-Host "Local feed ready: $outDir"
    Write-Host "Point the app at it with: TC_UPDATE_FEED=$outDir"
} else {
    Write-Host "vpk upload github ($Repo)"
    vpk upload github --repoUrl $Repo --outputDir $outDir --publish --releaseName "Pulse v$version" --tag "v$version"
    if ($LASTEXITCODE -ne 0) { throw "vpk upload failed ($LASTEXITCODE)" }
    Write-Host "Published v$version to $Repo"
}
