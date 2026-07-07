# Driving Pulse from scripts and agents (pulse-ctl.exe)

`%LOCALAPPDATA%\Pulse\bin\pulse-ctl.exe` (`pulse-ctl` below) controls the daemon. Output is
JSON — one object per invocation (`{"v":1,"ok":true,…}` on success,
`{"v":1,"ok":false,"code":…,"msg":…}` on refusal), JSON-lines for `watch`.

    pulse-ctl list                                  # sessions, folders, activity
    pulse-ctl run <name|id> "cargo test"            # runs at the prompt, returns
                                             # {"exit":0,"output":…} when done
    pulse-ctl run <name|id> "cargo test" --timeout 300 --no-wait
    pulse-ctl send <name|id> --text "hi" --enter    # raw input (TUIs: claude, ssh…)
    pulse-ctl send <name|id> --key ctrl+c           # interrupt
    pulse-ctl read <name|id> --screen               # what the terminal shows now
    pulse-ctl read <name|id> --tail --lines 200     # recent output, ANSI-stripped
    pulse-ctl blocks <name|id>                      # recent commands + exit codes
    pulse-ctl block-text <name|id> <start_off>      # one command's full output
    pulse-ctl wait <name|id> --for prompt           # block until the shell is idle
    pulse-ctl wait <name|id> --for output "error" --timeout 120
    pulse-ctl wait <name|id> --for block-close [--after <off>]  # a command finishes
    pulse-ctl watch [--events blocks,exit,state]    # live JSON events (default: all 3)
    pulse-ctl token list                            # scoped agent tokens (name+scope)
    pulse-ctl create --name scratch [--cwd C:\repo] # new terminal (shell|claude|custom)
    pulse-ctl kill / pulse-ctl restart / pulse-ctl delete --yes
    pulse-ctl sleep <name|id> [--force]             # kill the process tree, keep
                                             # scrollback/blocks/resume identity
    pulse-ctl sleep --folder <name> [--force]       # every running member, one pass
    pulse-ctl wake <name|id>                        # relaunch through the restore path
    pulse-ctl wake --folder <name>                  # staggered like boot restore
    pulse-ctl info                                  # daemon pid/port/proto + self id

`<name|id>` is a terminal UUID, an exact name, or an unambiguous
case-insensitive name prefix.

Exit codes let you branch without parsing: 0 ok · 1 transport/usage
(no daemon, protocol skew) · 2 refused by a gate/policy · 3 timeout ·
4 target resolution (not found / ambiguous).

`run` refuses while something is running (exit code 2, `"code":"busy"`,
`"open_cmd"` names the offender) — pass `--force` only if you really mean to
type into a running program. It also refuses hookless terminals
(`"not_hooked"`: claude tabs, cmd.exe) — drive those with `send` and read
them with `read --screen`. `read --tail` is the journal and works for dead
terminals; a TUI's journal is redraw soup, use `--screen` there.

From inside a managed terminal, `pulse-ctl` refuses to send input to (or kill) its
own terminal (`"self_target"`) unless you pass `--force-self` — an agent
typing into its own transcript is a feedback loop.

Scoped credentials for agents (optional): `pulse-ctl token create --name agents
--scope input`, then set `TC_CTL_TOKEN` in the agent's environment — it can
then read and type, but never kill/delete terminals, stop the daemon, or
mint tokens. Scoped tokens survive daemon restarts (`ctl-tokens.json`);
revoke with `pulse-ctl token revoke --name agents`. Anything on this machine
running as you can read the master token; scopes are a seatbelt, not a lock.
Note that even a read-only token discloses a lot: `list` returns every
terminal's cwd, program, open/recent command lines and CLI session identities
(including resume tokens) — treat a "read" token as sensitive, not as
harmless telemetry access.

Sleep (proto 9) is per-terminal hibernation: the process tree dies (the RAM
comes back), everything persisted — journal, blocks, state, pinned claude
session — stays exactly as a reboot leaves it, and boot restore SKIPS asleep
terminals until an explicit wake. `pulse-ctl list` reports `status`
`"asleep"` (and the sub-second `"sleeping"` drain transient; `activity`
reads `"asleep"` too) — filter on it for wake sweeps. Refusal codes:
`busy` (open block or output <3s; `--force` overrides — folder sleep
without `--force` skips busy members instead), `asleep` (already asleep,
and also what `run`/`send` answer against an asleep terminal — input never
auto-wakes; wake explicitly), `not_asleep` (wake target is running/dead),
`sleeping` (drain in flight, retry), `not_running` (spawn in flight),
`dead` (sleep needs a running terminal — a dead one wants `restart`).
`pulse-ctl restart` on an asleep terminal is equivalent to `wake`. Waiters parked
on a terminal being slept fail with `"code":"asleep"` — except
`wait --for exit`, which resolves truthfully.

Timeouts resolve on a 250ms tick (a `--timeout 30` answers within ~30.25s).
`wait --for output` matches ANSI-stripped text (add `--regex` for the
linear-time regex engine, `--from <off>` to also scan journal history and
close the register-after-output race — `run --no-wait` prints the `at_off`
to feed it). `--for block-close [--after <off>]` waits for a command to
finish (the `at_off` a `run --no-wait` prints is what you feed `--after`).

`pulse-ctl create` makes only `shell` (default), `claude`, or `custom` (a bare
program) terminals — WSL, ssh, and cmd terminals are created from the GUI
launcher, not the controller (a live capability gap, not a permanent rule).
