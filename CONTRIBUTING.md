# Contributing to Pulse

Thanks for your interest in contributing! This is a native Rust application for
Windows; the notes below get you building, testing, and matching the project's
conventions.

## Prerequisites

- Windows 10 or 11.
- A recent [Rust toolchain](https://rustup.rs) (stable, MSVC target). The
  release profile uses fat LTO, so a full `--release` build takes a couple of
  minutes.
- For icon/social-image regeneration only: [`resvg`](https://github.com/linebender/resvg)
  (`cargo install resvg`) and Python with Pillow. Not needed for a normal
  build — `assets/icon.ico` is committed, and `build.rs` no-ops gracefully if
  it is ever absent, so a build never requires the image toolchain.

## Build and run

```sh
cargo build              # debug
cargo build --release    # optimized (what ships)
```

- `pulse.exe` (no args) is the GUI. It spawns its own `--daemon`
  child if one isn't already running.
- `pulse-ctl.exe` is the controller CLI, a thin console binary built from the
  same crate. See [`docs/controller-api.md`](docs/controller-api.md).

The app is usually running while you develop, and the built exe stays **locked**
by the live GUI/daemon. Before rebuilding, stop them:

```powershell
Get-Process pulse | Stop-Process -Force
```

Relaunching is expected to restore your terminals from their journals — that's
correct behavior, not a bug.

## Testing

Two complementary layers:

```sh
cargo test --release     # unit tests (pure logic; no GUI harness)
cargo clippy --all-targets   # must be warning-clean
```

The daemon also ships a **probe suite** — an end-to-end self-test that drives a
real daemon over the wire. The probes are *clients*: they need a daemon already
running in the same data dir, so start one first.

Probes must run against an **isolated** daemon so they never touch your real
sessions. Set `TC_DATA_DIR` to a scratch directory for both the daemon and the
probe run — it redirects the entire data universe (state, journals, socket
info, logs) to that directory on its own port and token:

```powershell
$env:TC_DATA_DIR = "$env:TEMP\tc-probe"
Start-Process .\target\release\pulse.exe -ArgumentList '--daemon'
Start-Sleep -Seconds 2   # let the daemon boot and publish its socket info
.\target\release\pulse.exe --probe all | Out-Host
```

The `| Out-Host` matters: the release exe is a GUI-subsystem binary, so an
unpiped invocation returns immediately and prints nothing — piping makes
PowerShell wait, stream the results, and set `$LASTEXITCODE` (0 = all green).

When you're done, stop the sandbox daemon (it keeps running detached):

```powershell
Get-Process pulse |
  Where-Object { $_.Path -like "*\target\release\*" } | Stop-Process -Force
```

A live GUI is itself a client that resizes terminals, so kill all
`pulse` processes before probing a shared daemon. The ssh/WSL
transport cases SKIP with an explanatory message unless you opt in
(e.g. `TC_SSH_VIA_WSL`) — that's expected on a plain checkout.

> **Historical `TC_` prefix**: internal names predate the Pulse rebrand —
> env vars (`TC_DATA_DIR`, `TC_CTL_TOKEN`, `TC_UPDATE_FEED`, …), the private
> OSC 7717 `tcbeacon` protocol, remote `~/.tc/` helper paths, and various
> `tc-` identifiers in code. They are stable interfaces with zero user-facing
> surface, kept as-is deliberately; don't rename them piecemeal.

## UI doctrine

Pulse follows a deliberate design doctrine — please keep changes in
line with it:

- **Seamless, zero-divider UI.** No dividers or widget strokes; structure comes
  from spacing and fills. The composer covers the prompt row so the terminal
  reads as one surface.
- **Hover-only chrome.** Controls appear on hover; the resting state is quiet.
- **Mouse-first, honest copy.** Every action has a visible control; status text
  says what actually happens (no marketing, no lies about state).
- **The "terminal with magic" test.** A screenshot should still read as a
  terminal app, not a dashboard.

The `docs/` specs record the reasoning behind each subsystem. Read them for the
*why*; the code is the current truth.

## Contributing changes

Development happens on forks: write access to the main repo is owner-only and
`main` only takes pull requests. Fork
[aipulsedaily/pulse](https://github.com/aipulsedaily/pulse), create a topic
branch on your fork, and open a PR against `main` — direct branches or pushes
to the main repo are not accepted.

```sh
gh repo fork aipulsedaily/pulse --clone   # or fork on GitHub + git clone
cd pulse
git checkout -b my-change
# ...hack, commit...
git push -u origin my-change
gh pr create --repo aipulsedaily/pulse --base main
```

PRs run against the checklist in the PR template.

## Pull requests

- Keep changes focused and match the surrounding style.
- `cargo test --release` green and `cargo clippy --all-targets` clean.
- Describe what you changed and how you verified it.

CI runs the same gates on every PR (`.github/workflows/ci.yml`): clippy with
warnings denied, plus the full test suite, on `windows-latest`.

## Releases (maintainers)

Releases are built and published by GitHub Actions
(`.github/workflows/release.yml`) — not from a dev machine. The process:

1. Bump `version` in `Cargo.toml` (the single source of truth) and commit.
2. Tag **that commit** `vX.Y.Z` — the tag must match `Cargo.toml` exactly or
   the workflow fails fast — and push the tag.
3. Actions builds `--release`, runs the full test suite (a failing suite
   aborts the release), packs with Velopack (`vpk`), and publishes
   `Setup.exe` + full/delta `.nupkg` + the update manifest to the GitHub
   Release for that tag. Installed apps pick it up via the built-in updater.

```sh
# example: shipping 0.2.0
#   edit Cargo.toml -> version = "0.2.0"
git commit -am "release: v0.2.0"
git tag v0.2.0
git push origin main v0.2.0
gh run watch    # follow the release build
```

To rehearse the pipeline without publishing anything, run the `release`
workflow manually from the Actions tab with `dry_run` checked (the default):
it builds, tests, and packs, but uploads nothing.

`installer\publish.ps1` remains the local/emergency fallback — it is the same
pack engine the workflow runs, so it produces identical artifacts. It needs
the Rust toolchain, `vpk` (`dotnet tool install -g vpk`), and a GitHub token
in `VPK_GITHUB_TOKEN` for the upload. Prefer the Actions path: every shipped
build then comes from a clean, tested runner.

When editing the workflows, note the org policy: every action reference must
be pinned to a full-length commit SHA (with a trailing `# vX.Y.Z` comment) —
workflows that reference floating tags are refused.

By contributing, you agree that your contributions are dual-licensed under
MIT OR Apache-2.0, matching the project [license](README.md#license).
