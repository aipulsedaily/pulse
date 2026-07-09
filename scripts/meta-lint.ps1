# meta-lint.ps1 - Pulse repo meta-lint: fast hygiene greps run before every
# commit (via .githooks) and mirrored in CI so a bypassed hook still gets
# caught. No cargo build/test here - file checks and regexes only (<5s).
#
# Modes:
#   --staged              hook mode: scan lines ADDED by the staged diff
#                         (default when no mode is given)
#   --all                 CI mode: scan every tracked file in the tree
#   --commit-msg <file>   commit-msg hook: scan the commit message file
#
# Rules (each one encodes a real incident from this repo's history):
#   privacy      personal names / LAN IPs / hostname tripwires
#   encoding     UTF-8 BOM or mojibake in rs/toml/md/yml/ps1
#   brand        pre-rebrand product name in src/, docs/, README.md
#   placeholder  release placeholders + dbg!/todo!/unimplemented! in src
#   commit-msg   AI attribution lines in commit messages
#   version      (advisory only) Cargo.toml version bump tag reminder
#   large        files >5MB, or tcdata/scratchpad/*.log paths
#
# Allowlisting a legitimate hit:
#   - inline: put  lint-allow: <rule>  (or  lint-allow: all) on the line
#   - file:   add "<rule> <path-prefix>" to scripts/meta-lint-allowlist.txt
#
# This file is deliberately pure ASCII (PowerShell 5.1 misreads BOM-less
# non-ASCII .ps1 as ANSI, and our own encoding rule forbids a BOM), so all
# non-ASCII pattern characters are built from [char] codepoints below.

Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch { }

# ---- arguments (no param block: '--staged' style flags must survive -File) --
$mode = '--staged'
$msgFile = $null
if ($args.Count -ge 1) { $mode = [string]$args[0] }
if ($args.Count -ge 2) { $msgFile = [string]$args[1] }
$mode = $mode.TrimStart('-').ToLowerInvariant()
if ($mode -notin @('staged', 'all', 'commit-msg', 'commitmsg')) {
    Write-Host "meta-lint: unknown mode '$mode' (use --staged | --all | --commit-msg <file>)"
    exit 2
}
if ($mode -eq 'commitmsg') { $mode = 'commit-msg' }

$repoRoot = (& git rev-parse --show-toplevel 2>$null | Select-Object -First 1)
if (-not $repoRoot) { Write-Host 'meta-lint: not inside a git repository'; exit 2 }
Set-Location -LiteralPath $repoRoot

# ---- findings ---------------------------------------------------------------
$findings = New-Object System.Collections.ArrayList
function Add-Finding([string]$rule, [string]$file, [int]$line, [string]$excerpt) {
    $t = $excerpt.Trim()
    if ($t.Length -gt 110) { $t = $t.Substring(0, 110) + '...' }
    [void]$findings.Add(('[{0,-11}] {1}:{2}: {3}' -f $rule, $file, $line, $t))
}

# ---- allowlist --------------------------------------------------------------
$allow = @()
$allowFile = Join-Path $repoRoot 'scripts/meta-lint-allowlist.txt'
if (Test-Path -LiteralPath $allowFile) {
    foreach ($raw in Get-Content -LiteralPath $allowFile) {
        $l = ($raw -split '#', 2)[0].Trim()
        if (-not $l) { continue }
        $parts = $l -split '\s+', 2
        if ($parts.Count -eq 2) {
            $allow += , @($parts[0].ToLowerInvariant(), $parts[1].Trim().Replace('\', '/').ToLowerInvariant())
        }
    }
}
function Test-Allowed([string]$rule, [string]$path) {
    $p = $path.Replace('\', '/').ToLowerInvariant()
    foreach ($e in $allow) {
        if ($e[0] -eq $rule -and $p.StartsWith($e[1])) { return $true }
    }
    return $false
}

# ---- rule patterns ----------------------------------------------------------
# PRIVACY: the tripwire strings are stored base64-encoded so this script does
# not itself re-introduce the values the anonymization sweep removed (the
# original incident was exactly that: a source comment re-leaked them).
# Decoded, they are: the old dev username, the two 192.168.50.* LAN addresses
# (one regex covers both), an old project codename, an alec<digit> handle /
# personal-email surname, and the old machine hostname.
$privacyB64 = @(
    'emFueQ==',
    'MTkyXC4xNjhcLjUwXC4xNA==',
    'ZXFmb3Jt',
    'YWxlY1swLTld',
    'a29pZm1hbg==',
    'REVTS1RPUC00TUlKNTdQ'
)
$privacyPat = ($privacyB64 | ForEach-Object {
        [Text.Encoding]::ASCII.GetString([Convert]::FromBase64String($_))
    }) -join '|'
$privacyRe = New-Object regex($privacyPat, 'IgnoreCase')

# BRAND: pre-rebrand names. Case-sensitive on purpose: lowercase 'terminal
# control' in prose is ordinary English, the three exact forms are not.
$brandRe = New-Object regex('Terminal Control|TerminalControl|terminal-control')
$brandScopeRe = New-Object regex('^(src/|docs/|README\.md$)')

# PLACEHOLDER: tokens are assembled by concatenation so this script never
# matches itself when CI scans the whole tree.
$phTokens = @(
    ('TODO-set-' + 'releases'),
    ('AUTHOR' + '_NAME'),
    ('OWNER' + '/REPO'),
    ('PLACEHOLDER' + '(#')
)
$phTextRe = New-Object regex((($phTokens | ForEach-Object { [regex]::Escape($_) }) -join '|'))
$phRustRe = New-Object regex(('\b(' + 'dbg' + '|' + 'todo' + '|' + 'unimplemented' + ')!\('))
$rustScopeRe = New-Object regex('^src/.*\.rs$')

# ENCODING: BOM (as U+FEFF once decoded) + classic mojibake markers, built
# from codepoints to keep this file ASCII-only:
#   0x00E2 0x20AC = 'a-circumflex, euro'   (the "smart quote" wreck)
#   0x00C3 [0x00A0-0x00BF] = 'A-tilde + latin-1 suffix' (the e-acute class)
$mojiA = [string][char]0x00E2 + [string][char]0x20AC
$mojiB = [string][char]0x00C3 + '[' + [string][char]0x00A0 + '-' + [string][char]0x00BF + ']'
$mojiRe = New-Object regex(([regex]::Escape($mojiA) + '|' + $mojiB))
$bomChar = [string][char]0xFEFF
$encExtRe = New-Object regex('\.(rs|toml|md|yml|yaml|ps1)$')

# LARGE / GENERATED PATHS
$maxBytes = 5MB
$blockPathRe = New-Object regex('(^|/)(tcdata|scratchpad)(/|$)|\.log$', 'IgnoreCase')

# Cheap prefilter union used by --all so most lines skip the per-rule work.
$anyRe = New-Object regex(($privacyPat + '|Terminal Control|TerminalControl|terminal-control|' +
        $phTextRe.ToString() + '|' + $phRustRe.ToString() + '|' + $mojiRe.ToString() + '|' + $bomChar), 'IgnoreCase')

# ---- per-line rule application (shared by --staged and --all) ---------------
function Test-Line([string]$file, [int]$lineNo, [string]$text) {
    $tag = ''
    if ($text -match 'lint-allow:\s*([a-z-]+)') { $tag = $Matches[1].ToLowerInvariant() }
    if ($tag -eq 'all') { return }

    if ($tag -ne 'privacy' -and -not (Test-Allowed 'privacy' $file) -and $privacyRe.IsMatch($text)) {
        Add-Finding 'privacy' $file $lineNo $text
    }
    if ($file -cmatch $encExtRe -and $tag -ne 'encoding' -and -not (Test-Allowed 'encoding' $file)) {
        if ($text.Contains($bomChar) -or $mojiRe.IsMatch($text)) {
            Add-Finding 'encoding' $file $lineNo 'UTF-8 BOM or mojibake sequence on this line'
        }
    }
    if ($brandScopeRe.IsMatch($file) -and $tag -ne 'brand' -and -not (Test-Allowed 'brand' $file) -and $brandRe.IsMatch($text)) {
        Add-Finding 'brand' $file $lineNo $text
    }
    if ($tag -ne 'placeholder' -and -not (Test-Allowed 'placeholder' $file)) {
        if ($phTextRe.IsMatch($text)) {
            Add-Finding 'placeholder' $file $lineNo $text
        }
        if ($rustScopeRe.IsMatch($file) -and $phRustRe.IsMatch($text)) {
            Add-Finding 'placeholder' $file $lineNo $text
        }
    }
}

function Test-PathRules([string]$file, [long]$size) {
    if (Test-Allowed 'large' $file) { return }
    if ($size -gt $maxBytes) {
        Add-Finding 'large' $file 0 ("file is {0:n1} MB (limit 5 MB)" -f ($size / 1MB))
    }
    if ($blockPathRe.IsMatch($file)) {
        Add-Finding 'large' $file 0 'tcdata/, scratchpad/ and *.log paths must not be committed'
    }
}

# =============================== commit-msg ==================================
if ($mode -eq 'commit-msg') {
    if (-not $msgFile -or -not (Test-Path -LiteralPath $msgFile)) {
        Write-Host "meta-lint: commit-msg mode needs the message file path"; exit 2
    }
    $robot = [string][char]0xD83E + [string][char]0xDD16   # robot emoji, U+1F916
    $lineNo = 0
    foreach ($l in [IO.File]::ReadAllLines($msgFile, (New-Object Text.UTF8Encoding($false)))) {
        $lineNo++
        if ($l -match '^\s*#') { continue }   # comment lines are not part of the message
        if ($l -match '(?i)co-authored-by') { Add-Finding 'commit-msg' 'COMMIT_MSG' $lineNo $l }
        if ($l -match '(?i)generated with') { Add-Finding 'commit-msg' 'COMMIT_MSG' $lineNo $l }
        if ($l.Contains($robot)) { Add-Finding 'commit-msg' 'COMMIT_MSG' $lineNo $l }
    }
    if ($findings.Count -gt 0) {
        Write-Host 'meta-lint: commit message REJECTED (attribution lines are not allowed here):'
        $findings | ForEach-Object { Write-Host "  $_" }
        exit 1
    }
    exit 0
}

# =============================== staged mode =================================
if ($mode -eq 'staged') {
    # Content rules run on the lines the commit ADDS (pre-existing allowlisted
    # content in a touched file does not block unrelated edits).
    $diff = & git -c core.quotepath=false diff --cached -U0 --diff-filter=ACMR
    $cur = $null
    $ln = 0
    foreach ($line in @($diff)) {
        if ($null -eq $line) { continue }
        if ($line.StartsWith('+++ ')) {
            $cur = $null
            if ($line -match '^\+\+\+ b/(.*)$') { $cur = $Matches[1] }
            continue
        }
        if ($line -match '^@@ [^+]*\+(\d+)') { $ln = [int]$Matches[1]; continue }
        if ($null -ne $cur -and $line.Length -ge 1 -and $line[0] -eq '+') {
            Test-Line $cur $ln $line.Substring(1)
            $ln++
        }
    }

    $staged = @(& git -c core.quotepath=false diff --cached --name-only --diff-filter=ACMR)
    foreach ($f in $staged) {
        if (-not $f) { continue }
        $size = 0
        $s = & git cat-file -s (':' + $f) 2>$null
        if ($s) { $size = [long]($s | Select-Object -First 1) }
        Test-PathRules $f $size

        # BOM check on the file as it will be committed. The worktree copy is
        # used as a byte-level proxy for the staged blob (identical in every
        # normal flow; the U+FEFF diff-line check above covers partial stages).
        if ($f -cmatch $encExtRe -and (Test-Path -LiteralPath $f) -and -not (Test-Allowed 'encoding' $f)) {
            $fs = [IO.File]::OpenRead((Join-Path $repoRoot $f))
            try {
                $head = New-Object byte[] 3
                $n = $fs.Read($head, 0, 3)
            } finally { $fs.Close() }
            if ($n -eq 3 -and $head[0] -eq 0xEF -and $head[1] -eq 0xBB -and $head[2] -eq 0xBF) {
                Add-Finding 'encoding' $f 1 'file starts with a UTF-8 BOM'
            }
        }
    }

    # Rule 6 (advisory, never blocks): version bump means the release tag must
    # match - the release workflow hard-asserts tag == Cargo.toml version.
    if ($staged -contains 'Cargo.toml') {
        $vdiff = @(@(& git diff --cached -U0 -- Cargo.toml) | Where-Object { $_ -match '^\+version\s*=\s*"' })
        if ($vdiff.Count -gt 0) {
            Write-Host 'meta-lint: NOTE Cargo.toml version changed in this commit - remember the release tag'
            Write-Host ("            must match: {0}" -f ($vdiff[0].TrimStart('+').Trim()))
        }
    }
}

# ================================= all mode ==================================
if ($mode -eq 'all') {
    $utf8 = New-Object Text.UTF8Encoding($false)
    foreach ($f in @(& git -c core.quotepath=false ls-files)) {
        if (-not $f) { continue }
        $full = Join-Path $repoRoot $f
        if (-not (Test-Path -LiteralPath $full)) { continue }
        $fi = Get-Item -LiteralPath $full
        Test-PathRules $f $fi.Length

        # binary sniff: NUL byte in the first 512 bytes means skip content rules
        $fs = [IO.File]::OpenRead($full)
        try {
            $head = New-Object byte[] 512
            $n = $fs.Read($head, 0, 512)
        } finally { $fs.Close() }
        $isBinary = $false
        for ($i = 0; $i -lt $n; $i++) { if ($head[$i] -eq 0) { $isBinary = $true; break } }
        if ($isBinary) { continue }

        if ($f -cmatch $encExtRe -and -not (Test-Allowed 'encoding' $f) -and
            $n -ge 3 -and $head[0] -eq 0xEF -and $head[1] -eq 0xBB -and $head[2] -eq 0xBF) {
            Add-Finding 'encoding' $f 1 'file starts with a UTF-8 BOM'
        }

        $text = [IO.File]::ReadAllText($full, $utf8)
        if (-not $anyRe.IsMatch($text)) { continue }   # fast path: most files
        $lines = $text -split "`n"
        for ($i = 0; $i -lt $lines.Count; $i++) {
            $l = $lines[$i].TrimEnd("`r")
            if ($anyRe.IsMatch($l)) { Test-Line $f ($i + 1) $l }
        }
    }
}

# ---- verdict ----------------------------------------------------------------
# De-duplicate (a line can be reached twice via overlapping prefilter matches).
$unique = @($findings | Select-Object -Unique)
if ($unique.Count -gt 0) {
    Write-Host ("meta-lint: FAIL - {0} finding(s) [mode: {1}]" -f $unique.Count, $mode)
    $unique | ForEach-Object { Write-Host "  $_" }
    Write-Host ''
    Write-Host 'Legitimate hit? Add "lint-allow: <rule>" on that line, or a justified entry'
    Write-Host 'in scripts/meta-lint-allowlist.txt. Bypassing with --no-verify only defers'
    Write-Host 'the failure: CI runs this same script with --all.'
    exit 1
}
Write-Host ("meta-lint: OK [mode: {0}]" -f $mode)
exit 0
