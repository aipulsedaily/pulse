# Pulse - icon asset generator.
#
# Renders the app mark to a multi-resolution icon.ico, plus a raw RGBA blob
# for the eframe window icon. Repeatable: re-run after editing icon.svg;
# commit the generated icon.ico + window-icon-48.rgba so `cargo build`
# never needs this toolchain.
#
# EVERY frame (16..256) comes from the FULL mark (icon.svg). A simplified
# small-size variant (icon-16.svg) used to feed the 16/24 frames, but that
# made the taskbar icon visibly different between a 4K monitor (picks 32px+)
# and a 1080p/100%-scaling monitor (picks 16/24px). Verdict 2026-07-10: the
# full mark is legible at 16px and consistency across DPI wins — do NOT
# reintroduce a divergent small mark.
#
# Toolchain (verified on this machine): resvg (SVG->PNG, cargo-installed) +
# Python Pillow (PNG->ICO packing, raw RGBA). No ImageMagick dependency -
# `convert.exe` in system32 is the Windows disk tool, not ImageMagick.

[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot

$resvg = (Get-Command resvg -ErrorAction SilentlyContinue)
if (-not $resvg) { throw "resvg not found. Install with: cargo install resvg" }
$py = (Get-Command python -ErrorAction SilentlyContinue)
if (-not $py) { throw "python (with Pillow) not found" }

$full = Join-Path $here 'icon.svg'
$tmp = Join-Path $here '_iconbuild'
New-Item -ItemType Directory -Force $tmp | Out-Null

$sizes = @(16, 24, 32, 48, 64, 128, 256)
foreach ($s in $sizes) {
    $out = Join-Path $tmp "icon-$s.png"
    & resvg -w $s -h $s $full $out | Out-Null
    if (-not (Test-Path $out)) { throw "resvg failed for size $s" }
}
# 48px window icon (eframe IconData needs raw RGBA of a single size).
& resvg -w 48 -h 48 $full (Join-Path $tmp 'window-48.png') | Out-Null

$icoOut = Join-Path $here 'icon.ico'
$rgbaOut = Join-Path $here 'window-icon-48.rgba'
$pyScript = Join-Path $tmp 'pack.py'
@"
from PIL import Image
import os
tmp = r'$tmp'
sizes = [16, 24, 32, 48, 64, 128, 256]
imgs = [Image.open(os.path.join(tmp, f'icon-{s}.png')).convert('RGBA') for s in sizes]
# Pillow packs every provided size into the .ico; pass the largest and the
# explicit size list so all frames are embedded.
imgs[-1].save(r'$icoOut', format='ICO', sizes=[(s, s) for s in sizes],
              append_images=imgs[:-1])
# raw RGBA (row-major, 4 bytes/px) for eframe IconData.
w = Image.open(os.path.join(tmp, 'window-48.png')).convert('RGBA')
with open(r'$rgbaOut', 'wb') as fh:
    fh.write(w.tobytes())
print('wrote', r'$icoOut', 'and', r'$rgbaOut')
"@ | Set-Content -Path $pyScript -Encoding utf8
& python $pyScript
if ($LASTEXITCODE -ne 0) { throw "python packing failed ($LASTEXITCODE)" }

Remove-Item -Recurse -Force $tmp

# Installer splash (static PNG, 600x340) from splash.svg - same toolchain.
$splashSvg = Join-Path $here 'splash.svg'
if (Test-Path $splashSvg) {
    & resvg -w 600 -h 340 $splashSvg (Join-Path $here 'splash.png') | Out-Null
    Write-Host "Generated: assets\splash.png (600x340)"
}

Write-Host "Generated: assets\icon.ico (16-256) + assets\window-icon-48.rgba"
