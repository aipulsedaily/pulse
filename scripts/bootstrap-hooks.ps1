# One-time dev setup: route git hooks to the committed .githooks directory so
# the pre-commit / commit-msg meta-lint runs on every commit. Idempotent.
#   pwsh -NoProfile -File scripts/bootstrap-hooks.ps1     (or powershell.exe)
$ErrorActionPreference = 'Stop'
& git config core.hooksPath .githooks
if ($LASTEXITCODE -ne 0) { Write-Host 'bootstrap-hooks: git config failed'; exit 1 }
Write-Host 'bootstrap-hooks: core.hooksPath -> .githooks (pre-commit + commit-msg meta-lint active)'
exit 0
