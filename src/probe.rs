//! Hidden `--probe [case]` subcommand: exercises the daemon end-to-end from the
//! CLI. Default runs every case; a nonzero exit signals any failure.
//!
//! Cases:
//!   basic         create / echo / journal replay across a reconnect
//!   restore       kill + RestartTerminal appends exactly one restore marker
//!   remnant       the restore seam is contiguous in the serialized replay:
//!                 old output / marker / new prompt, ≤2 blank lines between
//!   folders       folder create/rename/move; deleting a folder reparents terminals
//!   backpressure  a wedged client cannot starve a live one
//!   resize_owner  only the client that sent Resize sets the daemon's grid
//!   peb           the PEB CurrentDirectory (0x38) offset self-check passes
//!   tracker       a shell's cd is captured into live_cwd by the tracker
//!   resize_stress adversarial Resize storms leave Term/PTY/state in agreement
//!   resize_race   restart×resize and kill×resize races always converge
//!   journal_reap  a delete mid-output-storm cannot resurrect the journal
//!   replay_cap    a journal past the 2MB replay cap reopens coherently
//!   keys          keyboard fidelity through a real PSReadLine: win32-input
//!                 Ctrl+Backspace word-deletes, win32 Ctrl+C interrupts
//!   latency       keystroke-echo round trip through a real PSReadLine:
//!                 paced p95 bound + no backlog after sustained typing
//!   flood         ~50MB output flood: pipeline survives, stays ordered, and
//!                 prints wall/CPU/throughput numbers for perf comparison
//!   blocks_roundtrip  shell hooks → daemon block records through a real
//!                 PSReadLine: cmd text, exit codes (0 and 3), cwd, offsets
//!   blocks_restore    blocks survive kill+restore: epoch bumps, old records
//!                 intact, dangling block closed exit=None, offsets monotonic
//!   blocks_antispoof  a forged hook OSC (wrong token) is rejected + logged,
//!                 and the scanner doesn't desync
//!   blocks_compact_evict  journal compaction evicts pre-base records, flags
//!                 straddlers truncated, persists the new base in the sidecar
//!   blocks_stream_pos  D2C::StreamPos ordering (Replay → StreamPos → Blocks
//!                 → Output, on attach AND restore-resync) and the P2 money
//!                 assertion: GUI-side offset math reproduces daemon record
//!                 keys bit-for-bit
//!   blocks_text   C2D::BlockText round trip: clean stripped output text,
//!                 excludes echo + prompt, partial text for open blocks
//!   blocks_rerun_gate  the Re-run gate's record leg through a real shell:
//!                 true at idle, injected re-run re-captured, false while a
//!                 block is open, true again after win32 Ctrl+C
//!   blocks_hookless_silent  a hookless terminal (cmd.exe) produces no block
//!                 frames, no records, and no sidecar — the zero-cost gate
//!   composer_submit  P3 §10.1: the clear chord (win32 Ctrl+C) cancels stray
//!                 prompt text and the submission that shares its Input frame
//!                 is recorded byte-exact; an empty submit (bare \r) renders
//!                 a fresh prompt and stays block-silent
//!   composer_multiline  P3 §10.2: PS 5.1 paste semantics — one Input frame
//!                 with two \r-separated lines yields two sequential blocks
//!   composer_gate_replay  P3 §10.3: the composer state machine + gate
//!                 replayed over REAL session bytes (chunked at 7); exec
//!                 disarms before the app draws; every pre is followed by a
//!                 PromptEnd that lands AFTER the prompt text in the stream
//!   ctl_scope     P5: scoped-token minting/enforcement, legacy-frame drop
//!                 for scoped controllers, and the recursion guard
//!   ctl_run_wait  P5: the run→RunDone composite, Prompt/OutputMatch waits
//!                 (live + from_off history), timeout sweep, multiline refusal
//!   ctl_busy_gate P5: busy refusal against a real open block, chord
//!                 interrupt, hookless not_hooked + ungated SendRaw
//!   restore_fidelity  daemon shutdown while a real `ls` is mid-render →
//!                 restart → restore replays the last command's output
//!                 COMPLETELY (on-disk journal AND serialized replay; locks
//!                 the shutdown output drain)
//!   width_mismatch_replay  the 2026-07-09 restored-claude garble: alt
//!                 content recorded at 175×49 → daemon restart → proto-12
//!                 attach at 147×49 is readable (no fused rows), the PTY is
//!                 resized to the attacher BEFORE serialization, the
//!                 restore-resync push is suppressed for proto-12 clients
//!                 (Reset only; they re-attach) and kept for legacy ones
//!   attach_alt_flood  pw1 attach lock-split coherence: attach a proto-12
//!                 client to a live alt-screen TUI MID-FLOOD (the serialize
//!                 runs outside the journal lock; hold 2 appends the raw
//!                 ingest delta + StreamPos atomically) and pin the client
//!                 reconstruction == daemon-mirror ReadScreen, every row +
//!                 cursor + alt flag — a split gap/dup diverges them
//!   compact_crash journal compaction is crash-atomic: flood past MAX_LEN,
//!                 TerminateProcess the daemon right after the compaction,
//!                 assert journal present (no remove+rename window), no .tmp
//!                 orphan, marker on disk + in the restart replay, sidecar
//!                 base intact
//!   boot_cover    the REAL boot sequence with the actual TermBackend:
//!                 restore → attach → PromptState seed → corrective resize →
//!                 conhost repaint; the composer cover gate survives and no
//!                 scrollback rows are destroyed (locks conhost-style grow)
//!   ctl_read      P5: ReadTail/ReadScreen/ReadBlocks, BlockText shared-helper
//!                 equivalence, and the Subscribe event stream
//!   reclaim_extract  P4: typeahead-reclaim extraction against real
//!                 PSReadLine echo through the real TermBackend path —
//!                 simple / chunk-invariant / wrapped / multi-line-refused /
//!                 clean-empty
//!   history_cross_session  P4: the cross-session history corpus arrives at
//!                 a fresh connection from attach Blocks syncs alone (two
//!                 terminals + a restore epoch bump), then drives the real
//!                 gui::history index: dedupe, ×N, recency, AND-filter
//!   wsl_hooks     P6a: a WslShell terminal comes up hooked end-to-end —
//!                 token-checked init (shell/home fields), block round-trip
//!                 with a real bash exit code, POSIX cwd in the record,
//!                 PromptState at_prompt+clean (SKIPs without a WSL distro)
//!   wsl_composer_semantics  P6a: bracketed-paste advertised, Ctrl+C
//!                 re-latches a clean prompt (D15), multi-line paste yields
//!                 ONE block carrying both lines (SKIPs without WSL)
//!   wsl_restore   P6a: cd /tmp is hook-tracked into live_cwd verbatim,
//!                 survives a graceful daemon restart, respawns via
//!                 `wsl --cd /tmp`, and the seam rules hold (SKIPs without WSL)
//!   cmd_hooks     P6b: PROMPT-env hooks live (pre/9;9/133;B, NO exec ever),
//!                 SubmitCommand ledger write:true round-trip (exit None,
//!                 duration+cwd real) + write:false (journal head unmoved),
//!                 multi-line refusal, D14 run-gate refusal under `ping -t`
//!   cmd_restore   P6b: kill+restore a cd'd cmd — respawn cwd from the
//!                 9;9-tracked live_cwd, PROMPT re-injected (hooks live in
//!                 the new epoch), blocks sidecar continuity across the bump
//!   ssh_bootstrap_local  P6c: the one-shot remote bootstrap end-to-end —
//!                 WSL-transport stand-in (TC_SSH_VIA_WSL=<host> on
//!                 daemon+probe) runs the exact sh -c body through a real
//!                 ConPTY: hooked prompt, block round-trip (exit 0, posix
//!                 cwd), rc self-delete; a localhost sshd adds the full
//!                 `ssh 127.0.0.1` variant (SKIPs without either)
//!   ssh_cli_resume  remote CLI resume (task #27, spec §9.2): bare `claude`
//!                 over the ssh stand-in → M0 sidecar → link death →
//!                 auto-reconnect correlates over a FRESH sftp connection ⇒
//!                 `claude --resume <uuid>` block + Explicit inner_cli;
//!                 /clear rotation ⇒ R-NEWEST; the NAMED ACCEPTANCE
//!                 SCENARIO: two terminals, same remote cwd, staggered
//!                 starts, both blocks open at daemon shutdown ⇒ second
//!                 Correlated+resumed, first honestly Ambiguous with both
//!                 candidates prefaced newest-first; simultaneous variant ⇒
//!                 BOTH Ambiguous (needs TC_SSH_VIA_WSL +
//!                 TC_SSH_PROBE_TRANSPORT + the §9.3 staging; SKIPs without)
//!   ssh_cli_resume_fallback  §5.2 no-sidecar fallback: exactly-one store
//!                 entry correlates; a second entry ⇒ definitive Ambiguous
//!                 (cleared inner_cli, candidates prefaced, NO re-probe on
//!                 the next restore)
//!   ssh_cli_authdead  §4.6: auth-refusing probe transport ⇒ M0 fails fast,
//!                 probes skip (cache), inner_cli kept; the respawn's hooks
//!                 clear the cache; stub off ⇒ next restore correlates
//!   history_parity  proto 7 restored-history anchors: styled commands +
//!                 an empty-Enter spacer → fresh attach delivers
//!                 ReplayAnchors LAST in the sequence, EVERY closed record
//!                 has a hint whose replay row renders the command at the
//!                 hinted column (§8.1 bit-exact bar), the spacer maps to a
//!                 bare-prompt row; repeated across a daemon restart
//!                 (sidecar path, where the dangling-prompt dedupe is also
//!                 pinned) and for a cmd-family terminal (exit:None,
//!                 synthetic SubmitCommand records)
//!   history_parity_wsl  the same bar for a WSL bash terminal: posix cwd on
//!                 the record, hint row renders the bash prompt + command
//!                 (SKIPs without a WSL distro)
//!   sleep_roundtrip  SLEEP P-S1 (acceptance): idle sleep is gate-free; the
//!                 process tree is GONE (Toolhelp — the RAM-reclaim proxy);
//!                 journal+sidecar byte-identical; state.json flags asleep;
//!                 daemon restart boot-skips it while a sibling restores;
//!                 Wake → hooked prompt with block history + ReplayAnchors
//!                 hints for the pre-sleep command
//!   sleep_busy_gate  SLEEP P-S2: open block ⇒ busy refusal naming the cmd;
//!                 --force sleeps; dangling block closes exit=None; Wake on
//!                 a running terminal ⇒ not_asleep; Run/SendRaw on the
//!                 asleep one ⇒ "asleep" (input never wakes)
//!   sleep_waiters_folder  SLEEP P-S3: folder sleep fails BlockClose waiters
//!                 "asleep" while Exit waiters resolve Exited; both members
//!                 sleep off ONE drain window (wall-clock bound); WakeFolder
//!                 staggers both prompts back; a dead non-asleep member is
//!                 untouched both ways
//!   sleep_freeze_frame  SLEEP P-S4 (§17): a quiet alt-screen TUI with an
//!                 OPEN block sleeps no-force (the tc-run-claude gate fix);
//!                 the pre-kill frame lands in journals/<id>.frame (crc,
//!                 alt-flagged, row text intact); a fresh attach replays
//!                 underlay THEN ?1049h + frame with hints skipped; wake
//!                 removes the sidecar and restores a hooked prompt
//!   frame_corrupt_degrade  SLEEP P-S5 (§17): a primary sleep writes NO
//!                 frame; a planted corrupt/truncated sidecar degrades an
//!                 attach to the plain reconstruction (no ?1049h), is
//!                 removed on first read, and never blocks the wake
//!   banner        (#31 + respawn-banner fix) the version-faithful PS startup
//!                 banner reproduces on the FIRST-EVER spawn only; respawns
//!                 replay the journaled copy bannerless, so a restored
//!                 terminal shows exactly one banner, at the top
//!   cold_attach   the daemon-certified at-prompt PromptState seed lands so
//!                 the composer arms with the cover on at app open
//!   ssh_reconnect (proto 10) an unexpected hooked-ssh death schedules the
//!                 backoff ladder and a fresh hooked prompt clears it
//!   claude_beacon Attribution Layer 3 end-to-end over the staged transport:
//!                 startup + /clear-switch beacons ⇒ Explicit, probe-free
//!                 wake-resume, anti-spoof drop, remote installer idempotence
//!                 (needs the full ssh_cli_env staging; SKIPs without)
//!   ssh_nested_claude  F1 nested-shell breadcrumb (nested-resume spec §7):
//!                 `sudo su` opens the chain; a beacon-less nested claude
//!                 stays unattributed and sleep/wake restores shell-only
//!                 with the variant-C re-establish preface (never a resume);
//!                 a v2-beacon root claude mints the nested:true identity +
//!                 witnessed cli_cwd, a daemon restart boot-restores with
//!                 the variant-A preface (`cd '/'; claude --resume <sid>`),
//!                 no resume block, identity retired only by the restored
//!                 prompt's own pre (needs ssh_cli_env staging +
//!                 passwordless sudo; SKIPs without)
//!   codex_beacon  the codex mirror of claude_beacon's WSL beacon lane +
//!                 anti-spoof (SKIPs without a WSL distro)
//!   cwd_broadcast a `cd` folds into live_cwd and broadcasts one Snapshot so
//!                 the GUI lane label updates the frame the prompt renders
//!   paste_stuck_child  r3 lock-discipline pin: a 1MB paste into a terminal
//!                 whose frozen app stopped reading stdin wedges only ITS
//!                 connection — a second connection's echo stays fast (no
//!                 fleet-wide seizure); thaws the conhost before judging
//!   launcher_claude_cwd  Shiro report #1: the launcher's claude-row
//!                 composition (`gui::launcher::claude_dir_spec`) spawns a
//!                 Claude-kind terminal IN the chosen directory — proven at
//!                 the PTY level by a staged fake claude printing its
//!                 process cwd + argv (needs TC_PROBE_FAKE_CLAUDE=1 on a rig
//!                 with a fake claude.exe FIRST on the daemon's PATH; SKIPs
//!                 without — a real claude would burn tokens and write into
//!                 the live ~/.claude store)
//!
//! Convention: a new case gets a header line here IN THE SAME PATCH (the
//! r2 header rewrite decayed within one round without it).
//!
//! Hidden verbs (matched BEFORE the sweep, never part of `--probe all`):
//!   `blocks_demo_create` / `blocks_demo_run` / `composer_demo_arm` are
//!   screenshot rigs whose demo terminal must survive between invocations;
//!   `shutdown` gracefully stops the data dir's daemon through the real
//!   drain path; `perf_attach` / `perf_idle` print report-only,
//!   machine-dependent perf numbers; `sweep` deletes leftover `__probe_*`
//!   terminals. The daemon-killing cases (`compact_crash`, `restore_fidelity`,
//!   `history_parity`, `wsl_restore`, `sleep_roundtrip`) refuse to run against
//!   a non-overridden data dir unless `TC_PROBE_LIVE=1` (never hard-kill the
//!   user's live daemon — r2 policy guard).

use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::protocol::{
    read_frame, write_frame, AnchorHint, C2D, CtlBody, CtlChord, CtlEvent, CtlRequest, D2C,
    DaemonInfo, DebugTermInfo, RunWait, WaitCond, WaitHit, ANCHOR_BLOCK, ANCHOR_SPACER,
    EV_BLOCKS, EV_EXIT, SCOPE_READ,
};
use crate::state::{
    daemon_info_path, state_path, BlockRec, CliConfidence, NewTerminal, SharedState, TermKind,
    TermStatus,
};
use uuid::Uuid;

/// A client connection with a read timeout, already past the Hello handshake.
struct Conn {
    stream: TcpStream,
    write: TcpStream,
}

impl Conn {
    fn open() -> anyhow::Result<Self> {
        let info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
        let stream = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], info.port)),
            Duration::from_secs(1),
        )?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut write = stream.try_clone()?;
        write_frame(&mut write, &C2D::Hello { token: info.token })?;
        Ok(Self { stream, write })
    }

    /// Proto-12 handshake (`Hello2`): the client contract where the daemon
    /// suppresses its restore-resync Replay push and the client re-attaches
    /// itself on `D2C::Reset` (the width-mismatch garble fix). Cases probing
    /// that contract connect this way; plain `open()` stays legacy so every
    /// existing case keeps exercising the pre-12 compat push.
    fn open_v2() -> anyhow::Result<Self> {
        let info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
        anyhow::ensure!(
            info.proto >= 12,
            "daemon predates Hello2 (proto {})",
            info.proto
        );
        let stream = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], info.port)),
            Duration::from_secs(1),
        )?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut write = stream.try_clone()?;
        write_frame(
            &mut write,
            &C2D::Hello2 { token: info.token, proto: crate::protocol::PROTO },
        )?;
        Ok(Self { stream, write })
    }

    /// P5 controller handshake: HelloCtl with an arbitrary token (master or
    /// scoped) and an optional self_session (the recursion-guard identity).
    fn open_ctl(token: &str, self_session: Option<Uuid>) -> anyhow::Result<Self> {
        let info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
        let stream = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], info.port)),
            Duration::from_secs(1),
        )?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut write = stream.try_clone()?;
        write_frame(
            &mut write,
            &C2D::HelloCtl {
                token: token.to_string(),
                self_session,
            },
        )?;
        Ok(Self { stream, write })
    }

    /// One controller request → its reply body (frames for other req ids and
    /// broadcast Snapshots are skipped), bounded by `secs`.
    fn ctl(&mut self, req_id: u64, req: CtlRequest, secs: u64) -> anyhow::Result<CtlBody> {
        self.send(&C2D::Ctl { req_id, req })?;
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(D2C::Ctl { req_id: rid, body }) = self.recv() {
                if rid == req_id {
                    return Ok(body);
                }
            }
        }
        anyhow::bail!("no Ctl reply for req {req_id} within {secs}s")
    }

    fn send(&mut self, msg: &C2D) -> anyhow::Result<()> {
        write_frame(&mut self.write, msg)
    }

    fn recv(&mut self) -> anyhow::Result<D2C> {
        read_frame::<_, D2C>(&mut self.stream)
    }

    /// Read frames until the first Snapshot (the daemon sends one after Hello).
    fn first_snapshot(&mut self) -> anyhow::Result<SharedState> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let D2C::Snapshot { state } = self.recv()? {
                return Ok(state);
            }
        }
        anyhow::bail!("no snapshot received");
    }

    /// Wait for a snapshot where `pred` holds, returning that state.
    fn snapshot_until(
        &mut self,
        secs: u64,
        pred: impl Fn(&SharedState) -> bool,
    ) -> anyhow::Result<SharedState> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            match self.recv() {
                Ok(D2C::Snapshot { state }) if pred(&state) => return Ok(state),
                Ok(_) => {}
                Err(_) => {}
            }
        }
        anyhow::bail!("condition not met within {secs}s");
    }

    /// Collect Replay/Output bytes for `id` until a decoded line satisfies
    /// `pred`. Decoding is a STREAMING strip (escape state carried across
    /// frames): re-stripping a fixed-size tail window used to start
    /// mid-sequence — harmless historically, but a window opening inside a
    /// block-hook OSC body leaves hex junk + a stray BEL glued onto the
    /// front of the prompt line, defeating starts_with predicates. Complete
    /// lines are tested once and dropped, so a multi-MB stream stays O(n).
    fn await_output(
        &mut self,
        id: Uuid,
        secs: u64,
        pred: impl Fn(&str) -> bool,
    ) -> anyhow::Result<Vec<u8>> {
        let mut collected: Vec<u8> = Vec::new();
        let mut stripper = AnsiStripper::default();
        let mut pending = String::new(); // stripped tail not yet line-complete
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            match self.recv() {
                Ok(D2C::Replay { id: rid, bytes }) | Ok(D2C::Output { id: rid, bytes })
                    if rid == id =>
                {
                    collected.extend_from_slice(&bytes);
                    stripper.feed(&bytes, &mut pending);
                    if pending.lines().any(&pred) {
                        return Ok(collected);
                    }
                    // Complete lines can't change; keep only the partial tail.
                    if let Some(p) = pending.rfind('\n') {
                        pending.drain(..=p);
                    }
                }
                _ => {}
            }
        }
        let tail = String::from_utf8_lossy(&collected);
        let tail = &tail[tail.len().saturating_sub(400)..];
        anyhow::bail!(
            "expected output never arrived ({} bytes seen, tail: {:?})",
            collected.len(),
            tail
        )
    }

    /// Collect D2C::Blocks frames for `id` — maintaining the same upserted
    /// local list a GUI would (full replaces, incrementals key on
    /// (epoch, start_off)) — until `pred` over the list holds.
    fn await_blocks(
        &mut self,
        id: Uuid,
        secs: u64,
        pred: impl Fn(&[BlockRec]) -> bool,
    ) -> anyhow::Result<Vec<BlockRec>> {
        let mut list: Vec<BlockRec> = Vec::new();
        let mut saw_any = false;
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            match self.recv() {
                Ok(D2C::Blocks { id: rid, full, recs, .. }) if rid == id => {
                    saw_any = true;
                    if full {
                        list = recs;
                    } else {
                        for r in recs {
                            match list
                                .iter_mut()
                                .find(|x| (x.epoch, x.start_off) == (r.epoch, r.start_off))
                            {
                                Some(x) => *x = r,
                                None => list.push(r),
                            }
                        }
                    }
                    if pred(&list) {
                        return Ok(list);
                    }
                }
                _ => {}
            }
        }
        anyhow::bail!(
            "block condition not met within {secs}s (saw_any_frame={saw_any}, {} recs: {:?})",
            list.len(),
            list.iter().map(|r| (&r.cmd, r.exit)).collect::<Vec<_>>()
        )
    }

    /// Await the D2C::BlockText reply for one request (requester-only frame).
    fn await_block_text(
        &mut self,
        id: Uuid,
        start_off: u64,
        secs: u64,
    ) -> anyhow::Result<(String, bool)> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(D2C::BlockText {
                id: rid,
                start_off: so,
                text,
                truncated,
            }) = self.recv()
            {
                if rid == id && so == start_off {
                    return Ok((text, truncated));
                }
            }
        }
        anyhow::bail!("no BlockText reply for offset {start_off} within {secs}s")
    }

    /// The daemon answers a Ping — its client loop is still healthy.
    fn assert_alive(&mut self) -> anyhow::Result<()> {
        self.send(&C2D::Ping)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(D2C::Pong) = self.recv() {
                return Ok(());
            }
        }
        anyhow::bail!("daemon did not answer Ping")
    }

    /// Attach and collect the replay tail for a terminal.
    fn replay(&mut self, id: Uuid) -> anyhow::Result<Vec<u8>> {
        self.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(D2C::Replay { id: rid, bytes }) = self.recv() {
                if rid == id {
                    return Ok(bytes);
                }
            }
        }
        anyhow::bail!("no replay for {id}");
    }
}

/// Create a fresh PowerShell probe terminal and wait until it is Running.
fn create_probe_terminal(c: &mut Conn, name: &str) -> anyhow::Result<Uuid> {
    c.send(&C2D::CreateTerminal {
        spec: NewTerminal {
            name: name.into(),
            folder: None,
            kind: TermKind::Shell,
            program: "powershell.exe".into(),
            args: vec!["-NoLogo".into()],
            cwd: "C:\\".into(),
            already_launched: false,
            shell_cfg: None,
        },
    })?;
    let state = c.snapshot_until(10, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == name && t.status == TermStatus::Running)
    })?;
    Ok(state.terminals.iter().find(|t| t.name == name).unwrap().id)
}

fn delete_terminal(c: &mut Conn, id: Uuid) {
    let _ = c.send(&C2D::DeleteTerminal { id });
    std::thread::sleep(Duration::from_millis(200));
}

// ─────────────────────────────── cases ───────────────────────────────

fn case_basic() -> anyhow::Result<()> {
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_basic__")?;

    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    c.send(&C2D::Input {
        id,
        bytes: b"echo PROBE_MARKER_12345\r".to_vec(),
    })?;

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match c.recv() {
            Ok(D2C::Replay { id: rid, bytes }) | Ok(D2C::Output { id: rid, bytes })
                if rid == id =>
            {
                collected.extend_from_slice(&bytes);
                let text = String::from_utf8_lossy(&collected);
                if text
                    .lines()
                    .any(|l| l.trim_start().starts_with("PROBE_MARKER_12345") && !l.contains("echo"))
                {
                    break;
                }
            }
            _ => {}
        }
    }
    anyhow::ensure!(
        String::from_utf8_lossy(&collected).contains("PROBE_MARKER_12345"),
        "never saw command output"
    );

    // Reconnect: journal replay must still contain the marker.
    drop(c);
    std::thread::sleep(Duration::from_millis(300));
    let mut c2 = Conn::open()?;
    let _ = c2.first_snapshot()?;
    let replay = c2.replay(id)?;
    anyhow::ensure!(
        String::from_utf8_lossy(&replay).contains("PROBE_MARKER_12345"),
        "replay missing marker ({} bytes)",
        replay.len()
    );
    delete_terminal(&mut c2, id);
    Ok(())
}

fn case_restore() -> anyhow::Result<()> {
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_restore__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    c.send(&C2D::Input {
        id,
        bytes: b"echo RESTORE_OLD_1\r".to_vec(),
    })?;
    c.await_output(id, 20, |l| {
        l.trim_start().starts_with("RESTORE_OLD_1") && !l.contains("echo")
    })?;

    // Kill, wait Dead, restart, wait Running.
    c.send(&C2D::KillTerminal { id })?;
    c.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Dead)
    })?;
    c.send(&C2D::RestartTerminal { id })?;
    c.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
    })?;
    std::thread::sleep(Duration::from_millis(1500));

    // The seam must be INVISIBLE: old output survives, but no marker text and
    // no sentinel leaks into what a client sees.
    let mut c2 = Conn::open()?;
    let _ = c2.first_snapshot()?;
    let text = strip_ansi(&String::from_utf8_lossy(&c2.replay(id)?));
    anyhow::ensure!(
        text.contains("RESTORE_OLD_1"),
        "old output missing after restore"
    );
    anyhow::ensure!(
        !text.contains("restored") && !text.contains("tc:seam"),
        "restore seam leaked visible text"
    );
    delete_terminal(&mut c2, id);
    Ok(())
}

/// Dead-relaunch (fix a wiring, daemon side): a Custom terminal that DIES on
/// its own (`exit 1` — the reviewer's timed-out-ssh stand-in, no WSL needed)
/// refuses input while Dead with the typed `dead` code, never raises
/// reconnect supervision (not ssh), and RestartTerminal — the one verb
/// behind Enter / body-click / `↻ Restore` — re-runs the SAME command in
/// the SAME terminal id with the journal preserved (the second lifetime's
/// output lands below the first's).
fn case_dead_relaunch() -> anyhow::Result<()> {
    let name = "__probe_dead_relaunch__";
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    c.send(&C2D::CreateTerminal {
        spec: NewTerminal {
            name: name.into(),
            folder: None,
            kind: TermKind::Custom,
            program: "powershell.exe".into(),
            args: vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-Command".into(),
                "Write-Output DEAD_RELAUNCH_XYZZY; exit 1".into(),
            ],
            cwd: "C:\\".into(),
            already_launched: false,
            shell_cfg: None,
        },
    })?;
    // The command exits 1 on its own — wait straight for Dead (the Running
    // window is sub-second, but every broadcast still queues on this conn).
    let state = c.snapshot_until(20, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == name && t.status == TermStatus::Dead)
    })?;
    let t = state.terminals.iter().find(|t| t.name == name).unwrap();
    let id = t.id;
    anyhow::ensure!(
        !t.reconnecting,
        "a dead non-ssh Custom must never raise reconnect supervision"
    );

    // TEST 1 — input is REFUSED while Dead (the controller path carries the
    // typed refusal; the GUI's C2D::Input to a dead session is a silent
    // drop by design — no session writer exists).
    let master = master_token()?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    match ctl.ctl(
        9700,
        CtlRequest::SendRaw {
            id,
            bytes: b"echo hi\r".to_vec(),
            force_self: false,
        },
        15,
    )? {
        CtlBody::Err { code, .. } => {
            anyhow::ensure!(code == "dead", "want the `dead` refusal, got `{code}`")
        }
        other => anyhow::bail!("dead input must refuse, got {other:?}"),
    }

    // TEST 2 — RestartTerminal: the same id flips Dead → Running (the
    // Running broadcast queues on this conn even though the command dies
    // again in under a second)…
    c.send(&C2D::RestartTerminal { id })?;
    c.snapshot_until(15, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Running)
    })?;
    // …re-dies (it always exits 1 — same as a still-down ssh host)…
    c.snapshot_until(20, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Dead)
    })?;
    // Let the exit drain land in the journal (case_restore's settle wait).
    std::thread::sleep(Duration::from_millis(1500));
    // …and the journal is PRESERVED across the relaunch: both lifetimes'
    // markers in one scrollback, same terminal identity. Read via the
    // journal-truth ReadTail (a dead-terminal Replay is a screen-sized
    // reconstruction — the first lifetime's marker can scroll out of that
    // window while sitting safely in the journal).
    let tail = match ctl.ctl(9701, CtlRequest::ReadTail { id, lines: 300 }, 15)? {
        CtlBody::Tail { lines, .. } => lines.join("\n"),
        other => anyhow::bail!("ReadTail failed: {other:?}"),
    };
    let hits = tail.matches("DEAD_RELAUNCH_XYZZY").count();
    anyhow::ensure!(
        hits >= 2,
        "relaunch must append below the preserved journal (want 2+ markers, got {hits}); tail:\n{tail}"
    );
    delete_terminal(&mut c, id);
    Ok(())
}

/// Dead-relaunch fix b WITHOUT WSL: a never-hooked ssh tab (TEST-NET
/// 192.0.2.1, unroutable — the reviewer's timed-out-host stand-in) dies
/// without ever qualifying for AUTO reconnect (`hooks_were_live` never
/// rose), exactly the field gap. The manual C2D::RetryReconnect raises
/// supervision anyway (the click is the consent), and CancelReconnect
/// inside the 2s pre-attempt window stops the ladder cleanly: flag down,
/// still dead, zero attempts fired. With the TC_RETRY_BACKOFF_MS staging
/// env (sandbox daemons only) a second leg proves the F1 UNLIMITED manual
/// ladder: attempts sail past the auto path's 3-attempt cap with the
/// Snapshot-stamped progress rising, no give-up line ever, and Cancel
/// stops it mid-ladder clearing the stamps. The full reconnect round-trip
/// lives in `ssh_reconnect` (WSL-gated); this case pins the manual entry +
/// cancel on any box with an OpenSSH client.
fn case_dead_retry_manual() -> anyhow::Result<()> {
    if std::process::Command::new("ssh.exe").arg("-V").output().is_err() {
        return Err(skip("no ssh.exe on PATH (Windows OpenSSH client)".into()));
    }
    let name = "__probe_dead_retry__";
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    c.send(&C2D::CreateTerminal {
        spec: NewTerminal {
            name: name.into(),
            folder: None,
            kind: TermKind::Shell,
            program: "ssh.exe".into(),
            args: vec![
                "-oConnectTimeout=3".into(),
                "-oStrictHostKeyChecking=no".into(),
                "192.0.2.1".into(),
            ],
            cwd: std::path::PathBuf::new(),
            already_launched: false,
            shell_cfg: None,
        },
    })?;
    // Connect times out (~3s) and the terminal dies never having hooked.
    let state = c.snapshot_until(45, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == name && t.status == TermStatus::Dead)
    })?;
    let t = state.terminals.iter().find(|t| t.name == name).unwrap();
    let id = t.id;
    anyhow::ensure!(
        !t.reconnecting,
        "a never-hooked ssh death must NOT auto-reconnect (hooks_were_live gate)"
    );
    // Manual retry: supervision rises by explicit consent…
    c.send(&C2D::RetryReconnect { id })?;
    c.snapshot_until(10, |s| {
        s.terminals.iter().any(|t| t.id == id && t.reconnecting)
    })?;
    // …and Cancel inside the 2s pre-attempt window stops it cleanly.
    c.send(&C2D::CancelReconnect { id })?;
    c.snapshot_until(10, |s| {
        s.terminals.iter().any(|t| t.id == id && !t.reconnecting)
    })?;
    // Past the would-be first rung: still dead, no attempt ever fired.
    std::thread::sleep(Duration::from_millis(3500));
    let master = master_token()?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    if let CtlBody::Listing { terminals, .. } = ctl.ctl(9800, CtlRequest::List, 20)? {
        let t = terminals.iter().find(|t| t.id == id);
        anyhow::ensure!(
            t.is_some_and(|t| t.status == "dead"),
            "cancelled manual retry must leave the terminal dead: {:?}",
            t.map(|t| &t.status)
        );
    }
    let log = log_since(log0);
    anyhow::ensure!(
        log.contains("manual ssh reconnect requested"),
        "no manual-retry scheduling line in daemon.log"
    );
    anyhow::ensure!(
        !log.contains("ssh reconnect attempt"),
        "cancel preceded the first rung — no attempt may fire"
    );

    // ── F1 (ssh-reestablish): the UNLIMITED manual ladder. Needs the
    // flattened-backoff staging env (TC_RETRY_BACKOFF_MS on the SANDBOX
    // daemon — TC_DATA_DIR-gated, the TC_SSH_VIA_WSL guard class); without
    // it the 2s/10s/30s ramp makes attempt 4 a ~90s wait, so this leg skips
    // honestly rather than lying with a shorter assertion.
    if std::env::var("TC_RETRY_BACKOFF_MS").is_ok() {
        c.send(&C2D::RetryReconnect { id })?;
        // Past the AUTO cap: the old ladder gave up after 3 attempts; the
        // manual one must sail past 4 and NEVER log the give-up line. The
        // Snapshot-stamped progress (retry_attempt) is the witness the GUI
        // lane renders from.
        c.snapshot_until(90, |s| {
            s.terminals
                .iter()
                .any(|t| t.id == id && t.retry_attempt >= 4)
        })?;
        let log = log_since(log0);
        anyhow::ensure!(
            !log.contains("ssh reconnect gave up"),
            "manual ladder must never give up (unlimited attempts)"
        );
        anyhow::ensure!(
            log.contains("(manual, unlimited)"),
            "manual attempts must log their unlimited class"
        );
        // Cancel stops it mid-ladder and clears the progress stamps.
        c.send(&C2D::CancelReconnect { id })?;
        c.snapshot_until(10, |s| {
            s.terminals
                .iter()
                .any(|t| t.id == id && !t.reconnecting && t.retry_attempt == 0 && t.retry_next_s == 0)
        })?;
    } else {
        eprintln!(
            "  [dead_retry_manual] unlimited-ladder leg skipped (TC_RETRY_BACKOFF_MS not staged)"
        );
    }
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

fn case_remnant() -> anyhow::Result<()> {
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_remnant__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    // Put recognizable old content on screen before the restore.
    c.send(&C2D::Input {
        id,
        bytes: b"echo SEAM_OLD_777\r".to_vec(),
    })?;
    c.await_output(id, 20, |l| {
        l.trim_start().starts_with("SEAM_OLD_777") && !l.contains("echo")
    })?;

    c.send(&C2D::KillTerminal { id })?;
    c.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Dead)
    })?;
    c.send(&C2D::RestartTerminal { id })?;
    c.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
    })?;
    // Give the restored shell time to print its first prompt.
    std::thread::sleep(Duration::from_millis(1500));

    // A fresh attach gets the SERIALIZED world: the seam must be contiguous —
    // old output, marker, then the new prompt within a couple of lines. No
    // blank voids (the old pad-length assertion), no stale remnants.
    let mut c2 = Conn::open()?;
    let _ = c2.first_snapshot()?;
    let replay = c2.replay(id)?;
    let text = strip_ansi(&String::from_utf8_lossy(&replay));
    let lines: Vec<&str> = text.lines().collect();
    // The seam must be invisible: old output present, no marker/sentinel text,
    // no blank void, and the dead session's dangling prompt deduped against
    // the fresh one — exactly ONE bare prompt line survives (the live one).
    let old_idx = lines
        .iter()
        .position(|l| l.contains("SEAM_OLD_777") && !l.contains("echo"))
        .ok_or_else(|| anyhow::anyhow!("old output missing after restore"))?;
    anyhow::ensure!(
        !text.contains("restored") && !text.contains("tc:seam"),
        "restore seam leaked visible text"
    );
    // Blank-void check up to the last content line only: rows below the live
    // prompt are the (legitimately empty) fresh screen.
    let last_content = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .unwrap_or(0);
    let mut blank_run = 0usize;
    for l in &lines[..=last_content] {
        if l.trim().is_empty() {
            blank_run += 1;
            anyhow::ensure!(blank_run <= 2, "blank void survived in the seam region");
        } else {
            blank_run = 0;
        }
    }
    // The dead session's dangling prompt must be deduped against the fresh
    // one: exactly ONE bare prompt line (any "PS …>" with nothing typed)
    // after the old output.
    let bare_prompts = lines[old_idx..]
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("PS ") && t.ends_with('>')
        })
        .count();
    anyhow::ensure!(
        bare_prompts == 1,
        "expected exactly one live prompt after dedupe, found {bare_prompts}; tail: {:?}",
        lines[old_idx..]
            .iter()
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
    );
    // Any-height correctness: attaching at a given grid size resizes the
    // session first, so the serialized cursor must land on the prompt line at
    // EVERY client height — never float on a blank row mid-screen (the
    // "floating cursor" reopen bug).
    for rows in [24u16, 42, 60] {
        let mut ch = Conn::open()?;
        let _ = ch.first_snapshot()?;
        ch.send(&C2D::Attach { id, cols: 160, rows })?;
        let sized_replay = loop {
            match ch.recv() {
                Ok(D2C::Replay { id: rid, bytes }) if rid == id => break bytes,
                Ok(_) => {}
                Err(e) => anyhow::bail!("no sized replay at {rows} rows: {e}"),
            }
        };
        let cur_line = parse_cursor_line(&sized_replay, 160, rows);
        anyhow::ensure!(
            cur_line.trim_start().starts_with("PS "),
            "cursor landed on {cur_line:?} instead of the prompt at {rows} rows"
        );
    }
    delete_terminal(&mut c2, id);
    Ok(())
}

/// Banner-visibility fix, both halves (task: "pwsh tabs missing the startup
/// banner; cmd stacks ~15 banners across restarts") + the respawn-banner fix
/// (field bug: every daemon restart stamped a fresh banner at the BOTTOM of
/// every restored pwsh tab, under the replayed scrollback):
///   - a hooked pwsh spawned WITHOUT -NoLogo shows the real Windows
///     PowerShell banner on its FIRST-EVER launch (the `-Command
///     . '<bootstrap>'` launch suppresses the native logo; the bootstrap
///     reproduces it);
///   - across TWO kill/restart cycles the replay shows the banner EXACTLY
///     once, and it is the FIRST launch's copy at the TOP of the history
///     (respawns generate a bannerless bootstrap — the replayed scrollback
///     already carries the banner) while old output survives below it;
///   - the SEAM is prompt-clean (respawn-seam fix, the "bare PS C:\> under
///     the scrollback" second half): an attach landing BEFORE the respawned
///     shell's first prompt — the GUI's boot timing — already carries no
///     dead dangling prompt (build-time preface truncation on 133;B byte
///     proof), and the settled replay holds exactly ONE bare prompt (live);
///   - the seam GEOMETRY is live-tight (the "~3-row gap" follow-up): after
///     every cycle the live prompt sits ≤1 blank row under the old output
///     (the truncation removes the dead prompt's seam-residue blanks), and
///     repeated restores never eat an extra row;
///   - first spawn is honest: banner bytes precede the first prompt;
///   - probe terminals elsewhere pass -NoLogo and stay bannerless (honored).
fn case_banner() -> anyhow::Result<()> {
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    c.send(&C2D::CreateTerminal {
        spec: NewTerminal {
            name: "__probe_banner__".into(),
            folder: None,
            kind: TermKind::Shell,
            program: "powershell.exe".into(),
            args: vec![], // no -NoLogo ⇒ the banner is wanted
            cwd: "C:\\".into(),
            already_launched: false,
            shell_cfg: None,
        },
    })?;
    let state = c.snapshot_until(10, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == "__probe_banner__" && t.status == TermStatus::Running)
    })?;
    let id = state
        .terminals
        .iter()
        .find(|t| t.name == "__probe_banner__")
        .unwrap()
        .id;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;

    // Fresh spawn: the reproduced banner is visible, version-faithful (this
    // machine runs Windows PowerShell 5.1 ⇒ logo + copyright + install hint).
    // ONE await on the banner's last element (a second await would starve —
    // the first consumes the frames carrying the whole banner), then assert
    // the rest from the returned bytes. contains-based: conhost renders the
    // banner with absolute cursor moves (`ESC[4;1H` instead of CRLF), so
    // stripped lines can carry leading \r or glue adjacent rows.
    let seen = c.await_output(id, 20, |l| l.contains("Install the latest PowerShell"))?;
    let seen = strip_ansi(&String::from_utf8_lossy(&seen));
    anyhow::ensure!(
        seen.contains("Windows PowerShell")
            && seen.contains("Copyright (C) Microsoft Corporation. All rights reserved."),
        "banner incomplete on fresh spawn: {seen:?}"
    );
    // First-spawn honesty (respawn-seam fix): the banner precedes the first
    // prompt in the byte stream — no real console renders a prompt above its
    // own startup logo. (The field "PS C:\Users> above the banner" was the
    // DEAD lifetime's dangling prompt above the restore seam, pinned by the
    // early-attach assertion below — but the first-spawn order is asserted
    // here so a bootstrap reordering can never reintroduce the shape.)
    if let Some(prompt_at) = seen.find("PS C:\\") {
        let banner_at = seen
            .find("Windows PowerShell")
            .expect("banner presence asserted above");
        anyhow::ensure!(
            banner_at < prompt_at,
            "first spawn must print banner BEFORE the first prompt: {seen:?}"
        );
    }

    // Recognizable old content, then two kill/restart cycles (the user
    // acceptance: "restart the app twice → exactly one banner").
    c.send(&C2D::Input {
        id,
        bytes: b"echo BANNER_OLD_1\r".to_vec(),
    })?;
    c.await_output(id, 20, |l| {
        l.trim_start().starts_with("BANNER_OLD_1") && !l.contains("echo")
    })?;
    for cycle in 0..2 {
        c.send(&C2D::KillTerminal { id })?;
        c.snapshot_until(10, |s| {
            s.terminal(id).is_some_and(|t| t.status == TermStatus::Dead)
        })?;
        c.send(&C2D::RestartTerminal { id })?;
        c.snapshot_until(15, |s| {
            s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
        })?;
        // EARLY-ATTACH pin (respawn-seam fix — the GUI boot shape): attach
        // BEFORE the respawned shell prints its first prompt. The replay
        // must ALREADY be free of the dead lifetime's bare dangling prompt:
        // the preface-build truncation (`tail_ends_at_bare_prompt`) needs no
        // future prompt to compare against, unlike the attach-time matcher —
        // which is exactly why the field GUI (attaching within ms of boot
        // restore, seconds before a cold pwsh prompts) kept showing the dead
        // `PS C:\>` under the scrollback. Timing-tolerant: if the live
        // prompt DID sneak in first, it must be the only bare prompt and sit
        // on the final content line.
        if cycle == 1 {
            let mut ce = Conn::open()?;
            let _ = ce.first_snapshot()?;
            let early = strip_ansi(&String::from_utf8_lossy(&ce.replay(id)?));
            let elines: Vec<&str> = early.lines().collect();
            let bare: Vec<usize> = elines
                .iter()
                .enumerate()
                .filter(|(_, l)| {
                    let t = l.trim();
                    t.starts_with("PS ") && t.ends_with('>')
                })
                .map(|(i, _)| i)
                .collect();
            let last_content = elines.iter().rposition(|l| !l.trim().is_empty());
            anyhow::ensure!(
                bare.is_empty() || (bare.len() == 1 && Some(bare[0]) == last_content),
                "early attach: dead lifetime's dangling prompt survived at the seam \
                 (bare prompt rows {bare:?}, last content {last_content:?}): {:?}",
                elines
                    .iter()
                    .filter(|l| !l.trim().is_empty())
                    .collect::<Vec<_>>()
            );
        }
        // Let the restored shell print its first prompt — NO banner: the
        // respawn bootstrap is generated bannerless (the next cycle's
        // journal must contain a full lifetime). >3s: a shorter-lived
        // lifetime counts as a FAST EXIT (note_fast_exit) and two in a row
        // push the honest "exited immediately N×" info line into the next
        // preface — real restores live minutes, so the probe must not
        // trigger the crash-loop warning the geometry pin would then trip
        // on (it sits, correctly, above the live prompt).
        std::thread::sleep(Duration::from_millis(3200));
        // Seam GEOMETRY pin (the "~3-row gap above Pulse's prompt row"
        // follow-up): once settled, the live prompt is the LAST content row,
        // sitting directly under the old output — at most one blank row, the
        // spacing an unbroken live session shows (pwsh renders its prompt
        // flush under output). Checked after EVERY cycle: the truncation
        // takes the dead prompt's seam-residue blanks with it and must not
        // eat one more row per restore (BANNER_OLD_1 stays the row above).
        let mut cg = Conn::open()?;
        let _ = cg.first_snapshot()?;
        let gtext = strip_ansi(&String::from_utf8_lossy(&cg.replay(id)?));
        let glines: Vec<&str> = gtext.lines().collect();
        let last = glines
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("cycle {cycle}: empty settled replay"))?;
        anyhow::ensure!(
            glines[last].trim() == "PS C:\\>",
            "cycle {cycle}: last content row must be the live prompt, got {:?}",
            glines[last]
        );
        let prev = glines[..last]
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("cycle {cycle}: nothing above the live prompt"))?;
        let gap = last - prev - 1;
        anyhow::ensure!(
            gap <= 1,
            "cycle {cycle}: {gap} blank rows between the last output and the live \
             prompt (live-session spacing allows at most 1): {:?}",
            &glines[prev..=last]
        );
        anyhow::ensure!(
            glines[prev].trim_start().starts_with("BANNER_OLD_1"),
            "cycle {cycle}: the row above the live prompt must be the old output \
             (trimming must not eat content across cycles), got {:?}",
            glines[prev]
        );
    }

    let mut c2 = Conn::open()?;
    let _ = c2.first_snapshot()?;
    let text = strip_ansi(&String::from_utf8_lossy(&c2.replay(id)?));
    let lines: Vec<&str> = text.lines().collect();
    let banners = lines
        .iter()
        .filter(|l| l.trim() == "Windows PowerShell")
        .count();
    anyhow::ensure!(
        banners == 1,
        "expected exactly one banner after two restarts, found {banners}; lines: {:?}",
        lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
    );
    let old = lines
        .iter()
        .position(|l| l.trim_start().starts_with("BANNER_OLD_1") && !l.contains("echo"))
        .ok_or_else(|| anyhow::anyhow!("old output missing after restarts"))?;
    let banner_at = lines
        .iter()
        .position(|l| l.trim() == "Windows PowerShell")
        .unwrap();
    // Respawn-banner fix: the one surviving banner is the FIRST launch's,
    // ABOVE the old output — a restored terminal looks continuous, never a
    // fresh logo stamped under the seam. (Pre-fix this asserted the
    // opposite: the newest spawn reprinted the banner below old content and
    // the seam dedupe collapsed the older copies — the exact duplicate the
    // field bug reported.)
    anyhow::ensure!(
        banner_at < old,
        "the surviving banner must be the FIRST launch's (above old content): banner at line {banner_at}, old output at {old}"
    );
    // Respawn-seam fix: after everything settles, exactly ONE bare prompt
    // line remains — the live one. Every dead lifetime's dangling prompt was
    // dropped (build-time truncation for the final one, the render-time seam
    // pass for older copies already flanked in the journal). This is the
    // "restored terminal = scrollback + Pulse's prompt row, nothing else"
    // acceptance in text form.
    let bare = lines
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("PS ") && t.ends_with('>')
        })
        .count();
    anyhow::ensure!(
        bare == 1,
        "expected exactly one bare prompt (the live one) after two restarts, found {bare}: {:?}",
        lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
    );
    delete_terminal(&mut c2, id);
    Ok(())
}

/// Hidden diagnosis rig (respawn-banner fix, seam-pollution half): dump the
/// EXACT replay bytes a GUI-shaped sized attach receives after a respawn, so
/// the "bare PS C:\> at the restore seam" artifact can be located in the
/// stream (dead lifetime's dangling prompt vs live re-render vs preface bug).
/// Never in the sweep — run with `--probe banner_diag` against a staging
/// daemon.
fn case_banner_diag() -> anyhow::Result<()> {
    let esc = |b: &[u8]| -> String {
        String::from_utf8_lossy(b)
            .replace('\x1b', "<ESC>")
            .replace('\x07', "<BEL>")
            .replace('\r', "<CR>")
            .replace('\n', "<LF>\n")
    };
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    c.send(&C2D::CreateTerminal {
        spec: NewTerminal {
            name: "__probe_bannerdiag__".into(),
            folder: None,
            kind: TermKind::Shell,
            program: "powershell.exe".into(),
            args: vec![], // banner wanted
            cwd: "C:\\".into(),
            already_launched: false,
            shell_cfg: None,
        },
    })?;
    let state = c.snapshot_until(10, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == "__probe_bannerdiag__" && t.status == TermStatus::Running)
    })?;
    let id = state
        .terminals
        .iter()
        .find(|t| t.name == "__probe_bannerdiag__")
        .unwrap()
        .id;
    // GUI-shaped: sized attach.
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let _ = c.await_output(id, 20, |l| l.contains("Install the latest PowerShell"))?;
    c.send(&C2D::Input {
        id,
        bytes: b"echo DIAG_OLD\r".to_vec(),
    })?;
    c.await_output(id, 20, |l| {
        l.trim_start().starts_with("DIAG_OLD") && !l.contains("echo")
    })?;
    c.send(&C2D::KillTerminal { id })?;
    c.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Dead)
    })?;
    c.send(&C2D::RestartTerminal { id })?;
    c.snapshot_until(15, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
    })?;
    std::thread::sleep(Duration::from_millis(2500));

    // Fresh conn, attach at the SAME size the terminal is running at.
    let mut c2 = Conn::open()?;
    let _ = c2.first_snapshot()?;
    c2.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let same = loop {
        match c2.recv() {
            Ok(D2C::Replay { id: rid, bytes }) if rid == id => break bytes,
            Ok(_) => {}
            Err(e) => anyhow::bail!("no same-size replay: {e}"),
        }
    };
    println!("\n===== REPLAY same-size (120x30) =====\n{}", esc(&same));

    // Fresh conn, attach at a DIFFERENT size (the boot-attach shape: the GUI
    // window rarely matches the daemon-restore grid).
    let mut c3 = Conn::open()?;
    let _ = c3.first_snapshot()?;
    c3.send(&C2D::Attach { id, cols: 100, rows: 26 })?;
    let diff = loop {
        match c3.recv() {
            Ok(D2C::Replay { id: rid, bytes }) if rid == id => break bytes,
            Ok(_) => {}
            Err(e) => anyhow::bail!("no diff-size replay: {e}"),
        }
    };
    println!("\n===== REPLAY resized (100x26) =====\n{}", esc(&diff));
    // Give the shell a moment to repaint at the new size, then dump the
    // follow-up Output frames (the resize re-render the mirror sees).
    std::thread::sleep(Duration::from_millis(1500));
    let mut post: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match c3.recv() {
            Ok(D2C::Output { id: rid, bytes }) if rid == id => post.extend_from_slice(&bytes),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    println!("\n===== post-resize Output frames =====\n{}", esc(&post));
    delete_terminal(&mut c3, id);
    Ok(())
}

use crate::strip::AnsiStripper;

/// Strip CSI (ESC [ … final), OSC (ESC ] … BEL/ST), and bare ESC-x sequences
/// so probe assertions can reason about plain text lines.
fn strip_ansi(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            i += 1;
            match b.get(i) {
                Some(b'[') => {
                    i += 1;
                    while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                        i += 1;
                    }
                    i += 1; // final byte
                }
                Some(b']') => {
                    i += 1;
                    while i < b.len() && b[i] != 0x07 {
                        if b[i] == 0x1b && b.get(i + 1) == Some(&b'\\') {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    i += 1; // BEL or the '\' of ST
                }
                Some(_) => i += 1, // ESC x (e.g. ESC =)
                None => {}
            }
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

/// With two clients attached, only one sends Resize; the daemon's stored grid
/// must reflect exactly that client's size (low-priority sanity check).
fn case_resize_owner() -> anyhow::Result<()> {
    let mut a = Conn::open()?;
    let _ = a.first_snapshot()?;
    let mut b = Conn::open()?;
    let _ = b.first_snapshot()?;
    let id = create_probe_terminal(&mut a, "__probe_resize__")?;
    a.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    b.send(&C2D::Attach { id, cols: 0, rows: 0 })?;

    a.send(&C2D::Resize { id, cols: 101, rows: 37 })?;
    std::thread::sleep(Duration::from_millis(200));
    // Resize does not broadcast; a rename forces a snapshot carrying last_*.
    a.send(&C2D::RenameTerminal {
        id,
        name: "__probe_resize__".into(),
    })?;
    a.snapshot_until(5, |s| {
        s.terminal(id)
            .is_some_and(|t| t.last_cols == 101 && t.last_rows == 37)
    })?;

    drop(b);
    delete_terminal(&mut a, id);
    Ok(())
}

fn case_folders() -> anyhow::Result<()> {
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;

    c.send(&C2D::CreateFolder {
        name: "__probe_folder__".into(),
    })?;
    let state = c.snapshot_until(5, |s| {
        s.folders.iter().any(|f| f.name == "__probe_folder__")
    })?;
    let fid = state
        .folders
        .iter()
        .find(|f| f.name == "__probe_folder__")
        .unwrap()
        .id;

    let tid = create_probe_terminal(&mut c, "__probe_folder_term__")?;

    // Move terminal into the folder.
    c.send(&C2D::MoveTerminal {
        id: tid,
        folder: Some(fid),
    })?;
    c.snapshot_until(5, |s| {
        s.terminal(tid).is_some_and(|t| t.folder == Some(fid))
    })?;

    // Bug B: reorder coverage. A second member joins the folder (appended
    // after tid), then ReorderTerminal{delta:-1} must swap the pair's
    // sidebar order (D6: presentation sorts by `order` alone).
    let tid2 = create_probe_terminal(&mut c, "__probe_folder_term2__")?;
    c.send(&C2D::MoveTerminal {
        id: tid2,
        folder: Some(fid),
    })?;
    c.snapshot_until(5, |s| {
        s.terminal(tid2).is_some_and(|t| t.folder == Some(fid))
    })?;
    c.send(&C2D::ReorderTerminal { id: tid2, delta: -1 })?;
    c.snapshot_until(5, |s| {
        match (s.terminal(tid), s.terminal(tid2)) {
            (Some(a), Some(b)) => b.order < a.order,
            _ => false,
        }
    })?;

    // Bug B: MoveFolder{delta:1} must swap folder order with a second
    // folder created after (and therefore ordered below) the first.
    c.send(&C2D::CreateFolder {
        name: "__probe_folder_b__".into(),
    })?;
    let state = c.snapshot_until(5, |s| {
        s.folders.iter().any(|f| f.name == "__probe_folder_b__")
    })?;
    let fid2 = state
        .folders
        .iter()
        .find(|f| f.name == "__probe_folder_b__")
        .unwrap()
        .id;
    c.send(&C2D::MoveFolder { id: fid, delta: 1 })?;
    c.snapshot_until(5, |s| {
        let a = s.folders.iter().find(|f| f.id == fid);
        let b = s.folders.iter().find(|f| f.id == fid2);
        match (a, b) {
            (Some(a), Some(b)) => b.order < a.order,
            _ => false,
        }
    })?;
    c.send(&C2D::DeleteFolder { id: fid2 })?;
    c.snapshot_until(5, |s| !s.folders.iter().any(|f| f.id == fid2))?;

    // Rename the folder.
    c.send(&C2D::RenameFolder {
        id: fid,
        name: "__probe_folder_renamed__".into(),
    })?;
    c.snapshot_until(5, |s| {
        s.folders.iter().any(|f| f.id == fid && f.name == "__probe_folder_renamed__")
    })?;

    // Delete the folder: both members must be reparented to none.
    c.send(&C2D::DeleteFolder { id: fid })?;
    c.snapshot_until(5, |s| {
        !s.folders.iter().any(|f| f.id == fid)
            && s.terminal(tid).is_some_and(|t| t.folder.is_none())
            && s.terminal(tid2).is_some_and(|t| t.folder.is_none())
    })?;

    delete_terminal(&mut c, tid);
    delete_terminal(&mut c, tid2);
    Ok(())
}

fn case_backpressure() -> anyhow::Result<()> {
    // Client A attaches to a flooding terminal and then stops reading.
    let mut a = Conn::open()?;
    let _ = a.first_snapshot()?;
    let flood = create_probe_terminal(&mut a, "__probe_flood__")?;
    a.send(&C2D::Attach { id: flood, cols: 0, rows: 0 })?;
    // Kick off a large burst of output, then never read A again.
    a.send(&C2D::Input {
        id: flood,
        bytes: b"1..8000 | ForEach-Object { \"backpressure line $_\" }\r".to_vec(),
    })?;
    // A deliberately stops draining its socket here.

    std::thread::sleep(Duration::from_millis(500));

    // Client B must stay fully live while A is wedged.
    let mut b = Conn::open()?;
    let _ = b.first_snapshot()?;
    let started = Instant::now();
    let bid = create_probe_terminal(&mut b, "__probe_bp_live__")?;
    anyhow::ensure!(
        started.elapsed() < Duration::from_secs(5),
        "client B was starved by the wedged client A"
    );

    // B keeps receiving output too.
    b.send(&C2D::Attach { id: bid, cols: 0, rows: 0 })?;
    b.send(&C2D::Input {
        id: bid,
        bytes: b"echo B_ALIVE_MARKER\r".to_vec(),
    })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = false;
    while Instant::now() < deadline {
        if let Ok(D2C::Output { id: rid, bytes }) = b.recv() {
            if rid == bid && String::from_utf8_lossy(&bytes).contains("B_ALIVE_MARKER") {
                seen = true;
                break;
            }
        }
    }
    anyhow::ensure!(seen, "client B stopped receiving output while A was wedged");

    delete_terminal(&mut b, flood);
    delete_terminal(&mut b, bid);
    drop(a);
    Ok(())
}

fn case_peb() -> anyhow::Result<()> {
    // Reads this process's own cwd through the PEB path and compares to the real
    // cwd, validating the 0x38 CurrentDirectory offset on this build/runtime.
    anyhow::ensure!(
        crate::daemon::verify_peb_offset(),
        "PEB CurrentDirectory offset self-check failed"
    );
    Ok(())
}

fn case_tracker() -> anyhow::Result<()> {
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_tracker__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    // The PowerShell prompt wrapper emits OSC 9;9;<location> after this cd; the
    // daemon's raw scanner captures it and the tracker folds it into live_cwd.
    c.send(&C2D::Input {
        id,
        bytes: b"cd C:\\Windows\r".to_vec(),
    })?;
    std::thread::sleep(Duration::from_secs(3)); // several 300ms tracker ticks
    let state: SharedState = serde_json::from_slice(&std::fs::read(state_path())?)?;
    let t = state
        .terminals
        .iter()
        .find(|t| t.name == "__probe_tracker__")
        .ok_or_else(|| anyhow::anyhow!("terminal missing from state"))?;
    let cwd = t
        .live_cwd
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("live_cwd not captured by tracker"))?;
    let s = cwd.to_string_lossy().to_lowercase();
    anyhow::ensure!(
        s.trim_end_matches(['\\', '/']).ends_with("windows"),
        "live_cwd did not follow the shell's cd: {s}"
    );
    delete_terminal(&mut c, id);
    Ok(())
}

// ───────────────────── resize / reopen stress support ─────────────────────

/// Accurate length of a possibly-open file. `fs::metadata` can serve a stale
/// size on Windows while an append handle is open; a fresh handle cannot.
fn file_len(path: &std::path::Path) -> u64 {
    std::fs::File::open(path)
        .and_then(|f| f.metadata())
        .map(|m| m.len())
        .unwrap_or(0)
}

fn daemon_log_len() -> u64 {
    file_len(&crate::state::daemon_log_path())
}

/// daemon.log content appended since `from` (a byte offset).
fn log_since(from: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(crate::state::daemon_log_path()) else {
        return String::new();
    };
    let _ = f.seek(SeekFrom::Start(from));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// No daemon thread panicked since `from` (a daemon.log byte offset).
fn ensure_no_new_panics(from: u64) -> anyhow::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(crate::state::daemon_log_path()) else {
        return Ok(());
    };
    let _ = f.seek(SeekFrom::Start(from));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    anyhow::ensure!(
        !text.contains("panicked"),
        "daemon.log gained a panic during the case:\n{}",
        text.lines()
            .filter(|l| l.contains("panicked"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    Ok(())
}

/// Ask the daemon for its per-session size views (headless Term / PTY /
/// state) via the file-based DebugDump reply.
fn debug_dump(c: &mut Conn) -> anyhow::Result<Vec<DebugTermInfo>> {
    let path = crate::state::data_dir().join("debug_dump.json");
    let _ = std::fs::remove_file(&path);
    c.send(&C2D::DebugDump)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(v) = serde_json::from_slice(&bytes) {
                return Ok(v);
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    anyhow::bail!("daemon never wrote debug_dump.json")
}

struct NullListener;
impl alacritty_terminal::event::EventListener for NullListener {
    fn send_event(&self, _: alacritty_terminal::event::Event) {}
}

/// Reconstruct the screen a reopened GUI would show: feed `bytes` through a
/// fresh VT parser + grid at `cols`×`rows` (exactly the attach/replay path)
/// and return every row, scrollback then screen, right-trimmed.
/// Parse a byte stream into a grid at (cols, rows) and return the text of the
/// line the cursor ends on. Proves the serialized cursor placement is
/// height-independent: at ANY client height, the cursor must land on the
/// prompt line, not float mid-screen.
fn parse_cursor_line(bytes: &[u8], cols: u16, rows: u16) -> String {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Column;
    use alacritty_terminal::term::{self, test::TermSize, Term};

    let mut term = Term::new(
        term::Config {
            scrolling_history: 5000,
            ..term::Config::default()
        },
        &TermSize::new(cols as usize, rows as usize),
        NullListener,
    );
    // Immediate (non-deferring) parser: a replay tail could end inside a
    // DECSET 2026 sync block, which the default parser would hold back.
    let mut parser = crate::daemon::ImmediateProcessor::new();
    parser.advance(&mut term, bytes);
    let cur = term.grid().cursor.point;
    let row = &term.grid()[cur.line];
    let mut s = String::with_capacity(cols as usize);
    for c in 0..term.columns() {
        s.push(row[Column(c)].c);
    }
    s.trim_end().to_string()
}

fn parse_screen(bytes: &[u8], cols: u16, rows: u16) -> Vec<String> {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::{self, test::TermSize, Term};

    let mut term = Term::new(
        term::Config {
            scrolling_history: 5000,
            ..term::Config::default()
        },
        &TermSize::new(cols as usize, rows as usize),
        NullListener,
    );
    let mut parser = crate::daemon::ImmediateProcessor::new();
    parser.advance(&mut term, bytes);
    let hist = term.grid().history_size() as i32;
    let mut out = Vec::new();
    for l in -hist..term.screen_lines() as i32 {
        let row = &term.grid()[Line(l)];
        let mut s = String::with_capacity(cols as usize);
        for c in 0..term.columns() {
            s.push(row[Column(c)].c);
        }
        out.push(s.trim_end().to_string());
    }
    out
}

/// Adversarial C2D::Resize sequences — rapid alternation, same-size repeats,
/// degenerate/hostile sizes (0, 1, 65535: must be clamped, never crash), and
/// resizes during an output storm — then assert the daemon is healthy, all
/// three size views (headless Term, PTY, state.json) agree on the final size,
/// the shell still round-trips input, and a fresh attach replays a coherent
/// screen at that size.
fn case_resize_stress() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_rstress__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;

    // Rapid alternation: every message is a real size change.
    for i in 0..40u16 {
        let (cols, rows) = if i % 2 == 0 { (120, 40) } else { (81, 25) };
        c.send(&C2D::Resize { id, cols, rows })?;
    }
    // Same-size repeats: must not thrash ConPTY or fsync state.json.
    for _ in 0..40 {
        c.send(&C2D::Resize { id, cols: 101, rows: 31 })?;
    }
    // Degenerate and hostile sizes: unclamped, 0/1 underflow alacritty's grid
    // indices and 65535² tries to allocate billions of cells in the broker.
    for (cols, rows) in [
        (0, 0),
        (1, 1),
        (0, 50),
        (50, 0),
        (u16::MAX, u16::MAX),
        (2, 2),
        (1000, 2),
        (2, 1000),
    ] {
        c.send(&C2D::Resize { id, cols, rows })?;
    }
    // Resizes racing a live output storm.
    c.send(&C2D::Input {
        id,
        bytes: b"1..2000 | ForEach-Object { \"storm line $_\" }\r".to_vec(),
    })?;
    for i in 0..30u16 {
        let (cols, rows) = if i % 2 == 0 { (90, 30) } else { (140, 45) };
        c.send(&C2D::Resize { id, cols, rows })?;
        std::thread::sleep(Duration::from_millis(10));
    }
    // Settle on a recognizable final size.
    c.send(&C2D::Resize { id, cols: 137, rows: 33 })?;

    // An input round-trip proves the whole pipeline (client loop → PTY →
    // shell → ConPTY → reader → journal → fanout) survived. Messages on one
    // connection are processed in order, so once the marker echoes, every
    // Resize above has been applied.
    c.send(&C2D::Input {
        id,
        bytes: b"echo RSTRESS_OK_7731\r".to_vec(),
    })?;
    c.await_output(id, 60, |l| {
        l.contains("RSTRESS_OK_7731") && !l.contains("echo")
    })?;

    // All three size views must agree on the final size.
    let dump = debug_dump(&mut c)?;
    let e = dump
        .iter()
        .find(|e| e.id == id)
        .ok_or_else(|| anyhow::anyhow!("terminal missing from debug dump"))?;
    anyhow::ensure!(
        (e.term_cols, e.term_rows) == (137, 33)
            && (e.pty_cols, e.pty_rows) == (137, 33)
            && (e.state_cols, e.state_rows) == (137, 33),
        "size views diverged after storm: term {}x{}, pty {}x{}, state {}x{}",
        e.term_cols,
        e.term_rows,
        e.pty_cols,
        e.pty_rows,
        e.state_cols,
        e.state_rows
    );

    // state.json on disk agrees too (that's what a reboot respawns from).
    let state: SharedState = serde_json::from_slice(&std::fs::read(state_path())?)?;
    let t = state
        .terminal(id)
        .ok_or_else(|| anyhow::anyhow!("terminal missing from state.json"))?;
    anyhow::ensure!(
        (t.last_cols, t.last_rows) == (137, 33),
        "state.json stale after storm: {}x{}",
        t.last_cols,
        t.last_rows
    );

    // Reopen path: a fresh attach must replay a screen in which the marker
    // reconstructs as an intact row at the final grid size.
    drop(c);
    let mut c2 = Conn::open()?;
    let _ = c2.first_snapshot()?;
    let replay = c2.replay(id)?;
    let rows_txt = parse_screen(&replay, 137, 33);
    anyhow::ensure!(
        rows_txt
            .iter()
            .any(|l| l.trim_start().starts_with("RSTRESS_OK_7731") && !l.contains("echo")),
        "replayed screen lost the marker row ({} bytes replayed)",
        replay.len()
    );

    c2.assert_alive()?;
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c2, id);
    Ok(())
}

/// Restart×Resize and kill×Resize races from two clients. A Resize landing
/// while the process is mid-spawn used to be silently lost by the PTY and
/// headless Term (only state was updated); the daemon now applies state's
/// size to the fresh session before publishing it, so the three views must
/// always converge. Also: Resize/Input aimed at a deleted terminal are inert.
fn case_resize_race() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut a = Conn::open()?;
    let _ = a.first_snapshot()?;
    let mut b = Conn::open()?;
    let _ = b.first_snapshot()?;
    let id = create_probe_terminal(&mut a, "__probe_rrace__")?;

    for i in 0..5u16 {
        a.send(&C2D::KillTerminal { id })?;
        a.snapshot_until(10, |s| {
            s.terminal(id).is_some_and(|t| t.status == TermStatus::Dead)
        })?;
        // Fire restart and resize from different clients so the daemon can
        // interleave them at any point of the (slow) ConPTY spawn.
        let (cols, rows) = (61 + i * 7, 21 + i);
        b.send(&C2D::RestartTerminal { id })?;
        a.send(&C2D::Resize { id, cols, rows })?;
        a.snapshot_until(10, |s| {
            s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
        })?;
        std::thread::sleep(Duration::from_millis(300));
        let dump = debug_dump(&mut a)?;
        let e = dump
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("terminal missing from debug dump (iter {i})"))?;
        anyhow::ensure!(
            (e.term_cols, e.term_rows) == (e.state_cols, e.state_rows)
                && (e.pty_cols, e.pty_rows) == (e.state_cols, e.state_rows),
            "iter {i}: size views diverged: term {}x{}, pty {}x{}, state {}x{}",
            e.term_cols,
            e.term_rows,
            e.pty_cols,
            e.pty_rows,
            e.state_cols,
            e.state_rows
        );
    }

    // A resize storm racing a kill must not wedge or panic anything.
    for j in 0..20u16 {
        a.send(&C2D::Resize {
            id,
            cols: 70 + j,
            rows: 20 + (j % 5),
        })?;
        if j == 10 {
            b.send(&C2D::KillTerminal { id })?;
        }
    }
    a.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Dead)
    })?;

    // Resize/Input aimed at a deleted terminal must be inert and harmless.
    delete_terminal(&mut a, id);
    for _ in 0..10 {
        a.send(&C2D::Resize { id, cols: 80, rows: 24 })?;
        a.send(&C2D::Input {
            id,
            bytes: b"echo ghost\r".to_vec(),
        })?;
    }
    a.assert_alive()?;
    let state: SharedState = serde_json::from_slice(&std::fs::read(state_path())?)?;
    anyhow::ensure!(
        state.terminal(id).is_none(),
        "deleted terminal reappeared in state"
    );
    anyhow::ensure!(
        !crate::state::journals_dir()
            .join(format!("{id}.log"))
            .exists(),
        "journal for deleted terminal exists"
    );
    ensure_no_new_panics(log0)?;
    drop(b);
    Ok(())
}

/// Deleting a terminal mid-output-storm: the dying session's reader thread
/// keeps draining buffered ConPTY output after the kill; the journal file
/// must be deleted and STAY deleted (the lazy-open in the output path used to
/// resurrect it — 133 orphan journals were found on this machine).
fn case_journal_reap() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_reap__")?;
    // A long storm so plenty of output is still in flight at delete time.
    c.send(&C2D::Input {
        id,
        bytes: b"1..20000 | ForEach-Object { \"reap flood line $_\" }\r".to_vec(),
    })?;
    std::thread::sleep(Duration::from_millis(400)); // storm well underway
    c.send(&C2D::DeleteTerminal { id })?;
    c.snapshot_until(10, |s| s.terminal(id).is_none())?;

    let path = crate::state::journals_dir().join(format!("{id}.log"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::ensure!(!path.exists(), "journal file was not deleted");
    // The resurrection window is the tail of buffered reader output — give it
    // ample time to prove the file stays gone.
    std::thread::sleep(Duration::from_millis(1200));
    anyhow::ensure!(!path.exists(), "journal file resurrected after delete");
    c.assert_alive()?;
    ensure_no_new_panics(log0)?;
    Ok(())
}

/// Push a journal past the 2MB replay cap, then reopen: the replayed tail
/// must respect the cap and still reconstruct a screen ending with the
/// latest output (the cap cut lands on a line boundary).
fn case_replay_cap() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_cap__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    // ~2.7MB of output (1350 × 2000 chars), then an end marker; the shell
    // runs the queued commands in order.
    c.send(&C2D::Input {
        id,
        bytes: b"1..1350 | ForEach-Object { 'C' * 2000 }\r".to_vec(),
    })?;
    c.send(&C2D::Input {
        id,
        bytes: b"echo CAP_END_4413\r".to_vec(),
    })?;
    c.await_output(id, 120, |l| l.contains("CAP_END_4413") && !l.contains("echo"))?;

    let jlen = file_len(&crate::state::journals_dir().join(format!("{id}.log")));
    anyhow::ensure!(
        jlen > 2 * 1024 * 1024,
        "journal too small to exercise the replay cap ({jlen} bytes)"
    );

    drop(c);
    let mut c2 = Conn::open()?;
    let _ = c2.first_snapshot()?;
    let replay = c2.replay(id)?;
    anyhow::ensure!(
        replay.len() as u64 <= 2 * 1024 * 1024,
        "replay exceeds the cap: {} bytes",
        replay.len()
    );
    let rows_txt = parse_screen(&replay, 160, 42);
    anyhow::ensure!(
        rows_txt
            .iter()
            .any(|l| l.trim_start().starts_with("CAP_END_4413") && !l.contains("echo")),
        "reopened screen lost the trailing marker ({} bytes replayed)",
        replay.len()
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c2, id);
    Ok(())
}

/// Keyboard fidelity end-to-end through a REAL PSReadLine. Asserts the
/// premise (conhost requests DECSET 9001 win32-input-mode), then that a
/// win32-encoded Ctrl+Backspace deletes a whole word (PSReadLine
/// BackwardKillWord — impossible over lossy VT bytes), and that a win32
/// Ctrl+C interrupts a running command, exactly as from Windows Terminal.
fn case_keys() -> anyhow::Result<()> {
    use egui::{Key, Modifiers};

    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_keys__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;

    // Prompt up, and the stream (replay trailer or live re-assert) must carry
    // the ?9001h request the client-side encoder keys off.
    let seen = c.await_output(id, 20, |l| l.trim_start().starts_with("PS C:\\"))?;
    let mut scan = crate::win32_input::ModeScanner::new();
    anyhow::ensure!(
        scan.feed(&seen) == Some(true),
        "stream never carried the win32-input-mode request (?9001h)"
    );

    // Type a command as plain text (the passthrough path), kill the last word
    // with a win32 Ctrl+Backspace, and run what remains.
    c.send(&C2D::Input {
        id,
        bytes: b"echo KEYS_AAA KEYS_BBB".to_vec(),
    })?;
    std::thread::sleep(Duration::from_millis(400));
    let cbk = crate::win32_input::encode_key(Key::Backspace, Modifiers::CTRL)
        .expect("ctrl+backspace encodes");
    c.send(&C2D::Input { id, bytes: cbk })?;
    std::thread::sleep(Duration::from_millis(200));
    c.send(&C2D::Input {
        id,
        bytes: b"\r".to_vec(),
    })?;
    let all = c.await_output(id, 20, |l| l.trim() == "KEYS_AAA")?;
    let text = strip_ansi(&String::from_utf8_lossy(&all));
    anyhow::ensure!(
        !text.lines().any(|l| l.trim() == "KEYS_AAA KEYS_BBB"),
        "Ctrl+Backspace did not delete the word (echo printed both tokens)"
    );

    // Interrupt: a win32 Ctrl+C must stop a running ping. The follow-up echo
    // only ever executes if the prompt came back.
    c.send(&C2D::Input {
        id,
        bytes: b"ping -t 127.0.0.1\r".to_vec(),
    })?;
    c.await_output(id, 20, |l| l.contains("Reply from 127.0.0.1"))?;
    let cc = crate::win32_input::encode_key(Key::C, Modifiers::CTRL).expect("ctrl+c encodes");
    c.send(&C2D::Input { id, bytes: cc })?;
    std::thread::sleep(Duration::from_millis(500));
    c.send(&C2D::Input {
        id,
        bytes: b"echo KEYS_INT_OK\r".to_vec(),
    })?;
    c.await_output(id, 25, |l| l.trim() == "KEYS_INT_OK")?;

    delete_terminal(&mut c, id);
    Ok(())
}

/// r3 lock-discipline pin: a huge paste into a terminal whose foreground app
/// has stopped reading console input must never stall OTHER clients' typing.
/// Pre-fix, C2D::Input wrote to the ConPTY pipe while holding the GLOBAL
/// sessions mutex (edition-2021 if-let temporary); once the stuck terminal's
/// input path backed up, the pasting conn thread wedged INSIDE the lock and
/// every other connection's Input — plus the tracker tick, spawns, kills —
/// blocked until the stuck app read a byte. Post-fix the writer Arc is cloned
/// out and the write happens outside the lock, so the wedge is confined to
/// the pasting connection. Two real connections; the paste rides GUI-sized
/// 64KiB frames with no newline (nothing ever executes).
fn case_paste_stuck_child() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;

    // The daemon's conhost children before/after the stuck spawn — the delta
    // is the stuck terminal's conhost (spawn() documents conhosts as direct
    // daemon children).
    let conhosts = |daemon_pid: u32| -> Vec<u32> {
        crate::daemon::procinfo::snapshot_processes()
            .into_iter()
            .filter(|(_, ppid, exe)| *ppid == daemon_pid && exe.eq_ignore_ascii_case("conhost.exe"))
            .map(|(pid, _, _)| pid)
            .collect()
    };
    let before: Vec<u32> = conhosts(info.pid);

    // Conn A owns the stuck terminal and fires the paste from a helper
    // thread (its socket may legitimately wedge with the conn thread — the
    // fix bounds the damage to conn A, not to other clients).
    let mut a = Conn::open()?;
    let _ = a.first_snapshot()?;
    let stuck = create_probe_terminal(&mut a, "__probe_paste_stuck__")?;
    a.send(&C2D::Attach { id: stuck, cols: 0, rows: 0 })?;
    a.await_output(stuck, 20, |l| l.trim_start().starts_with("PS "))?;
    let stuck_conhost = conhosts(info.pid)
        .into_iter()
        .find(|p| !before.contains(p))
        .ok_or_else(|| anyhow::anyhow!("could not identify the stuck terminal's conhost"))?;

    let mut b = Conn::open()?;
    let _ = b.first_snapshot()?;
    let live = create_probe_terminal(&mut b, "__probe_paste_live__")?;
    b.send(&C2D::Attach { id: live, cols: 0, rows: 0 })?;
    b.await_output(live, 20, |l| l.trim_start().starts_with("PS "))?;

    // Freeze the stuck terminal's conhost — the ConPTY input pipe's only
    // reader stops, so the paste below fills the pipe and the daemon-side
    // write_all genuinely blocks (a merely-idle shell doesn't wedge:
    // conhost's input buffer keeps absorbing translated records).
    suspend_process(stuck_conhost, true)?;

    // 1MB of 'a' in GUI-sized 64KiB Input frames, no newline (nothing ever
    // executes). Plenty to fill a pipe measured in KBs.
    let paster = std::thread::spawn(move || {
        let chunk = vec![b'a'; 64 * 1024];
        for _ in 0..16 {
            if a
                .send(&C2D::Input {
                    id: stuck,
                    bytes: chunk.clone(),
                })
                .is_err()
            {
                break;
            }
        }
    });

    // Let the write wedge, then prove a DIFFERENT connection still types
    // with echo. Pre-fix this stalls: the wedged conn thread holds the
    // GLOBAL sessions mutex and conn B's Input blocks on it.
    std::thread::sleep(Duration::from_millis(1500));
    let t0 = Instant::now();
    b.send(&C2D::Input {
        id: live,
        bytes: b"echo PASTE_LIVE_OK\r".to_vec(),
    })?;
    let echo = b.await_output(live, 10, |l| l.trim() == "PASTE_LIVE_OK");
    let echo_ms = t0.elapsed().as_millis();
    // ALWAYS thaw before judging, so even a failing run leaves the daemon
    // un-wedged (the resumed conhost drains the pipe, the blocked write
    // completes, cleanup below proceeds).
    let _ = suspend_process(stuck_conhost, false);
    echo?;
    println!("(echo beside wedged 1MB paste: {echo_ms}ms) ");

    // Cleanup through conn B.
    b.send(&C2D::KillTerminal { id: stuck })?;
    std::thread::sleep(Duration::from_millis(500));
    delete_terminal(&mut b, stuck);
    delete_terminal(&mut b, live);
    let _ = paster.join();
    ensure_no_new_panics(log0)?;
    Ok(())
}

/// Suspend (`freeze`=true) or resume every thread of `pid` via Toolhelp —
/// probe-only plumbing for `paste_stuck_child`'s frozen-conhost scenario.
fn suspend_process(pid: u32, freeze: bool) -> anyhow::Result<()> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows::Win32::System::Threading::{
        OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
    };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)?;
        let mut te = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut touched = 0u32;
        if Thread32First(snap, &mut te).is_ok() {
            loop {
                if te.th32OwnerProcessID == pid {
                    if let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME, false, te.th32ThreadID) {
                        if freeze {
                            SuspendThread(h);
                        } else {
                            ResumeThread(h);
                        }
                        touched += 1;
                        let _ = CloseHandle(h);
                    }
                }
                if Thread32Next(snap, &mut te).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        anyhow::ensure!(touched > 0, "no threads of pid {pid} could be opened");
    }
    Ok(())
}

/// Keystroke-echo latency through a real PSReadLine session — the daemon half
/// of the typing pipeline (probe client → daemon → ConPTY → conhost echo →
/// reader → ingest → fanout → client), the same path every GUI keystroke
/// takes. Two regimes:
///   • paced (10 chars/s): per-char round trip; p95 must stay under 150ms.
///   • sustained (30 chars/s for ~3s): after the LAST char is sent, the final
///     echo must land within 400ms. A queue anywhere on the path (Nagle,
///     an unflushed writer, a resettable debounce) can pass the paced phase
///     and still fail this one — it is exactly the reported "I stop typing
///     and it keeps typing" symptom.
fn case_latency() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_latency__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    c.await_output(id, 20, |l| l.trim_start().starts_with("PS "))?;

    // Dedicated reader: timestamp every Output frame for `id` the moment it
    // arrives. Blocking reads (the 3s timeout is cleared — SO_RCVTIMEO is
    // per-socket, shared with the clone); the thread exits when the case
    // drops the connection. The main thread MUST NOT recv() past this point
    // or frames would split between the two readers.
    let mut rstream = c.stream.try_clone()?;
    rstream.set_read_timeout(None)?;
    let (tx, rx) = std::sync::mpsc::channel::<Instant>();
    std::thread::spawn(move || {
        while let Ok(msg) = read_frame::<_, D2C>(&mut rstream) {
            if let D2C::Output { id: rid, .. } = msg {
                if rid == id && tx.send(Instant::now()).is_err() {
                    break;
                }
            }
        }
    });

    // Warmup: PSReadLine JITs its key-handling path on the first keystroke;
    // keep that one-off cost out of the measurement.
    c.send(&C2D::Input { id, bytes: b"z".to_vec() })?;
    let _ = rx.recv_timeout(Duration::from_secs(3));
    c.send(&C2D::Input { id, bytes: vec![0x1b] })?; // ESC clears the line
    while rx.recv_timeout(Duration::from_millis(600)).is_ok() {}

    // Paced phase: 40 single chars at 10/s; RTT = send → first echo frame.
    let mut rtts_ms: Vec<u64> = Vec::new();
    for i in 0..40u32 {
        let sent = Instant::now();
        c.send(&C2D::Input {
            id,
            bytes: vec![b'a' + (i % 26) as u8],
        })?;
        let arrival = rx
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow::anyhow!("paced char {i}: echo never arrived"))?;
        rtts_ms.push(arrival.saturating_duration_since(sent).as_millis() as u64);
        // Absorb follow-up render frames inside this char's 100ms slot so
        // they can't be mistaken for the next char's echo.
        while let Some(left) = Duration::from_millis(100).checked_sub(sent.elapsed()) {
            let _ = rx.recv_timeout(left);
        }
    }
    // Full per-char vector in SEND ORDER (pre-sort): the repeatable one-time
    // ~0.44s early-echo stall (r3 latency 4) shows up as one slow index —
    // a constant offset from spawn across runs means shell-side one-time
    // init (PSReadLine/.NET), a moving one means look at daemon.log.
    println!("[latency] paced rtts_ms by index: {rtts_ms:?}");
    rtts_ms.sort_unstable();
    let p50 = rtts_ms[rtts_ms.len() / 2];
    let p95 = rtts_ms[rtts_ms.len() * 95 / 100];
    let max = *rtts_ms.last().unwrap();

    // Clear the line, settle.
    c.send(&C2D::Input { id, bytes: vec![0x1b] })?;
    while rx.recv_timeout(Duration::from_millis(600)).is_ok() {}

    // Sustained phase: 90 chars at ~30/s. The probe only clocks the tail —
    // any backlog built while typing shows up as echo still arriving after
    // the last send.
    let start = Instant::now();
    let mut last_send = start;
    for i in 0..90u32 {
        let due = start + Duration::from_millis(i as u64 * 33);
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        last_send = Instant::now();
        c.send(&C2D::Input {
            id,
            bytes: vec![b'a' + (i % 26) as u8],
        })?;
    }
    let mut last_arrival: Option<Instant> = None;
    let mut frames = 0u32;
    while let Ok(t) = rx.recv_timeout(Duration::from_millis(600)) {
        last_arrival = Some(t);
        frames += 1;
    }
    let last_arrival =
        last_arrival.ok_or_else(|| anyhow::anyhow!("sustained phase: no echo at all"))?;
    let drain_ms = last_arrival.saturating_duration_since(last_send).as_millis() as u64;

    println!();
    println!(
        "[latency] paced(10cps) p50={p50}ms p95={p95}ms max={max}ms | sustained(30cps) drain_after_last_key={drain_ms}ms frames={frames}"
    );

    anyhow::ensure!(p95 < 150, "paced echo p95 {p95}ms ≥ 150ms — typing lags");
    anyhow::ensure!(
        drain_ms < 400,
        "echo kept arriving {drain_ms}ms after the last key — output is queuing behind sustained typing"
    );

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// Output-smoothness measurement + sanity: push ~50MB through one session and
/// assert the whole pipeline (PTY → reader → journal → fanout → client)
/// survives to an in-order end sentinel. Prints wall time, daemon CPU time,
/// received bytes, and throughput so before/after builds can be compared —
/// the numbers are reported, not asserted (they are machine-dependent).
/// Also reports whether ConPTY passes DECSET 2026 (synchronized output)
/// through to us — informational, it depends on the inbox conhost build.
fn case_flood() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_flood50__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    // Wait for the prompt so shell startup stays out of the measurement.
    c.await_output(id, 20, |l| l.trim_start().starts_with("PS "))?;

    let info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
    let cpu0 = crate::daemon::process_cpu_ms(info.pid).unwrap_or(0);
    let t0 = Instant::now();

    // A raw BSU/ESU pair: if the bytes survive to our reader, the platform
    // passes synchronized output through and the GUI can present atomically.
    c.send(&C2D::Input {
        id,
        bytes: b"[Console]::Out.Write(\"$([char]27)[?2026hSYNC_2026_PROBE$([char]27)[?2026l`n\")\r"
            .to_vec(),
    })?;
    // ~50MB in 1MB console writes, then an end sentinel. The shell runs the
    // queued commands in order, so seeing the sentinel proves nothing was
    // dropped or reordered on the way to this client.
    c.send(&C2D::Input {
        id,
        bytes: b"$s=('X'*199+\"`n\")*5000; for($i=0;$i -lt 50;$i++){[Console]::Out.Write($s)}; [Console]::Out.Write(\"FLOOD_DONE_9911`n\")\r"
            .to_vec(),
    })?;
    let collected = c.await_output(id, 240, |l| l.trim() == "FLOOD_DONE_9911")?;
    let wall = t0.elapsed();
    let cpu1 = crate::daemon::process_cpu_ms(info.pid).unwrap_or(cpu0);

    let sync_2026 = collected.windows(8).any(|w| w == b"\x1b[?2026h");
    let mb = collected.len() as f64 / (1024.0 * 1024.0);
    println!();
    println!(
        "[flood] bytes={} ({mb:.1} MB) wall_ms={} daemon_cpu_ms={} throughput={:.1} MB/s sync2026_passthrough={}",
        collected.len(),
        wall.as_millis(),
        cpu1.saturating_sub(cpu0),
        mb / wall.as_secs_f64().max(0.001),
        if sync_2026 { "yes" } else { "no" },
    );

    anyhow::ensure!(
        collected.len() > 40 * 1024 * 1024,
        "flood arrived too small ({} bytes) — output was lost",
        collected.len()
    );
    c.assert_alive()?;
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// Hidden measurement case (NOT in the sweep): attach cost for a session with
/// full scrollback. Fills the mirror's 2000-line history, then measures 20
/// Attach→Replay round-trips at a constant size (constant so the daemon-side
/// resize no-ops and the number is pure serialize+frame+deliver). Report-only.
fn case_perf_attach() -> anyhow::Result<()> {
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_perf_attach__")?;
    c.send(&C2D::Attach { id, cols: 120, rows: 40 })?;
    c.await_output(id, 20, |l| l.trim_start().starts_with("PS "))?;
    // ~600KB / 3000 realistic rows: saturates scrolling_history (2000).
    c.send(&C2D::Input {
        id,
        bytes: b"$s=('Y'*199+\"`n\")*3000; [Console]::Out.Write($s); [Console]::Out.Write(\"HIST_FULL_77`n\")\r"
            .to_vec(),
    })?;
    c.await_output(id, 120, |l| l.trim() == "HIST_FULL_77")?;
    // Let the prompt render + any residual output drain, then discard it.
    std::thread::sleep(Duration::from_millis(800));
    while c.recv().is_ok() {}
    let mut times_us: Vec<u64> = Vec::new();
    let mut replay_len = 0usize;
    for _ in 0..20 {
        c.send(&C2D::Detach { id })?;
        let t0 = Instant::now();
        c.send(&C2D::Attach { id, cols: 120, rows: 40 })?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            anyhow::ensure!(Instant::now() < deadline, "no Replay within 10s");
            match c.recv() {
                Ok(D2C::Replay { id: rid, bytes }) if rid == id => {
                    times_us.push(t0.elapsed().as_micros() as u64);
                    replay_len = bytes.len();
                    break;
                }
                _ => {}
            }
        }
        // Drain the rest of the attach sequence (StreamPos/Blocks/PromptState).
        std::thread::sleep(Duration::from_millis(50));
    }
    times_us.sort_unstable();
    println!();
    println!(
        "[perf_attach] runs={} replay_bytes={replay_len} p50_us={} p95_us={} max_us={}",
        times_us.len(),
        times_us[times_us.len() / 2],
        times_us[times_us.len() * 95 / 100],
        times_us.last().unwrap(),
    );
    delete_terminal(&mut c, id);
    Ok(())
}

/// Hidden measurement case (NOT in the sweep): daemon CPU at rest with 20
/// idle sessions — the "20-session daemon at rest should be ~0" target.
/// No client is attached to any of them during the measurement. Report-only.
fn case_perf_idle() -> anyhow::Result<()> {
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let mut ids = Vec::new();
    for i in 0..20 {
        ids.push(create_probe_terminal(&mut c, &format!("__probe_idle_{i:02}__"))?);
    }
    // Shells reach their prompts; the world goes quiet.
    std::thread::sleep(Duration::from_secs(8));
    let info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
    let cpu0 = crate::daemon::process_cpu_ms(info.pid).unwrap_or(0);
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs(30));
    let cpu1 = crate::daemon::process_cpu_ms(info.pid).unwrap_or(cpu0);
    let wall_s = t0.elapsed().as_secs_f64();
    println!();
    println!(
        "[perf_idle] sessions=20 cpu_ms={} over {wall_s:.0}s = {:.2} ms/s",
        cpu1.saturating_sub(cpu0),
        cpu1.saturating_sub(cpu0) as f64 / wall_s,
    );
    for id in ids {
        let _ = c.send(&C2D::DeleteTerminal { id });
    }
    std::thread::sleep(Duration::from_millis(500));
    Ok(())
}

/// Journal Blocks end-to-end through a REAL PSReadLine: the injected pwsh
/// bootstrap's exec/pre hooks must yield daemon block records carrying the
/// typed command, the true exit code (0, then a native 3), the cwd, and
/// journal offsets that bracket the output.
fn case_blocks_roundtrip() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_blocks_rt__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    // Attach must be answered with a full Blocks sync (empty is fine).
    c.await_blocks(id, 10, |_| true)?;

    c.send(&C2D::Input {
        id,
        bytes: b"echo BLK_A\r".to_vec(),
    })?;
    // Output first (proves the stream), then the close (proves the hooks).
    c.await_output(id, 20, |l| l.trim() == "BLK_A")?;
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("echo BLK_A") && r.end_off.is_some())
    })?;
    let r = recs
        .iter()
        .find(|r| r.cmd.contains("echo BLK_A"))
        .unwrap();
    anyhow::ensure!(r.exit == Some(0), "expected exit 0, got {:?}", r.exit);
    anyhow::ensure!(
        r.end_off.unwrap() > r.start_off,
        "offsets don't bracket output: {}..{:?}",
        r.start_off,
        r.end_off
    );
    anyhow::ensure!(
        r.cwd.as_ref().is_some_and(|p| !p.as_os_str().is_empty()),
        "block cwd missing"
    );
    anyhow::ensure!(r.n > 0, "closing prompt counter not recorded");

    // A native exit code must come through verbatim.
    c.send(&C2D::Input {
        id,
        bytes: b"cmd /c exit 3\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("cmd /c exit 3") && r.end_off.is_some())
    })?;
    let r = recs
        .iter()
        .find(|r| r.cmd.contains("cmd /c exit 3"))
        .unwrap();
    anyhow::ensure!(r.exit == Some(3), "expected exit 3, got {:?}", r.exit);

    // Fix B (cmdlet exit-code inheritance): a PURE CMDLET run after the
    // failing native command above must NOT inherit its $LASTEXITCODE — pwsh
    // only sets $LASTEXITCODE for native commands, so `ls` leaves it at the
    // stale 3. Before the fix the bootstrap trusted that stale code and `ls`
    // was mis-flagged failed, drawing its own red gutter that abutted the
    // native command's into one "bleeding" stripe. It must fold $? to 0/1.
    c.send(&C2D::Input {
        id,
        bytes: b"ls\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.trim() == "ls" && r.end_off.is_some())
    })?;
    let r = recs.iter().find(|r| r.cmd.trim() == "ls").unwrap();
    anyhow::ensure!(
        r.exit != Some(3),
        "cmdlet inherited the prior native exit code: {:?}",
        r.exit
    );
    anyhow::ensure!(
        matches!(r.exit, Some(0) | Some(1)),
        "cmdlet-only exit must fold $? to 0/1, got {:?}",
        r.exit
    );

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// Blocks survive kill + restore: the epoch bumps, prior-epoch records stay
/// intact (absolute offsets), a block left open by the dying session closes
/// with exit=None, offsets stay strictly monotonic across the seam, and the
/// remnant-style attach asserts (cursor on the prompt at any height) still
/// pass with the hook OSCs in the stream.
fn case_blocks_restore() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_blocks_rs__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;

    c.send(&C2D::Input {
        id,
        bytes: b"echo BLKRS_ONE\r".to_vec(),
    })?;
    c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("echo BLKRS_ONE") && r.end_off.is_some())
    })?;
    c.send(&C2D::Input {
        id,
        bytes: b"echo BLKRS_TWO\r".to_vec(),
    })?;
    c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("echo BLKRS_TWO") && r.end_off.is_some())
    })?;
    // Leave a block OPEN (ping runs until killed; no closing prompt).
    c.send(&C2D::Input {
        id,
        bytes: b"ping -t 127.0.0.1\r".to_vec(),
    })?;
    c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("ping -t") && r.end_off.is_none())
    })?;

    c.send(&C2D::KillTerminal { id })?;
    c.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Dead)
    })?;
    c.send(&C2D::RestartTerminal { id })?;
    c.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
    })?;
    std::thread::sleep(Duration::from_millis(1500));

    // Fresh attach: the full sync must carry both epochs.
    let mut c2 = Conn::open()?;
    let _ = c2.first_snapshot()?;
    c2.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    let full = c2.await_blocks(id, 10, |recs| recs.len() >= 3)?;
    let first_epoch = full.iter().map(|r| r.epoch).min().unwrap();
    c2.send(&C2D::Input {
        id,
        bytes: b"echo BLKRS_THREE\r".to_vec(),
    })?;
    let inc = c2.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("echo BLKRS_THREE") && r.end_off.is_some())
    })?;
    // await_blocks starts a fresh local list per call; merge the incremental
    // records over the attach-time full sync the way a GUI would.
    let mut recs = full;
    for r in inc {
        match recs
            .iter_mut()
            .find(|x| (x.epoch, x.start_off) == (r.epoch, r.start_off))
        {
            Some(x) => *x = r,
            None => recs.push(r),
        }
    }

    let three = recs
        .iter()
        .find(|r| r.cmd.contains("echo BLKRS_THREE"))
        .unwrap();
    anyhow::ensure!(
        three.epoch > first_epoch,
        "epoch did not bump across restore ({} -> {})",
        first_epoch,
        three.epoch
    );
    for marker in ["BLKRS_ONE", "BLKRS_TWO"] {
        let r = recs
            .iter()
            .find(|r| r.cmd.contains(marker))
            .ok_or_else(|| anyhow::anyhow!("old-epoch record {marker} lost across restore"))?;
        anyhow::ensure!(r.epoch == first_epoch && r.exit == Some(0));
    }
    let dangling = recs
        .iter()
        .find(|r| r.cmd.contains("ping -t"))
        .ok_or_else(|| anyhow::anyhow!("dangling record lost across restore"))?;
    anyhow::ensure!(
        dangling.exit.is_none() && dangling.end_off.is_some(),
        "dangling block not closed with exit=None: exit {:?}, end {:?}",
        dangling.exit,
        dangling.end_off
    );
    // Offsets strictly monotonic across the seam, in record order.
    let mut sorted = recs.clone();
    sorted.sort_by_key(|r| (r.epoch, r.start_off));
    for w in sorted.windows(2) {
        anyhow::ensure!(
            w[1].start_off > w[0].start_off,
            "offsets not strictly monotonic across the seam"
        );
        if let Some(e) = w[0].end_off {
            anyhow::ensure!(w[1].start_off >= e, "blocks overlap across the seam");
        }
    }

    // Remnant-style any-height asserts still hold with hook OSCs around.
    // Settle first: await_blocks returned the instant the closing `pre`
    // landed, but the prompt TEXT renders on a later async conhost frame
    // (and PSReadLine's takeover — whose console API traffic used to flush
    // that frame early — now starts after the bootstrap's 15ms 133;B
    // flush-sleep, P3). Attaching inside that ≤~35ms gap serializes a
    // mid-render screen whose cursor sits on a still-blank prompt row —
    // true, transient, and not what this assert is about.
    std::thread::sleep(Duration::from_millis(400));
    for rows in [24u16, 42] {
        let mut ch = Conn::open()?;
        let _ = ch.first_snapshot()?;
        ch.send(&C2D::Attach { id, cols: 160, rows })?;
        let sized_replay = loop {
            match ch.recv() {
                Ok(D2C::Replay { id: rid, bytes }) if rid == id => break bytes,
                Ok(_) => {}
                Err(e) => anyhow::bail!("no sized replay at {rows} rows: {e}"),
            }
        };
        let cur_line = parse_cursor_line(&sized_replay, 160, rows);
        anyhow::ensure!(
            cur_line.trim_start().starts_with("PS "),
            "cursor landed on {cur_line:?} instead of the prompt at {rows} rows"
        );
        let text = strip_ansi(&String::from_utf8_lossy(&sized_replay));
        anyhow::ensure!(
            !text.contains("restored") && !text.contains("tc:seam"),
            "restore seam leaked visible text"
        );
    }
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c2, id);
    Ok(())
}

/// A hook OSC forged with the wrong token — emitted through the shell so it
/// lands in the OUTPUT stream where the scanner reads — must be rejected and
/// logged, without desyncing the scanner or minting a phantom record.
fn case_blocks_antispoof() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_blocks_as__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    c.await_blocks(id, 10, |_| true)?;

    c.send(&C2D::Input {
        id,
        bytes: b"[Console]::Write([char]27+']7717;00000000deadbeef;pre;7b7d'+[char]7)\r"
            .to_vec(),
    })?;
    // The forging command's own block must still close correctly (exit 0) —
    // the forged pre inside its output didn't confuse the scanner.
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("7717") && r.end_off.is_some())
    })?;
    anyhow::ensure!(
        recs.len() == 1,
        "forged hook minted extra records: {:?}",
        recs.iter().map(|r| &r.cmd).collect::<Vec<_>>()
    );
    anyhow::ensure!(
        recs[0].exit == Some(0),
        "forging command's own block misclosed: {:?}",
        recs[0].exit
    );
    anyhow::ensure!(
        log_since(log0).contains("block hook with wrong token rejected"),
        "daemon.log has no rejection line for the forged hook"
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// Journal compaction (the 8MiB cap) must evict block records that now point
/// fully before the file's head, flag straddlers truncated, and persist the
/// new base in the sidecar — while every surviving offset stays resolvable
/// against the compacted file.
fn case_blocks_compact_evict() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_blocks_ce__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;

    // A few small blocks destined for eviction.
    for i in 0..3 {
        let cmd = format!("echo BLKCE_SMALL_{i}\r");
        c.send(&C2D::Input {
            id,
            bytes: cmd.into_bytes(),
        })?;
        let marker = format!("BLKCE_SMALL_{i}");
        c.await_blocks(id, 20, move |recs| {
            recs.iter()
                .any(|r| r.cmd.contains(&marker) && r.end_off.is_some())
        })?;
    }
    // One block that floods past MAX_LEN (8 MiB) to force compaction; its own
    // output straddles the cut.
    c.send(&C2D::Input {
        id,
        bytes: b"$s=('C'*199+\"`n\")*5000; for($i=0;$i -lt 9;$i++){[Console]::Out.Write($s)}; echo BLKCE_FLOOD_END\r"
            .to_vec(),
    })?;
    c.await_blocks(id, 240, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("BLKCE_FLOOD_END") && r.end_off.is_some())
    })?;
    // One clean post-compaction block.
    c.send(&C2D::Input {
        id,
        bytes: b"echo BLKCE_AFTER\r".to_vec(),
    })?;
    c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("echo BLKCE_AFTER") && r.end_off.is_some())
    })?;

    // The sidecar is the durable truth: read it back.
    let side_path = crate::state::journals_dir().join(format!("{id}.blocks.json"));
    let side: serde_json::Value = serde_json::from_slice(&std::fs::read(&side_path)?)?;
    let base = side["base"].as_u64().unwrap_or(0);
    let recs: Vec<BlockRec> = serde_json::from_value(side["recs"].clone())?;
    anyhow::ensure!(base > 0, "compaction never moved the base");
    anyhow::ensure!(
        !recs.iter().any(|r| r.cmd.contains("BLKCE_SMALL_")),
        "fully-pre-base records were not evicted"
    );
    let flood = recs
        .iter()
        .find(|r| r.cmd.contains("BLKCE_FLOOD_END"))
        .ok_or_else(|| anyhow::anyhow!("straddling record was evicted"))?;
    anyhow::ensure!(flood.truncated, "straddling record not flagged truncated");
    anyhow::ensure!(flood.start_off < base, "straddler doesn't straddle");
    let jlen = file_len(&crate::state::journals_dir().join(format!("{id}.log")));
    for r in &recs {
        if let Some(end) = r.end_off {
            anyhow::ensure!(end > base, "surviving record ends before the base");
            anyhow::ensure!(
                end - base <= jlen,
                "record offset unresolvable in the compacted file ({} - {} > {})",
                end,
                base,
                jlen
            );
        }
    }
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// Upsert incremental block records over a running list the way a GUI would
/// (P1 key: (epoch, start_off); start_off alone is unique, epoch kept for
/// clarity against the P1 helpers above).
fn merge_blocks(all: &mut Vec<BlockRec>, recs: Vec<BlockRec>) {
    for r in recs {
        match all
            .iter_mut()
            .find(|x| (x.epoch, x.start_off) == (r.epoch, r.start_off))
        {
            Some(x) => *x = r,
            None => all.push(r),
        }
    }
}

/// P2 §8.1 — StreamPos contract + the money assertion. A GUI anchors blocks
/// by computing `StreamPos.off + Output-byte offset of each hook`; that
/// arithmetic must reproduce the daemon's record keys BIT-FOR-BIT, because
/// `anchor.start_off == rec.start_off` equality is the entire basis of
/// anchor↔record joins. Also asserts frame ordering on attach (Replay →
/// StreamPos → no Output before it) and on a restore-resync (Reset → Replay
/// → StreamPos, plus the §2.2 full Blocks sync).
fn case_blocks_stream_pos() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_blocks_sp__")?;
    c.send(&C2D::Attach {
        id,
        cols: 120,
        rows: 30,
    })?;

    // Attach ordering: Replay, then StreamPos, before any Output for `id`.
    let mut saw_replay = false;
    let mut off: Option<u64> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && off.is_none() {
        match c.recv() {
            Ok(D2C::Replay { id: rid, .. }) if rid == id => saw_replay = true,
            Ok(D2C::StreamPos { id: rid, off: o }) if rid == id => {
                anyhow::ensure!(saw_replay, "StreamPos arrived before the Replay");
                off = Some(o);
            }
            Ok(D2C::Output { id: rid, .. }) if rid == id => {
                anyhow::bail!("live Output arrived before StreamPos");
            }
            _ => {}
        }
    }
    let off = off.ok_or_else(|| anyhow::anyhow!("no StreamPos after the Replay"))?;

    // Count every live Output byte from the base, GUI-style, and scan the
    // same bytes with the shared BlockScanner (chunked at 7 bytes to
    // exercise carry across chunk boundaries).
    c.send(&C2D::Input {
        id,
        bytes: b"echo POSMARK_1\r".to_vec(),
    })?;
    let mut buf: Vec<u8> = Vec::new();
    let mut rec: Option<BlockRec> = None;
    let mut list: Vec<BlockRec> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && rec.is_none() {
        match c.recv() {
            Ok(D2C::Output { id: rid, bytes }) if rid == id => {
                buf.extend_from_slice(&bytes);
            }
            Ok(D2C::Blocks {
                id: rid,
                full,
                recs,
                ..
            }) if rid == id => {
                if full {
                    list = recs;
                } else {
                    merge_blocks(&mut list, recs);
                }
                rec = list
                    .iter()
                    .find(|r| r.cmd.contains("echo POSMARK_1"))
                    .cloned();
            }
            _ => {}
        }
    }
    let rec = rec.ok_or_else(|| anyhow::anyhow!("no block record for POSMARK_1"))?;

    let mut scanner = crate::daemon::blocks::BlockScanner::new();
    let mut abs: Option<u64> = None;
    let mut base = 0usize;
    for chunk in buf.chunks(7) {
        for ev in scanner.feed(chunk) {
            if let crate::daemon::blocks::HookVerb::Exec { cmd } = &ev.verb {
                if cmd.contains("echo POSMARK_1") {
                    abs = Some(off + (base + ev.offset_in_chunk) as u64);
                }
            }
        }
        base += chunk.len();
    }
    let abs = abs.ok_or_else(|| {
        anyhow::anyhow!("exec hook for POSMARK_1 never appeared in the Output stream")
    })?;
    // THE money assertion: never loosen — fix the math instead.
    anyhow::ensure!(
        rec.start_off == abs,
        "GUI-computed start_off {} != daemon record key {} (StreamPos base {})",
        abs,
        rec.start_off,
        off
    );

    // Restore-resync ordering: Reset → Replay → StreamPos before new Output,
    // plus the post-resync full Blocks sync (§2.2 — a reconnected GUI must
    // never keep a stale open record).
    c.send(&C2D::KillTerminal { id })?;
    c.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Dead)
    })?;
    c.send(&C2D::RestartTerminal { id })?;
    let (mut saw_reset, mut saw_replay2, mut saw_sp2, mut saw_full) =
        (false, false, false, false);
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !(saw_sp2 && saw_full) {
        match c.recv() {
            Ok(D2C::Reset { id: rid }) if rid == id => saw_reset = true,
            Ok(D2C::Replay { id: rid, .. }) if rid == id && saw_reset => saw_replay2 = true,
            Ok(D2C::StreamPos { id: rid, .. }) if rid == id && saw_reset => {
                anyhow::ensure!(saw_replay2, "resync StreamPos before its Replay");
                saw_sp2 = true;
            }
            Ok(D2C::Output { id: rid, .. }) if rid == id && saw_reset && !saw_sp2 => {
                anyhow::bail!("resync Output arrived before StreamPos");
            }
            Ok(D2C::Blocks {
                id: rid,
                full: true,
                ..
            }) if rid == id && saw_reset => saw_full = true,
            _ => {}
        }
    }
    anyhow::ensure!(saw_reset, "no Reset on restore-resync");
    anyhow::ensure!(saw_sp2, "no StreamPos after the resync Replay");
    anyhow::ensure!(saw_full, "no full Blocks sync after the resync (§2.2)");
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// P2 §8.2 — BlockText round trip: the reply is clean clipboard text (no
/// escapes, no BELs, no hook OSCs), covers exactly the block's output (no
/// command echo, no next prompt), and works partially for an OPEN block.
fn case_blocks_text() -> anyhow::Result<()> {
    use egui::{Key, Modifiers};
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_blocks_tx__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;

    c.send(&C2D::Input {
        id,
        bytes: b"echo BLK_OUT_alpha; echo BLK_OUT_beta\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("BLK_OUT_alpha") && r.end_off.is_some())
    })?;
    let rec = recs
        .iter()
        .find(|r| r.cmd.contains("BLK_OUT_alpha"))
        .unwrap();
    c.send(&C2D::BlockText {
        id,
        start_off: rec.start_off,
    })?;
    let (text, truncated) = c.await_block_text(id, rec.start_off, 10)?;
    anyhow::ensure!(
        text.contains("BLK_OUT_alpha") && text.contains("BLK_OUT_beta"),
        "block text missing output lines: {text:?}"
    );
    anyhow::ensure!(
        !text.contains('\u{1b}') && !text.contains('\u{7}'),
        "escapes/BEL leaked into block text"
    );
    anyhow::ensure!(!text.contains("7717"), "hook OSC leaked into block text");
    let last = text.rfind("BLK_OUT_beta").unwrap();
    anyhow::ensure!(
        !text[last..].contains("PS "),
        "next prompt leaked into block text (end_off too late): {:?}",
        &text[last..]
    );
    anyhow::ensure!(!truncated, "small block wrongly flagged truncated");

    // Open-block variant: partial output up to the journal head.
    c.send(&C2D::Input {
        id,
        bytes: b"ping -t 127.0.0.1\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("ping -t") && r.end_off.is_none())
    })?;
    let open_off = recs
        .iter()
        .find(|r| r.cmd.contains("ping -t"))
        .unwrap()
        .start_off;
    c.await_output(id, 20, |l| l.contains("Reply from 127.0.0.1"))?;
    c.send(&C2D::BlockText {
        id,
        start_off: open_off,
    })?;
    let (text, _) = c.await_block_text(id, open_off, 10)?;
    anyhow::ensure!(
        text.contains("Reply from"),
        "open-block text missing partial output: {text:?}"
    );
    let cc = crate::win32_input::encode_key(Key::C, Modifiers::CTRL).expect("ctrl+c encodes");
    c.send(&C2D::Input { id, bytes: cc })?;
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// P2 §8.3 — the Re-run gate's record leg, evaluated exactly as the GUI
/// does (recs non-empty && all end_off.is_some()), through a real PSReadLine:
/// TRUE at an idle prompt; an injected re-run (`cmd\r` as Input — the same
/// bytes rerun_block sends) is accepted and re-captured by the hooks; FALSE
/// while a block is open; TRUE again after a win32 Ctrl+C closes it. (The
/// alt-screen leg is GUI-side TermMode, covered by unit tests.)
fn case_blocks_rerun_gate() -> anyhow::Result<()> {
    use egui::{Key, Modifiers};
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_blocks_rg__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    let gate = |all: &[BlockRec]| !all.is_empty() && all.iter().all(|r| r.end_off.is_some());
    let mut all: Vec<BlockRec> = Vec::new();

    c.send(&C2D::Input {
        id,
        bytes: b"echo RERUN_TAG\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("echo RERUN_TAG") && r.end_off.is_some())
    })?;
    merge_blocks(&mut all, recs);
    let first_off = all
        .iter()
        .find(|r| r.cmd.contains("echo RERUN_TAG"))
        .unwrap()
        .start_off;
    anyhow::ensure!(gate(&all), "gate must be TRUE at an idle prompt");

    // Inject the re-run exactly as the GUI would.
    c.send(&C2D::Input {
        id,
        bytes: b"echo RERUN_TAG\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 20, move |recs| {
        recs.iter().any(|r| {
            r.cmd.contains("echo RERUN_TAG")
                && r.start_off > first_off
                && r.end_off.is_some()
                && r.exit == Some(0)
        })
    })?;
    merge_blocks(&mut all, recs);
    anyhow::ensure!(
        gate(&all),
        "gate must be TRUE again after the re-run completed"
    );

    // Open block ⇒ gate FALSE (this is also the "TUI at the prompt" case).
    c.send(&C2D::Input {
        id,
        bytes: b"ping -t 127.0.0.1\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("ping -t") && r.end_off.is_none())
    })?;
    merge_blocks(&mut all, recs);
    anyhow::ensure!(!gate(&all), "gate must be FALSE while a block is open");

    // win32 Ctrl+C interrupts; the closing pre hook re-arms the gate.
    let cc = crate::win32_input::encode_key(Key::C, Modifiers::CTRL).expect("ctrl+c encodes");
    c.send(&C2D::Input { id, bytes: cc })?;
    let recs = c.await_blocks(id, 25, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("ping -t") && r.end_off.is_some())
    })?;
    merge_blocks(&mut all, recs);
    anyhow::ensure!(gate(&all), "gate must be TRUE again after Ctrl+C");
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// P2 §8.4 — the zero-cost hookless gate: a Custom cmd.exe terminal (no
/// bootstrap, no hooks) must never produce a hooked Blocks frame (epoch stays
/// 0, recs stay empty), never mint a record for a command, and never write a
/// blocks sidecar. epoch==0 is exactly what keeps the GUI's scanner off.
fn case_blocks_hookless_silent() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    c.send(&C2D::CreateTerminal {
        spec: NewTerminal {
            name: "__probe_blocks_hl__".into(),
            folder: None,
            kind: TermKind::Custom,
            program: "cmd.exe".into(),
            args: vec!["/q".into()],
            cwd: "C:\\".into(),
            already_launched: false,
            shell_cfg: None,
        },
    })?;
    let state = c.snapshot_until(10, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == "__probe_blocks_hl__" && t.status == TermStatus::Running)
    })?;
    let id = state
        .terminals
        .iter()
        .find(|t| t.name == "__probe_blocks_hl__")
        .unwrap()
        .id;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;

    // Bounded drain: any attach-time Blocks frame must be inert (epoch 0,
    // empty), and `echo hi` must not produce an incremental one.
    let check = |c: &mut Conn, secs: u64| -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(D2C::Blocks {
                id: rid,
                epoch,
                recs,
                ..
            }) = c.recv()
            {
                if rid == id {
                    anyhow::ensure!(
                        epoch == 0 && recs.is_empty(),
                        "hookless terminal produced a hooked Blocks frame \
                         (epoch {epoch}, {} recs)",
                        recs.len()
                    );
                }
            }
        }
        Ok(())
    };
    check(&mut c, 2)?;
    c.send(&C2D::Input {
        id,
        bytes: b"echo hi\r".to_vec(),
    })?;
    check(&mut c, 2)?;
    let side = crate::state::journals_dir().join(format!("{id}.blocks.json"));
    anyhow::ensure!(
        !side.exists(),
        "hookless terminal wrote a blocks sidecar: {side:?}"
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// P3 §10.1 — the clear chord + submission through a real PSReadLine. Stray
/// text is typed (no Enter); the GUI's manual-activation byte sequence — a
/// win32-encoded Ctrl+C (CancelLine) and the submission text + `\r`, in ONE
/// Input frame — must cancel the stray text and run exactly the submitted
/// command (conhost's input queue is strictly ordered; PSReadLine processes
/// sequentially). The empty-submit leg proves a bare `\r` is block-silent.
fn case_composer_submit() -> anyhow::Result<()> {
    use egui::{Key, Modifiers};
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_comp_sub__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    c.await_output(id, 20, |l| l.trim_start().starts_with("PS "))?;

    // Stray text WITHOUT enter, awaited via its echo.
    c.send(&C2D::Input {
        id,
        bytes: b"JUNKJUNK".to_vec(),
    })?;
    c.await_output(id, 10, |l| l.contains("JUNKJUNK"))?;

    // The manual-activation sequence exactly as the GUI ships it: clear
    // chord + submission bytes (PS 5.1 never sets DECSET 2004 ⇒ plain text)
    // sharing one Input frame.
    let mut bytes =
        crate::win32_input::encode_key(Key::C, Modifiers::CTRL).expect("ctrl+c encodes");
    bytes.extend_from_slice(b"echo COMPOSED_OK\r");
    c.send(&C2D::Input { id, bytes })?;

    // The money assertion: the submission is recorded byte-exact with exit 0
    // and NO record carries the cancelled stray text.
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter().any(|r| {
            r.cmd == "echo COMPOSED_OK" && r.end_off.is_some() && r.exit == Some(0)
        })
    })?;
    anyhow::ensure!(
        !recs.iter().any(|r| r.cmd.contains("JUNK")),
        "cancelled stray text minted a record: {:?}",
        recs.iter().map(|r| &r.cmd).collect::<Vec<_>>()
    );

    // Empty-submit leg: a bare \r refreshes the prompt without a record. The
    // follow-up marker bounds the wait; every incremental record seen while
    // waiting must be the marker's own (nothing for the blank line).
    c.send(&C2D::Input {
        id,
        bytes: b"\r".to_vec(),
    })?;
    c.send(&C2D::Input {
        id,
        bytes: b"echo AFTER_EMPTY\r".to_vec(),
    })?;
    let recs2 = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd == "echo AFTER_EMPTY" && r.end_off.is_some())
    })?;
    anyhow::ensure!(
        recs2.iter().all(|r| r.cmd == "echo AFTER_EMPTY"),
        "the empty submit minted a record: {:?}",
        recs2.iter().map(|r| &r.cmd).collect::<Vec<_>>()
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// P3 §10.2 — PS 5.1 multi-line paste semantics: PSReadLine 2.0 accepts one
/// line per `\r`, so a sanitized two-line submission in ONE Input frame
/// yields TWO sequential blocks, both exit 0, in stream order.
fn case_composer_multiline() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_comp_ml__")?;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    c.await_output(id, 20, |l| l.trim_start().starts_with("PS "))?;

    c.send(&C2D::Input {
        id,
        bytes: b"echo ML_A\recho ML_B\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 30, |recs| {
        ["echo ML_A", "echo ML_B"].iter().all(|m| {
            recs.iter()
                .any(|r| r.cmd == *m && r.end_off.is_some() && r.exit == Some(0))
        })
    })?;
    let a = recs.iter().find(|r| r.cmd == "echo ML_A").unwrap();
    let b = recs.iter().find(|r| r.cmd == "echo ML_B").unwrap();
    anyhow::ensure!(
        a.start_off < b.start_off,
        "accept-per-line ordering violated: ML_A at {} vs ML_B at {}",
        a.start_off,
        b.start_off
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// Byte-subslice search (for stream-order assertions on raw capture).
fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Scan a captured stream with a fresh BlockScanner, chunked at `chunk`
/// bytes, returning (verb, buffer_offset) per event.
fn scan_events(buf: &[u8], chunk: usize) -> Vec<(crate::daemon::blocks::HookVerb, usize)> {
    let mut sc = crate::daemon::blocks::BlockScanner::new();
    let mut out = Vec::new();
    let mut base = 0usize;
    for c in buf.chunks(chunk.max(1)) {
        for ev in sc.feed(c) {
            out.push((ev.verb, base + ev.offset_in_chunk));
        }
        base += c.len();
    }
    out
}

/// P3 §10.3 — the composer state machine + gate against REAL session bytes.
/// Captures the whole live Output stream (from the attach StreamPos on) plus
/// the record mirror, replays it offline through the shared BlockScanner
/// (chunked at 7 — chunk-boundary carry exercised) into a `ComposerState`,
/// and asserts the verdict walk: NoPrompt → AutoArm at the prompt →
/// Busy the moment `exec` is scanned (and BEFORE the app's first output
/// byte — the claude-safety property) → AutoArm again after the closing
/// `pre`. The PromptEnd leg asserts every `pre` is followed by a `133;B`
/// that lands AFTER the rendered prompt text (the ordering the bootstrap's
/// ReadLine-side emission + flush-sleep exists to guarantee), and that the
/// daemon logged no token warnings for the inert verb.
/// Cold-attach prompt certification (task #15): a hooked shell at an idle
/// prompt, then a FRESH client attaching at a DIFFERENT size, must receive a
/// `PromptState{at_prompt:true, clean:true}` AFTER Replay/StreamPos/Blocks,
/// and its (line,col) must be exactly where the reconstructed replay leaves
/// the cursor — so the GUI seeds prompt_end on the right cell and arms. A
/// prompt carrying stray typed bytes must report `clean:false`.
fn case_cold_attach() -> anyhow::Result<()> {
    use alacritty_terminal::event::{Event as AlacEvent, EventListener};
    use alacritty_terminal::term::{test::TermSize, Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    struct Void;
    impl EventListener for Void {
        fn send_event(&self, _: AlacEvent) {}
    }

    // Reconstruct the replay into a fresh Term of the attach size and read the
    // cursor cell — exactly what the GUI's grid shows, so the certified
    // prompt-end cell can be checked against reality (the §8.1
    // offset-consistency ethos, applied to the prompt-end cell).
    fn replay_cursor(bytes: &[u8], cols: u16, rows: u16) -> (i32, u32) {
        let mut term = Term::new(
            Config::default(),
            &TermSize::new(cols as usize, rows as usize),
            Void,
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        let p = term.grid().cursor.point;
        (p.line.0, p.column.0 as u32)
    }

    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_cold__")?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    c.send(&C2D::Input {
        id,
        bytes: b"echo COLD_READY\r".to_vec(),
    })?;
    c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("echo COLD_READY") && r.end_off.is_some())
    })?;
    // Let the post-command prompt render and its 133;B be scanned.
    std::thread::sleep(Duration::from_millis(600));

    // A fresh client at a DIFFERENT size: collect the attach sequence and the
    // trailing PromptState, asserting the ordering.
    let cold_attach =
        |cols: u16, rows: u16| -> anyhow::Result<(bool, i32, u32, bool, Vec<u8>)> {
            let mut c2 = Conn::open()?;
            let _ = c2.first_snapshot()?;
            c2.send(&C2D::Attach { id, cols, rows })?;
            let (mut replay, mut saw_sp, mut saw_blocks) = (None, false, false);
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                anyhow::ensure!(Instant::now() < deadline, "no PromptState within 15s");
                match c2.recv() {
                    Ok(D2C::Replay { id: rid, bytes }) if rid == id => replay = Some(bytes),
                    Ok(D2C::StreamPos { id: rid, .. }) if rid == id => saw_sp = true,
                    Ok(D2C::Blocks { id: rid, full: true, .. }) if rid == id => saw_blocks = true,
                    Ok(D2C::PromptState {
                        id: rid,
                        at_prompt,
                        line,
                        col,
                        clean,
                    }) if rid == id => {
                        let replay = replay
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("PromptState before Replay"))?;
                        anyhow::ensure!(saw_sp, "PromptState arrived before StreamPos");
                        anyhow::ensure!(saw_blocks, "PromptState arrived before Blocks");
                        return Ok((at_prompt, line, col, clean, replay));
                    }
                    _ => {}
                }
            }
        };

    // Idle: at_prompt + clean, and the cell matches the reconstruction.
    let (at_prompt, line, col, clean, replay) = cold_attach(100, 28)?;
    anyhow::ensure!(at_prompt, "idle hooked prompt must report at_prompt");
    anyhow::ensure!(clean, "an untouched prompt must report clean");
    let (cl, cc) = replay_cursor(&replay, 100, 28);
    anyhow::ensure!(
        cl == line && cc == col,
        "PromptState cell ({line},{col}) != reconstructed replay cursor ({cl},{cc})"
    );

    // Dirty: stray typed bytes (no Enter) ⇒ clean:false at the next attach.
    c.send(&C2D::Input {
        id,
        bytes: b"xy".to_vec(),
    })?;
    std::thread::sleep(Duration::from_millis(400));
    let (_at2, _l2, _c2, clean2, _r2) = cold_attach(90, 24)?;
    anyhow::ensure!(
        !clean2,
        "a prompt holding stray typed input must report clean:false"
    );
    // Tidy the stray input so the shell isn't left mid-line for the sweep.
    c.send(&C2D::Input { id, bytes: vec![0x03] })?;

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

// ───────────────────── history parity (proto 7) ─────────────────────

/// One full fresh-client attach: the ordered sequence Replay → StreamPos →
/// Blocks(full) → [PromptState] → ReplayAnchors, collected with ordering
/// asserted. `hints` is empty only if the deadline passes with no
/// ReplayAnchors frame — the caller decides whether that is a failure.
struct AttachView {
    replay: Vec<u8>,
    recs: Vec<BlockRec>,
    hints: Vec<AnchorHint>,
}

fn attach_view(id: Uuid, cols: u16, rows: u16, secs: u64) -> anyhow::Result<AttachView> {
    attach_view_inner(id, cols, rows, secs, true)
}

/// attach_view for attaches that may legitimately send NO ReplayAnchors —
/// the frame-overlay path skips hints by design (sleep-spec §17.3). Once
/// Replay/StreamPos/Blocks are in, a ~2s grace window catches a straggler
/// anchors frame (returned so the caller can assert about it); the window
/// closing empty returns hints=[].
fn attach_view_tolerant(id: Uuid, cols: u16, rows: u16, secs: u64) -> anyhow::Result<AttachView> {
    attach_view_inner(id, cols, rows, secs, false)
}

fn attach_view_inner(
    id: Uuid,
    cols: u16,
    rows: u16,
    secs: u64,
    require_hints: bool,
) -> anyhow::Result<AttachView> {
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    c.send(&C2D::Attach { id, cols, rows })?;
    let (mut replay, mut saw_sp, mut recs) = (None, false, None);
    let deadline = Instant::now() + Duration::from_secs(secs);
    // Anchors-straggler grace (tolerant mode): armed when the rest is in.
    let mut grace: Option<Instant> = None;
    loop {
        if !require_hints {
            if grace.is_none() && replay.is_some() && saw_sp && recs.is_some() {
                grace = Some(Instant::now() + Duration::from_secs(2));
            }
            if grace.is_some_and(|g| Instant::now() >= g) {
                return Ok(AttachView {
                    replay: replay.expect("checked above"),
                    recs: recs.expect("checked above"),
                    hints: Vec::new(),
                });
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "no ReplayAnchors within {secs}s (replay={} streampos={saw_sp} blocks={})",
            replay.is_some(),
            recs.is_some()
        );
        match c.recv() {
            Ok(D2C::Replay { id: rid, bytes }) if rid == id => replay = Some(bytes),
            Ok(D2C::StreamPos { id: rid, .. }) if rid == id => saw_sp = true,
            Ok(D2C::Blocks {
                id: rid,
                full: true,
                recs: r,
                ..
            }) if rid == id => recs = Some(r),
            Ok(D2C::ReplayAnchors { id: rid, items }) if rid == id => {
                let replay = replay
                    .ok_or_else(|| anyhow::anyhow!("ReplayAnchors before Replay"))?;
                anyhow::ensure!(saw_sp, "ReplayAnchors arrived before StreamPos");
                let recs =
                    recs.ok_or_else(|| anyhow::anyhow!("ReplayAnchors before Blocks full"))?;
                return Ok(AttachView {
                    replay,
                    recs,
                    hints: items,
                });
            }
            _ => {}
        }
    }
}

/// The §8.1 bit-exact bar: EVERY closed record has a block hint, and the
/// replay reconstruction's row at that hint renders the record's command
/// starting exactly at the hinted column. Returns the reconstruction rows
/// (trimmed) for extra caller assertions.
fn verify_history_parity(v: &AttachView, cols: u16, rows: u16) -> anyhow::Result<Vec<String>> {
    use alacritty_terminal::event::{Event as AlacEvent, EventListener};
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::term::{test::TermSize, Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    struct Void;
    impl EventListener for Void {
        fn send_event(&self, _: AlacEvent) {}
    }
    let mut term = Term::new(
        Config {
            scrolling_history: 10_000,
            ..Config::default()
        },
        &TermSize::new(cols as usize, rows as usize),
        Void,
    );
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, &v.replay);

    let row_text = |line: i32| -> String {
        let grid = term.grid();
        let hist = grid.history_size() as i32;
        if line < -hist || line >= grid.screen_lines() as i32 {
            return String::new();
        }
        let row = &grid[Line(line)];
        let mut s = String::new();
        for c in 0..grid.columns() {
            let cell = &row[Column(c)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            s.push(if cell.c == '\0' { ' ' } else { cell.c });
        }
        s.trim_end().to_string()
    };

    for rec in v.recs.iter().filter(|r| r.end_off.is_some()) {
        let hint = v
            .hints
            .iter()
            .find(|h| h.kind == ANCHOR_BLOCK && h.start_off == rec.start_off)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "record {:?} (start_off {}) has NO anchor hint; hints: {:?}",
                    rec.cmd,
                    rec.start_off,
                    v.hints
                )
            })?;
        let text = row_text(hint.row);
        let first = rec.cmd.lines().next().unwrap_or("");
        let from_col: String = {
            // The hinted column counts grid cells; the trimmed text above
            // has spacers removed, so re-read the raw row cells from col.
            let grid = term.grid();
            let row = &grid[Line(hint.row)];
            let mut s = String::new();
            for c in (hint.col as usize)..grid.columns() {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                s.push(if cell.c == '\0' { ' ' } else { cell.c });
            }
            s.trim_end().to_string()
        };
        anyhow::ensure!(
            !from_col.is_empty()
                && (from_col.starts_with(first) || first.starts_with(&from_col)),
            "hint row {} does not render {:?} at col {}: row {:?} (from col: {:?})",
            hint.row,
            rec.cmd,
            hint.col,
            text,
            from_col
        );
    }
    // Reconstruction rows for caller-side extra checks (dedupe counts …).
    let grid = term.grid();
    let hist = grid.history_size() as i32;
    let all: Vec<String> = (-hist..grid.screen_lines() as i32).map(row_text).collect();
    Ok(all)
}

/// THE field bar (2026-07-04, "for cmd yes for powershell no"): every
/// bare-prompt row in the reconstruction EXCEPT the final (live) one must
/// carry a hint — un-hinted bare prompt rows are exactly what a reopened GUI
/// renders raw, and PS journals full of Enter-spam runs and conhost repaint
/// doubles used to leave whole runs uncovered. The final prompt row must
/// carry NO hint (blanking the live prompt is worse).
fn verify_bare_prompt_coverage(
    v: &AttachView,
    rows: &[String],
    grid_rows: u16,
) -> anyhow::Result<()> {
    let hist = rows.len() as i32 - grid_rows as i32;
    let hinted: std::collections::HashSet<i32> = v.hints.iter().map(|h| h.row).collect();
    let bare_idx: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_end();
            t.starts_with("PS ") && t.ends_with('>')
        })
        .map(|(i, _)| i)
        .collect();
    let Some((&last, above)) = bare_idx.split_last() else {
        return Ok(());
    };
    let last_row = last as i32 - hist;
    anyhow::ensure!(
        !hinted.contains(&last_row),
        "the live prompt row {last_row} must never be hinted: {:?}",
        v.hints
    );
    for &i in above {
        let row = i as i32 - hist;
        anyhow::ensure!(
            hinted.contains(&row),
            "bare prompt row {row} ({:?}) has no hint — a reopened GUI \
             renders it raw; hints: {:?}",
            rows[i],
            v.hints
        );
    }
    Ok(())
}

/// THE user's acceptance bar, machine-checked (must-have): styled commands →
/// close → reopen keeps every block's prompt row addressable. A fresh client
/// attach must deliver ReplayAnchors covering EVERY record, rows verified
/// against the replay reconstruction — live, across a daemon restart
/// (sidecar path + the dangling-prompt dedupe), and for the cmd family
/// (exit:None synthetic records).
fn case_history_parity() -> anyhow::Result<()> {
    ensure_isolated_daemon("history_parity")?;
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let log0 = daemon_log_len();
    let master = master_token()?;

    // ── phase A: live attach parity (pwsh) ─────────────────────────────
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_hparity__")?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4700u64;
    await_hooked_prompt(&mut ctl, &mut rid, id, 30)?;

    c.send(&C2D::Input { id, bytes: b"echo HP_ONE\r".to_vec() })?;
    c.await_blocks(id, 20, |recs| {
        recs.iter().any(|r| r.cmd == "echo HP_ONE" && r.end_off.is_some())
    })?;
    std::thread::sleep(Duration::from_millis(500));
    // Empty Enter at the prompt: the superseded bare prompt is the spacer.
    c.send(&C2D::Input { id, bytes: b"\r".to_vec() })?;
    std::thread::sleep(Duration::from_millis(500));
    c.send(&C2D::Input { id, bytes: b"echo HP_TWO\r".to_vec() })?;
    c.await_blocks(id, 20, |recs| {
        recs.iter().any(|r| r.cmd == "echo HP_TWO" && r.end_off.is_some())
    })?;
    std::thread::sleep(Duration::from_millis(600));
    // FIELD SHAPE (2026-07-04): an Enter-SPAM run — several consecutive
    // empty Enters — then one more command. The old greedy hint matcher
    // wholesale-missed such runs (dozens of IDENTICAL bare rows let early
    // checkpoints steal later rows); reopened GUIs rendered them as raw
    // bare `PS C:\>` rows while cmd sessions looked fine.
    for _ in 0..4 {
        c.send(&C2D::Input { id, bytes: b"\r".to_vec() })?;
        std::thread::sleep(Duration::from_millis(300));
    }
    c.send(&C2D::Input { id, bytes: b"echo HP_THREE\r".to_vec() })?;
    c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd == "echo HP_THREE" && r.end_off.is_some())
    })?;
    std::thread::sleep(Duration::from_millis(600));

    let v = attach_view(id, 100, 28, 20)?;
    anyhow::ensure!(
        v.recs.iter().filter(|r| r.end_off.is_some()).count() >= 3,
        "all three commands recorded"
    );
    let rows_a = verify_history_parity(&v, 100, 28)?;
    verify_bare_prompt_coverage(&v, &rows_a, 28)?;
    anyhow::ensure!(
        v.hints.iter().any(|h| h.kind == ANCHOR_SPACER),
        "the empty-Enter bare prompt must yield a spacer hint: {:?}",
        v.hints
    );
    // Spacer rows are bare prompts (nothing right of the prompt text).
    for h in v.hints.iter().filter(|h| h.kind == ANCHOR_SPACER) {
        let hist = rows_a.len() as i32 - 28;
        let idx = (h.row + hist) as usize;
        let text = rows_a.get(idx).cloned().unwrap_or_default();
        anyhow::ensure!(
            !text.is_empty() && text.chars().count() <= h.col as usize,
            "spacer row must be a bare prompt: {text:?} (col {})",
            h.col
        );
    }

    // ── phase B: daemon restart — the sidecar path + dedupe ───────────
    let old_info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
    c.send(&C2D::Shutdown)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if c.recv().is_err() {
            break;
        }
    }
    let lock = crate::state::data_dir().join("daemon.lock");
    for _ in 0..50 {
        if !lock.exists() || std::fs::remove_file(&lock).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::process::Command::new(std::env::current_exe()?)
        .arg("--daemon")
        .creation_flags(DETACHED_PROCESS)
        .spawn()?;
    let mut c2 = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            anyhow::ensure!(Instant::now() < deadline, "restarted daemon never came up");
            std::thread::sleep(Duration::from_millis(200));
            let fresh = std::fs::read(daemon_info_path())
                .ok()
                .and_then(|b| serde_json::from_slice::<DaemonInfo>(&b).ok())
                .is_some_and(|i| i.pid != old_info.pid);
            if !fresh {
                continue;
            }
            if let Ok(conn) = Conn::open() {
                break conn;
            }
        }
    };
    c2.snapshot_until(20, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
    })?;
    let master2 = master_token()?;
    let mut ctl2 = Conn::open_ctl(&master2, None)?;
    let mut rid2 = 4750u64;
    await_hooked_prompt(&mut ctl2, &mut rid2, id, 60)?;
    std::thread::sleep(Duration::from_millis(600));

    let vb = attach_view(id, 120, 30, 20)?;
    anyhow::ensure!(
        vb.recs.iter().any(|r| r.cmd == "echo HP_ONE")
            && vb.recs.iter().any(|r| r.cmd == "echo HP_TWO")
            && vb.recs.iter().any(|r| r.cmd == "echo HP_THREE"),
        "records survive the restart (sidecar)"
    );
    let rows_b = verify_history_parity(&vb, 120, 30)?;
    // The dangling-prompt dedupe (restored-render-fix bug, fixed in the
    // parity pass): the dead session's final bare prompt must NOT stack —
    // exactly SIX bare-prompt rows remain (the phase-A empty-Enter spacer +
    // the 4 spam rows + the live one).
    let bare = rows_b
        .iter()
        .filter(|l| {
            let t = l.trim_end();
            t.starts_with("PS ") && t.ends_with('>')
        })
        .count();
    anyhow::ensure!(
        bare == 6,
        "dangling-prompt dedupe: expected exactly 6 bare prompts \
         (spacer + 4 spam + live), got {bare}: {:?}",
        rows_b
            .iter()
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
    );
    // And every one of them except the live prompt is hint-covered — the
    // reopened GUI blanks them exactly like the live session did.
    verify_bare_prompt_coverage(&vb, &rows_b, 30)?;

    // ── phase C: cmd family (exit:None synthetic records) ─────────────
    let cmd_id = create_cmd_terminal(&mut c2, "__probe_hparity_cmd__", "C:\\")?;
    c2.send(&C2D::Attach { id: cmd_id, cols: 120, rows: 30 })?;
    await_hooked_prompt(&mut ctl2, &mut rid2, cmd_id, 30)?;
    c2.send(&C2D::SubmitCommand {
        id: cmd_id,
        cmd: "echo HP_CMD& ping -n 1 127.0.0.1>nul".into(),
        write: true,
    })?;
    c2.await_blocks(cmd_id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("HP_CMD") && r.end_off.is_some())
    })?;
    std::thread::sleep(Duration::from_millis(600));
    let vc = attach_view(cmd_id, 110, 26, 20)?;
    let rec = vc
        .recs
        .iter()
        .find(|r| r.cmd.contains("HP_CMD"))
        .ok_or_else(|| anyhow::anyhow!("cmd record missing"))?;
    anyhow::ensure!(rec.exit.is_none(), "cmd exit stays None (D7)");
    verify_history_parity(&vc, 110, 26)?;

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c2, cmd_id);
    delete_terminal(&mut c2, id);
    Ok(())
}

/// The same §8.1 bar for a WSL bash terminal: the record carries a POSIX cwd
/// and the hint row renders the bash prompt + command. SKIPs without a
/// distro (P6 §12 discipline).
fn case_history_parity_wsl() -> anyhow::Result<()> {
    let Some(distro) = wsl_probe_distro() else {
        return Err(skip("no WSL distro in the Lxss registry".into()));
    };
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_wsl_terminal(&mut c, "__probe_hparity_wsl__", &distro)?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4800u64;
    await_hooked_prompt(&mut ctl, &mut rid, id, 90)?;

    let body = ctl_run_retry(
        &mut ctl,
        &mut rid,
        id,
        "echo HP_WSL_1",
        Some(RunWait { timeout_ms: 30_000, tail_bytes: 4096 }),
        60,
    )?;
    anyhow::ensure!(
        matches!(&body, CtlBody::RunDone { exit: Some(0), .. }),
        "wsl run must complete: {body:?}"
    );
    std::thread::sleep(Duration::from_millis(600));

    let v = attach_view(id, 110, 28, 20)?;
    let rec = v
        .recs
        .iter()
        .find(|r| r.cmd == "echo HP_WSL_1")
        .ok_or_else(|| anyhow::anyhow!("wsl record missing"))?;
    let cwd = rec.cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
    anyhow::ensure!(
        cwd.as_deref().is_some_and(|p| p.starts_with('/')),
        "wsl record cwd must be POSIX: {cwd:?}"
    );
    verify_history_parity(&v, 110, 28)?;

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// The user's #1 requirement, locked forever: a daemon shutdown → restart →
/// restore must replay the LAST command's output COMPLETELY — including the
/// --install pattern that truncated `ls` output three times in the field:
/// conhost renders text on ASYNCHRONOUS frames, so a Shutdown arriving while
/// a real `ls` is mid-render used to exit with the table's tail rows still in
/// the ConPTY pipe (probe-proven: the last row died). The shutdown drain
/// (Core::drain_output_tail) must save every row. One restart cycle:
///   phase 1 (hot): real Get-ChildItem output; Shutdown fired the INSTANT the
///   FIRST row is visible (worst case — tail rows in flight); assert every
///   row is on disk after the process dies — separates write-side loss
///   (journal incomplete) from restore-side loss.
///   phase 2 (restore): respawn the daemon exactly as --install does, wait
///   for auto-restore, attach and assert the serialized replay carries every
///   row in order, seam-free.
fn case_restore_fidelity() -> anyhow::Result<()> {
    ensure_isolated_daemon("restore_fidelity")?;
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    // Fixture dir with known file names (real filesystem enumeration through
    // the real PowerShell formatter — echo loops arrive in one chunk and
    // never reproduced the field loss).
    let fix = std::env::temp_dir().join("tc_fid_fix");
    std::fs::create_dir_all(&fix)?;
    for i in 1..=8 {
        std::fs::write(fix.join(format!("FIDROW_{i}.txt")), b"x")?;
    }

    let old_info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_fidelity__")?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    // Hooked prompt live (blocks full-sync answered) before driving it.
    c.await_blocks(id, 10, |_| true)?;

    c.send(&C2D::Input {
        id,
        bytes: format!("Get-ChildItem '{}'\r", fix.display()).into_bytes(),
    })?;
    // Shutdown the INSTANT the first row lands — the remaining table rows are
    // still in conhost's next async frame / the ConPTY pipe.
    c.await_output(id, 20, |l| l.contains("FIDROW_1.txt"))?;

    // Linger like request_shutdown: the daemon must read the frame before the
    // socket drops, and its closing our end proves it ran the flush path.
    c.send(&C2D::Shutdown)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if c.recv().is_err() {
            break;
        }
    }
    // Wait for the process to fully exit (the install path's lock-wait).
    let lock = crate::state::data_dir().join("daemon.lock");
    for _ in 0..50 {
        if !lock.exists() || std::fs::remove_file(&lock).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Phase 1: the journal on disk, post-mortem, must hold every row.
    let jpath = crate::state::journals_dir().join(format!("{id}.log"));
    let jbytes = std::fs::read(&jpath)?;
    let jtext = strip_ansi(&String::from_utf8_lossy(&jbytes));
    for i in 1..=8 {
        let row = format!("FIDROW_{i}.txt");
        anyhow::ensure!(
            jtext.contains(&row),
            "WRITE-SIDE LOSS: {row} missing from the on-disk journal after shutdown \
             ({} bytes; tail: {:?})",
            jbytes.len(),
            &jtext[jtext.len().saturating_sub(400)..]
        );
    }

    // Phase 2: respawn the daemon exactly as --install does and wait for the
    // auto-restore to bring the terminal back.
    std::process::Command::new(std::env::current_exe()?)
        .arg("--daemon")
        .creation_flags(DETACHED_PROCESS)
        .spawn()?;
    let mut c2 = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            anyhow::ensure!(Instant::now() < deadline, "restarted daemon never came up");
            std::thread::sleep(Duration::from_millis(200));
            let fresh = std::fs::read(daemon_info_path())
                .ok()
                .and_then(|b| serde_json::from_slice::<DaemonInfo>(&b).ok())
                .is_some_and(|i| i.pid != old_info.pid);
            if !fresh {
                continue;
            }
            if let Ok(conn) = Conn::open() {
                break conn;
            }
        }
    };
    c2.snapshot_until(20, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
    })?;
    // Give the restored shell time to print its first prompt.
    std::thread::sleep(Duration::from_millis(1500));

    // The serialized replay must carry every row, in order, seam-free.
    let mut c3 = Conn::open()?;
    let _ = c3.first_snapshot()?;
    c3.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let replay = loop {
        match c3.recv() {
            Ok(D2C::Replay { id: rid, bytes }) if rid == id => break bytes,
            Ok(_) => {}
            Err(e) => anyhow::bail!("no replay after restore: {e}"),
        }
    };
    let text = strip_ansi(&String::from_utf8_lossy(&replay));
    let mut pos = 0usize;
    for i in 1..=8 {
        let row = format!("FIDROW_{i}.txt");
        match text[pos..].find(&row) {
            Some(p) => pos += p + row.len(),
            None => anyhow::bail!(
                "RESTORE-SIDE LOSS: {row} missing (or out of order) in the replay; \
                 journal had it. Replay tail: {:?}",
                text.lines().rev().take(24).collect::<Vec<_>>()
            ),
        }
    }
    anyhow::ensure!(
        !text.contains("tc:seam") && !text.contains("── restored"),
        "restore seam leaked visible text"
    );
    delete_terminal(&mut c3, id);
    Ok(())
}

/// THE 2026-07-09 WIDTH-MISMATCH REPLAY regression (the restored-claude
/// garble): a session live on the ALT screen with content recorded at
/// 175×49, after a full daemon restart, attached by a proto-12 client at
/// 147×49. Pins all three legs of the fix:
///   1. the attach replay is readable at the FOREIGN width — the frame row
///      addressed to row 6 carries only its own text (the raw-tail replay
///      this replaced fused it with the wrapped spill of the 170-col row
///      above it: the field screenshot, char-for-char);
///   2. ConPTY/mirror/state are resized to the ATTACHER's grid before the
///      replay is serialized (the attach contract, via DebugDump);
///   3. a restore-resync sends a proto-12 client Reset ONLY (no blind-size
///      Replay/StreamPos/Blocks push — the client re-attaches itself, GUI
///      style), while a LEGACY client on the same restart still gets the
///      full compat push.
fn case_width_mismatch_replay() -> anyhow::Result<()> {
    ensure_isolated_daemon("width_mismatch_replay")?;
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let log0 = daemon_log_len();

    // `await_output` also scans Replay frames, and after the restore the
    // replayed history contains the phase-1 sentinels (the alt-closure fix
    // flattens the killed frame into scrollback) — a draw-completion wait
    // must therefore watch LIVE Output only, or it returns before the draw
    // ever executed (observed: the phase-4 attach then races the redraw and
    // finds the mirror still on the primary screen).
    fn await_live(
        c: &mut Conn,
        id: Uuid,
        secs: u64,
        pred: impl Fn(&str) -> bool,
    ) -> anyhow::Result<()> {
        let mut stripper = AnsiStripper::default();
        let mut pending = String::new();
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            match c.recv() {
                Ok(D2C::Output { id: rid, bytes }) if rid == id => {
                    stripper.feed(&bytes, &mut pending);
                    if pending.lines().any(&pred) {
                        return Ok(());
                    }
                    if let Some(p) = pending.rfind('\n') {
                        pending.drain(..=p);
                    }
                }
                _ => {}
            }
        }
        anyhow::bail!("live output never matched within {secs}s (tail: {pending:?})")
    }

    // The alt-frame fixture, claude-shaped: a full-width (170-col) row
    // directly above an absolutely-addressed row — the fusion pair — plus a
    // completion sentinel. Markers are assembled by string concatenation so
    // the PSReadLine ECHO of the command never contains the joined text.
    const DRAW: &[u8] = b"$e=[char]27; [Console]::Out.Write(\"$e[?1049h$e[?25l$e[5;1H\" + ('A'*170) + \"$e[6;30HWM\" + \"ROW_SIX_TEXT\" + \"$e[7;1HDRAW\" + \"N_OK\")\r";

    // Phase 1: live session at 175×49, alt content recorded at that width.
    let old_info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_width_mismatch__")?;
    c.send(&C2D::Attach { id, cols: 175, rows: 49 })?;
    c.await_blocks(id, 10, |_| true)?;
    c.send(&C2D::Input { id, bytes: DRAW.to_vec() })?;
    await_live(&mut c, id, 20, |l| l.contains("DRAWN_OK"))?;

    // Phase 2: full daemon restart (the field event), restore_fidelity's
    // exact shutdown/respawn recipe.
    c.send(&C2D::Shutdown)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if c.recv().is_err() {
            break;
        }
    }
    let lock = crate::state::data_dir().join("daemon.lock");
    for _ in 0..50 {
        if !lock.exists() || std::fs::remove_file(&lock).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::process::Command::new(std::env::current_exe()?)
        .arg("--daemon")
        .creation_flags(DETACHED_PROCESS)
        .spawn()?;
    let mut c2 = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            anyhow::ensure!(Instant::now() < deadline, "restarted daemon never came up");
            std::thread::sleep(Duration::from_millis(200));
            let fresh = std::fs::read(daemon_info_path())
                .ok()
                .and_then(|b| serde_json::from_slice::<DaemonInfo>(&b).ok())
                .is_some_and(|i| i.pid != old_info.pid);
            if !fresh {
                continue;
            }
            if let Ok(conn) = Conn::open() {
                break conn;
            }
        }
    };
    c2.snapshot_until(20, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
    })?;
    std::thread::sleep(Duration::from_millis(1500));

    // Phase 3: the restored session re-enters the alt screen (the field's
    // `claude --resume` did exactly this). The respawned PTY inherited the
    // pre-kill 175×49, so the new frame is again recorded at 175. The
    // sentinel differs from phase 1's: the restore flattened the first
    // frame into primary scrollback, so the attach Replay already carries
    // "DRAWN_OK" and would satisfy the wait before the redraw ever ran.
    const DRAW3: &[u8] = b"$e=[char]27; [Console]::Out.Write(\"$e[?1049h$e[?25l$e[5;1H\" + ('A'*170) + \"$e[6;30HWM\" + \"ROW_SIX_TEXT\" + \"$e[7;1HDRAW\" + \"N2_OK\")\r";
    c2.send(&C2D::Attach { id, cols: 175, rows: 49 })?;
    c2.send(&C2D::Input { id, bytes: DRAW3.to_vec() })?;
    await_live(&mut c2, id, 30, |l| l.contains("DRAWN2_OK"))?;

    // Phase 4: a proto-12 client attaches at the FOREIGN width.
    let mut c3 = Conn::open_v2()?;
    let _ = c3.first_snapshot()?;
    c3.send(&C2D::Attach { id, cols: 147, rows: 49 })?;
    let mut replay: Option<Vec<u8>> = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        anyhow::ensure!(Instant::now() < deadline, "no attach Replay/StreamPos");
        match c3.recv() {
            Ok(D2C::Replay { id: rid, bytes }) if rid == id => replay = Some(bytes),
            Ok(D2C::StreamPos { id: rid, .. }) if rid == id => {
                anyhow::ensure!(replay.is_some(), "StreamPos before the Replay");
                break;
            }
            Ok(D2C::Output { id: rid, .. }) if rid == id => {
                anyhow::bail!("live Output before StreamPos on attach");
            }
            _ => {}
        }
    }
    let replay = replay.expect("checked above");

    // Leg 2: the attach contract — Term/PTY/state all at the attacher's
    // grid; the replay was serialized after this resize by lock order.
    let dump = debug_dump(&mut c3)?;
    let d = dump
        .iter()
        .find(|d| d.id == id)
        .ok_or_else(|| anyhow::anyhow!("terminal missing from DebugDump"))?;
    anyhow::ensure!(
        (d.term_cols, d.term_rows) == (147, 49)
            && (d.pty_cols, d.pty_rows) == (147, 49)
            && (d.state_cols, d.state_rows) == (147, 49),
        "attach must bring Term/PTY/state to the attacher's 147×49 first, got \
         term {}x{} pty {}x{} state {}x{}",
        d.term_cols, d.term_rows, d.pty_cols, d.pty_rows, d.state_cols, d.state_rows
    );

    // Leg 1: the replay, parsed at the client's own 147×49 (exactly the
    // GUI's parse), is width-honest. The replay must still land on the alt
    // grid (live-TUI semantics)…
    anyhow::ensure!(
        memchr::memmem::find(&replay, b"\x1b[?1049h").is_some(),
        "live-alt replay must enter the alternate screen"
    );
    // Exact row indices/lengths are conhost's business (it re-renders alt
    // frames with relative moves and reflows them on resize); the invariant
    // the field bug violated is READABILITY — the marker row and the wide
    // row never share cells (the raw-tail replay fused the wrapped spill of
    // the 170-col row into the marker row: `repro-attach-state.txt`,
    // char-for-char the field screenshot).
    let rows = parse_screen(&replay, 147, 49);
    anyhow::ensure!(rows.len() >= 49, "parse yielded {} rows", rows.len());
    let screen = &rows[rows.len() - 49..];
    anyhow::ensure!(
        screen
            .iter()
            .any(|r| r.len() >= 100 && r.chars().all(|ch| ch == 'A')),
        "the wide row must survive as a pure A-run on the alt screen: {screen:?}"
    );
    anyhow::ensure!(
        screen.iter().any(|r| r.trim() == "WMROW_SIX_TEXT"),
        "the marker row must survive UNFUSED on the alt screen: {screen:?}"
    );
    for r in &rows {
        if r.contains("WMROW_SIX_TEXT") {
            anyhow::ensure!(
                !r.contains('A'),
                "row fusion at the foreign width (the restored-claude garble): {r:?}"
            );
        }
    }

    // Leg 4: "after claude repaints" — the PTY now sits at the AGREED
    // 147×49 (leg 2), so a TUI full repaint sized for it must parse clean
    // in the live stream too (the field heal: the manual resize forced
    // exactly this repaint; here the attach itself already resized).
    const DRAW2: &[u8] = b"$e=[char]27; [Console]::Out.Write(\"$e[2J$e[5;1H\" + ('B'*140) + \"$e[6;20HRE\" + \"PAINTED_ROW\" + \"$e[7;1HREPAINT_\" + \"OK\")\r";
    c3.send(&C2D::Input { id, bytes: DRAW2.to_vec() })?;
    let live = c3.await_output(id, 20, |l| l.contains("REPAINT_OK"))?;
    let mut view = replay.clone();
    view.extend_from_slice(&live);
    let rows2 = parse_screen(&view, 147, 49);
    let screen2 = &rows2[rows2.len() - 49..];
    anyhow::ensure!(
        screen2
            .iter()
            .any(|r| r.len() >= 100 && r.chars().all(|ch| ch == 'B')),
        "post-resize repaint wide row intact at the agreed width: {screen2:?}"
    );
    anyhow::ensure!(
        screen2
            .iter()
            .any(|r| r.trim() == "REPAINTED_ROW" && !r.contains('B')),
        "post-resize repaint must stay readable (no fusion): {screen2:?}"
    );

    // Leg 3: restore-resync push suppression. c2 (legacy) and c3 (proto 12)
    // are both attached; kill + restart the terminal (blocks_stream_pos's
    // recipe — launch() is not a live-restart verb) and watch both
    // contracts.
    c2.send(&C2D::KillTerminal { id })?;
    c2.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Dead)
    })?;
    c2.send(&C2D::RestartTerminal { id })?;
    // c3 first: Reset arrives…
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        anyhow::ensure!(Instant::now() < deadline, "no Reset at the proto-12 client");
        if let Ok(D2C::Reset { id: rid }) = c3.recv() {
            if rid == id {
                break;
            }
        }
    }
    // …then NOTHING for this terminal until we re-attach (the suppressed
    // push): any Replay/StreamPos/Blocks/Output here is the pre-12 blind-
    // size push racing our re-attach.
    let silent_until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < silent_until {
        match c3.recv() {
            Ok(D2C::Replay { id: rid, .. }) if rid == id => {
                anyhow::bail!("daemon pushed a Replay at a proto-12 client after Reset")
            }
            Ok(D2C::StreamPos { id: rid, .. }) if rid == id => {
                anyhow::bail!("daemon pushed StreamPos at a proto-12 client after Reset")
            }
            Ok(D2C::Blocks { id: rid, .. }) if rid == id => {
                anyhow::bail!("daemon pushed Blocks at a proto-12 client after Reset")
            }
            Ok(D2C::Output { id: rid, .. }) if rid == id => {
                anyhow::bail!("live Output at a detached proto-12 client after Reset")
            }
            _ => {}
        }
    }
    // GUI-style re-attach at our real grid: the ordered attach sequence.
    c3.send(&C2D::Attach { id, cols: 147, rows: 49 })?;
    let mut saw_replay = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        anyhow::ensure!(Instant::now() < deadline, "no re-attach Replay/StreamPos");
        match c3.recv() {
            Ok(D2C::Replay { id: rid, bytes }) if rid == id => {
                saw_replay = true;
                let text = strip_ansi(&String::from_utf8_lossy(&bytes));
                anyhow::ensure!(
                    !text.contains("tc:seam"),
                    "re-attach replay leaked the seam sentinel"
                );
            }
            Ok(D2C::StreamPos { id: rid, .. }) if rid == id => {
                anyhow::ensure!(saw_replay, "re-attach StreamPos before its Replay");
                break;
            }
            Ok(D2C::Output { id: rid, .. }) if rid == id => {
                anyhow::bail!("re-attach Output before StreamPos");
            }
            _ => {}
        }
    }
    // The legacy client got the full compat push, unprompted.
    let (mut saw_reset2, mut saw_replay2, mut saw_sp2) = (false, false, false);
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !saw_sp2 {
        match c2.recv() {
            Ok(D2C::Reset { id: rid }) if rid == id => saw_reset2 = true,
            Ok(D2C::Replay { id: rid, .. }) if rid == id && saw_reset2 => saw_replay2 = true,
            Ok(D2C::StreamPos { id: rid, .. }) if rid == id && saw_replay2 => saw_sp2 = true,
            _ => {}
        }
    }
    anyhow::ensure!(
        saw_reset2 && saw_replay2 && saw_sp2,
        "legacy client must keep the pre-12 push (reset={saw_reset2} replay={saw_replay2} \
         streampos={saw_sp2})"
    );

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c2, id);
    Ok(())
}

/// pw1 attach lock-split coherence: attaching to a session LIVE on the alt
/// screen WHILE it floods must reconstruct byte-coherent state. The split
/// serializes the journal tail OUTSIDE the journal lock (hold 1 snapshots
/// tail+offset, hold 2 appends the raw delta ingested in between and takes
/// StreamPos atomically), so a split bug shows up here as a gapped or
/// doubled stream: garbled counter rows, a wrong final row, or a cursor
/// that disagrees with the daemon mirror. Pins client-reconstructed screen
/// == daemon-mirror ReadScreen (all rows + cursor + alt flag) after
/// Replay + delta + live Output settle, with the attach demonstrably
/// landing mid-flood (live Output frames follow StreamPos).
fn case_attach_alt_flood() -> anyhow::Result<()> {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::{self, test::TermSize, Term, TermMode};

    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut c1 = Conn::open()?;
    let _ = c1.first_snapshot()?;
    let id = create_probe_terminal(&mut c1, "__probe_attach_alt_flood__")?;
    c1.send(&C2D::Attach { id, cols: 120, rows: 40 })?;
    c1.await_blocks(id, 10, |_| true)?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 7400u64;

    // A synthetic TUI (temp script — no escape bytes survive prompt
    // quoting): enter the alt screen, paint an SGR-dense prefill so the
    // attach-time serialize has real work, then WAIT for a go-file, flood
    // absolutely-addressed counter rows, stamp a done marker, park. The
    // final screen is deterministic once quiet.
    let go = std::env::temp_dir().join(format!("tc_probe_aaf_go_{id}"));
    let _ = std::fs::remove_file(&go);
    let script = std::env::temp_dir().join("tc_probe_attach_alt_flood.ps1");
    std::fs::write(
        &script,
        format!(
            concat!(
                "$e=[char]27\n",
                "[Console]::Write(\"$e[?1049h$e[2J$e[H\")\n",
                "foreach ($i in 1..1200) {{\n",
                "  $r = ($i % 36) + 2\n",
                "  $k = ($i % 7) + 31\n",
                "  [Console]::Write(\"$e[$r;1H$e[0;1;$($k)mPRE $('{{0:D6}}' -f $i) \" + ('p' * 80) + \"$e[0m\")\n",
                "}}\n",
                "[Console]::Write(\"$e[1;1HPREFILL_DONE\")\n",
                "while (-not (Test-Path \"{go}\")) {{ Start-Sleep -Milliseconds 25 }}\n",
                "foreach ($i in 1..6000) {{\n",
                "  $r = ($i % 36) + 2\n",
                "  $k = ($i % 7) + 31\n",
                "  [Console]::Write(\"$e[$r;1H$e[0;1;$($k)mFLD $('{{0:D6}}' -f $i) \" + ('f' * 80) + \"$e[0m\")\n",
                // Pace the flood to a multi-second window so the attach
                // deterministically lands INSIDE it (an unpaced PS loop
                // finishes in well under a second).
                "  if ($i % 100 -eq 0) {{ Start-Sleep -Milliseconds 25 }}\n",
                "}}\n",
                "[Console]::Write(\"$e[40;1H$e[0mFLOOD_DONE_MARK\")\n",
                "Start-Sleep 180\n",
            ),
            go = go.display()
        ),
    )?;
    match ctl_run_retry(
        &mut ctl,
        &mut rid,
        id,
        &format!(
            "powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
            script.display()
        ),
        None,
        25,
    )? {
        CtlBody::RunStarted { .. } => {}
        other => anyhow::bail!("TUI Run returned {other:?}"),
    }
    // Prefill painted, mirror on the alt screen.
    {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            rid += 1;
            let ready = match ctl.ctl(rid, CtlRequest::ReadScreen { id }, 10)? {
                CtlBody::Screen { lines, alt_screen, .. } => {
                    alt_screen && lines.iter().any(|l| l.contains("PREFILL_DONE"))
                }
                other => anyhow::bail!("ReadScreen returned {other:?}"),
            };
            if ready {
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "prefill never settled");
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    // GO: start the flood, then attach mid-stream (the paced flood runs
    // ≥1.5s; 300ms in it is solidly under way, so ingest keeps appending
    // across the split's two lock holds).
    std::fs::write(&go, b"go")?;
    std::thread::sleep(Duration::from_millis(300));
    let mut c2 = Conn::open_v2()?;
    let _ = c2.first_snapshot()?;
    c2.send(&C2D::Attach { id, cols: 120, rows: 40 })?;

    // Ordered attach sequence, then live Output until the flood settles.
    // `view` accumulates Replay + Output verbatim — parsing the
    // concatenation reproduces exactly what the GUI's backend would hold.
    let mut view: Vec<u8> = Vec::new();
    let mut live_frames = 0usize;
    {
        let mut saw_replay = false;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            anyhow::ensure!(Instant::now() < deadline, "no attach Replay/StreamPos");
            match c2.recv() {
                Ok(D2C::Replay { id: rid2, bytes }) if rid2 == id => {
                    anyhow::ensure!(
                        memchr::memmem::find(&bytes, b"\x1b[?1049h").is_some(),
                        "live-alt replay must enter the alternate screen"
                    );
                    view.extend_from_slice(&bytes);
                    saw_replay = true;
                }
                Ok(D2C::StreamPos { id: rid2, .. }) if rid2 == id => {
                    anyhow::ensure!(saw_replay, "StreamPos before the Replay");
                    break;
                }
                Ok(D2C::Output { id: rid2, .. }) if rid2 == id => {
                    anyhow::bail!("live Output before StreamPos on attach");
                }
                _ => {}
            }
        }
    }
    // Drain live output until FLOOD_DONE_MARK has been ingested, then until
    // 700ms of silence (the parked script emits nothing more).
    {
        let mut stripper = AnsiStripper::default();
        let mut pending = String::new();
        let mut done = memchr::memmem::find(&view, b"FLOOD_DONE_MARK").is_some();
        let mut last_data = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(90);
        c2.stream.set_read_timeout(Some(Duration::from_millis(200)))?;
        loop {
            anyhow::ensure!(Instant::now() < deadline, "flood never settled");
            match c2.recv() {
                Ok(D2C::Output { id: rid2, bytes }) if rid2 == id => {
                    live_frames += 1;
                    stripper.feed(&bytes, &mut pending);
                    view.extend_from_slice(&bytes);
                    if pending.contains("FLOOD_DONE_MARK") {
                        done = true;
                    }
                    if let Some(p) = pending.rfind('\n') {
                        pending.drain(..=p);
                    }
                    last_data = Instant::now();
                }
                _ => {
                    if done && last_data.elapsed() >= Duration::from_millis(700) {
                        break;
                    }
                }
            }
        }
    }
    anyhow::ensure!(
        live_frames > 0,
        "attach must land mid-flood (live Output after StreamPos) — the case \
         exercises the delta path; got zero live frames"
    );

    // Client-side reconstruction: exactly the GUI's parse (fresh grid at
    // the attach dims, one deterministic advance over Replay+Output).
    let mut term = Term::new(
        term::Config {
            scrolling_history: 10_000,
            ..term::Config::default()
        },
        &TermSize::new(120, 40),
        NullListener,
    );
    let mut parser = crate::daemon::ImmediateProcessor::new();
    parser.advance(&mut term, &view);
    let client_alt = term.mode().contains(TermMode::ALT_SCREEN);
    let client_rows: Vec<String> = (0..term.screen_lines() as i32)
        .map(|l| {
            let row = &term.grid()[Line(l)];
            let mut s = String::with_capacity(term.columns());
            for c in 0..term.columns() {
                let cell = &row[Column(c)];
                if cell
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
                {
                    continue;
                }
                s.push(cell.c);
            }
            s.trim_end().to_string()
        })
        .collect();
    let cur = term.grid().cursor.point;
    let (client_cur_row, client_cur_col) = (cur.line.0.max(0) as u16, cur.column.0 as u16);

    // Daemon mirror, same moment (both sides are quiet): ReadScreen walks
    // the live mirror's visible grid with the identical cell rules.
    rid += 1;
    let (d_lines, d_cur_row, d_cur_col, d_alt) =
        match ctl.ctl(rid, CtlRequest::ReadScreen { id }, 10)? {
            CtlBody::Screen { lines, cursor_row, cursor_col, alt_screen } => {
                (lines, cursor_row, cursor_col, alt_screen)
            }
            other => anyhow::bail!("ReadScreen returned {other:?}"),
        };

    anyhow::ensure!(d_alt, "daemon mirror must still be on the alt screen");
    anyhow::ensure!(client_alt, "client reconstruction must end on the alt screen");
    anyhow::ensure!(
        client_rows.iter().any(|r| r.contains("FLOOD_DONE_MARK")),
        "client screen must carry the settle marker: {client_rows:?}"
    );
    anyhow::ensure!(
        d_lines.len() == client_rows.len(),
        "row-count mismatch: daemon {} vs client {}",
        d_lines.len(),
        client_rows.len()
    );
    for (i, (d, cl)) in d_lines.iter().zip(&client_rows).enumerate() {
        anyhow::ensure!(
            d == cl,
            "row {i} diverged after Replay+delta+Output (stream gap/dup!):\n \
             daemon: {d:?}\n client: {cl:?}"
        );
    }
    anyhow::ensure!(
        (client_cur_row, client_cur_col) == (d_cur_row, d_cur_col),
        "cursor diverged: daemon ({d_cur_row},{d_cur_col}) vs client \
         ({client_cur_row},{client_cur_col})"
    );

    ensure_no_new_panics(log0)?;
    let _ = std::fs::remove_file(&go);
    let _ = std::fs::remove_file(&script);
    delete_terminal(&mut c1, id);
    Ok(())
}

/// HIGH-1 (correctness review): journal compaction must be crash-atomic.
/// Flood a hooked terminal past MAX_LEN so compaction fires (fsync'd tmp +
/// rename-over inside the journal lock — the old remove_file+rename left a
/// window with NO journal on disk, and the un-fsynced tmp a seconds-long
/// truncation window after every compaction), then HARD-KILL the daemon
/// (TerminateProcess — no drain, no flush, the closest a test can get to a
/// power cut) immediately after the post-compaction marker, and assert:
/// the journal file exists (never absent), no .log.tmp orphan is left, the
/// on-disk bytes hold the marker, the sidecar still carries a base > 0, and
/// a restarted daemon's full restore→attach replay reproduces the tail.
fn case_compact_crash() -> anyhow::Result<()> {
    ensure_isolated_daemon("compact_crash")?;
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    fn kill_pid_hard(pid: u32) -> anyhow::Result<()> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
            PROCESS_TERMINATE,
        };
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid)?;
            TerminateProcess(h, 137)?;
            WaitForSingleObject(h, 5000);
            let _ = CloseHandle(h);
        }
        Ok(())
    }

    let old_info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_compact_crash__")?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    c.await_blocks(id, 10, |_| true)?;

    // Flood past MAX_LEN (8 MiB) in one block — compaction is synchronous
    // inside append(), so once the closing marker is visible the swap has
    // already happened.
    c.send(&C2D::Input {
        id,
        bytes: b"$s=('K'*199+\"`n\")*5000; for($i=0;$i -lt 9;$i++){[Console]::Out.Write($s)}; echo CCRASH_MARK\r"
            .to_vec(),
    })?;
    c.await_blocks(id, 240, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("CCRASH_MARK") && r.end_off.is_some())
    })?;

    // Hard kill: no Shutdown frame, no drain, no flush.
    kill_pid_hard(old_info.pid)?;
    let lock = crate::state::data_dir().join("daemon.lock");
    for _ in 0..50 {
        if !lock.exists() || std::fs::remove_file(&lock).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Post-mortem: the swap must be atomic and complete.
    let jpath = crate::state::journals_dir().join(format!("{id}.log"));
    anyhow::ensure!(
        jpath.exists(),
        "JOURNAL ABSENT after a kill right after compaction (the remove+rename window)"
    );
    let tmp = crate::state::journals_dir().join(format!("{id}.log.tmp"));
    anyhow::ensure!(!tmp.exists(), "compaction left a .log.tmp orphan behind");
    let jlen = file_len(&jpath);
    anyhow::ensure!(
        jlen > 0 && jlen < 8 * 1024 * 1024,
        "journal length {jlen} does not look compacted"
    );
    let jtext = strip_ansi(&String::from_utf8_lossy(&std::fs::read(&jpath)?));
    anyhow::ensure!(
        jtext.contains("CCRASH_MARK"),
        "post-compaction tail lost across the kill (journal {jlen} bytes)"
    );
    // The sidecar must still carry the moved base (it is the sole carrier of
    // compaction state across restarts).
    let side_path = crate::state::journals_dir().join(format!("{id}.blocks.json"));
    let side: serde_json::Value = serde_json::from_slice(&std::fs::read(&side_path)?)?;
    anyhow::ensure!(
        side["base"].as_u64().unwrap_or(0) > 0,
        "sidecar lost the compaction base across the kill"
    );

    // Restart, restore, and prove full replay of the compacted tail.
    std::process::Command::new(std::env::current_exe()?)
        .arg("--daemon")
        .creation_flags(DETACHED_PROCESS)
        .spawn()?;
    let mut c2 = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            anyhow::ensure!(Instant::now() < deadline, "restarted daemon never came up");
            std::thread::sleep(Duration::from_millis(200));
            let fresh = std::fs::read(daemon_info_path())
                .ok()
                .and_then(|b| serde_json::from_slice::<DaemonInfo>(&b).ok())
                .is_some_and(|i| i.pid != old_info.pid);
            if !fresh {
                continue;
            }
            if let Ok(conn) = Conn::open() {
                break conn;
            }
        }
    };
    c2.snapshot_until(20, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
    })?;
    std::thread::sleep(Duration::from_millis(1500));

    let mut c3 = Conn::open()?;
    let _ = c3.first_snapshot()?;
    c3.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let replay = loop {
        match c3.recv() {
            Ok(D2C::Replay { id: rid, bytes }) if rid == id => break bytes,
            Ok(_) => {}
            Err(e) => anyhow::bail!("no replay after the crash-restart: {e}"),
        }
    };
    let text = strip_ansi(&String::from_utf8_lossy(&replay));
    anyhow::ensure!(
        text.contains("CCRASH_MARK"),
        "replay after the crash-restart lost the post-compaction tail"
    );
    delete_terminal(&mut c3, id);
    Ok(())
}

/// The REAL boot sequence, end to end, with the actual TermBackend (the gap
/// the cold_attach unit test never modeled): restore → attach → Replay →
/// StreamPos → Blocks → PromptState seed → the corrective resize terminal_card
/// sends → the conhost REPAINT that resize triggers. Asserts the composer
/// cover's gate (cursor_at_prompt_end) survives the repaint and that NO
/// history rows are destroyed by the resize. Pre-fix failure mode: alacritty's
/// grow pulled the preface's last screenful onto the screen, the repaint
/// blanked those rows (user-visible "restore truncated my ls") and stranded
/// prompt_end below the repainted cursor (boot cover never painted).
fn case_boot_cover() -> anyhow::Result<()> {
    use crate::gui::term_backend::{GridSize, TermBackend};
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column as GCol, Line as GLine};

    fn row_text(b: &TermBackend, line: i32) -> String {
        let grid = b.term.grid();
        let cols = grid.columns();
        let mut s = String::new();
        for c in 0..cols {
            s.push(grid[GLine(line)][GCol(c)].c);
        }
        s.trim_end().to_string()
    }

    fn run(id: Uuid, from: (u16, u16), to: (u16, u16)) -> anyhow::Result<()> {
        let (cols, rows) = from;
        let mut c2 = Conn::open()?;
        let _ = c2.first_snapshot()?;
        c2.send(&C2D::Attach { id, cols, rows })?;
        let (mut replay, mut off, mut epoch) = (None, None, 0u32);
        let ps = {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                anyhow::ensure!(Instant::now() < deadline, "no PromptState within 15s");
                match c2.recv() {
                    Ok(D2C::Replay { id: rid, bytes }) if rid == id => replay = Some(bytes),
                    Ok(D2C::StreamPos { id: rid, off: o }) if rid == id => off = Some(o),
                    Ok(D2C::Blocks { id: rid, epoch: e, full: true, .. }) if rid == id => {
                        epoch = e
                    }
                    Ok(D2C::PromptState { id: rid, at_prompt, line, col, clean }) if rid == id => {
                        break (at_prompt, line, col, clean)
                    }
                    _ => {}
                }
            }
        };
        let (at_prompt, line, col, clean) = ps;
        anyhow::ensure!(
            at_prompt && clean,
            "expected a clean prompt at {from:?}->{to:?} (at_prompt={at_prompt} clean={clean})"
        );

        // GUI boot replica: fresh backend at the attach size, replay, feed,
        // seed — exactly drain_ipc's order.
        let mut b = TermBackend::new(GridSize {
            cols,
            rows,
            cell_width: 8.0,
            cell_height: 16.0,
        });
        b.advance(&replay.ok_or_else(|| anyhow::anyhow!("no replay"))?);
        if let Some(o) = off {
            b.set_stream_pos(o);
        }
        if epoch > 0 {
            b.enable_block_scan();
        }
        b.seed_prompt_end(line, col as usize);
        anyhow::ensure!(
            b.cursor_at_prompt_end(),
            "seed must arm: replay cursor ({}, {}) != certified cell ({line}, {col})",
            b.cursor_line(),
            b.term.grid().cursor.point.column.0
        );
        let hist_before = b.history_size();

        // The corrective strip resize, exactly as terminal_card does it.
        let (tc, tr) = to;
        let changed = b.resize_to(
            egui::Vec2::new(tc as f32 * 8.0, tr as f32 * 16.0),
            egui::Vec2::new(8.0, 16.0),
        );
        anyhow::ensure!(changed == Some(to), "resize_to didn't commit: {changed:?}");
        c2.send(&C2D::Resize { id, cols: tc, rows: tr })?;
        anyhow::ensure!(
            b.cursor_at_prompt_end(),
            "cover gate must survive the local reflow"
        );

        // Ingest the conhost repaint for 1.5s (live path, like drain_ipc).
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            if let Ok(D2C::Output { id: rid, bytes }) = c2.recv() {
                if rid == id {
                    b.advance_live(&bytes);
                }
            }
        }
        let cur = b.term.grid().cursor.point;
        // The money assertions. (1) The cover gate survives the repaint —
        // pre-fix, alacritty's grow-pull left prompt_end rows below the
        // repainted cursor and the boot cover never painted.
        anyhow::ensure!(
            b.cursor_at_prompt_end(),
            "cover gate died in the conhost repaint: cursor=({}, {}) pe={:?} (pe row {:?})",
            cur.line.0,
            cur.column.0,
            b.block_feed.as_ref().and_then(|f| f.prompt_end),
            b.block_feed
                .as_ref()
                .and_then(|f| f.prompt_end)
                .map(|(l, _)| row_text(&b, l))
        );
        // (2) No history rows destroyed by the resize — pre-fix, the pulled
        // preface tail (the previous command's output) was blanked forever.
        anyhow::ensure!(
            b.history_size() >= hist_before,
            "resize destroyed scrollback: {} rows before, {} after",
            hist_before,
            b.history_size()
        );
        anyhow::ensure!(
            row_text(&b, cur.line.0 - 1) == "BC_FILL_60",
            "the previous output must still sit directly above the prompt, found {:?}",
            row_text(&b, cur.line.0 - 1)
        );
        Ok(())
    }

    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_bootcov__")?;
    c.send(&C2D::Attach { id, cols: 100, rows: 40 })?;
    // LIVE session with real scrollback (mirror AND conhost both saw it).
    c.send(&C2D::Input {
        id,
        bytes: b"1..60 | % { \"BC_FILL_$_\" }\r".to_vec(),
    })?;
    c.await_output(id, 20, |l| l.trim() == "BC_FILL_60")?;
    std::thread::sleep(Duration::from_millis(800));
    // Variant 0: LIVE session grow — conhost keeps its content top-anchored;
    // the GUI grid must agree (no history pull) or the repaint kills the
    // cover and the pulled rows.
    run(id, (100, 30), (100, 44))?;
    // Put the session back at a known size before the restore variants.
    c.send(&C2D::Resize { id, cols: 100, rows: 40 })?;
    std::thread::sleep(Duration::from_millis(600));
    // Real-boot shape: the dead session left a full screen of output, so the
    // restored world has a rich PREFACE (GUI-side history) while the fresh
    // mirror has NONE — the divergent-reflow case.
    c.send(&C2D::KillTerminal { id })?;
    c.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Dead)
    })?;
    c.send(&C2D::RestartTerminal { id })?;
    c.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
    })?;
    // Let the restored shell print its first prompt + hooks.
    std::thread::sleep(Duration::from_millis(1800));

    // The boot pattern: attach unshrunk, corrective SHRINK by the strip rows.
    run(id, (100, 40), (100, 38))?;
    // And a GROW (window taller than the dead session's size).
    run(id, (100, 30), (100, 44))?;

    delete_terminal(&mut c, id);
    Ok(())
}

/// P4 §8.1: typeahead-reclaim extraction against REAL PSReadLine echo bytes,
/// through the REAL GUI path — a TermBackend rebuilt from the captured attach
/// sequence (Replay → StreamPos → Blocks → PromptState seed) with every live
/// Output frame fed via advance_live, exactly drain_ipc's order. Legs:
/// simple text, chunk-invariance (same bytes re-fed at 7-byte chunks),
/// wrapped input (WRAPLINE walk across a real conhost wrap), a multi-line
/// continuation buffer (refused — MultiLine), and the clean prompt (empty).
fn case_reclaim_extract() -> anyhow::Result<()> {
    use crate::gui::term_backend::{GridSize, Reclaim, TermBackend};
    use egui::{Key, Modifiers};

    const COLS: u16 = 120;
    const ROWS: u16 = 30;

    /// Feed live Output frames for `id` into the backend until `pred` holds.
    fn pump_until(
        c: &mut Conn,
        b: &mut TermBackend,
        id: Uuid,
        live: &mut Vec<u8>,
        secs: u64,
        what: &str,
        pred: impl Fn(&TermBackend) -> bool,
    ) -> anyhow::Result<()> {
        if pred(b) {
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(D2C::Output { id: rid, bytes }) = c.recv() {
                if rid == id {
                    live.extend_from_slice(&bytes);
                    b.advance_live(&bytes);
                    if pred(b) {
                        return Ok(());
                    }
                }
            }
        }
        let cur = b.term.grid().cursor.point;
        anyhow::bail!(
            "{what} not reached within {secs}s (cursor=({}, {}), prompt_end={:?}, reclaim={:?})",
            cur.line.0,
            cur.column.0,
            b.block_feed.as_ref().and_then(|f| f.prompt_end),
            b.reclaim_text()
        )
    }

    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_reclaim__")?;
    c.send(&C2D::Attach { id, cols: COLS, rows: ROWS })?;

    // Capture the attach sequence exactly as drain_ipc would see it.
    let (mut replay, mut off, mut epoch) = (None, None, 0u32);
    let seed = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            anyhow::ensure!(Instant::now() < deadline, "no PromptState within 15s");
            match c.recv() {
                Ok(D2C::Replay { id: rid, bytes }) if rid == id => replay = Some(bytes),
                Ok(D2C::StreamPos { id: rid, off: o }) if rid == id => off = Some(o),
                Ok(D2C::Blocks { id: rid, epoch: e, full: true, .. }) if rid == id => epoch = e,
                Ok(D2C::PromptState { id: rid, at_prompt, line, col, .. }) if rid == id => {
                    break at_prompt.then_some((line, col as usize));
                }
                _ => {}
            }
        }
    };
    let replay = replay.ok_or_else(|| anyhow::anyhow!("no Replay before PromptState"))?;
    let off = off.ok_or_else(|| anyhow::anyhow!("no StreamPos before PromptState"))?;
    anyhow::ensure!(epoch > 0, "probe shell must spawn hooked");

    // GUI boot replica (boot_cover's shape) + accumulated live bytes for the
    // chunk-invariance leg.
    let mk_backend = |seed: Option<(i32, usize)>| {
        let mut b = TermBackend::new(GridSize {
            cols: COLS,
            rows: ROWS,
            cell_width: 8.0,
            cell_height: 16.0,
        });
        b.advance(&replay);
        b.set_stream_pos(off);
        b.enable_block_scan();
        if let Some((l, cl)) = seed {
            b.seed_prompt_end(l, cl);
        }
        b
    };
    let mut b = mk_backend(seed);
    let mut live: Vec<u8> = Vec::new();
    let ctrl_c = crate::win32_input::encode_key(Key::C, Modifiers::CTRL).expect("ctrl+c encodes");

    // Reach a clean captured prompt WITHOUT sending anything: either the
    // PromptState seed already arms (prompt rendered pre-attach) or the
    // FIRST live prompt's pre + 133;B arrive as Output and are scanned. A
    // Ctrl+C here would be fatal — Running is set at spawn, and an interrupt
    // during the bootstrap's `-Command` dot-source kills the shell.
    pump_until(&mut c, &mut b, id, &mut live, 20, "initial clean prompt", |b| {
        b.cursor_at_prompt_end()
    })?;

    // Leg 1: plain stray typed text (NO enter) is exactly recoverable.
    c.send(&C2D::Input { id, bytes: b"RECLAIM_XYZ_77".to_vec() })?;
    pump_until(&mut c, &mut b, id, &mut live, 15, "leg1 echo reclaim", |b| {
        b.reclaim_text() == Reclaim::Text("RECLAIM_XYZ_77".into())
    })?;

    // Leg 2: chunk invariance — a second backend fed the SAME live bytes
    // re-chunked at 7 must agree (ModeScanner/BlockScanner ethos).
    {
        let mut b2 = mk_backend(seed);
        for chunk in live.chunks(7) {
            b2.advance_live(chunk);
        }
        anyhow::ensure!(
            b2.reclaim_text() == Reclaim::Text("RECLAIM_XYZ_77".into()),
            "7-byte re-chunk diverged: {:?}",
            b2.reclaim_text()
        );
    }

    // Leg 3: a wrapped input line (prompt 8 cols + 150 chars > 120 cols)
    // extracts in full via the WRAPLINE walk.
    let marker = format!("RQ_{}", "x".repeat(147));
    c.send(&C2D::Input { id, bytes: ctrl_c.clone() })?;
    pump_until(&mut c, &mut b, id, &mut live, 15, "post-leg1 clean prompt", |b| {
        b.cursor_at_prompt_end()
    })?;
    c.send(&C2D::Input { id, bytes: marker.clone().into_bytes() })?;
    {
        let want = Reclaim::Text(marker.clone());
        pump_until(&mut c, &mut b, id, &mut live, 15, "wrapped reclaim", |b| {
            b.reclaim_text() == want
        })?;
    }

    // Leg 4: an incomplete string + Enter puts PSReadLine into continuation
    // mode (hard newline in the span) — refusal, never a guessed strip.
    c.send(&C2D::Input { id, bytes: ctrl_c.clone() })?;
    pump_until(&mut c, &mut b, id, &mut live, 15, "post-leg3 clean prompt", |b| {
        b.cursor_at_prompt_end()
    })?;
    c.send(&C2D::Input { id, bytes: b"echo 'RECLAIM_ML\r".to_vec() })?;
    pump_until(&mut c, &mut b, id, &mut live, 15, "multi-line refusal", |b| {
        b.reclaim_text() == Reclaim::MultiLine
    })?;

    // Leg 5: Ctrl+C kills the whole buffer; the fresh clean prompt extracts
    // empty (totality of the clean case).
    c.send(&C2D::Input { id, bytes: ctrl_c })?;
    pump_until(&mut c, &mut b, id, &mut live, 15, "clean-extracts-empty", |b| {
        b.cursor_at_prompt_end() && b.reclaim_text() == Reclaim::Text(String::new())
    })?;

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// P4 §8.2: the cross-session history corpus is already client-side —
/// commands from TWO terminals, across a kill+restore epoch bump, all arrive
/// at a FRESH connection's Blocks syncs (the §3.1 zero-wire-change claim,
/// end-to-end, including the dead→restored sidecar path) — then the captured
/// lists drive the REAL `gui::history` index: dedupe, ×N counts, recency
/// order, and the tokenized AND filter.
fn case_history_cross_session() -> anyhow::Result<()> {
    use crate::gui::history::{build_index, filter};

    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let a = create_probe_terminal(&mut c, "__probe_hist_a__")?;
    let b = create_probe_terminal(&mut c, "__probe_hist_b__")?;

    c.send(&C2D::Attach { id: a, cols: 100, rows: 30 })?;
    c.send(&C2D::Input { id: a, bytes: b"echo HIST_A_1\r".to_vec() })?;
    c.await_blocks(a, 20, |recs| {
        recs.iter().any(|r| r.cmd.contains("HIST_A_1") && r.end_off.is_some())
    })?;
    c.send(&C2D::Attach { id: b, cols: 100, rows: 30 })?;
    c.send(&C2D::Input { id: b, bytes: b"echo HIST_B_1\r".to_vec() })?;
    c.await_blocks(b, 20, |recs| {
        recs.iter().any(|r| r.cmd.contains("HIST_B_1") && r.end_off.is_some())
    })?;

    // Kill + restore A: epoch bump; old-epoch records must persist.
    c.send(&C2D::KillTerminal { id: a })?;
    c.snapshot_until(10, |s| {
        s.terminal(a).is_some_and(|t| t.status == TermStatus::Dead)
    })?;
    c.send(&C2D::RestartTerminal { id: a })?;
    c.snapshot_until(10, |s| {
        s.terminal(a).is_some_and(|t| t.status == TermStatus::Running)
    })?;
    // Let the restored shell reach its hooked prompt.
    std::thread::sleep(Duration::from_millis(1800));
    c.send(&C2D::Input { id: a, bytes: b"echo HIST_A_2\r".to_vec() })?;
    c.await_blocks(a, 20, |recs| {
        recs.iter().any(|r| r.cmd.contains("HIST_A_2") && r.end_off.is_some())
    })?;

    // A FRESH connection (GUI restart): the full corpus must arrive from the
    // attach Blocks syncs alone. Sequential attach — await_blocks discards
    // frames for other ids, and each terminal's full sync arrives once.
    let mut c2 = Conn::open()?;
    let _ = c2.first_snapshot()?;
    c2.send(&C2D::Attach { id: a, cols: 100, rows: 30 })?;
    let recs_a = c2.await_blocks(a, 15, |recs| {
        recs.iter().any(|r| r.cmd.contains("HIST_A_1"))
            && recs.iter().any(|r| r.cmd.contains("HIST_A_2"))
    })?;
    let ep1 = recs_a
        .iter()
        .find(|r| r.cmd.contains("HIST_A_1"))
        .map(|r| r.epoch)
        .unwrap();
    let ep2 = recs_a
        .iter()
        .find(|r| r.cmd.contains("HIST_A_2"))
        .map(|r| r.epoch)
        .unwrap();
    anyhow::ensure!(
        ep1 < ep2,
        "epochs must order across the restore (A_1 e{ep1} !< A_2 e{ep2})"
    );
    c2.send(&C2D::Attach { id: b, cols: 100, rows: 30 })?;
    let recs_b = c2.await_blocks(b, 15, |recs| {
        recs.iter().any(|r| r.cmd.contains("HIST_B_1") && r.end_off.is_some())
    })?;

    // The REAL GUI index over the captured lists.
    let lists: Vec<(Uuid, String, bool, &[BlockRec])> = vec![
        (a, "A".into(), false, recs_a.as_slice()),
        (b, "B".into(), true, recs_b.as_slice()), // fabricated dead flag
    ];
    let idx = build_index(&lists);
    anyhow::ensure!(
        idx.len() == 3,
        "expected exactly 3 entries, got {}: {:?}",
        idx.len(),
        idx.iter().map(|e| &e.cmd).collect::<Vec<_>>()
    );
    anyhow::ensure!(
        idx[0].cmd == "echo HIST_A_2" && idx[0].term == a,
        "recency order: newest (A_2, terminal A) first, got {:?}",
        idx[0].cmd
    );
    anyhow::ensure!(
        idx.iter().any(|e| e.cmd == "echo HIST_A_1" && e.term == a),
        "A_1 attributed to A"
    );
    anyhow::ensure!(
        idx.iter()
            .any(|e| e.cmd == "echo HIST_B_1" && e.term == b && e.term_dead),
        "B_1 attributed to B with the supplied dead flag"
    );
    let b1_last = idx
        .iter()
        .find(|e| e.cmd == "echo HIST_B_1")
        .map(|e| e.last_ms)
        .unwrap();

    // Dedupe leg: the same command again in B ⇒ ONE entry, count 2, newest
    // instance representing. await_blocks accumulates only frames arriving
    // AFTER the call (the old rec was consumed by the earlier await), so
    // await the NEW closed rec by key and merge with the captured list.
    let old_key = recs_b
        .iter()
        .find(|r| r.cmd.contains("HIST_B_1"))
        .map(|r| (r.epoch, r.start_off))
        .unwrap();
    c2.send(&C2D::Input { id: b, bytes: b"echo HIST_B_1\r".to_vec() })?;
    let fresh = c2.await_blocks(b, 20, move |recs| {
        recs.iter().any(|r| {
            r.cmd.contains("HIST_B_1")
                && r.end_off.is_some()
                && (r.epoch, r.start_off) != old_key
        })
    })?;
    let mut recs_b2 = recs_b.clone();
    for r in fresh {
        match recs_b2
            .iter_mut()
            .find(|x| (x.epoch, x.start_off) == (r.epoch, r.start_off))
        {
            Some(x) => *x = r,
            None => recs_b2.push(r),
        }
    }
    recs_b2.sort_by_key(|r| (r.epoch, r.start_off));
    let lists2: Vec<(Uuid, String, bool, &[BlockRec])> = vec![
        (a, "A".into(), false, recs_a.as_slice()),
        (b, "B".into(), false, recs_b2.as_slice()),
    ];
    let idx2 = build_index(&lists2);
    anyhow::ensure!(idx2.len() == 3, "dedupe: still 3 entries, got {}", idx2.len());
    let b1 = idx2
        .iter()
        .find(|e| e.cmd == "echo HIST_B_1")
        .ok_or_else(|| anyhow::anyhow!("B_1 entry vanished"))?;
    anyhow::ensure!(b1.count == 2, "×N badge count, got {}", b1.count);
    anyhow::ensure!(
        b1.last_ms >= b1_last,
        "the newest instance must represent (last_ms regressed)"
    );
    anyhow::ensure!(
        idx2[0].cmd == "echo HIST_B_1",
        "re-run moves B_1 to the top (recency), got {:?}",
        idx2[0].cmd
    );

    // Filter leg (pure): tokenized AND over cmd, case-insensitive, identity
    // on empty.
    let hits = filter(&idx2, "hist_a");
    anyhow::ensure!(
        hits.len() == 2
            && hits
                .iter()
                .all(|&i| idx2[i as usize].cmd.contains("HIST_A")),
        "filter 'hist_a' must hit exactly the two A entries"
    );
    let hits = filter(&idx2, "echo a_2");
    anyhow::ensure!(
        hits.len() == 1 && idx2[hits[0] as usize].cmd == "echo HIST_A_2",
        "tokenized AND 'echo a_2' must hit exactly A_2"
    );
    anyhow::ensure!(filter(&idx2, "").len() == 3, "empty query is identity");

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c2, a);
    delete_terminal(&mut c2, b);
    Ok(())
}

fn case_composer_gate_replay() -> anyhow::Result<()> {
    use crate::daemon::blocks::HookVerb;
    use crate::gui::composer::{gate, ComposerState, GateInputs, GateVerdict, RawReason};
    use egui::{Key, Modifiers};

    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_comp_gr__")?;
    c.send(&C2D::Attach {
        id,
        cols: 120,
        rows: 30,
    })?;

    // The StreamPos base makes buffer offsets absolute (P2 contract).
    let mut base_off: Option<u64> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && base_off.is_none() {
        if let Ok(D2C::StreamPos { id: rid, off }) = c.recv() {
            if rid == id {
                base_off = Some(off);
            }
        }
    }
    let base_off = base_off.ok_or_else(|| anyhow::anyhow!("no StreamPos on attach"))?;

    let mut buf: Vec<u8> = Vec::new();
    let mut store: Vec<BlockRec> = Vec::new();
    let pump = |c: &mut Conn,
                    buf: &mut Vec<u8>,
                    store: &mut Vec<BlockRec>,
                    secs: u64,
                    what: &str,
                    done: &dyn Fn(&[u8], &[BlockRec]) -> bool|
     -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if done(buf, store) {
                return Ok(());
            }
            match c.recv() {
                Ok(D2C::Output { id: rid, bytes }) if rid == id => {
                    buf.extend_from_slice(&bytes);
                }
                Ok(D2C::Blocks {
                    id: rid,
                    full,
                    recs,
                    ..
                }) if rid == id => {
                    if full {
                        *store = recs;
                    } else {
                        merge_blocks(store, recs);
                    }
                }
                _ => {}
            }
        }
        anyhow::bail!("gate-replay capture stalled waiting for: {what}")
    };

    // A bare \r guarantees at least one LIVE pre+prompt render lands in the
    // capture even if the first prompt raced the attach (a serialized Replay
    // carries no hooks by design).
    c.send(&C2D::Input {
        id,
        bytes: b"\r".to_vec(),
    })?;
    pump(&mut c, &mut buf, &mut store, 20, "first live prompt", &|b, _| {
        scan_events(b, 4096)
            .iter()
            .any(|(v, _)| matches!(v, HookVerb::Pre { .. }))
    })?;

    c.send(&C2D::Input {
        id,
        bytes: b"ping -t 127.0.0.1\r".to_vec(),
    })?;
    pump(&mut c, &mut buf, &mut store, 30, "ping output + open block", &|b, s| {
        find_sub(b, b"Reply from").is_some()
            && s.iter().any(|r| r.cmd.contains("ping -t") && r.end_off.is_none())
    })?;
    let cc = crate::win32_input::encode_key(Key::C, Modifiers::CTRL).expect("ctrl+c encodes");
    c.send(&C2D::Input { id, bytes: cc })?;
    pump(&mut c, &mut buf, &mut store, 30, "interrupt closes the block", &|_, s| {
        s.iter().any(|r| r.cmd.contains("ping -t") && r.end_off.is_some())
    })?;
    c.send(&C2D::Input {
        id,
        bytes: b"echo GATE_END\r".to_vec(),
    })?;
    pump(&mut c, &mut buf, &mut store, 20, "GATE_END block closes", &|_, s| {
        s.iter()
            .any(|r| r.cmd == "echo GATE_END" && r.end_off.is_some())
    })?;
    // Trailing 133;B of the final prompt (the bootstrap emits it ~15ms after
    // the pre): every pre must end up paired.
    pump(&mut c, &mut buf, &mut store, 10, "trailing PromptEnd", &|b, _| {
        let evs = scan_events(b, 4096);
        let pres = evs
            .iter()
            .filter(|(v, _)| matches!(v, HookVerb::Pre { .. }))
            .count();
        let pends = evs
            .iter()
            .filter(|(v, _)| matches!(v, HookVerb::PromptEnd))
            .count();
        pres > 0 && pends >= pres
    })?;

    // ── Offline replay: shared scanner, 7-byte chunks, pure gate logic. ──
    let events = scan_events(&buf, 7);
    anyhow::ensure!(
        events == scan_events(&buf, 4096),
        "chunk size changed the scanned event stream"
    );
    let mut st = ComposerState::default();
    let now = Instant::now();
    let (mut pre_n, mut exec_n) = (0u64, 0u64);
    let gate_at = |st: &ComposerState, boff: usize| -> GateVerdict {
        let abs = base_off + boff as u64;
        gate(&GateInputs {
            hooked: true,
            running: true,
            alt: false,
            mouse: false,
            open_block: store
                .iter()
                .any(|r| r.start_off <= abs && r.end_off.is_none_or(|e| e > abs)),
            at_prompt: st.at_prompt_latched(),
            // Grid-free replay: the settle window and the cursor cell are
            // simulated (their real counterparts are unit-tested).
            settled: st.at_prompt_latched(),
            cursor_clean: true,
            episode_used: false,
            asleep: false,
        })
    };
    anyhow::ensure!(
        gate_at(&st, 0) == GateVerdict::Blocked(RawReason::NoPrompt),
        "verdict before any pre must be Blocked(NoPrompt)"
    );
    let mut ping_exec_boff: Option<usize> = None;
    // D* (perf-wave-3): a pre that closes a block defers end_off to the
    // following 133;A, so at the pre's own offset the record still reads
    // open (Busy) — the AutoArm lands at the PromptStart that carries the
    // close. A pre with no open block (bare Enter, ^C at a prompt) still
    // AutoArms immediately.
    let mut arm_due_at_prompt_start = false;
    for (verb, boff) in &events {
        match verb {
            HookVerb::Pre { .. } => pre_n += 1,
            HookVerb::Exec { cmd } => {
                exec_n += 1;
                if cmd.contains("ping -t") {
                    ping_exec_boff = Some(*boff);
                }
            }
            _ => {}
        }
        st.on_stream_events(pre_n, exec_n, now);
        let verdict = gate_at(&st, *boff);
        match verb {
            HookVerb::Pre { .. } => match verdict {
                GateVerdict::AutoArm => {}
                GateVerdict::Blocked(RawReason::Busy) => arm_due_at_prompt_start = true,
                v => anyhow::bail!(
                    "after a pre the gate must AutoArm (no open block) or stay \
                     Busy until the 133;A close anchor, got {v:?}"
                ),
            },
            HookVerb::PromptStart if arm_due_at_prompt_start => {
                arm_due_at_prompt_start = false;
                anyhow::ensure!(
                    verdict == GateVerdict::AutoArm,
                    "at the 133;A close anchor the gate must AutoArm, got {verdict:?}"
                );
            }
            HookVerb::Exec { .. } => {
                arm_due_at_prompt_start = false;
                anyhow::ensure!(
                    verdict == GateVerdict::Blocked(RawReason::Busy),
                    "after an exec the gate must be Busy, got {verdict:?}"
                );
            }
            _ => {}
        }
    }
    anyhow::ensure!(
        !arm_due_at_prompt_start,
        "a deferred pre close never met its 133;A anchor in the capture"
    );

    // The claude-safety property, on real bytes: exec disarms BEFORE the
    // interactive app's first output byte reaches the stream.
    let exec_boff =
        ping_exec_boff.ok_or_else(|| anyhow::anyhow!("no exec event for ping in the capture"))?;
    let ping_out = find_sub(&buf, b"Pinging")
        .ok_or_else(|| anyhow::anyhow!("ping banner missing from the capture"))?;
    anyhow::ensure!(
        exec_boff < ping_out,
        "exec ({exec_boff}) did not precede the app's first output ({ping_out})"
    );
    // Offset plumbing sanity: the GUI-style absolute exec offset IS the
    // daemon's record key (P2 §8.1 money assertion, re-walked here).
    let ping_rec = store
        .iter()
        .find(|r| r.cmd.contains("ping -t"))
        .ok_or_else(|| anyhow::anyhow!("ping record missing"))?;
    anyhow::ensure!(
        base_off + exec_boff as u64 == ping_rec.start_off,
        "captured exec offset {} != record key {}",
        base_off + exec_boff as u64,
        ping_rec.start_off
    );

    // PromptEnd leg: after every pre, the next hook event is a PromptEnd
    // (skipping the D* 133;A close anchor, which sits between the pre and
    // the prompt text), and the rendered prompt TEXT sits between them in
    // the stream — the ordering the composer's cursor capture is grounded on.
    let mut prior_end = 0usize;
    for (i, (verb, boff)) in events.iter().enumerate() {
        if !matches!(verb, HookVerb::Pre { .. }) {
            continue;
        }
        let next = events[i + 1..]
            .iter()
            .find(|(v, _)| !matches!(v, HookVerb::Init { .. } | HookVerb::PromptStart));
        match next {
            Some((HookVerb::PromptEnd, pe_boff)) => {
                let between = strip_ansi(&String::from_utf8_lossy(&buf[*boff..*pe_boff]));
                anyhow::ensure!(
                    between.contains("PS "),
                    "prompt text did not precede its 133;B (pre at {boff}, 133;B at {pe_boff}: {between:?})"
                );
                prior_end = *pe_boff;
            }
            other => anyhow::bail!(
                "pre at {boff} not followed by a PromptEnd (next hook: {other:?})"
            ),
        }
    }
    let _ = prior_end;
    // Inert verb: the daemon never warned about the tokenless 133;B.
    anyhow::ensure!(
        !log_since(log0).contains("wrong token"),
        "daemon logged a token warning during the case (PromptEnd must be inert)"
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

// ───────────────────────── P5 controller cases ─────────────────────────

fn master_token() -> anyhow::Result<String> {
    let info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
    Ok(info.token)
}

/// Run with retry while the bootstrap hasn't reported yet (hooks_unverified):
/// a freshly spawned shell needs a moment to dot-source and emit `init`.
fn ctl_run_retry(
    c: &mut Conn,
    req_id: &mut u64,
    id: Uuid,
    cmd: &str,
    wait: Option<RunWait>,
    secs: u64,
) -> anyhow::Result<CtlBody> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        *req_id += 1;
        let body = c.ctl(
            *req_id,
            CtlRequest::Run {
                id,
                cmd: cmd.into(),
                force: false,
                force_self: false,
                wait,
            },
            secs,
        )?;
        match &body {
            CtlBody::Err { code, .. } if code == "hooks_unverified" => {
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "hooks never went live within {secs}s"
                );
                std::thread::sleep(Duration::from_millis(500));
            }
            _ => return Ok(body),
        }
    }
}

fn err_code(body: &CtlBody) -> Option<&str> {
    match body {
        CtlBody::Err { code, .. } => Some(code.as_str()),
        _ => None,
    }
}

/// §14.1 ctl_scope — token minting + scope enforcement + the legacy-frame
/// drop + the recursion guard.
fn case_ctl_scope() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut legacy = Conn::open()?;
    let _ = legacy.first_snapshot()?;
    let id = create_probe_terminal(&mut legacy, "__probe_ctl_scope__")?;
    let id2 = create_probe_terminal(&mut legacy, "__probe_ctl_scope2__")?;

    // 1. Mint scoped tokens via the master ctl channel.
    let mut mc = Conn::open_ctl(&master, None)?;
    let ro_token = match mc.ctl(
        1,
        CtlRequest::TokenCreate {
            name: "probe_ro".into(),
            scope: SCOPE_READ,
        },
        10,
    )? {
        CtlBody::Token { token, scope, .. } => {
            anyhow::ensure!(scope == SCOPE_READ, "minted scope mangled");
            token
        }
        other => anyhow::bail!("TokenCreate returned {other:?}"),
    };
    let _in_token = match mc.ctl(
        2,
        CtlRequest::TokenCreate {
            name: "probe_in".into(),
            scope: SCOPE_READ | crate::protocol::SCOPE_INPUT,
        },
        10,
    )? {
        CtlBody::Token { token, .. } => token,
        other => anyhow::bail!("TokenCreate returned {other:?}"),
    };
    let tok_file = crate::state::data_dir().join("ctl-tokens.json");
    let tok_text = std::fs::read_to_string(&tok_file)?;
    anyhow::ensure!(
        tok_text.contains("probe_ro") && tok_text.contains("probe_in"),
        "ctl-tokens.json missing minted names"
    );

    // 2. Read-scoped connection: List works, everything else is forbidden.
    let mut ro = Conn::open_ctl(&ro_token, None)?;
    match ro.ctl(10, CtlRequest::List, 10)? {
        CtlBody::Listing { terminals, .. } => {
            anyhow::ensure!(
                terminals.iter().any(|t| t.id == id),
                "read scope List missing the probe terminal"
            );
        }
        other => anyhow::bail!("scoped List returned {other:?}"),
    }
    for (rid, req) in [
        (
            11,
            CtlRequest::Run {
                id,
                cmd: "echo NO".into(),
                force: false,
                force_self: false,
                wait: None,
            },
        ),
        (
            12,
            CtlRequest::SendRaw {
                id,
                bytes: b"echo NO\r".to_vec(),
                force_self: false,
            },
        ),
        (
            13,
            CtlRequest::Kill {
                id,
                force_self: false,
            },
        ),
        (
            14,
            CtlRequest::TokenCreate {
                name: "ladder".into(),
                scope: SCOPE_READ,
            },
        ),
    ] {
        let body = ro.ctl(rid, req, 10)?;
        anyhow::ensure!(
            err_code(&body) == Some("forbidden"),
            "read scope req {rid} not refused: {body:?}"
        );
    }

    // 3. A scoped legacy frame is DROPPED (never reaches the PTY) and the
    //    connection survives.
    ro.send(&C2D::Input {
        id,
        bytes: b"echo LEAK_XYZ\r".to_vec(),
    })?;
    std::thread::sleep(Duration::from_secs(2));
    match mc.ctl(3, CtlRequest::ReadTail { id, lines: 200 }, 10)? {
        CtlBody::Tail { lines, .. } => {
            anyhow::ensure!(
                !lines.iter().any(|l| l.contains("LEAK_XYZ")),
                "scoped legacy Input leaked into the terminal"
            );
        }
        other => anyhow::bail!("ReadTail returned {other:?}"),
    }
    ro.assert_alive()?;
    anyhow::ensure!(
        log_since(log0).contains("scoped controller sent a non-Ctl frame"),
        "legacy-frame drop was not logged"
    );

    // 4. Recursion guard: a controller living inside `id` may not drive it…
    let mut selfc = Conn::open_ctl(&master, Some(id))?;
    let body = selfc.ctl(
        20,
        CtlRequest::Run {
            id,
            cmd: "echo CTL_SELF_NO".into(),
            force: false,
            force_self: false,
            wait: None,
        },
        10,
    )?;
    anyhow::ensure!(
        err_code(&body) == Some("self_target"),
        "self Run not refused: {body:?}"
    );
    let body = selfc.ctl(
        21,
        CtlRequest::Kill {
            id,
            force_self: false,
        },
        10,
    )?;
    anyhow::ensure!(
        err_code(&body) == Some("self_target"),
        "self Kill not refused: {body:?}"
    );
    // …unless forced (asserted via the block record the run produces)…
    let mut rid = 21u64;
    let deadline = Instant::now() + Duration::from_secs(20);
    let done = loop {
        rid += 1;
        let body = selfc.ctl(
            rid,
            CtlRequest::Run {
                id,
                cmd: "echo CTL_SELF_OK_9".into(),
                force: false,
                force_self: true,
                wait: Some(RunWait {
                    timeout_ms: 15_000,
                    tail_bytes: 4096,
                }),
            },
            20,
        )?;
        match &body {
            CtlBody::Err { code, .. } if code == "hooks_unverified" => {
                anyhow::ensure!(Instant::now() < deadline, "hooks never went live");
                std::thread::sleep(Duration::from_millis(500));
            }
            _ => break body,
        }
    };
    match done {
        CtlBody::RunDone { exit, output, .. } => {
            anyhow::ensure!(exit == Some(0), "forced self run exit {exit:?}");
            anyhow::ensure!(
                output.contains("CTL_SELF_OK_9"),
                "forced self run output missing marker: {output:?}"
            );
        }
        other => anyhow::bail!("forced self Run returned {other:?}"),
    }
    // …and a DIFFERENT terminal is fair game for the same connection.
    match selfc.ctl(
        40,
        CtlRequest::Kill {
            id: id2,
            force_self: false,
        },
        10,
    )? {
        CtlBody::Done => {}
        other => anyhow::bail!("Kill other returned {other:?}"),
    }

    // 5. Revoke both; the list must be clean of probe names.
    for (rid, name) in [(50, "probe_ro"), (51, "probe_in")] {
        match mc.ctl(
            rid,
            CtlRequest::TokenRevoke { name: name.into() },
            10,
        )? {
            CtlBody::Done => {}
            other => anyhow::bail!("TokenRevoke returned {other:?}"),
        }
    }
    match mc.ctl(52, CtlRequest::TokenList, 10)? {
        CtlBody::Tokens { list } => anyhow::ensure!(
            !list.iter().any(|t| t.name.starts_with("probe_")),
            "revoked tokens still listed"
        ),
        other => anyhow::bail!("TokenList returned {other:?}"),
    }

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut legacy, id);
    delete_terminal(&mut legacy, id2);
    Ok(())
}

/// §14.2 ctl_run_wait — the composite round trip (the money case).
fn case_ctl_run_wait() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut legacy = Conn::open()?;
    let _ = legacy.first_snapshot()?;
    let id = create_probe_terminal(&mut legacy, "__probe_ctl_run__")?;
    let mut c = Conn::open_ctl(&master, None)?;
    let mut rid = 100u64;

    // 1+2. One Run → ONE RunDone with clean, block-exact output.
    let body = ctl_run_retry(
        &mut c,
        &mut rid,
        id,
        "echo CTL_RUN_OK_77",
        Some(RunWait {
            timeout_ms: 20_000,
            tail_bytes: 8192,
        }),
        25,
    )?;
    match &body {
        CtlBody::RunDone {
            exit,
            duration_ms,
            output,
            ..
        } => {
            anyhow::ensure!(*exit == Some(0), "exit {exit:?}");
            anyhow::ensure!(*duration_ms < 20_000, "duration {duration_ms}");
            anyhow::ensure!(
                output.contains("CTL_RUN_OK_77"),
                "output missing marker: {output:?}"
            );
            anyhow::ensure!(
                !output.contains('\u{1b}') && !output.contains('\u{7}') && !output.contains("7717"),
                "output not stripped: {output:?}"
            );
            anyhow::ensure!(
                output
                    .lines()
                    .all(|l| !l.trim_start().starts_with("PS ")),
                "prompt text leaked into the block range: {output:?}"
            );
        }
        other => anyhow::bail!("Run(wait) returned {other:?}"),
    }

    // 3. Prompt wait resolves (immediately at an idle prompt, or on the next
    //    live pre).
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::Wait {
            id,
            cond: WaitCond::Prompt,
            timeout_ms: 5000,
        },
        10,
    )? {
        CtlBody::Waited {
            hit: WaitHit::Prompt,
        } => {}
        other => anyhow::bail!("Wait(Prompt) returned {other:?}"),
    }

    // 4. Live OutputMatch: register on a second conn, then cause the output.
    let mut w = Conn::open_ctl(&master, None)?;
    w.send(&C2D::Ctl {
        req_id: 900,
        req: CtlRequest::Wait {
            id,
            cond: WaitCond::OutputMatch {
                pattern: "CTL_WM_9".into(),
                regex: false,
                from_off: None,
            },
            timeout_ms: 15_000,
        },
    })?;
    std::thread::sleep(Duration::from_millis(300)); // registration beats the run
    rid += 1;
    let at_off = match c.ctl(
        rid,
        CtlRequest::Run {
            id,
            cmd: "echo CTL_WM_9".into(),
            force: false,
            force_self: false,
            wait: None,
        },
        10,
    )? {
        CtlBody::RunStarted { at_off } => at_off,
        other => anyhow::bail!("Run(no wait) returned {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let line = loop {
        anyhow::ensure!(Instant::now() < deadline, "live OutputMatch never resolved");
        if let Ok(D2C::Ctl { req_id: 900, body }) = w.recv() {
            match body {
                CtlBody::Waited {
                    hit: WaitHit::Output { line, .. },
                } => break line,
                other => anyhow::bail!("live wait returned {other:?}"),
            }
        }
    };
    anyhow::ensure!(line.contains("CTL_WM_9"), "matched line {line:?}");

    // 5. from_off leg: the same match resolves from journal HISTORY (the
    //    register-after-output race, closed).
    std::thread::sleep(Duration::from_millis(500));
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::Wait {
            id,
            cond: WaitCond::OutputMatch {
                pattern: "CTL_WM_9".into(),
                regex: false,
                from_off: Some(at_off),
            },
            timeout_ms: 5000,
        },
        10,
    )? {
        CtlBody::Waited {
            hit: WaitHit::Output { line, .. },
        } => anyhow::ensure!(line.contains("CTL_WM_9"), "history line {line:?}"),
        other => anyhow::bail!("from_off wait returned {other:?}"),
    }

    // 6. Timeout leg: sweeps run on the 250ms flush tick.
    rid += 1;
    let t0 = Instant::now();
    let body = c.ctl(
        rid,
        CtlRequest::Wait {
            id,
            cond: WaitCond::OutputMatch {
                pattern: "NEVER_MATCHES_42".into(),
                regex: false,
                from_off: None,
            },
            timeout_ms: 1200,
        },
        10,
    )?;
    let waited = t0.elapsed();
    anyhow::ensure!(
        err_code(&body) == Some("timeout"),
        "no-match wait returned {body:?}"
    );
    anyhow::ensure!(
        waited >= Duration::from_millis(1100) && waited <= Duration::from_secs(3),
        "timeout fired at {waited:?} (expected ~1.2–2.0s)"
    );

    // 7. Embedded newline is refused (each \r is a separate submission — one
    //    exit code for N commands would be a lie).
    rid += 1;
    let body = c.ctl(
        rid,
        CtlRequest::Run {
            id,
            cmd: "echo a\necho b".into(),
            force: false,
            force_self: false,
            wait: None,
        },
        10,
    )?;
    anyhow::ensure!(
        err_code(&body) == Some("multiline"),
        "multiline Run returned {body:?}"
    );

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut legacy, id);
    Ok(())
}

/// §14.3 ctl_busy_gate — refuse-when-busy through a real shell, chord
/// interrupt, and the ungated raw path on a hookless terminal.
fn case_ctl_busy_gate() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut legacy = Conn::open()?;
    let _ = legacy.first_snapshot()?;
    let id = create_probe_terminal(&mut legacy, "__probe_ctl_busy__")?;
    let mut c = Conn::open_ctl(&master, None)?;
    let mut rid = 200u64;

    // 1. Open a long-running block.
    match ctl_run_retry(&mut c, &mut rid, id, "ping -t 127.0.0.1", None, 25)? {
        CtlBody::RunStarted { .. } => {}
        other => anyhow::bail!("ping Run returned {other:?}"),
    }
    let poll_blocks = |c: &mut Conn,
                       rid: &mut u64,
                       secs: u64,
                       pred: &dyn Fn(&[BlockRec]) -> bool|
     -> anyhow::Result<Vec<BlockRec>> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            *rid += 1;
            if let CtlBody::Blocks { recs } =
                c.ctl(*rid, CtlRequest::ReadBlocks { id, last: 20 }, 10)?
            {
                if pred(&recs) {
                    return Ok(recs);
                }
            }
            anyhow::ensure!(Instant::now() < deadline, "block condition not met in {secs}s");
            std::thread::sleep(Duration::from_millis(300));
        }
    };
    poll_blocks(&mut c, &mut rid, 20, &|recs| {
        recs.iter()
            .any(|r| r.cmd.contains("ping -t") && r.end_off.is_none())
    })?;

    // 2. A Run against the open block refuses with the offender named.
    rid += 1;
    let body = c.ctl(
        rid,
        CtlRequest::Run {
            id,
            cmd: "echo NOPE".into(),
            force: false,
            force_self: false,
            wait: None,
        },
        10,
    )?;
    match &body {
        CtlBody::Err { code, msg } if code == "busy" => {
            anyhow::ensure!(msg.contains("ping"), "busy msg lacks the command: {msg}");
        }
        other => anyhow::bail!("busy Run returned {other:?}"),
    }

    // 3. Daemon-encoded interrupt chord closes it (regression-tests chord
    //    encoding against whatever input mode conhost negotiated).
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::SendChord {
            id,
            chord: CtlChord::CtrlC,
            force_self: false,
        },
        10,
    )? {
        CtlBody::Done => {}
        other => anyhow::bail!("SendChord returned {other:?}"),
    }
    poll_blocks(&mut c, &mut rid, 20, &|recs| {
        recs.iter()
            .any(|r| r.cmd.contains("ping -t") && r.end_off.is_some())
    })?;

    // 4. The same Run now succeeds. NOTE `cmd /c echo`: a bare cmdlet echo
    //    would report the interrupted ping's stale $LASTEXITCODE
    //    (0xC000013A) — exit codes are documented best-effort for
    //    cmdlet-only pipelines; a native command sets a fresh one.
    let body = ctl_run_retry(
        &mut c,
        &mut rid,
        id,
        "cmd /c echo BUSY_CLEAR_5",
        Some(RunWait {
            timeout_ms: 15_000,
            tail_bytes: 4096,
        }),
        20,
    )?;
    match &body {
        CtlBody::RunDone { exit, output, .. } => {
            anyhow::ensure!(*exit == Some(0), "post-interrupt run exit {exit:?}");
            anyhow::ensure!(output.contains("BUSY_CLEAR_5"), "output {output:?}");
        }
        other => anyhow::bail!("post-interrupt Run returned {other:?}"),
    }

    // 5. Hookless leg: cmd.exe refuses run but SendRaw is ungated by design.
    rid += 1;
    let cmd_id = match c.ctl(
        rid,
        CtlRequest::CreateTerminal {
            spec: NewTerminal {
                name: "__probe_ctl_cmd__".into(),
                folder: None,
                kind: TermKind::Custom,
                program: "cmd.exe".into(),
                args: vec![],
                cwd: "C:\\".into(),
                already_launched: false,
                shell_cfg: None,
            },
        },
        15,
    )? {
        CtlBody::Created { id } => id,
        other => anyhow::bail!("CreateTerminal returned {other:?}"),
    };
    // Wait for it to be Running (spawn is async relative to the reply).
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            rid += 1;
            if let CtlBody::Listing { terminals, .. } = c.ctl(rid, CtlRequest::List, 10)? {
                if terminals
                    .iter()
                    .any(|t| t.id == cmd_id && t.status == "running")
                {
                    break;
                }
            }
            anyhow::ensure!(Instant::now() < deadline, "cmd.exe terminal never ran");
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    rid += 1;
    let body = c.ctl(
        rid,
        CtlRequest::Run {
            id: cmd_id,
            cmd: "echo NOPE".into(),
            force: false,
            force_self: false,
            wait: None,
        },
        10,
    )?;
    anyhow::ensure!(
        err_code(&body) == Some("not_hooked"),
        "hookless Run returned {body:?}"
    );
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::SendRaw {
            id: cmd_id,
            bytes: b"echo RAW_OK_5\r".to_vec(),
            force_self: false,
        },
        10,
    )? {
        CtlBody::Done => {}
        other => anyhow::bail!("SendRaw returned {other:?}"),
    }
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            rid += 1;
            if let CtlBody::Tail { lines, .. } = c.ctl(
                rid,
                CtlRequest::ReadTail {
                    id: cmd_id,
                    lines: 100,
                },
                10,
            )? {
                // conhost renders cmd.exe line breaks as cursor positioning,
                // so the STRIPPED journal glues output to the next prompt
                // ("RAW_OK_5C:\>"): match the line START, and exclude the
                // echoed command line (which ends with the marker instead).
                if lines
                    .iter()
                    .any(|l| l.trim_start().starts_with("RAW_OK_5") && !l.contains("echo"))
                {
                    break;
                }
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "raw echo output never reached the journal"
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut legacy, id);
    delete_terminal(&mut legacy, cmd_id);
    Ok(())
}

/// §14.4 ctl_read — the read surfaces + shared-helper equivalence + events.
fn case_ctl_read() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut legacy = Conn::open()?;
    let _ = legacy.first_snapshot()?;
    let id = create_probe_terminal(&mut legacy, "__probe_ctl_read__")?;
    let mut c = Conn::open_ctl(&master, None)?;
    let mut rid = 300u64;

    // 1. Two commands with distinct exits.
    let body = ctl_run_retry(
        &mut c,
        &mut rid,
        id,
        "echo CTL_READ_A",
        Some(RunWait {
            timeout_ms: 15_000,
            tail_bytes: 4096,
        }),
        25,
    )?;
    anyhow::ensure!(
        matches!(&body, CtlBody::RunDone { exit: Some(0), .. }),
        "echo run returned {body:?}"
    );
    let body = ctl_run_retry(
        &mut c,
        &mut rid,
        id,
        "cmd /c exit 3",
        Some(RunWait {
            timeout_ms: 15_000,
            tail_bytes: 4096,
        }),
        25,
    )?;
    anyhow::ensure!(
        matches!(&body, CtlBody::RunDone { exit: Some(3), .. }),
        "exit-3 run returned {body:?}"
    );

    // 2. ReadTail: stripped, seam-free.
    rid += 1;
    match c.ctl(rid, CtlRequest::ReadTail { id, lines: 100 }, 10)? {
        CtlBody::Tail { lines, .. } => {
            anyhow::ensure!(
                lines.iter().any(|l| l.trim() == "CTL_READ_A"),
                "tail missing output: {lines:?}"
            );
            let joined = lines.join("\n");
            anyhow::ensure!(
                !joined.contains('\u{1b}')
                    && !joined.contains('\u{7}')
                    && !joined.contains("7717")
                    && !joined.contains("tc:seam"),
                "tail not clean"
            );
        }
        other => anyhow::bail!("ReadTail returned {other:?}"),
    }

    // 3. ReadScreen: the live prompt is visible, cursor in range.
    rid += 1;
    match c.ctl(rid, CtlRequest::ReadScreen { id }, 10)? {
        CtlBody::Screen {
            lines,
            cursor_row,
            alt_screen,
            ..
        } => {
            anyhow::ensure!(!alt_screen, "shell reported alt-screen");
            anyhow::ensure!(
                lines.iter().any(|l| l.starts_with("PS ")),
                "no prompt on screen: {lines:?}"
            );
            anyhow::ensure!(
                (cursor_row as usize) < lines.len(),
                "cursor row {cursor_row} out of {} lines",
                lines.len()
            );
        }
        other => anyhow::bail!("ReadScreen returned {other:?}"),
    }

    // 4. ReadBlocks carries both records with their exits.
    rid += 1;
    let echo_rec = match c.ctl(rid, CtlRequest::ReadBlocks { id, last: 10 }, 10)? {
        CtlBody::Blocks { recs } => {
            anyhow::ensure!(
                recs.iter()
                    .any(|r| r.cmd.contains("CTL_READ_A") && r.exit == Some(0)),
                "echo rec missing"
            );
            anyhow::ensure!(
                recs.iter()
                    .any(|r| r.cmd.contains("exit 3") && r.exit == Some(3)),
                "exit-3 rec missing"
            );
            recs.iter()
                .find(|r| r.cmd.contains("CTL_READ_A"))
                .cloned()
                .unwrap()
        }
        other => anyhow::bail!("ReadBlocks returned {other:?}"),
    };

    // 5. Shared-helper equivalence: the Ctl reply and the legacy
    //    C2D::BlockText reply are the same text, byte for byte.
    rid += 1;
    let (ctl_text, ctl_trunc) = match c.ctl(
        rid,
        CtlRequest::ReadBlockText {
            id,
            start_off: echo_rec.start_off,
        },
        10,
    )? {
        CtlBody::BlockText { text, truncated } => (text, truncated),
        other => anyhow::bail!("ReadBlockText returned {other:?}"),
    };
    legacy.send(&C2D::BlockText {
        id,
        start_off: echo_rec.start_off,
    })?;
    let (legacy_text, legacy_trunc) = legacy.await_block_text(id, echo_rec.start_off, 10)?;
    anyhow::ensure!(
        ctl_text == legacy_text && ctl_trunc == legacy_trunc,
        "shared helper drift: ctl {:?} vs legacy {:?}",
        ctl_text,
        legacy_text
    );

    // 6. Events: Subscribe on a second conn, run a command, kill.
    let mut sub = Conn::open_ctl(&master, None)?;
    match sub.ctl(
        900,
        CtlRequest::Subscribe {
            ids: Some(vec![id]),
            kinds: EV_BLOCKS | EV_EXIT,
        },
        10,
    )? {
        CtlBody::Subscribed => {}
        other => anyhow::bail!("Subscribe returned {other:?}"),
    }
    // `cmd /c echo`: a cmdlet echo would inherit the stale $LASTEXITCODE
    // from the `cmd /c exit 3` above (best-effort exit semantics); the
    // closed-event exit assertion needs a native command's fresh 0.
    let body = ctl_run_retry(
        &mut c,
        &mut rid,
        id,
        "cmd /c echo CTL_EV_1",
        Some(RunWait {
            timeout_ms: 15_000,
            tail_bytes: 4096,
        }),
        20,
    )?;
    anyhow::ensure!(
        matches!(&body, CtlBody::RunDone { .. }),
        "event-leg run returned {body:?}"
    );
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::Kill {
            id,
            force_self: false,
        },
        10,
    )? {
        CtlBody::Done => {}
        other => anyhow::bail!("Kill returned {other:?}"),
    }
    // Collect until we have open→close for the command AND the exit.
    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut opened_at, mut closed_at, mut exited) = (None, None, false);
    let mut n = 0usize;
    while Instant::now() < deadline && !(opened_at.is_some() && closed_at.is_some() && exited) {
        if let Ok(D2C::Ctl {
            req_id: 900,
            body: CtlBody::Event { ev },
        }) = sub.recv()
        {
            n += 1;
            match ev {
                CtlEvent::BlockOpened { rec, .. } if rec.cmd.contains("CTL_EV_1") => {
                    opened_at = Some(n);
                }
                CtlEvent::BlockClosed { rec, .. } if rec.cmd.contains("CTL_EV_1") => {
                    anyhow::ensure!(rec.exit == Some(0), "closed event exit {:?}", rec.exit);
                    closed_at = Some(n);
                }
                CtlEvent::Exited { id: eid, .. } if eid == id => exited = true,
                _ => {}
            }
        }
    }
    let (o, cl) = match (opened_at, closed_at) {
        (Some(o), Some(cl)) => (o, cl),
        _ => anyhow::bail!("missing block events (opened {opened_at:?}, closed {closed_at:?})"),
    };
    anyhow::ensure!(o < cl, "BlockClosed arrived before BlockOpened");
    anyhow::ensure!(exited, "no Exited event after Kill");

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut legacy, id);
    Ok(())
}

/// Hidden screenshot-verification helpers — never part of the default suite.
/// P2's in-grid chrome only forms for LIVE post-attach output, so verifying
/// it by screenshot without injecting input into the user's session takes
/// two steps: `--probe blocks_demo_create` (make a demo terminal and float
/// it to the sidebar top so a fresh GUI auto-selects it), launch the GUI,
/// then `--probe blocks_demo_run` (drive commands into that terminal over
/// IPC while the GUI watches). Clean up with `--probe sweep`.
fn case_blocks_demo_create() -> anyhow::Result<()> {
    let mut c = Conn::open()?;
    let state = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_blocks_demo__")?;
    // Sidebar order walks folders first: to be auto-selected by a fresh GUI
    // the demo must live in the FIRST folder (when one exists), at the top.
    let first_folder = {
        let mut fs = state.folders.clone();
        fs.sort_by_key(|f| f.order);
        fs.first().map(|f| f.id)
    };
    if let Some(fid) = first_folder {
        c.send(&C2D::MoveTerminal {
            id,
            folder: Some(fid),
        })?;
    }
    c.send(&C2D::ReorderTerminal { id, delta: -1000 })?;
    std::thread::sleep(Duration::from_millis(300));
    println!("demo terminal {id} ready");
    Ok(())
}

fn case_blocks_demo_run() -> anyhow::Result<()> {
    let mut c = Conn::open()?;
    let state = c.first_snapshot()?;
    let id = state
        .terminals
        .iter()
        .find(|t| t.name == "__probe_blocks_demo__")
        .ok_or_else(|| anyhow::anyhow!("run blocks_demo_create first"))?
        .id;
    c.send(&C2D::Attach { id, cols: 0, rows: 0 })?;
    for (cmd, marker) in [
        ("echo ok", "echo ok"),
        ("cmd /c exit 3", "exit 3"),
        ("Get-ChildItem C:\\ | Select-Object -First 4", "Select-Object"),
    ] {
        c.send(&C2D::Input {
            id,
            bytes: format!("{cmd}\r").into_bytes(),
        })?;
        let m = marker.to_string();
        c.await_blocks(id, 20, move |recs| {
            recs.iter().any(|r| r.cmd.contains(&m) && r.end_off.is_some())
        })?;
    }
    // Leave an OPEN block so the "separator only, nothing else" rule for
    // open blocks is photographable too.
    c.send(&C2D::Input {
        id,
        bytes: b"ping -t 127.0.0.1\r".to_vec(),
    })?;
    c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("ping -t") && r.end_off.is_none())
    })?;
    println!("demo commands done");
    Ok(())
}

/// Hidden screenshot helper (P3): interrupt whatever the demo terminal is
/// running with a win32 Ctrl+C. At an idle prompt this cancels the (empty)
/// line. Either way a FRESH prompt renders — live `pre` + `133;B` — so a
/// watching GUI's composer auto-arms, photographable without touching user
/// input. Pair with blocks_demo_create/run; clean up with `--probe sweep`.
fn case_composer_demo_arm() -> anyhow::Result<()> {
    use egui::{Key, Modifiers};
    let mut c = Conn::open()?;
    let state = c.first_snapshot()?;
    let id = state
        .terminals
        .iter()
        .find(|t| t.name == "__probe_blocks_demo__")
        .ok_or_else(|| anyhow::anyhow!("run blocks_demo_create first"))?
        .id;
    let cc = crate::win32_input::encode_key(Key::C, Modifiers::CTRL).expect("ctrl+c encodes");
    c.send(&C2D::Input { id, bytes: cc })?;
    std::thread::sleep(Duration::from_millis(1500));
    println!("demo prompt refreshed (a watching GUI's composer should now be armed)");
    Ok(())
}

// ─────────────────────────── P6a WSL cases ───────────────────────────

/// Marker error for environment-gated cases (P6 §12): the runner prints
/// `SKIP(<case>): <reason>` and counts it separately from passes.
fn skip(reason: String) -> anyhow::Error {
    anyhow::anyhow!("SKIP: {reason}")
}

/// The distro the WSL probes run in: the Lxss default, else the first
/// installed one (same enumerator the launcher uses — no `wsl -l` parse).
fn wsl_probe_distro() -> Option<String> {
    let ds = crate::gui::shells::wsl_distros();
    ds.iter()
        .find(|d| d.is_default)
        .or_else(|| ds.first())
        .map(|d| d.name.clone())
}

/// Create a WslShell probe terminal (exactly the launcher's persisted shape:
/// program wsl.exe, args `-d <distro>` — the daemon synthesizes the
/// --cd/--exec tail) and wait until Running.
fn create_wsl_terminal(c: &mut Conn, name: &str, distro: &str) -> anyhow::Result<Uuid> {
    c.send(&C2D::CreateTerminal {
        spec: NewTerminal {
            name: name.into(),
            folder: None,
            kind: TermKind::Shell,
            program: "wsl.exe".into(),
            args: vec!["-d".into(), distro.into()],
            cwd: "C:\\".into(),
            already_launched: false,
            shell_cfg: None,
        },
    })?;
    let state = c.snapshot_until(15, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == name && t.status == TermStatus::Running)
    })?;
    Ok(state.terminals.iter().find(|t| t.name == name).unwrap().id)
}

/// Await the daemon's first hooked prompt for `id` via the P5 wait engine
/// (resolves on a token-checked `pre` — proves hook delivery, token, and
/// hook grammar end-to-end for ANY hooked family: rcfile for WSL, PROMPT
/// env for cmd). Wait{Prompt} REFUSES registration with `hooks_unverified`
/// until the bootstrap's first event lands, so this retries through the
/// boot window (a cold distro takes seconds).
fn await_hooked_prompt(ctl: &mut Conn, rid: &mut u64, id: Uuid, secs: u64) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        *rid += 1;
        match ctl.ctl(
            *rid,
            CtlRequest::Wait {
                id,
                cond: WaitCond::Prompt,
                timeout_ms: 10_000,
            },
            20,
        )? {
            CtlBody::Waited {
                hit: WaitHit::Prompt,
            } => return Ok(()),
            CtlBody::Err { code, .. } if code == "hooks_unverified" || code == "timeout" => {
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "shell never reached a hooked prompt within {secs}s (last: {code})"
                );
                std::thread::sleep(Duration::from_millis(500));
            }
            other => anyhow::bail!("Wait(Prompt) returned {other:?}"),
        }
    }
}

/// Fresh attach → the trailing PromptState (the composer's cold-arm input).
fn attach_prompt_state(id: Uuid, cols: u16, rows: u16) -> anyhow::Result<(bool, bool)> {
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    c.send(&C2D::Attach { id, cols, rows })?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        anyhow::ensure!(Instant::now() < deadline, "no PromptState within 15s");
        if let Ok(D2C::PromptState {
            id: rid,
            at_prompt,
            clean,
            ..
        }) = c.recv()
        {
            if rid == id {
                return Ok((at_prompt, clean));
            }
        }
    }
}

/// P6a P1 `wsl_hooks`: a WslShell terminal comes up HOOKED end-to-end —
/// token-checked init (with the bash-reported shell/home fields in the log),
/// a real block round-trip via `tc run` semantics (exit 0, output captured),
/// the POSIX cwd namespace in the record, and PromptState certifying the
/// composer gate inputs (at_prompt + clean) on a cold attach.
fn case_wsl_hooks() -> anyhow::Result<()> {
    let Some(distro) = wsl_probe_distro() else {
        return Err(skip("no WSL distro in the Lxss registry".into()));
    };
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_wsl_terminal(&mut c, "__probe_wsl_hooks__", &distro)?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;

    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4000u64;
    await_hooked_prompt(&mut ctl, &mut rid, id, 90)?;

    // Token-checked init with the P6a fields (bash bootstrap reports them).
    let log = log_since(log0);
    anyhow::ensure!(
        log.contains(&format!("terminal {id}: block hooks active")),
        "no token-checked init in daemon.log"
    );
    anyhow::ensure!(
        log.contains("shell=bash") && log.contains("home=/"),
        "init did not carry the bash shell/home fields: {:?}",
        log.lines()
            .filter(|l| l.contains("block hooks active"))
            .collect::<Vec<_>>()
    );

    // One real block round-trip through the P5 run gate.
    let body = ctl_run_retry(
        &mut ctl,
        &mut rid,
        id,
        "echo TC_WSL_OK_1",
        Some(RunWait {
            timeout_ms: 30_000,
            tail_bytes: 8192,
        }),
        60,
    )?;
    match &body {
        CtlBody::RunDone { exit, output, .. } => {
            anyhow::ensure!(*exit == Some(0), "bash $? should be 0, got {exit:?}");
            anyhow::ensure!(
                output.contains("TC_WSL_OK_1"),
                "block output missing marker: {output:?}"
            );
        }
        other => anyhow::bail!("Run(wait) on WSL returned {other:?}"),
    }
    // The record carries the POSIX cwd verbatim (namespace doctrine §4).
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("TC_WSL_OK_1") && r.end_off.is_some())
    })?;
    let rec = recs
        .iter()
        .find(|r| r.cmd.contains("TC_WSL_OK_1"))
        .unwrap();
    let cwd = rec.cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
    anyhow::ensure!(
        cwd.as_deref().is_some_and(|c| c.starts_with('/')),
        "block cwd should be a POSIX path, got {cwd:?}"
    );
    anyhow::ensure!(rec.exit == Some(0), "record exit {:?}", rec.exit);

    // Composer gate inputs on a cold attach: at_prompt + clean.
    std::thread::sleep(Duration::from_millis(600));
    let (at_prompt, clean) = attach_prompt_state(id, 100, 28)?;
    anyhow::ensure!(at_prompt, "idle hooked WSL prompt must certify at_prompt");
    anyhow::ensure!(clean, "untouched WSL prompt must certify clean");

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// P6a P2 `wsl_composer_semantics`: the composer-facing signal contract on
/// bash — readline advertises bracketed paste (?2004h: the GUI's submit path
/// keys on TermMode::BRACKETED_PASTE), the Ctrl+C clear chord re-latches a
/// fresh clean prompt (bash re-runs PROMPT_COMMAND after ^C — pins D15), and
/// a bracketed multi-line submit yields ONE block carrying both lines.
fn case_wsl_composer_semantics() -> anyhow::Result<()> {
    let Some(distro) = wsl_probe_distro() else {
        return Err(skip("no WSL distro in the Lxss registry".into()));
    };
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_wsl_terminal(&mut c, "__probe_wsl_comp__", &distro)?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4100u64;
    await_hooked_prompt(&mut ctl, &mut rid, id, 90)?;

    // Bracketed paste observed from the remote readline: the mode assertion
    // rides the serialized replay (modes re-asserted) or raw prompt bytes.
    std::thread::sleep(Duration::from_millis(400));
    let mut c2 = Conn::open()?;
    let _ = c2.first_snapshot()?;
    let replay = c2.replay(id)?;
    anyhow::ensure!(
        String::from_utf8_lossy(&replay).contains("[?2004h"),
        "bash readline did not advertise bracketed paste (?2004h) in the replay"
    );
    drop(c2);

    // Dirty prompt: stray typed bytes ⇒ clean:false on a cold attach.
    c.send(&C2D::Input {
        id,
        bytes: b"tcjunk".to_vec(),
    })?;
    std::thread::sleep(Duration::from_millis(600));
    let (at1, clean1) = attach_prompt_state(id, 100, 28)?;
    anyhow::ensure!(at1, "typed junk must not clear the prompt latch");
    anyhow::ensure!(!clean1, "typed junk must report clean:false");

    // The clear chord: win32 Ctrl+C. bash aborts the line AND re-runs
    // PROMPT_COMMAND ⇒ a NEW pre + 133;B latch, clean again (D15).
    {
        use egui::{Key, Modifiers};
        let cc = crate::win32_input::encode_key(Key::C, Modifiers::CTRL).expect("ctrl+c encodes");
        c.send(&C2D::Input { id, bytes: cc })?;
    }
    std::thread::sleep(Duration::from_millis(1200));
    let (at2, clean2) = attach_prompt_state(id, 100, 28)?;
    anyhow::ensure!(at2, "Ctrl+C must re-latch a fresh prompt (new pre+133;B)");
    anyhow::ensure!(clean2, "the re-latched prompt must be clean");

    // Multi-line bracketed submit ⇒ ONE accept, ONE exec latch, ONE block.
    // Measured bash reality (deviates from the spec's history-1 claim): two
    // pasted SIMPLE commands land as separate history entries, so the block's
    // cmd carries the FIRST line — but both commands run inside the one block
    // and its OUTPUT range brackets both results. Compound commands (loops)
    // fold to one entry via cmdhist and would carry every line.
    c.send(&C2D::Input {
        id,
        bytes: b"\x1b[200~echo TC_ML_A\necho TC_ML_B\x1b[201~\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 30, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("TC_ML_A") && r.end_off.is_some())
    })?;
    let ml: Vec<_> = recs
        .iter()
        .filter(|r| r.cmd.contains("TC_ML_A") || r.cmd.contains("TC_ML_B"))
        .collect();
    anyhow::ensure!(
        ml.len() == 1,
        "multi-line paste must yield ONE block, got {:?}",
        ml.iter().map(|r| &r.cmd).collect::<Vec<_>>()
    );
    anyhow::ensure!(ml[0].exit == Some(0), "multi-line exit {:?}", ml[0].exit);
    // The ^C-at-prompt above must NOT have minted a block: bash re-runs
    // PROMPT_COMMAND after ^C and the DEBUG trap fires for the __tc_pre call
    // itself — the rcfile's name guard + disarm-first choreography is what
    // keeps that from emitting a spurious exec (labeled with stale history).
    anyhow::ensure!(
        recs.iter().all(|r| r.exit != Some(130)),
        "the ^C at an armed prompt minted a spurious block: {:?}",
        recs.iter().map(|r| (&r.cmd, r.exit)).collect::<Vec<_>>()
    );
    // The single block's output brackets BOTH commands' results.
    let start_off = ml[0].start_off;
    c.send(&C2D::BlockText { id, start_off })?;
    let (text, _) = c.await_block_text(id, start_off, 15)?;
    anyhow::ensure!(
        text.contains("TC_ML_A") && text.contains("TC_ML_B"),
        "one block must bracket both pasted commands' output: {text:?}"
    );

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// Bug D pin `wsl_nested_shell` (option c — the recovery path), extended by
/// D2 (the heuristic composer's submission lane): a plain nested `bash` is
/// the SAME signal class as `sudo su` over ssh (staging-proven, no root
/// needed in CI) — the integration is process-local to the outer shell, so
/// the nested episode produces no hook events. Asserts the full episode
/// contract: the `bash` rec stays open (the honest raw-shell lane's feed)
/// while RAW-typed inner commands mint NO blocks and no prompt certifies;
/// D2 submissions ride SubmitCommand{write:true} — the daemon writes the
/// bytes and records synthetic inner recs (open at the pre-write head,
/// closed by the next submission, exit None throughout — including the LAST
/// one, which the returning login-shell pre must close WITHOUT stamping the
/// outer command's exit on it); the outer rec dangles closed at the first
/// inner submit; after `exit` the prompt re-certifies immediately
/// (Wait{Prompt} resolves — the composer's F7 re-attach signal) and one
/// more command forms and closes a normal hook block with a REAL exit code
/// — full machinery recovered, zero re-arm code.
fn case_wsl_nested_shell() -> anyhow::Result<()> {
    let Some(distro) = wsl_probe_distro() else {
        return Err(skip("no WSL distro in the Lxss registry".into()));
    };
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_wsl_terminal(&mut c, "__probe_wsl_nested__", &distro)?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4300u64;
    await_hooked_prompt(&mut ctl, &mut rid, id, 90)?;
    std::thread::sleep(Duration::from_millis(400));

    // Enter a nested interactive bash: the outer shell's DEBUG trap opens a
    // rec for it — and that rec must NOT close (its close signal is the next
    // tokened pre, which cannot arrive while we live inside the hookless
    // nested shell).
    c.send(&C2D::Input {
        id,
        bytes: b"bash\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 30, |recs| {
        recs.iter().any(|r| r.cmd.trim() == "bash" && r.end_off.is_none())
    })?;
    let bash_off = recs
        .iter()
        .find(|r| r.cmd.trim() == "bash")
        .map(|r| r.start_off)
        .unwrap();
    // The GUI-side classifier agrees this episode gets the honest raw-shell
    // lane, not the forever-counting Busy row (Bug D option a).
    anyhow::ensure!(
        crate::gui::composer::nested_shell_cmd("bash"),
        "the nested-shell classifier must match the rec cmd"
    );

    // Inside the nested shell: RAW-typed commands run but there are no
    // hooks — no rec may appear for them, and no prompt may certify (the
    // composer's daemon-side gate feed stays uncertified all visit).
    c.send(&C2D::Input {
        id,
        bytes: b"echo tc-nested-marker\r".to_vec(),
    })?;
    std::thread::sleep(Duration::from_millis(1500));
    let (at_in, _clean) = attach_prompt_state(id, 120, 30)?;
    anyhow::ensure!(
        !at_in,
        "no prompt may certify inside the hookless nested shell"
    );

    // D2: the heuristic composer SUBMITS in the nested shell through the
    // Cmd-family SubmitCommand{write:true} lane — the daemon writes the
    // bytes (bracketed-paste aware off its mirror) and opens a synthetic
    // rec at the pre-write journal head. The first inner submission
    // dangling-closes the outer `bash` rec (exit None at the inner start —
    // its exit can no longer be attributed once inner recs exist).
    c.send(&C2D::SubmitCommand {
        id,
        cmd: "echo tc-heur-inner".into(),
        write: true,
    })?;
    let recs = c.await_blocks(id, 30, |recs| {
        recs.iter().any(|r| r.cmd.contains("tc-heur-inner"))
    })?;
    let bash_rec = recs.iter().find(|r| r.start_off == bash_off).unwrap();
    anyhow::ensure!(
        bash_rec.end_off.is_some(),
        "the outer rec must dangling-close at the first inner submission"
    );
    anyhow::ensure!(
        bash_rec.exit.is_none(),
        "the dangled outer rec closes exit None (honest), got {:?}",
        bash_rec.exit
    );
    let inner1 = recs
        .iter()
        .find(|r| r.cmd.contains("tc-heur-inner"))
        .unwrap();
    anyhow::ensure!(
        inner1.start_off >= bash_rec.end_off.unwrap(),
        "the inner rec opens at the pre-write head, at/after the outer close"
    );
    anyhow::ensure!(
        inner1.end_off.is_none(),
        "the inner rec stays open until the next boundary"
    );
    let inner1_off = inner1.start_off;
    // The command actually RAN: its echoed output lands inside the rec.
    std::thread::sleep(Duration::from_millis(1200));
    // A second submission closes the first at its own pre-write head —
    // output attribution: everything between two submissions belongs to
    // the earlier one; exit stays None (no marker can prove it).
    c.send(&C2D::SubmitCommand {
        id,
        cmd: "echo tc-heur-inner2".into(),
        write: true,
    })?;
    let recs = c.await_blocks(id, 30, |recs| {
        recs.iter()
            .any(|r| r.start_off == inner1_off && r.end_off.is_some())
    })?;
    let inner1 = recs.iter().find(|r| r.start_off == inner1_off).unwrap();
    anyhow::ensure!(
        inner1.exit.is_none(),
        "inner recs carry exit None (honest degradation), got {:?}",
        inner1.exit
    );
    c.send(&C2D::BlockText {
        id,
        start_off: inner1_off,
    })?;
    let (text, _) = c.await_block_text(id, inner1_off, 15)?;
    anyhow::ensure!(
        text.contains("tc-heur-inner"),
        "the submitted inner command must have run: {text:?}"
    );
    let inner2_off = recs
        .iter()
        .find(|r| r.cmd.contains("tc-heur-inner2"))
        .map(|r| r.start_off)
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));

    // `exit`: the OUTER shell's PROMPT_COMMAND fires again — the returning
    // tokened pre closes the LAST inner rec. It carries `bash`'s real exit
    // code (0), which belongs to the OUTER command — the D2 daemon flag
    // must keep it off the inner rec (exit None, no misattribution).
    c.send(&C2D::Input {
        id,
        bytes: b"exit\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 30, |recs| {
        recs.iter()
            .any(|r| r.start_off == inner2_off && r.end_off.is_some())
    })?;
    let inner2 = recs.iter().find(|r| r.start_off == inner2_off).unwrap();
    anyhow::ensure!(
        inner2.exit.is_none(),
        "the returning pre must NOT misattribute the outer exit to the last \
         inner command, got {:?}",
        inner2.exit
    );
    anyhow::ensure!(
        recs.iter().all(|r| !r.cmd.contains("tc-nested-marker")),
        "a RAW-typed command inside the hookless nested shell minted a block: {:?}",
        recs.iter().map(|r| &r.cmd).collect::<Vec<_>>()
    );
    // The prompt re-certifies immediately (Wait{Prompt} resolves once no
    // rec is open — the same signal the composer re-arms on).
    await_hooked_prompt(&mut ctl, &mut rid, id, 30)?;

    // Full recovery: the next command forms and closes a normal block.
    c.send(&C2D::Input {
        id,
        bytes: b"echo tc-after-return\r".to_vec(),
    })?;
    let recs = c.await_blocks(id, 30, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("tc-after-return") && r.end_off.is_some())
    })?;
    let after = recs
        .iter()
        .find(|r| r.cmd.contains("tc-after-return"))
        .unwrap();
    anyhow::ensure!(after.exit == Some(0), "post-return exit {:?}", after.exit);

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// v0.1.1 `wsl_hostile_prompt_command` (the Arch/systemd-257 phantom-block
/// class, reproduced on the default distro — CI never needs Arch): a foreign
/// PROMPT_COMMAND element appended at RUNTIME lands AFTER `__tc_arm` in the
/// array — exactly what /etc/profile.d/80-systemd-osc-context.sh (and
/// starship/direnv-style lazy installs) produce — and fires the armed DEBUG
/// trap at every prompt render. Without the new-history witness that opened
/// a never-closing block labeled with stale `history 1` (the field phantom
/// "exit" block) and busy-gated run/composer forever. Asserts: the latch
/// survives, no phantom open block exists, the setup line yields exactly ONE
/// record, and a real round-trip still works.
fn case_wsl_hostile_prompt_command() -> anyhow::Result<()> {
    let Some(distro) = wsl_probe_distro() else {
        return Err(skip("no WSL distro in the Lxss registry".into()));
    };
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_wsl_terminal(&mut c, "__probe_wsl_hostile__", &distro)?;
    c.send(&C2D::Attach { id, cols: 160, rows: 40 })?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4700u64;
    await_hooked_prompt(&mut ctl, &mut rid, id, 90)?;

    // Install the hostile shape from inside the running shell: an array
    // PROMPT_COMMAND whose foreign element runs after our arm. (Wrapped in
    // a setup function so the element name is NOT a prefix of the history
    // line — the witness's ignoredups escape must not be tripped by an
    // engineered name collision no real integration exhibits.)
    c.send(&C2D::Input {
        id,
        bytes: b"tc_hostile_setup() { __probe_foreign() { :; }; PROMPT_COMMAND+=(__probe_foreign); }; tc_hostile_setup\r"
            .to_vec(),
    })?;
    std::thread::sleep(Duration::from_millis(1500));
    // Render several more prompts: each one runs __probe_foreign at an armed
    // latch — the witness must swallow it (no exec, latch preserved).
    for _ in 0..3 {
        c.send(&C2D::Input { id, bytes: b"\r".to_vec() })?;
        std::thread::sleep(Duration::from_millis(400));
    }
    std::thread::sleep(Duration::from_millis(1200));

    // The setup line is ONE closed record; nothing labeled with the foreign
    // element, no stale-history duplicates, no open block.
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("tc_hostile_setup") && r.end_off.is_some())
    })?;
    let setup: Vec<_> = recs
        .iter()
        .filter(|r| r.cmd.contains("tc_hostile_setup"))
        .collect();
    anyhow::ensure!(
        setup.len() == 1,
        "stale-history phantom duplicated the setup line: {:?}",
        setup.iter().map(|r| (&r.cmd, r.end_off)).collect::<Vec<_>>()
    );
    anyhow::ensure!(
        recs.iter().all(|r| r.end_off.is_some()),
        "phantom never-closing block: {:?}",
        recs.iter()
            .filter(|r| r.end_off.is_none())
            .map(|r| &r.cmd)
            .collect::<Vec<_>>()
    );

    // The prompt latch survived the hostile element (an open phantom would
    // report at_prompt:false)…
    let (at_prompt, clean) = attach_prompt_state(id, 100, 28)?;
    anyhow::ensure!(
        at_prompt,
        "foreign precmd element must not kill the prompt latch"
    );
    anyhow::ensure!(clean, "the idle hostile prompt must still certify clean");

    // …and a real command still round-trips as exactly one block (the old
    // code busy-gated this behind the phantom).
    let body = ctl_run_retry(
        &mut ctl,
        &mut rid,
        id,
        "echo TC_HOSTILE_OK",
        Some(RunWait {
            timeout_ms: 30_000,
            tail_bytes: 4096,
        }),
        60,
    )?;
    match &body {
        CtlBody::RunDone { exit, output, .. } => {
            anyhow::ensure!(*exit == Some(0), "exit {exit:?}");
            anyhow::ensure!(output.contains("TC_HOSTILE_OK"), "output {output:?}");
        }
        other => anyhow::bail!("Run(wait) returned {other:?}"),
    }
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("TC_HOSTILE_OK") && r.end_off.is_some())
    })?;
    let marker: Vec<_> = recs
        .iter()
        .filter(|r| r.cmd.contains("TC_HOSTILE_OK"))
        .collect();
    anyhow::ensure!(
        marker.len() == 1,
        "exactly one block for the real command, got {:?}",
        marker.iter().map(|r| &r.cmd).collect::<Vec<_>>()
    );

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// P6a P4 `wsl_restore`: the POSIX cwd round-trip across a graceful daemon
/// restart — a `cd /tmp` inside the distro is hook-tracked into live_cwd
/// VERBATIM (never Windows-normalized), the restore respawn passes it back
/// through `wsl --cd /tmp`, the restored shell actually sits in /tmp, and the
/// seam rules hold (old output present, nothing visible leaked).
fn case_wsl_restore() -> anyhow::Result<()> {
    ensure_isolated_daemon("wsl_restore")?;
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let Some(distro) = wsl_probe_distro() else {
        return Err(skip("no WSL distro in the Lxss registry".into()));
    };
    let master = master_token()?;
    let old_info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_wsl_terminal(&mut c, "__probe_wsl_restore__", &distro)?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4200u64;
    await_hooked_prompt(&mut ctl, &mut rid, id, 90)?;

    // Move the shell, leave a marker, and wait for the tracker to fold the
    // hook-reported POSIX cwd into persisted state (300ms tick + save).
    for cmd in ["cd /tmp", "echo TC_WSLR_OLD_1"] {
        match ctl_run_retry(
            &mut ctl,
            &mut rid,
            id,
            cmd,
            Some(RunWait { timeout_ms: 30_000, tail_bytes: 4096 }),
            60,
        )? {
            CtlBody::RunDone { .. } => {}
            other => anyhow::bail!("Run({cmd}) returned {other:?}"),
        }
    }
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let live: Option<String> = std::fs::read(state_path())
                .ok()
                .and_then(|b| serde_json::from_slice::<SharedState>(&b).ok())
                .and_then(|s| s.terminal(id).and_then(|t| t.live_cwd.clone()))
                .map(|p| p.to_string_lossy().into_owned());
            if live.as_deref() == Some("/tmp") {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "live_cwd never became the verbatim POSIX /tmp (got {live:?})"
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    // Graceful restart: Shutdown with the request_shutdown linger, wait out
    // the lock, respawn exactly as --install does.
    let restart_log0 = daemon_log_len();
    c.send(&C2D::Shutdown)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if c.recv().is_err() {
            break;
        }
    }
    let lock = crate::state::data_dir().join("daemon.lock");
    for _ in 0..50 {
        if !lock.exists() || std::fs::remove_file(&lock).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::process::Command::new(std::env::current_exe()?)
        .arg("--daemon")
        .creation_flags(DETACHED_PROCESS)
        .spawn()?;
    let mut c2 = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            anyhow::ensure!(Instant::now() < deadline, "restarted daemon never came up");
            std::thread::sleep(Duration::from_millis(200));
            let fresh = std::fs::read(daemon_info_path())
                .ok()
                .and_then(|b| serde_json::from_slice::<DaemonInfo>(&b).ok())
                .is_some_and(|i| i.pid != old_info.pid);
            if !fresh {
                continue;
            }
            if let Ok(conn) = Conn::open() {
                break conn;
            }
        }
    };
    c2.snapshot_until(30, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
    })?;

    // The respawn argv passed the POSIX cwd back verbatim through --cd.
    let spawn_log = log_since(restart_log0);
    let spawn_line = spawn_log
        .lines()
        .find(|l| l.contains(&format!("spawned terminal {id}")))
        .ok_or_else(|| anyhow::anyhow!("no respawn line in daemon.log"))?;
    anyhow::ensure!(
        spawn_line.contains("\"--cd\", \"/tmp\""),
        "respawn argv missing --cd /tmp: {spawn_line}"
    );

    // The restored shell really sits in /tmp (first pre reports it), and the
    // record proves it end-to-end.
    let master2 = master_token()?;
    let mut ctl2 = Conn::open_ctl(&master2, None)?;
    let mut rid2 = 4300u64;
    await_hooked_prompt(&mut ctl2, &mut rid2, id, 90)?;
    match ctl_run_retry(
        &mut ctl2,
        &mut rid2,
        id,
        "pwd",
        Some(RunWait { timeout_ms: 30_000, tail_bytes: 4096 }),
        60,
    )? {
        CtlBody::RunDone { output, .. } => {
            anyhow::ensure!(
                output.lines().any(|l| l.trim() == "/tmp"),
                "restored shell not in /tmp: {output:?}"
            );
        }
        other => anyhow::bail!("Run(pwd) returned {other:?}"),
    }

    // Seam rules: old output survives the restore, nothing visible leaks.
    let mut c3 = Conn::open()?;
    let _ = c3.first_snapshot()?;
    let text = strip_ansi(&String::from_utf8_lossy(&c3.replay(id)?));
    anyhow::ensure!(
        text.contains("TC_WSLR_OLD_1"),
        "old WSL output missing after restore"
    );
    anyhow::ensure!(
        !text.contains("restored") && !text.contains("tc:seam"),
        "restore seam leaked visible text"
    );
    delete_terminal(&mut c3, id);
    Ok(())
}

// ─────────────────────────── P6c ssh case ───────────────────────────

/// Create an Ssh-family probe terminal (the launcher's persisted shape:
/// TermKind::Shell + ssh.exe + `[user flags…, host]`, EMPTY cwd — the daemon
/// writes the one-shot remote rc and spawn() synthesizes keepalives + the
/// remote command, or the TC_SSH_VIA_WSL transport stand-in) and wait until
/// Running.
fn create_ssh_terminal(c: &mut Conn, name: &str, args: &[&str]) -> anyhow::Result<Uuid> {
    c.send(&C2D::CreateTerminal {
        spec: NewTerminal {
            name: name.into(),
            folder: None,
            kind: TermKind::Shell,
            program: "ssh.exe".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: std::path::PathBuf::new(),
            already_launched: false,
            shell_cfg: None,
        },
    })?;
    let state = c.snapshot_until(15, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == name && t.status == TermStatus::Running)
    })?;
    Ok(state.terminals.iter().find(|t| t.name == name).unwrap().id)
}

/// Shared body for both ssh variants once the terminal exists: hooked prompt
/// (token-checked init with the bash fields), one block round-trip with a
/// REAL remote exit code + POSIX cwd, the rc self-delete ($TC_RC exported by
/// the bootstrap body, file gone the moment the hooks are live), and the
/// composer's cold-attach PromptState.
fn assert_ssh_hooked_session(
    c: &mut Conn,
    ctl: &mut Conn,
    rid: &mut u64,
    id: Uuid,
    log0: u64,
    marker: &str,
) -> anyhow::Result<()> {
    await_hooked_prompt(ctl, rid, id, 90)?;
    let log = log_since(log0);
    anyhow::ensure!(
        log.contains(&format!("terminal {id}: block hooks active")),
        "no token-checked init in daemon.log"
    );
    anyhow::ensure!(
        log.contains("shell=bash") && log.contains("home=/"),
        "init did not carry the bash shell/home fields"
    );

    // One real block round-trip through the P5 run gate.
    let body = ctl_run_retry(
        ctl,
        rid,
        id,
        &format!("echo {marker}"),
        Some(RunWait {
            timeout_ms: 30_000,
            tail_bytes: 8192,
        }),
        60,
    )?;
    match &body {
        CtlBody::RunDone { exit, output, .. } => {
            anyhow::ensure!(*exit == Some(0), "bash $? should be 0, got {exit:?}");
            anyhow::ensure!(
                output.contains(marker),
                "block output missing marker: {output:?}"
            );
        }
        other => anyhow::bail!("Run(wait) on ssh returned {other:?}"),
    }
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains(marker) && r.end_off.is_some())
    })?;
    let rec = recs.iter().find(|r| r.cmd.contains(marker)).unwrap();
    let cwd = rec.cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
    anyhow::ensure!(
        cwd.as_deref().is_some_and(|c| c.starts_with('/')),
        "block cwd should be a POSIX path, got {cwd:?}"
    );

    // The one-shot rc self-deleted from the remote /tmp (§3.4.1): the
    // bootstrap body exported TC_RC before exec'ing bash, and the rc rm'd
    // itself right after the init emit — before any restore trailing runs.
    let body = ctl_run_retry(
        ctl,
        rid,
        id,
        "[ -n \"$TC_RC\" ] && [ ! -e \"$TC_RC\" ] && echo TC_RC_GONE",
        Some(RunWait {
            timeout_ms: 30_000,
            tail_bytes: 4096,
        }),
        60,
    )?;
    match &body {
        CtlBody::RunDone { output, .. } => anyhow::ensure!(
            output.contains("TC_RC_GONE"),
            "rc did not self-delete (or TC_RC missing): {output:?}"
        ),
        other => anyhow::bail!("Run(self-delete check) returned {other:?}"),
    }

    // Composer gate inputs on a cold attach: at_prompt + clean.
    std::thread::sleep(Duration::from_millis(600));
    let (at_prompt, clean) = attach_prompt_state(id, 100, 28)?;
    anyhow::ensure!(at_prompt, "idle hooked ssh prompt must certify at_prompt");
    anyhow::ensure!(clean, "untouched ssh prompt must certify clean");

    // v0.1.1 banner fidelity (NOTICE, never a hard fail — env-dependent):
    // if the host has motd content, its first line must have reached the
    // journal via the rc's pam_motd emulation (the MOTD_SHOWN=pam unset
    // fix); if wtmp holds a previous login, a "Last login:" line must too.
    let probe_line = |cmd: &str, tag: &str, ctl: &mut Conn, rid: &mut u64| -> Option<String> {
        match ctl_run_retry(
            ctl,
            rid,
            id,
            cmd,
            Some(RunWait {
                timeout_ms: 30_000,
                tail_bytes: 4096,
            }),
            60,
        ) {
            Ok(CtlBody::RunDone { output, .. }) => output
                .lines()
                .filter_map(|l| l.trim().strip_prefix(tag))
                .map(|v| v.trim().to_string())
                .find(|v| !v.is_empty()),
            _ => None,
        }
    };
    // Snapshot the journal BEFORE the probe commands run: the probe echoes
    // would otherwise plant the very strings the check looks for.
    let replay = {
        let mut c2 = Conn::open()?;
        let _ = c2.first_snapshot()?;
        String::from_utf8_lossy(&c2.replay(id)?).into_owned()
    };
    let motd_first = probe_line(
        "m=$(cat /run/motd.dynamic /etc/motd 2>/dev/null | head -n 1); echo \"TCMOTD>$m\"",
        "TCMOTD>",
        ctl,
        rid,
    );
    // Same NR==2 && NF>=9 guard as the rc's own awk: an empty/rotated wtmp
    // prints only the "wtmp begins …" trailer (NF 7), which is NOT a
    // previous login.
    let prev_login = probe_line(
        "p=$(last -2 -F -- \"$USER\" 2>/dev/null | awk 'NR==2 && NF>=9 {print $1}'); echo \"TCPREV>$p\"",
        "TCPREV>",
        ctl,
        rid,
    );
    if let Some(line) = motd_first {
        if !replay.contains(&line) {
            println!();
            println!(
                "  NOTICE(ssh banner): host motd first line {line:?} not found in the journal"
            );
        }
    }
    if prev_login.is_some() && !replay.contains("Last login:") {
        println!();
        println!(
            "  NOTICE(ssh banner): wtmp has a previous login but no \"Last login:\" line reached the journal"
        );
    }
    Ok(())
}

/// P6c §12 P6 `ssh_bootstrap_local`: the generated one-shot remote bootstrap
/// runs END-TO-END through a real ConPTY.
///
/// WSL-transport variant (needs `TC_SSH_VIA_WSL=tc-probe-host` on BOTH the
/// isolated daemon and this probe, plus a WSL distro — the knob's VALUE
/// names the ONE host the stand-in intercepts): an Ssh-family terminal is
/// created through the REAL launch()/write_bashrc_remote path; spawn()'s
/// stand-in executes the exact `sh -c` bootstrap body — mktemp + `base64 -d`
/// then `exec bash --rcfile` + self-delete — via `wsl.exe --exec /bin/sh -c`,
/// bypassing only the ssh link. If a localhost sshd answers port 22, the
/// full `ssh 127.0.0.1` variant ALSO runs on the REAL link (the stand-in
/// never matches that host; BatchMode: key/agent auth only — auth failure
/// prints a notice, never hangs on a password). Neither environment
/// available ⇒ SKIP-notice; the U4 goldens still pin the synthesized argv +
/// remote command string.
fn case_ssh_bootstrap_local() -> anyhow::Result<()> {
    let stand_in_host = std::env::var("TC_SSH_VIA_WSL")
        .ok()
        .filter(|v| !v.is_empty() && v != "127.0.0.1");
    let via_wsl = stand_in_host.is_some() && wsl_probe_distro().is_some();
    let sshd = TcpStream::connect_timeout(
        &"127.0.0.1:22".parse().unwrap(),
        Duration::from_millis(500),
    )
    .is_ok();
    if !via_wsl && !sshd {
        return Err(skip(
            "no WSL transport (set TC_SSH_VIA_WSL=tc-probe-host on daemon+probe, WSL required) and no localhost sshd".into(),
        ));
    }
    let master = master_token()?;
    let mut ran = Vec::new();

    if via_wsl {
        let host = stand_in_host.unwrap();
        let log0 = daemon_log_len();
        let mut c = Conn::open()?;
        let _ = c.first_snapshot()?;
        let id = create_ssh_terminal(&mut c, "__probe_ssh_wsl__", &[&host])?;
        c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
        // The stand-in must have engaged (never silently test the wrong path).
        let mut ctl = Conn::open_ctl(&master, None)?;
        let mut rid = 4300u64;
        assert_ssh_hooked_session(&mut c, &mut ctl, &mut rid, id, log0, "TC_SSH_OK_1")?;
        anyhow::ensure!(
            log_since(log0).contains("TC_SSH_VIA_WSL transport stand-in active"),
            "expected the WSL transport stand-in to engage"
        );
        ensure_no_new_panics(log0)?;
        delete_terminal(&mut c, id);
        ran.push("wsl-transport");
    }

    if sshd {
        let log0 = daemon_log_len();
        let mut c = Conn::open()?;
        let _ = c.first_snapshot()?;
        // BatchMode: key/agent auth only — an auth failure exits instead of
        // sitting at a password prompt the probe can never answer.
        let id = create_ssh_terminal(
            &mut c,
            "__probe_ssh_local__",
            &[
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "127.0.0.1",
            ],
        )?;
        c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
        let mut ctl = Conn::open_ctl(&master, None)?;
        let mut rid = 4400u64;
        // The stand-in only intercepts its named host, so this variant rides
        // the REAL ssh link (auth failure ⇒ notice, not a probe failure).
        match assert_ssh_hooked_session(&mut c, &mut ctl, &mut rid, id, log0, "TC_SSH_OK_2") {
            Ok(()) => {
                ensure_no_new_panics(log0)?;
                ran.push("ssh-127.0.0.1");
            }
            Err(e) => {
                println!();
                println!(
                    "  NOTICE(ssh_bootstrap_local): sshd answered but the session never \
                     reached a hooked prompt ({e:#}) — key/agent auth unavailable?"
                );
            }
        }
        delete_terminal(&mut c, id);
    }

    if ran.is_empty() {
        return Err(skip(
            "sshd answered but auth failed and no WSL transport was available".into(),
        ));
    }
    print!("[{}] ", ran.join("+"));
    Ok(())
}

/// SSH AUTO-RECONNECT (proto 10): a hooked ssh session whose transport dies
/// UNEXPECTEDLY reconnects by itself — supervision flag raised (Snapshot
/// `reconnecting:true`), launch() fires on the backoff engine, the fresh
/// link's first token-checked `pre` resolves it (flag drops, Running,
/// hooks re-armed). Uses the TC_SSH_VIA_WSL transport stand-in; the "link
/// death" is `kill -9 $$` in the remote shell (SIGKILL ⇒ nonzero exit —
/// exactly the not-a-clean-`exit` shape the qualification requires).
fn case_ssh_reconnect() -> anyhow::Result<()> {
    let stand_in_host = std::env::var("TC_SSH_VIA_WSL")
        .ok()
        .filter(|v| !v.is_empty() && v != "127.0.0.1");
    if stand_in_host.is_none() || wsl_probe_distro().is_none() {
        return Err(skip(
            "no WSL transport (set TC_SSH_VIA_WSL=tc-probe-host on daemon+probe, WSL required)"
                .into(),
        ));
    }
    let host = stand_in_host.unwrap();
    let master = master_token()?;
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_ssh_terminal(&mut c, "__probe_ssh_rc__", &[&host])?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4500u64;
    await_hooked_prompt(&mut ctl, &mut rid, id, 90)?;

    // Kill the transport mid-session (raw bytes, like a real link death —
    // no block machinery involved).
    c.send(&C2D::Input {
        id,
        bytes: b"kill -9 $$\r".to_vec(),
    })?;

    // The supervision flag must rise (the lane's `reconnecting…` witness)…
    c.snapshot_until(20, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.reconnecting)
    })?;
    // …and resolve: Running again with the flag DOWN (first token-checked
    // pre of the fresh link). First attempt fires at +2s; WSL hooks arm in
    // well under a second after spawn.
    c.snapshot_until(60, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Running && !t.reconnecting)
    })?;
    await_hooked_prompt(&mut ctl, &mut rid, id, 90)?;

    // daemon.log evidence: scheduled + resolved (never guessed).
    let log = log_since(log0);
    anyhow::ensure!(
        log.contains("auto-reconnect in"),
        "no reconnect scheduling line in daemon.log"
    );
    anyhow::ensure!(
        log.contains("ssh reconnected (attempt"),
        "no reconnect resolution line in daemon.log"
    );

    // A DELIBERATE kill must NOT reconnect (the user's Kill fights back).
    ctl.send(&C2D::KillTerminal { id })?;
    let s = c.snapshot_until(15, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Dead)
    })?;
    anyhow::ensure!(
        s.terminals.iter().any(|t| t.id == id && !t.reconnecting),
        "a deliberate Kill must not raise the reconnect flag"
    );
    // Give the (absent) supervision a beat, then confirm it stayed Dead.
    std::thread::sleep(Duration::from_millis(3500));
    if let CtlBody::Listing { terminals, .. } = ctl.ctl(rid + 1, CtlRequest::List, 20)? {
        let t = terminals.iter().find(|t| t.id == id);
        anyhow::ensure!(
            t.is_some_and(|t| t.status == "dead"),
            "killed ssh must stay dead (no zombie reconnect): {:?}",
            t.map(|t| &t.status)
        );
    }
    rid += 1;

    // ── Dead-relaunch fix b: MANUAL bounded retry (C2D::RetryReconnect,
    // proto 13). The tab was DELIBERATELY killed, so the auto path
    // correctly refused supervision — `Retry ▸` is the user overriding
    // that by explicit consent (no hooks_were_live gate on the manual
    // entry). Round 1: enter supervision, cancel inside the 2s
    // pre-attempt window, verify a clean stop (flag down, still dead
    // after the would-be first rung).
    c.send(&C2D::RetryReconnect { id })?;
    c.snapshot_until(10, |s| {
        s.terminals.iter().any(|t| t.id == id && t.reconnecting)
    })?;
    c.send(&C2D::CancelReconnect { id })?;
    c.snapshot_until(10, |s| {
        s.terminals.iter().any(|t| t.id == id && !t.reconnecting)
    })?;
    std::thread::sleep(Duration::from_millis(3500));
    rid += 1;
    if let CtlBody::Listing { terminals, .. } = ctl.ctl(rid, CtlRequest::List, 20)? {
        let t = terminals.iter().find(|t| t.id == id);
        anyhow::ensure!(
            t.is_some_and(|t| t.status == "dead"),
            "cancelled manual retry must stop the ladder cleanly: {:?}",
            t.map(|t| &t.status)
        );
    }
    // Round 2: retry again and let it run — the WSL stand-in host is
    // alive, so the first attempt reconnects and the fresh link's
    // token-checked pre resolves supervision (flag drops on its own).
    let log1 = daemon_log_len();
    c.send(&C2D::RetryReconnect { id })?;
    c.snapshot_until(60, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Running && !t.reconnecting)
    })?;
    await_hooked_prompt(&mut ctl, &mut rid, id, 90)?;
    anyhow::ensure!(
        log_since(log1).contains("manual ssh reconnect requested"),
        "no manual-retry scheduling line in daemon.log"
    );

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

// ───────────────────── remote CLI resume (task #27) ─────────────────────

/// Run a POSIX sh script in the DEFAULT WSL distro (the same one the
/// TC_SSH_VIA_WSL stand-in shells run in) and return its stdout. The script
/// is DELIVERED AS A FILE over /mnt — wsl.exe re-joins inline `sh -c`
/// argv through the default shell and destroys quoting (the documented §9.3
/// recipe trap). The child's stdout passes through wsl.exe verbatim (UTF-8)
/// — only wsl's OWN messages are UTF-16.
fn run_wsl_sh(script: &str) -> anyhow::Result<String> {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    static NONCE: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "tc-rr-{}-{}.sh",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    {
        // LF endings, UTF-8 no BOM.
        let mut f = std::fs::File::create(&path)?;
        f.write_all(script.replace("\r\n", "\n").as_bytes())?;
    }
    let mnt = crate::daemon::bootstrap::wsl_mnt_path(&path)
        .ok_or_else(|| anyhow::anyhow!("temp dir not /mnt-translatable"))?;
    let out = std::process::Command::new("wsl.exe")
        .args(["--", "sh", &mnt])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let _ = std::fs::remove_file(&path);
    let out = out?;
    anyhow::ensure!(
        out.status.success(),
        "wsl sh script failed: {script:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Environment gate shared by the ssh_cli_* cases: the WSL transport
/// stand-in (session leg), the sftp probe transport knob (probe leg), and
/// the staged fake claude (spec §9.2/§9.3 — WSL /tmp is volatile, re-stage
/// per session). Returns the stand-in host.
fn ssh_cli_env() -> anyhow::Result<String> {
    let host = std::env::var("TC_SSH_VIA_WSL")
        .ok()
        .filter(|v| !v.is_empty() && v != "127.0.0.1")
        .ok_or_else(|| skip("no WSL transport (set TC_SSH_VIA_WSL=tc-probe-host on daemon+probe)".into()))?;
    if wsl_probe_distro().is_none() {
        return Err(skip("no WSL distro installed".into()));
    }
    if std::env::var("TC_SSH_PROBE_TRANSPORT")
        .ok()
        .filter(|v| !v.is_empty())
        .is_none()
    {
        return Err(skip(
            "TC_SSH_PROBE_TRANSPORT not set on daemon+probe (point it at the staged \
             wsl sftp-server wrapper — spec §9.3 recipe)"
                .into(),
        ));
    }
    let staged = run_wsl_sh(
        "([ -x \"$HOME/bin/claude\" ] && [ -x /tmp/tcsftp/usr/lib/openssh/sftp-server ] \
         && [ -x /tmp/tcprobe-transport.sh ] && echo OK) || echo MISSING",
    )?;
    if !staged.contains("OK") {
        return Err(skip(
            "probe staging missing in the default distro (fake ~/bin/claude + /tmp/tcsftp \
             sftp-server + /tmp/tcprobe-transport.sh — spec §9.3 recipe; WSL /tmp is volatile)"
                .into(),
        ));
    }
    // Defensive: a crashed earlier run may have left flag files behind that
    // poison later cases — the auth-dead switch, and claude_beacon's beacon/
    // rotate flags (which flip fake-claude launches into Explicit-instead-of-
    // Correlated attribution). Clearing them here self-heals every gated run.
    let _ = run_wsl_sh("rm -f /tmp/tcprobe-authdead /tmp/tcprobe-beacon /tmp/tcprobe-rotate; exit 0");
    Ok(host)
}

/// The fake-claude store dir (home-relative) for a stand-in cwd.
fn ssh_cli_store_dir(cwd: &str) -> String {
    format!(
        ".claude/projects/{}",
        crate::state::claude_project_dir_name(std::path::Path::new(cwd))
    )
}

/// (name, size) store listing, NEWEST-FIRST (`ls -t`), via wsl. Empty when
/// the store dir does not exist.
fn ssh_cli_store_ls(cwd: &str) -> anyhow::Result<Vec<(String, u64)>> {
    let dir = ssh_cli_store_dir(cwd);
    let script = format!(
        "cd \"$HOME/{dir}\" 2>/dev/null || exit 0; \
         for f in $(ls -t 2>/dev/null); do printf '%s %s\\n' \"$f\" \"$(wc -c < \"$f\")\"; done"
    );
    let out = run_wsl_sh(&script)?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let (n, s) = l.trim().rsplit_once(' ')?;
            Some((n.to_string(), s.parse().ok()?))
        })
        .collect())
}

fn ssh_cli_wipe_store(cwd: &str) -> anyhow::Result<()> {
    let dir = ssh_cli_store_dir(cwd);
    run_wsl_sh(&format!("rm -rf \"$HOME/{dir}\"; exit 0")).map(|_| ())
}

/// Wait until the store holds exactly `files` entries and (optionally) has
/// been byte-stable for 1.5s — the fake claude's "active conversation went
/// quiet" edge the staggered acceptance scenario needs.
fn ssh_cli_wait_store(
    cwd: &str,
    files: usize,
    quiet: bool,
    secs: u64,
) -> anyhow::Result<Vec<(String, u64)>> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut stable: Option<(Vec<(String, u64)>, Instant)> = None;
    loop {
        let cur = ssh_cli_store_ls(cwd)?;
        if cur.len() == files {
            if !quiet {
                return Ok(cur);
            }
            match &stable {
                Some((prev, since)) if *prev == cur => {
                    if since.elapsed() >= Duration::from_millis(1500) {
                        return Ok(cur);
                    }
                }
                _ => stable = Some((cur, Instant::now())),
            }
        } else {
            stable = None;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "store {} never reached {files} quiet file(s): {:?}",
            ssh_cli_store_dir(cwd),
            ssh_cli_store_ls(cwd)
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// CODEX-SESSION BEACON end-to-end (attribution, `codex_beacon`) — the codex
/// mirror of `claude_beacon`, over the WSL stand-in (a fake `codex` that
/// prints the tcbeacon OSC to /dev/tty then sleeps, so the exact pty →
/// journal → BlockScanner → `on_beacon` path is exercised).
/// A) a fake `$HOME/bin/codex` fires the bash exec hook (opening a codex
///    inner_cli, Ambiguous) then prints `tcbeacon;codex;SessionStart;startup;
///    <sid>` — the daemon upgrades the codex inner_cli to Explicit(<sid>)
///    with ZERO probes (the accept line is in daemon.log);
/// B) anti-spoof: a codex beacon printed at a PLAIN prompt (no open codex
///    block) is dropped — inner_cli stays absent.
/// SKIPs without a WSL distro. (The Windows-native lane — hook →
/// `tc __codex-hook` → ReportCliSession — and the ssh remote installer are
/// validated live off-probe; this pins the daemon-side beacon apply that is
/// unique to the codex adapter parameterization.)
fn case_codex_beacon() -> anyhow::Result<()> {
    let Some(distro) = wsl_probe_distro() else {
        return Err(skip("no WSL distro in the Lxss registry".into()));
    };
    let sid = Uuid::new_v4();
    // Stage the fake codex in an ISOLATED temp dir (never ~/bin — must not
    // clobber a real user codex) and invoke it by absolute path; analyze_cmdline
    // stems argv[0] to "codex" regardless. It prints the beacon (adapter-
    // carrying form) then holds the foreground so the exec-opened codex block
    // stays open when the beacon lands.
    let stage = format!(
        "mkdir -p /tmp/tc-codex-probe\n\
         cat > /tmp/tc-codex-probe/codex <<'EOF'\n\
         #!/bin/sh\n\
         printf '\\033]7717;tcbeacon;codex;SessionStart;startup;{sid}\\007' > /dev/tty 2>/dev/null || true\n\
         sleep 25\n\
         EOF\n\
         chmod +x /tmp/tc-codex-probe/codex\n"
    );
    run_wsl_sh(&stage)?;
    let result = codex_beacon_body(&distro, sid);
    // ALWAYS clean, pass or fail (r2 roll — same leak class as
    // claude_beacon: the old per-call cleanup closure was bypassed by the
    // `ensure!` exits). The `sleep 25` fake codex dies on its own; the
    // stage dir must not linger.
    let _ = run_wsl_sh("pkill -f tc-codex-probe 2>/dev/null; rm -rf /tmp/tc-codex-probe; exit 0");
    result
}

/// The codex_beacon case body — every early return routes through the
/// wrapper's unconditional stage cleanup above.
fn codex_beacon_body(distro: &str, sid: Uuid) -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_wsl_terminal(&mut c, "__probe_codex_beacon__", distro)?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 5200u64;
    await_hooked_prompt(&mut ctl, &mut rid, id, 90)?;

    // ── A) fake codex → exec hook (codex Ambiguous) → beacon → Explicit(sid).
    c.send(&C2D::Input { id, bytes: b"/tmp/tc-codex-probe/codex\r".to_vec() })?;
    let want = sid.to_string();
    let res = ssh_cli_poll_state(60, |s| {
        s.terminals.iter().any(|t| {
            t.id == id
                && t.inner_cli.as_ref().is_some_and(|cli| {
                    cli.adapter == "codex"
                        && cli.confidence == CliConfidence::Explicit
                        && cli.resume_token.as_deref() == Some(want.as_str())
                })
        })
    });
    if let Err(e) = res {
        return Err(e.context("codex beacon never upgraded inner_cli to Explicit"));
    }
    anyhow::ensure!(
        log_since(log0).contains(&format!("terminal {id}: tcbeacon codex session {sid}")),
        "codex beacon accept line missing from daemon.log"
    );

    // Interrupt the sleeping fake codex → back to a plain hooked prompt (the
    // codex block closes; inner_cli clears via the close lifecycle). Killed
    // from OUTSIDE — the foreground fake codex would eat a typed ^C.
    let _ = run_wsl_sh("pkill -f tc-codex-probe 2>/dev/null; exit 0");
    ssh_cli_poll_state(20, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.inner_cli.is_none())
    })?;

    // ── B) anti-spoof: a codex beacon at a plain prompt (no open codex block)
    //    is dropped. Run a printf through the P5 run gate; its RunDone proves
    //    the bytes were scanned (ingest is ordered), yet no accept + no inner_cli.
    let spoof = Uuid::new_v4();
    let body = ctl_run_retry(
        &mut ctl,
        &mut rid,
        id,
        &format!("printf '\\033]7717;tcbeacon;codex;SessionStart;startup;{spoof}\\007'"),
        Some(RunWait { timeout_ms: 30_000, tail_bytes: 4096 }),
        60,
    )?;
    anyhow::ensure!(
        matches!(body, CtlBody::RunDone { .. }),
        "spoof printf never ran: {body:?}"
    );
    anyhow::ensure!(
        !log_since(log0).contains(&spoof.to_string()),
        "spoofed codex beacon must never be accepted"
    );
    let st: SharedState = serde_json::from_slice(&std::fs::read(state_path())?)?;
    anyhow::ensure!(
        st.terminals
            .iter()
            .find(|t| t.id == id)
            .is_some_and(|t| t.inner_cli.is_none()),
        "spoofed codex beacon must not mint an inner_cli"
    );

    delete_terminal(&mut c, id);
    // Stage cleanup lives in case_codex_beacon's wrapper (every exit path).
    ensure_no_new_panics(log0)?;
    Ok(())
}

/// Poll the persisted state.json for a predicate. inner_cli mutations are
/// capture-on-change SAVED but not Snapshot-BROADCAST (hook-fed families
/// skip the tracker tick that coalesces broadcasts), so snapshot_until can
/// never see them — the persisted file is the truthful witness.
fn ssh_cli_poll_state(
    secs: u64,
    pred: impl Fn(&SharedState) -> bool,
) -> anyhow::Result<SharedState> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(bytes) = std::fs::read(state_path()) {
            if let Ok(s) = serde_json::from_slice::<SharedState>(&bytes) {
                if pred(&s) {
                    return Ok(s);
                }
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "state.json never satisfied the predicate"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The M0 probe sidecar (probes\<id>.json), raw JSON.
fn ssh_cli_sidecar(id: Uuid) -> Option<serde_json::Value> {
    let p = crate::state::data_probes_dir().join(format!("{id}.json"));
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn ssh_cli_wait_sidecar(id: Uuid, secs: u64) -> anyhow::Result<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = ssh_cli_sidecar(id) {
            return Ok(v);
        }
        anyhow::ensure!(Instant::now() < deadline, "M0 sidecar never appeared for {id}");
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Hooked ssh terminal at a prompt in `cwd`, with the fake claude reachable.
fn ssh_cli_setup(
    c: &mut Conn,
    ctl: &mut Conn,
    rid: &mut u64,
    name: &str,
    host: &str,
    cwd: &str,
) -> anyhow::Result<Uuid> {
    let id = create_ssh_terminal(c, name, &[host])?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    await_hooked_prompt(ctl, rid, id, 90)?;
    // Marker split with '' so the COMMAND ECHO (which the ConPTY reorder can
    // land inside the block range) never matches the assembled marker.
    let body = ctl_run_retry(
        ctl,
        rid,
        id,
        "command -v claude || echo TC_NO''CLAUDE",
        Some(RunWait { timeout_ms: 30_000, tail_bytes: 4096 }),
        60,
    )?;
    if let CtlBody::RunDone { output, .. } = &body {
        if output.contains("TC_NOCLAUDE") {
            return Err(skip(
                "fake claude staged but not on the stand-in shell's PATH \
                 (~/.profile must add ~/bin — spec §9.3 recipe)"
                    .into(),
            ));
        }
    }
    let body = ctl_run_retry(
        ctl,
        rid,
        id,
        &format!("mkdir -p {cwd} && cd {cwd}"),
        Some(RunWait { timeout_ms: 30_000, tail_bytes: 4096 }),
        60,
    )?;
    anyhow::ensure!(
        matches!(body, CtlBody::RunDone { exit: Some(0), .. }),
        "cd {cwd} failed: {body:?}"
    );
    Ok(id)
}

/// Type a bare `claude` (the REAL user launch shape — raw bytes, no Run
/// machinery) and wait for the M0 evidence: inner_cli = claude/Ambiguous/
/// token-less (the D11 sanitize, live) + the snapshot sidecar.
fn ssh_cli_launch_bare(
    c: &mut Conn,
    id: Uuid,
    cwd: &str,
) -> anyhow::Result<serde_json::Value> {
    c.send(&C2D::Input { id, bytes: b"claude\r".to_vec() })?;
    // Generous windows: a real WSL bootstrap + fake-claude exec + the M0 sftp
    // probe chain is slow, and the full suite runs this after the flood case
    // on a possibly-loaded machine (60s, not 30 — the standalone-passes /
    // suite-flakes fingerprint).
    ssh_cli_poll_state(60, |s| {
        s.terminals.iter().any(|t| {
            t.id == id
                && t.inner_cli.as_ref().is_some_and(|cli| {
                    cli.adapter == "claude"
                        && cli.resume_token.is_none()
                        && cli.confidence == CliConfidence::Ambiguous
                        && cli.cwd == std::path::Path::new(cwd)
                })
        })
    })?;
    let sidecar = ssh_cli_wait_sidecar(id, 45)?;
    anyhow::ensure!(
        sidecar["adapter"] == "claude" && sidecar["cwd"] == cwd,
        "sidecar shape wrong: {sidecar}"
    );
    let dir = ssh_cli_store_dir(cwd);
    anyhow::ensure!(
        sidecar["listings"][0][0] == dir.as_str(),
        "sidecar listing dir wrong: {sidecar}"
    );
    Ok(sidecar)
}

/// The stand-in shell's LINUX pid, from the terminal's LAST init-hook log
/// line (`block hooks active (shell pid N…`) — the CLI holds the foreground,
/// so a typed `kill -9 $$` would go to IT, not bash; the link death is dealt
/// from outside, exactly like a real transport drop.
fn ssh_cli_shell_pid(id: Uuid) -> anyhow::Result<u32> {
    let needle = format!("terminal {id}: block hooks active (shell pid ");
    let text = log_since(0);
    let line = text
        .lines()
        .rev()
        .find(|l| l.contains(&needle))
        .ok_or_else(|| anyhow::anyhow!("no init-hook log line for {id}"))?;
    let tail = &line[line.find(&needle).unwrap() + needle.len()..];
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    Ok(digits.parse()?)
}

/// Kill the remote shell FROM OUTSIDE (SIGKILL to the bash pid inside WSL —
/// the CLI is foreground, so this is the only honest link-death shape while
/// a claude runs) and ride the auto-reconnect back to Running (the M4 probe
/// fires inside the reconnect launch).
fn ssh_cli_kill_and_reconnect(c: &mut Conn, id: Uuid) -> anyhow::Result<()> {
    let pid = ssh_cli_shell_pid(id)?;
    run_wsl_sh(&format!("kill -9 {pid}; exit 0"))?;
    c.snapshot_until(20, |s| s.terminals.iter().any(|t| t.id == id && t.reconnecting))?;
    c.snapshot_until(60, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Running && !t.reconnecting)
    })?;
    // Fresh attach: launch() suspended-and-resynced this conn while the
    // waits above were still pending, so a later await_blocks would see no
    // frames at all — a re-attach delivers the full Blocks sync again.
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    Ok(())
}

/// REMOTE CLI RESUME end-to-end (remote-cli-resume-spec §9.2 `ssh_cli_resume`):
/// A) bare `claude` in an ssh terminal → M0 sidecar (bare-launch snapshot,
///    inner_cli Ambiguous token-less per D11) → link death → auto-reconnect's
///    correlate leg diffs the store over a FRESH sftp connection → the
///    respawn's first CLI block is `claude --resume <uuid>` and inner_cli is
///    Explicit again (the hour-long-claude case, minus the sleep).
/// B) /clear analog: the fake claude rotates onto a SECOND jsonl mid-run ⇒
///    R-NEWEST resumes the newest uuid.
/// C) THE NAMED ACCEPTANCE SCENARIO (staggered): two TC terminals, same
///    fabricated cwd, staggered starts (first conversation quiet before the
///    second starts), both blocks open at daemon shutdown ⇒ per-block diff
///    gives each terminal its correct verdict: the second Correlated to ITS
///    uuid and resumed; the first honestly Ambiguous (its window contains
///    both births — sibling gate holds R-NEWEST off) with both candidates
///    listed newest-first in the preface and inner_cli cleared.
/// D) simultaneous variant: both claudes still writing when each other's
///    snapshot lands ⇒ BOTH go Ambiguous-with-candidates — never guess.
fn case_ssh_cli_resume() -> anyhow::Result<()> {
    let host = ssh_cli_env()?;
    let master = master_token()?;
    let mut ran: Vec<&str> = Vec::new();

    // ── A) bare launch → kill → auto-reconnect resumes ──
    let log0 = daemon_log_len();
    let cwd = "/tmp/tcprobe-home/proj";
    ssh_cli_wipe_store(cwd)?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4700u64;
    let id = ssh_cli_setup(&mut c, &mut ctl, &mut rid, "__probe_sshcli_a__", &host, cwd)?;
    ssh_cli_launch_bare(&mut c, id, cwd)?;
    let store = ssh_cli_wait_store(cwd, 1, true, 40)?;
    let uuid_a = store[0].0.trim_end_matches(".jsonl").to_string();
    let size_a = store[0].1;

    ssh_cli_kill_and_reconnect(&mut c, id)?;
    let want_cmd = format!("claude --resume {uuid_a}");
    c.await_blocks(id, 40, |recs| recs.iter().any(|r| r.cmd == want_cmd))?;
    ssh_cli_poll_state(20, |s| {
        s.terminals.iter().any(|t| {
            t.id == id
                && t.inner_cli.as_ref().is_some_and(|cli| {
                    cli.confidence == CliConfidence::Explicit
                        && cli.resume_token.as_deref() == Some(uuid_a.as_str())
                })
        })
    })?;
    anyhow::ensure!(
        log_since(log0).contains(&format!("remote claude session correlated -> {uuid_a}")),
        "no correlate log line for {uuid_a}"
    );
    // The resumed fake claude appended to the SAME transcript (still ONE
    // file, larger) — resume semantics, not a fork.
    let store = ssh_cli_wait_store(cwd, 1, true, 40)?;
    anyhow::ensure!(
        store[0].1 > size_a,
        "resumed claude did not append to {uuid_a} ({} -> {})",
        size_a,
        store[0].1
    );

    // ── A2) SLEEP with the CLI running → wake resumes again (the user's
    // sleep→wake shape). The sleep kill is a dangling close: identity
    // (inner_cli + sidecar) must survive it, and the wake rides the
    // RestartTerminal probe lane — with ONE recent session this must NEVER
    // end in "identity was ambiguous". ──
    let log_a2 = daemon_log_len();
    let size_a2 = store[0].1;
    c.send(&C2D::SleepTerminal { id })?;
    c.snapshot_until(30, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Dead && t.asleep)
    })?;
    c.send(&C2D::RestartTerminal { id })?;
    c.snapshot_until(60, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Running && !t.asleep)
    })?;
    // Same resync shape as the reconnect helper: re-attach for fresh Blocks.
    // Leg A's resume rec is in the sync too (closed by the sleep's dangling
    // close) — the wake's proof is an OPEN block with the resume command.
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    c.await_blocks(id, 40, |recs| {
        recs.iter().any(|r| r.cmd == want_cmd && r.end_off.is_none())
    })?;
    ssh_cli_poll_state(20, |s| {
        s.terminals.iter().any(|t| {
            t.id == id
                && t.inner_cli.as_ref().is_some_and(|cli| {
                    cli.confidence == CliConfidence::Explicit
                        && cli.resume_token.as_deref() == Some(uuid_a.as_str())
                })
        })
    })?;
    anyhow::ensure!(
        !log_since(log_a2).contains("definitively ambiguous"),
        "sleep→wake with one recent session must never go ambiguous"
    );
    let store = ssh_cli_wait_store(cwd, 1, true, 40)?;
    anyhow::ensure!(
        store[0].1 > size_a2,
        "wake-resumed claude did not append to {uuid_a} ({} -> {})",
        size_a2,
        store[0].1
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    ran.push("resume+sleepwake");

    // ── B) /clear rotation ⇒ R-NEWEST resumes the post-clear id ──
    let log0 = daemon_log_len();
    let cwd = "/tmp/tcprobe-home/proj2";
    ssh_cli_wipe_store(cwd)?;
    run_wsl_sh("touch /tmp/tcprobe-rotate")?;
    let rotate_off = |e: anyhow::Error| {
        let _ = run_wsl_sh("rm -f /tmp/tcprobe-rotate; exit 0");
        e
    };
    let id = ssh_cli_setup(&mut c, &mut ctl, &mut rid, "__probe_sshcli_b__", &host, cwd)
        .map_err(rotate_off)?;
    ssh_cli_launch_bare(&mut c, id, cwd).map_err(rotate_off)?;
    let store = ssh_cli_wait_store(cwd, 2, true, 60).map_err(rotate_off)?;
    run_wsl_sh("rm -f /tmp/tcprobe-rotate; exit 0")?;
    let uuid_b = store[0].0.trim_end_matches(".jsonl").to_string(); // newest = post-"/clear"
    ssh_cli_kill_and_reconnect(&mut c, id)?;
    let want_cmd = format!("claude --resume {uuid_b}");
    c.await_blocks(id, 40, |recs| recs.iter().any(|r| r.cmd == want_cmd))?;
    anyhow::ensure!(
        log_since(log0).contains(&format!("remote claude session correlated -> {uuid_b}")),
        "R-NEWEST did not pick the rotated id {uuid_b}"
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    ran.push("r-newest");

    // ── C) staggered acceptance: both blocks open at shutdown ──
    let cwd = "/tmp/tcprobe-home/projshared";
    ssh_cli_wipe_store(cwd)?;
    let t1 = ssh_cli_setup(&mut c, &mut ctl, &mut rid, "__probe_sshcli_c1__", &host, cwd)?;
    ssh_cli_launch_bare(&mut c, t1, cwd)?;
    // First conversation goes QUIET before the second starts — the
    // "separated starts" precondition that lets the second terminal's diff
    // exclude the first's transcript.
    let store = ssh_cli_wait_store(cwd, 1, true, 40)?;
    let uuid_c1 = store[0].0.trim_end_matches(".jsonl").to_string();
    let t2 = ssh_cli_setup(&mut c, &mut ctl, &mut rid, "__probe_sshcli_c2__", &host, cwd)?;
    let sidecar2 = ssh_cli_launch_bare(&mut c, t2, cwd)?;
    // T2's basis must already contain T1's transcript (the diff's whole point).
    anyhow::ensure!(
        sidecar2["listings"][0][1]
            .as_array()
            .is_some_and(|entries| entries
                .iter()
                .any(|e| e[0].as_str().is_some_and(|n| n.contains(&uuid_c1)))),
        "T2's M0 snapshot should already list T1's transcript: {sidecar2}"
    );
    let store = ssh_cli_wait_store(cwd, 2, true, 40)?;
    let uuid_c2 = store[0].0.trim_end_matches(".jsonl").to_string(); // newest = T2's
    anyhow::ensure!(uuid_c2 != uuid_c1, "store order did not surface T2's transcript first");

    // Graceful daemon restart with BOTH claude blocks open (M2: no probe at
    // shutdown; the persisted sidecars + boot-restore correlate cover it).
    let restart_log0 = daemon_log_len();
    let old_info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
    let _ = old_info;
    c.send(&C2D::Shutdown)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if c.recv().is_err() {
            break;
        }
    }
    let lock = crate::state::data_dir().join("daemon.lock");
    for _ in 0..50 {
        if !lock.exists() || std::fs::remove_file(&lock).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        std::process::Command::new(std::env::current_exe()?)
            .arg("--daemon")
            .creation_flags(DETACHED_PROCESS)
            .spawn()?;
    }
    let mut c2 = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match Conn::open() {
                Ok(conn) => break conn,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(250))
                }
                Err(e) => return Err(e),
            }
        }
    };
    // Boot restore brings both back; T2 correlates to ITS transcript, T1 is
    // honestly ambiguous (its block window contains both births and the
    // sibling gate holds R-NEWEST off) with inner_cli cleared.
    c2.snapshot_until(60, |s| {
        [t1, t2].iter().all(|id| {
            s.terminals
                .iter()
                .any(|t| t.id == *id && t.status == TermStatus::Running)
        })
    })?;
    ssh_cli_poll_state(30, |s| {
        let t2_explicit = s.terminals.iter().any(|t| {
            t.id == t2
                && t.inner_cli.as_ref().is_some_and(|cli| {
                    cli.confidence == CliConfidence::Explicit
                        && cli.resume_token.as_deref() == Some(uuid_c2.as_str())
                })
        });
        let t1_cleared = s
            .terminals
            .iter()
            .any(|t| t.id == t1 && t.inner_cli.is_none());
        t2_explicit && t1_cleared
    })?;
    let want_cmd = format!("claude --resume {uuid_c2}");
    c2.send(&C2D::Attach { id: t2, cols: 120, rows: 30 })?;
    c2.await_blocks(t2, 40, |recs| recs.iter().any(|r| r.cmd == want_cmd))?;
    let log = log_since(restart_log0);
    anyhow::ensure!(
        log.contains(&format!("remote claude session correlated -> {uuid_c2}")),
        "T2 did not correlate to its own transcript"
    );
    anyhow::ensure!(
        log.contains(&format!(
            "terminal {t1}: remote claude correlation definitively ambiguous (2 candidate(s))"
        )),
        "T1 should be definitively ambiguous with 2 candidates"
    );
    // T1's preface: the §6.4 candidates block, newest-first, paste-able.
    let text = strip_ansi(&String::from_utf8_lossy(&c2.replay(t1)?));
    anyhow::ensure!(
        text.contains(&format!("multiple claude sessions found in {cwd}")),
        "T1 preface missing the candidates header: {text:?}"
    );
    let p_new = text.find(&format!("claude --resume {uuid_c2}"));
    let p_old = text.find(&format!("claude --resume {uuid_c1}"));
    anyhow::ensure!(
        p_new.is_some() && p_old.is_some() && p_new < p_old,
        "T1 preface should list both candidates newest-first"
    );
    anyhow::ensure!(
        text.contains("(newest)"),
        "T1 preface missing the (newest) annotation"
    );
    ensure_no_new_panics(restart_log0)?;
    delete_terminal(&mut c2, t1);
    delete_terminal(&mut c2, t2);
    ran.push("staggered");

    // ── D) simultaneous same-dir variant: BOTH ambiguous, never guess ──
    // The sub-case-C restart rotated the daemon's master token — re-read it.
    let master = master_token()?;
    let cwd = "/tmp/tcprobe-home/projshared";
    ssh_cli_wipe_store(cwd)?;
    let mut ctl2 = Conn::open_ctl(&master, None)?;
    let d1 = ssh_cli_setup(&mut c2, &mut ctl2, &mut rid, "__probe_sshcli_d1__", &host, cwd)?;
    let d2 = ssh_cli_setup(&mut c2, &mut ctl2, &mut rid, "__probe_sshcli_d2__", &host, cwd)?;
    // TRUE simultaneity, made deterministic (wall-clock overlap is racy —
    // the losing timing degrades to the staggered case): the fake claude
    // slow-starts (delays its first store write 2s) so BOTH M0 snapshots
    // capture an EMPTY store ⇒ at restore each diff carries BOTH files ⇒
    // the sibling gate holds R-NEWEST off for BOTH ⇒ both Ambiguous.
    run_wsl_sh("touch /tmp/tcprobe-slowstart")?;
    let slow_off = |e: anyhow::Error| {
        let _ = run_wsl_sh("rm -f /tmp/tcprobe-slowstart; exit 0");
        e
    };
    c2.send(&C2D::Input { id: d1, bytes: b"claude\r".to_vec() })
        .map_err(slow_off)?;
    c2.send(&C2D::Input { id: d2, bytes: b"claude\r".to_vec() })
        .map_err(slow_off)?;
    // Both snapshots land during the 2s pre-write sleep (empty store)…
    ssh_cli_wait_sidecar(d1, 30).map_err(slow_off)?;
    ssh_cli_wait_sidecar(d2, 30).map_err(slow_off)?;
    run_wsl_sh("rm -f /tmp/tcprobe-slowstart; exit 0")?;
    // …then both files are written and grow.
    let store = ssh_cli_wait_store(cwd, 2, true, 60)?;
    let (du_new, du_old) = (
        store[0].0.trim_end_matches(".jsonl").to_string(),
        store[1].0.trim_end_matches(".jsonl").to_string(),
    );
    let restart_log0 = daemon_log_len();
    c2.send(&C2D::Shutdown)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if c2.recv().is_err() {
            break;
        }
    }
    for _ in 0..50 {
        if !lock.exists() || std::fs::remove_file(&lock).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        std::process::Command::new(std::env::current_exe()?)
            .arg("--daemon")
            .creation_flags(DETACHED_PROCESS)
            .spawn()?;
    }
    let mut c3 = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match Conn::open() {
                Ok(conn) => break conn,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(250))
                }
                Err(e) => return Err(e),
            }
        }
    };
    c3.snapshot_until(60, |s| {
        [d1, d2].iter().all(|id| {
            s.terminals
                .iter()
                .any(|t| t.id == *id && t.status == TermStatus::Running)
        })
    })?;
    ssh_cli_poll_state(30, |s| {
        [d1, d2].iter().all(|id| {
            s.terminals
                .iter()
                .any(|t| t.id == *id && t.inner_cli.is_none())
        })
    })?;
    for id in [d1, d2] {
        let text = strip_ansi(&String::from_utf8_lossy(&c3.replay(id)?));
        anyhow::ensure!(
            text.contains(&format!("multiple claude sessions found in {cwd}"))
                && text.contains(&format!("claude --resume {du_new}"))
                && text.contains(&format!("claude --resume {du_old}")),
            "terminal {id} preface should list BOTH candidates: {text:?}"
        );
    }
    let log = log_since(restart_log0);
    anyhow::ensure!(
        log.matches("definitively ambiguous (2 candidate(s))").count() >= 2,
        "both simultaneous terminals should be definitively ambiguous"
    );
    ensure_no_new_panics(restart_log0)?;
    delete_terminal(&mut c3, d1);
    delete_terminal(&mut c3, d2);
    let _ = run_wsl_sh("pkill -f 'sleep 86399'; exit 0");
    ran.push("simultaneous");

    print!("[{}] ", ran.join("+"));
    Ok(())
}

/// §5.2 no-snapshot fallback (`ssh_cli_resume_fallback`): with the M0
/// sidecar DELETED, exactly-one store entry in the cwd-scoped store still
/// correlates (claude only); a second entry ⇒ Ambiguous with candidates,
/// inner_cli cleared, and NO probe on the following restore (DO-NOT 9 — no
/// retry storms after a definitive verdict).
fn case_ssh_cli_resume_fallback() -> anyhow::Result<()> {
    let host = ssh_cli_env()?;
    let master = master_token()?;
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4800u64;

    // Part 1: exactly-one fires.
    let cwd = "/tmp/tcprobe-home/projf";
    ssh_cli_wipe_store(cwd)?;
    let id = ssh_cli_setup(&mut c, &mut ctl, &mut rid, "__probe_sshcli_f1__", &host, cwd)?;
    ssh_cli_launch_bare(&mut c, id, cwd)?;
    let store = ssh_cli_wait_store(cwd, 1, true, 40)?;
    let uuid_a = store[0].0.trim_end_matches(".jsonl").to_string();
    std::fs::remove_file(crate::state::data_probes_dir().join(format!("{id}.json")))?;
    ssh_cli_kill_and_reconnect(&mut c, id)?;
    let want_cmd = format!("claude --resume {uuid_a}");
    c.await_blocks(id, 40, |recs| recs.iter().any(|r| r.cmd == want_cmd))?;
    anyhow::ensure!(
        log_since(log0).contains(&format!("remote claude session correlated -> {uuid_a}")),
        "fallback exactly-one did not correlate"
    );
    delete_terminal(&mut c, id);

    // Part 2: a second (old) entry ⇒ definitive Ambiguous, cleared, no retry.
    let cwd = "/tmp/tcprobe-home/projg";
    ssh_cli_wipe_store(cwd)?;
    let id = ssh_cli_setup(&mut c, &mut ctl, &mut rid, "__probe_sshcli_f2__", &host, cwd)?;
    ssh_cli_launch_bare(&mut c, id, cwd)?;
    let store = ssh_cli_wait_store(cwd, 1, true, 40)?;
    let uuid_live = store[0].0.trim_end_matches(".jsonl").to_string();
    let extra = Uuid::new_v4();
    run_wsl_sh(&format!(
        "echo extra > \"$HOME/{}/{extra}.jsonl\"",
        ssh_cli_store_dir(cwd)
    ))?;
    std::fs::remove_file(crate::state::data_probes_dir().join(format!("{id}.json")))?;
    ssh_cli_kill_and_reconnect(&mut c, id)?;
    ssh_cli_poll_state(20, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.inner_cli.is_none())
    })?;
    let text = strip_ansi(&String::from_utf8_lossy(&c.replay(id)?));
    anyhow::ensure!(
        text.contains(&format!("multiple claude sessions found in {cwd}"))
            && text.contains(&format!("claude --resume {uuid_live}"))
            && text.contains(&format!("claude --resume {extra}")),
        "fallback-ambiguous preface should list both candidates: {text:?}"
    );
    // No retry storm: with inner_cli cleared, the next death/reconnect must
    // not probe at all (the two-phase reconnect wait proves a full
    // death→launch cycle actually happened between the log marks).
    let mark = daemon_log_len();
    ssh_cli_kill_and_reconnect(&mut c, id)?;
    anyhow::ensure!(
        !log_since(mark).contains(&format!("[probe] {id}")),
        "a definitive Ambiguous must never re-probe on the next restore"
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// §4.6 password-auth cache (`ssh_cli_authdead`): the probe transport is
/// flipped to an auth-refusing stub (flag file consumed by the staged
/// wrapper), so M0 fails fast BatchMode-style ⇒ probes skip for the
/// terminal; the reconnect respawn's hooks CLEAR the cache (non-interactive
/// auth proven); with the stub off, the next restore correlates normally.
fn case_ssh_cli_authdead() -> anyhow::Result<()> {
    let host = ssh_cli_env()?;
    let master = master_token()?;
    let log0 = daemon_log_len();
    let cwd = "/tmp/tcprobe-home/projad";
    ssh_cli_wipe_store(cwd)?;
    run_wsl_sh("touch /tmp/tcprobe-authdead")?;
    let stub_off = |e: anyhow::Error| {
        let _ = run_wsl_sh("rm -f /tmp/tcprobe-authdead; exit 0");
        e
    };
    let body = (|| -> anyhow::Result<()> {
        let mut c = Conn::open()?;
        let _ = c.first_snapshot()?;
        let mut ctl = Conn::open_ctl(&master, None)?;
        let mut rid = 4900u64;
        let id = ssh_cli_setup(&mut c, &mut ctl, &mut rid, "__probe_sshcli_ad__", &host, cwd)?;
        c.send(&C2D::Input { id, bytes: b"claude\r".to_vec() })?;
        // M0 hits the auth wall: cache set, NO sidecar written.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !log_since(log0).contains("M0 auth requires interaction") {
            anyhow::ensure!(Instant::now() < deadline, "M0 never hit the auth-dead path");
            std::thread::sleep(Duration::from_millis(300));
        }
        anyhow::ensure!(
            ssh_cli_sidecar(id).is_none(),
            "auth-dead M0 must not write a sidecar"
        );
        // Death → reconnect: the correlate leg is SKIPPED (cache), the shell
        // restores, inner_cli is KEPT (only transport certainty clears it)…
        ssh_cli_kill_and_reconnect(&mut c, id)?;
        let log = log_since(log0);
        anyhow::ensure!(
            log.contains(&format!("[probe] {id}: skipped (auth-dead cache)")),
            "reconnect launch should skip the probe while auth-dead"
        );
        ssh_cli_poll_state(20, |s| {
            s.terminals.iter().any(|t| {
                t.id == id
                    && t.inner_cli
                        .as_ref()
                        .is_some_and(|cli| cli.confidence == CliConfidence::Ambiguous)
            })
        })?;
        // …and the respawn's own hooks prove non-interactive auth ⇒ cleared.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !log_since(log0).contains(&format!("[probe] {id}: auth-dead cleared")) {
            anyhow::ensure!(
                Instant::now() < deadline,
                "hooks_live respawn never cleared the auth-dead cache"
            );
            std::thread::sleep(Duration::from_millis(300));
        }
        // Stub off ⇒ the next death correlates via the §5.2 fallback (no
        // sidecar ever existed) and resumes.
        run_wsl_sh("rm -f /tmp/tcprobe-authdead; exit 0")?;
        let store = ssh_cli_wait_store(cwd, 1, true, 40)?;
        let uuid_a = store[0].0.trim_end_matches(".jsonl").to_string();
        ssh_cli_kill_and_reconnect(&mut c, id)?;
        let want_cmd = format!("claude --resume {uuid_a}");
        c.await_blocks(id, 40, |recs| recs.iter().any(|r| r.cmd == want_cmd))?;
        ensure_no_new_panics(log0)?;
        delete_terminal(&mut c, id);
        Ok(())
    })();
    body.map_err(stub_off)
}

/// Bug 2 (stale input-lane cwd): a `cd` must reach the Snapshot's live_cwd
/// by the NEXT PROMPT RENDER — the token-checked pre hook folds it and
/// broadcasts one coalesced Snapshot (no other traffic may be needed to
/// carry it). PEB tracking can't produce this for pwsh (pwsh never updates
/// its process cwd on Set-Location), so a passing case proves the hook fold.
fn case_cwd_broadcast() -> anyhow::Result<()> {
    let master = master_token()?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_probe_terminal(&mut c, "__probe_cwd__")?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4600u64;
    await_hooked_prompt(&mut ctl, &mut rid, id, 60)?;

    // ENV-LINEAGE SCRUB (Bug 1 root cause): a spawned terminal must not
    // carry Claude-Code session markers, whatever spawned the daemon (a
    // marker makes any claude inside treat itself as a child session and
    // skip transcript persistence ⇒ unresumable). This probe process runs
    // inside agent sessions routinely, so the daemon under test usually
    // HAS the markers — the assertion is that the terminal does NOT.
    let body = ctl_run_retry(
        &mut ctl,
        &mut rid,
        id,
        "\"TC_ENV_SCRUB=[$env:CLAUDECODE][$env:CLAUDE_CODE_CHILD_SESSION][$env:CLAUDE_CODE_ENTRYPOINT]\"",
        Some(RunWait {
            timeout_ms: 30_000,
            tail_bytes: 4096,
        }),
        60,
    )?;
    match &body {
        CtlBody::RunDone { output, .. } => anyhow::ensure!(
            output.contains("TC_ENV_SCRUB=[][][]"),
            "claude session env markers leaked into the terminal: {output:?}"
        ),
        other => anyhow::bail!("Run(env scrub check) returned {other:?}"),
    }

    let target = std::env::temp_dir().join("tc_probe_cwd");
    std::fs::create_dir_all(&target)?;
    // Raw keystrokes, not ctl Run: the ONLY Snapshot-bearing event on this
    // path is the pre-hook cwd fold itself.
    c.send(&C2D::Input {
        id,
        bytes: format!("cd '{}'\r", target.display()).into_bytes(),
    })?;
    let want = target.to_string_lossy().to_lowercase();
    c.snapshot_until(10, |s| {
        s.terminals.iter().any(|t| {
            t.id == id
                && t.live_cwd
                    .as_ref()
                    .is_some_and(|p| p.to_string_lossy().to_lowercase() == want)
        })
    })?;
    delete_terminal(&mut c, id);
    let _ = std::fs::remove_dir(&target);
    Ok(())
}

/// Create a Cmd probe terminal (P6b first-class shape: TermKind::Shell +
/// cmd.exe, no args — launch() writes the PROMPT-env bootstrap and spawn()
/// injects it) and wait until Running.
fn create_cmd_terminal(c: &mut Conn, name: &str, cwd: &str) -> anyhow::Result<Uuid> {
    c.send(&C2D::CreateTerminal {
        spec: NewTerminal {
            name: name.into(),
            folder: None,
            kind: TermKind::Shell,
            program: "cmd.exe".into(),
            args: Vec::new(),
            cwd: cwd.into(),
            already_launched: false,
            shell_cfg: None,
        },
    })?;
    let state = c.snapshot_until(10, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == name && t.status == TermStatus::Running)
    })?;
    Ok(state.terminals.iter().find(|t| t.name == name).unwrap().id)
}

/// Ground-truth block records via the controller (poll — raw-typed commands
/// produce no Blocks frames to await).
fn ctl_read_blocks(ctl: &mut Conn, rid: &mut u64, id: Uuid) -> anyhow::Result<Vec<BlockRec>> {
    *rid += 1;
    match ctl.ctl(*rid, CtlRequest::ReadBlocks { id, last: 200 }, 10)? {
        CtlBody::Blocks { recs } => Ok(recs),
        other => anyhow::bail!("ReadBlocks returned {other:?}"),
    }
}

/// P6b P3 `cmd_hooks`: a Cmd terminal comes up first-class — PROMPT-env
/// hooks live (token-checked pre + 9;9 + 133;B, and NO exec, ever), the
/// SubmitCommand ledger records write:true round-trips (synthetic block
/// closed by the next pre; exit None — honest; duration + cwd real) and
/// write:false observations (journal head unmoved), multi-line is refused
/// with an Error frame, and the D14 run gate refuses while `ping -t` runs.
fn case_cmd_hooks() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_cmd_terminal(&mut c, "__probe_cmd_hooks__", "C:\\")?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4400u64;
    // Hooks live: the first token-checked pre (rendered by the PROMPT env)
    // resolves Wait{Prompt} — proves env delivery, token, and grammar.
    await_hooked_prompt(&mut ctl, &mut rid, id, 30)?;

    // 1. SubmitCommand{write:true}: daemon writes the bytes AND opens a
    //    synthetic block; the NEXT pre closes it. The `ping -n 1` tail keeps
    //    the echo's output frame ahead of the prompt's OSC passthrough (the
    //    P2 ConPTY reorder — cmd's PROMPT cannot carry a drain sleep like
    //    the bash hook's, and its D* 133;A close anchor rides the same OSC
    //    burst as the pre, so a bare `echo`'s tail bytes can still clip
    //    past end_off; records stay correct, Copy-output is best-effort).
    c.send(&C2D::SubmitCommand {
        id,
        cmd: "echo CMD_OK_1& ping -n 1 127.0.0.1>nul".into(),
        write: true,
    })?;
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd.contains("CMD_OK_1") && r.end_off.is_some())
    })?;
    let rec = recs.iter().find(|r| r.cmd.contains("CMD_OK_1")).unwrap();
    anyhow::ensure!(
        rec.exit.is_none(),
        "cmd exit codes are permanently unavailable (D7), got {:?}",
        rec.exit
    );
    anyhow::ensure!(
        rec.ended_ms.is_some() && rec.ended_ms >= Some(rec.started_ms),
        "duration must be real: {:?}..{:?}",
        rec.started_ms,
        rec.ended_ms
    );
    // cwd rides the adjacent OSC 9;9 ($P) — the static pre payload is empty.
    let cwd = rec.cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
    anyhow::ensure!(
        cwd.as_deref() == Some("C:\\"),
        "block cwd should be the 9;9-reported C:\\, got {cwd:?}"
    );
    let start_off = rec.start_off;
    c.send(&C2D::BlockText { id, start_off })?;
    let (text, _) = c.await_block_text(id, start_off, 15)?;
    anyhow::ensure!(
        text.contains("CMD_OK_1"),
        "block output missing marker: {text:?}"
    );

    // 2. NO exec hook, ever: a command typed RAW (no GUI observer here)
    //    leaves zero records — cmd cannot announce executions shell-side.
    c.send(&C2D::Input {
        id,
        bytes: b"echo CMD_RAW_2\r".to_vec(),
    })?;
    std::thread::sleep(Duration::from_millis(1500));
    let all = ctl_read_blocks(&mut ctl, &mut rid, id)?;
    anyhow::ensure!(
        all.iter().all(|r| !r.cmd.contains("CMD_RAW_2")),
        "a raw-typed cmd command must leave no record (no exec hook): {:?}",
        all.iter().map(|r| &r.cmd).collect::<Vec<_>>()
    );

    // 3. SubmitCommand{write:false}: records WITHOUT a PTY write — the
    //    journal head must not move (quiesce first: the raw echo above needs
    //    its prompt render to flush).
    std::thread::sleep(Duration::from_millis(700));
    let jpath = crate::state::journals_dir().join(format!("{id}.log"));
    let len0 = std::fs::metadata(&jpath)?.len();
    c.send(&C2D::SubmitCommand {
        id,
        cmd: "echo CMD_OBS_3".into(),
        write: false,
    })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let all = ctl_read_blocks(&mut ctl, &mut rid, id)?;
        if let Some(r) = all.iter().find(|r| r.cmd == "echo CMD_OBS_3") {
            anyhow::ensure!(r.end_off.is_none(), "nothing ran yet — the record stays open");
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "write:false never produced a record"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let len1 = std::fs::metadata(&jpath)?.len();
    anyhow::ensure!(
        len1 == len0,
        "write:false must not move the journal head ({len0} -> {len1})"
    );
    // A bare Enter renders a fresh prompt; its pre closes the observed rec.
    c.send(&C2D::Input { id, bytes: b"\r".to_vec() })?;
    let _ = c.await_blocks(id, 15, |recs| {
        recs.iter()
            .any(|r| r.cmd == "echo CMD_OBS_3" && r.end_off.is_some())
    })?;

    // 4. Multi-line refused with an Error frame (Q2: no ledger queue in v1).
    c.send(&C2D::SubmitCommand {
        id,
        cmd: "echo A\necho B".into(),
        write: true,
    })?;
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            anyhow::ensure!(Instant::now() < deadline, "no refusal Error frame");
            if let Ok(D2C::Error { message }) = c.recv() {
                anyhow::ensure!(
                    message.contains("one line at a time"),
                    "unexpected error text: {message}"
                );
                break;
            }
        }
    }

    // 5. D14 run gate: while `ping -t` runs (typed raw — no open block
    //    exists to trip the ordinary busy gate), the cursor sits off the
    //    latched prompt-end column and output keeps flowing — Run refuses.
    c.send(&C2D::Input {
        id,
        bytes: b"ping -t 127.0.0.1\r".to_vec(),
    })?;
    std::thread::sleep(Duration::from_millis(1500));
    rid += 1;
    let body = ctl.ctl(
        rid,
        CtlRequest::Run {
            id,
            cmd: "echo NOPE".into(),
            force: false,
            force_self: false,
            wait: None,
        },
        10,
    )?;
    anyhow::ensure!(
        err_code(&body) == Some("busy"),
        "run gate must refuse while ping -t runs (D14), got {body:?}"
    );
    rid += 1;
    let _ = ctl.ctl(
        rid,
        CtlRequest::SendChord {
            id,
            chord: CtlChord::CtrlC,
            force_self: false,
        },
        10,
    )?;

    // 6. Back at an idle prompt, `tc run` semantics work end-to-end:
    //    RunStarted/RunDone through the synthetic ledger, exit None.
    {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            rid += 1;
            let body = ctl.ctl(
                rid,
                CtlRequest::Run {
                    id,
                    cmd: "echo CMD_RUN_4& ping -n 1 127.0.0.1>nul".into(),
                    force: false,
                    force_self: false,
                    wait: Some(RunWait {
                        timeout_ms: 20_000,
                        tail_bytes: 4096,
                    }),
                },
                30,
            )?;
            match &body {
                CtlBody::RunDone { exit, output, .. } => {
                    anyhow::ensure!(exit.is_none(), "cmd RunDone.exit must be None, got {exit:?}");
                    anyhow::ensure!(
                        output.contains("CMD_RUN_4"),
                        "RunDone output missing marker: {output:?}"
                    );
                    break;
                }
                CtlBody::Err { code, .. } if code == "busy" => {
                    anyhow::ensure!(
                        Instant::now() < deadline,
                        "run gate never re-opened after the interrupt"
                    );
                    std::thread::sleep(Duration::from_millis(500));
                }
                other => anyhow::bail!("Run(wait) on cmd returned {other:?}"),
            }
        }
    }

    // 7. Cold-attach PromptState: an idle hooked cmd prompt certifies
    //    at_prompt AND clean (clean carries the D14 quiet evidence for cmd).
    std::thread::sleep(Duration::from_millis(700));
    let (at_prompt, clean) = attach_prompt_state(id, 100, 28)?;
    anyhow::ensure!(at_prompt, "idle hooked cmd prompt must certify at_prompt");
    anyhow::ensure!(clean, "untouched cmd prompt must certify clean");

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

/// P6b P8 `cmd_restore`: kill + restore a Cmd terminal after a `cd` — the
/// 9;9-tracked live_cwd drives the respawn cwd, the PROMPT env is
/// re-injected (hooks live again in the new epoch), and the blocks sidecar
/// carries the old records across the epoch bump.
fn case_cmd_restore() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let id = create_cmd_terminal(&mut c, "__probe_cmd_restore__", "C:\\")?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4500u64;
    await_hooked_prompt(&mut ctl, &mut rid, id, 30)?;

    // cd + a marker block in the first epoch.
    c.send(&C2D::SubmitCommand {
        id,
        cmd: "cd \\Windows".into(),
        write: true,
    })?;
    let recs = c.await_blocks(id, 20, |recs| {
        recs.iter()
            .any(|r| r.cmd == "cd \\Windows" && r.end_off.is_some())
    })?;
    let old_epoch = recs.iter().map(|r| r.epoch).max().unwrap_or(0);
    anyhow::ensure!(old_epoch >= 1, "hooked spawn must have a real epoch");
    // The post-cd prompt's 9;9 lands in live_cwd via the tracker fold.
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let live: Option<String> = std::fs::read(state_path())
                .ok()
                .and_then(|b| serde_json::from_slice::<SharedState>(&b).ok())
                .and_then(|s| s.terminal(id).and_then(|t| t.live_cwd.clone()))
                .map(|p| p.to_string_lossy().into_owned());
            if live.as_deref() == Some("C:\\Windows") {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "live_cwd never became C:\\Windows (got {live:?})"
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    // Kill + restore (the GUI's Restart path).
    c.send(&C2D::KillTerminal { id })?;
    c.snapshot_until(10, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Dead)
    })?;
    c.send(&C2D::RestartTerminal { id })?;
    c.snapshot_until(15, |s| {
        s.terminal(id).is_some_and(|t| t.status == TermStatus::Running)
    })?;

    // PROMPT env re-injected: the NEW epoch's hooks go live (Wait{Prompt}
    // refuses with hooks_unverified until the fresh token-checked pre).
    await_hooked_prompt(&mut ctl, &mut rid, id, 30)?;

    // Respawn cwd: cmd's bare `cd` prints the current directory.
    let body = {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            rid += 1;
            let b = ctl.ctl(
                rid,
                CtlRequest::Run {
                    id,
                    cmd: "cd& ping -n 1 127.0.0.1>nul".into(),
                    force: false,
                    force_self: false,
                    wait: Some(RunWait {
                        timeout_ms: 20_000,
                        tail_bytes: 4096,
                    }),
                },
                30,
            )?;
            match &b {
                CtlBody::Err { code, .. } if code == "busy" || code == "hooks_unverified" => {
                    anyhow::ensure!(Instant::now() < deadline, "restored cmd never idle");
                    std::thread::sleep(Duration::from_millis(500));
                }
                _ => break b,
            }
        }
    };
    match &body {
        CtlBody::RunDone { output, exit, .. } => {
            anyhow::ensure!(exit.is_none(), "cmd RunDone.exit must be None");
            anyhow::ensure!(
                output
                    .lines()
                    .any(|l| l.trim().eq_ignore_ascii_case("C:\\Windows")),
                "restored cmd not in C:\\Windows: {output:?}"
            );
        }
        other => anyhow::bail!("Run(cd) returned {other:?}"),
    }

    // Sidecar continuity: epoch bumped, old-epoch records intact.
    let all = ctl_read_blocks(&mut ctl, &mut rid, id)?;
    let new_epoch = all.iter().map(|r| r.epoch).max().unwrap_or(0);
    anyhow::ensure!(
        new_epoch > old_epoch,
        "restore must bump the epoch ({old_epoch} -> {new_epoch})"
    );
    anyhow::ensure!(
        all.iter()
            .any(|r| r.epoch == old_epoch && r.cmd == "cd \\Windows"),
        "old-epoch records must survive the restore: {:?}",
        all.iter().map(|r| (r.epoch, &r.cmd)).collect::<Vec<_>>()
    );

    // Scrollback continuity: the restore seam machinery is family-agnostic —
    // no visible seam text leaks, and the per-real-spawn Windows banner shows
    // exactly ONCE (each spawn honestly prints one; the seam banner dedupe +
    // preface opening splice collapse prior lifetimes' identical copies —
    // the "~15 stacked banners across restarts" field bug).
    let mut c2 = Conn::open()?;
    let _ = c2.first_snapshot()?;
    let text = strip_ansi(&String::from_utf8_lossy(&c2.replay(id)?));
    anyhow::ensure!(!text.contains("tc:seam"), "restore seam leaked visible text");
    let banners = text
        .lines()
        .filter(|l| l.trim_start().starts_with("Microsoft Windows [Version"))
        .count();
    anyhow::ensure!(
        banners == 1,
        "expected exactly one cmd banner after the restore, found {banners}"
    );

    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c, id);
    Ok(())
}

// ───────────────────────────── SLEEP probes ─────────────────────────────

/// `create_probe_terminal` with a folder assignment (folder-sleep cases).
fn create_probe_terminal_in(
    c: &mut Conn,
    name: &str,
    folder: Option<Uuid>,
) -> anyhow::Result<Uuid> {
    c.send(&C2D::CreateTerminal {
        spec: NewTerminal {
            name: name.into(),
            folder,
            kind: TermKind::Shell,
            program: "powershell.exe".into(),
            args: vec!["-NoLogo".into()],
            cwd: "C:\\".into(),
            already_launched: false,
            shell_cfg: None,
        },
    })?;
    let state = c.snapshot_until(10, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == name && t.status == TermStatus::Running)
    })?;
    Ok(state.terminals.iter().find(|t| t.name == name).unwrap().id)
}

/// The root shell pid of a probe terminal, found by its PEB command line —
/// the spawn dot-sources `bootstrap\<id>.ps1`, so the terminal id is in the
/// cmdline. Retries through the spawn window.
fn find_shell_pid(id: Uuid, secs: u64) -> anyhow::Result<u32> {
    let needle = id.to_string();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        for (pid, _, exe) in crate::daemon::procinfo::snapshot_processes() {
            if !exe.eq_ignore_ascii_case("powershell.exe") {
                continue;
            }
            let hit = crate::daemon::procinfo::read_process_cmdline(pid)
                .is_some_and(|args| args.iter().any(|a| a.contains(&needle)));
            if hit {
                return Ok(pid);
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "no powershell.exe cmdline mentions terminal {id} within {secs}s"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// conhost.exe children of the daemon process (ConPTY parents its conhost to
/// the pseudoconsole creator).
fn conhost_children(daemon_pid: u32) -> Vec<u32> {
    crate::daemon::procinfo::snapshot_processes()
        .into_iter()
        .filter(|(_, ppid, exe)| *ppid == daemon_pid && exe.eq_ignore_ascii_case("conhost.exe"))
        .map(|(pid, _, _)| pid)
        .collect()
}

/// The presented `status` string of one terminal from a controller Listing.
fn listing_status(ctl: &mut Conn, rid: &mut u64, id: Uuid) -> anyhow::Result<String> {
    *rid += 1;
    match ctl.ctl(*rid, CtlRequest::List, 10)? {
        CtlBody::Listing { terminals, .. } => terminals
            .into_iter()
            .find(|t| t.id == id)
            .map(|t| t.status)
            .ok_or_else(|| anyhow::anyhow!("terminal {id} missing from Listing")),
        other => anyhow::bail!("List returned {other:?}"),
    }
}

fn await_listing_status(
    ctl: &mut Conn,
    rid: &mut u64,
    id: Uuid,
    want: &str,
    secs: u64,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let got = listing_status(ctl, rid, id)?;
        if got == want {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "terminal {id} never reached status {want:?} (last {got:?}) within {secs}s"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// P-S1 `sleep_roundtrip` — the acceptance case: idle hooked shell sleeps
/// gate-free; the whole process tree is GONE (Toolhelp — the RAM-reclaim
/// proxy; RSS bands stay out of probes by design); journal + blocks sidecar
/// are byte-identical at rest; state.json persists the flag; a daemon
/// restart honors the boot skip (asleep stays asleep while an auto_restore
/// sibling respawns — both polarities); Ctl Wake restores a hooked prompt
/// with the pre-sleep block history intact and ReplayAnchors re-minting the
/// pre-sleep command's row hint on a fresh attach.
fn case_sleep_roundtrip() -> anyhow::Result<()> {
    ensure_isolated_daemon("sleep_roundtrip")?;
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut legacy = Conn::open()?;
    let _ = legacy.first_snapshot()?;
    let info: DaemonInfo = serde_json::from_slice(&std::fs::read(daemon_info_path())?)?;
    // Diff the daemon's conhost children around the TARGET terminal's spawn
    // alone (the sibling is created after the second snapshot) so the
    // session's conhost is attributable.
    let conhosts_before = conhost_children(info.pid);
    let id = create_probe_terminal(&mut legacy, "__probe_sleep_rt__")?;
    let session_conhosts: Vec<u32> = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let fresh: Vec<u32> = conhost_children(info.pid)
                .into_iter()
                .filter(|p| !conhosts_before.contains(p))
                .collect();
            if !fresh.is_empty() || Instant::now() >= deadline {
                break fresh;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let sib = create_probe_terminal(&mut legacy, "__probe_sleep_sib__")?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 6000u64;

    // One real block so wake can prove history survived.
    match ctl_run_retry(
        &mut ctl,
        &mut rid,
        id,
        "cmd /c echo SLEEP_MARK_1",
        Some(RunWait {
            timeout_ms: 15_000,
            tail_bytes: 4096,
        }),
        30,
    )? {
        CtlBody::RunDone { exit, output, .. } => {
            anyhow::ensure!(exit == Some(0), "pre-sleep run exit {exit:?}");
            anyhow::ensure!(output.contains("SLEEP_MARK_1"), "output {output:?}");
        }
        other => anyhow::bail!("pre-sleep Run returned {other:?}"),
    }
    let shell_pid = find_shell_pid(id, 10)?;

    // S7: idle = output-quiet ≥3s; wait it out so the no-force sleep is the
    // friction-free headline path.
    std::thread::sleep(Duration::from_millis(3200));
    let jpath = crate::state::journals_dir().join(format!("{id}.log"));
    let spath = crate::state::journals_dir().join(format!("{id}.blocks.json"));
    let jbytes = std::fs::read(&jpath)?;
    let sbytes = std::fs::read(&spath)?;

    rid += 1;
    match ctl.ctl(
        rid,
        CtlRequest::Sleep {
            id,
            force: false,
            force_self: false,
        },
        20,
    )? {
        CtlBody::Done => {}
        other => anyhow::bail!("idle Sleep refused: {other:?}"),
    }
    await_listing_status(&mut ctl, &mut rid, id, "asleep", 10)?;

    // Process absence — the deterministic reclaim proxy: the root shell,
    // anything whose cmdline mentions the terminal id, and the session's
    // conhost(s) are all gone.
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let table = crate::daemon::procinfo::snapshot_processes();
            let shell_alive = table.iter().any(|(p, ..)| *p == shell_pid);
            let conhost_alive = table
                .iter()
                .any(|(p, ..)| session_conhosts.contains(p));
            if !shell_alive && !conhost_alive {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "process tree survived sleep (shell={shell_alive} conhost={conhost_alive})"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        let needle = id.to_string();
        let lingering = crate::daemon::procinfo::snapshot_processes()
            .iter()
            .filter(|(_, _, exe)| {
                exe.eq_ignore_ascii_case("powershell.exe")
                    || exe.eq_ignore_ascii_case("cmd.exe")
            })
            .any(|(pid, ..)| {
                crate::daemon::procinfo::read_process_cmdline(*pid)
                    .is_some_and(|args| args.iter().any(|a| a.contains(&needle)))
            });
        anyhow::ensure!(!lingering, "a process still references the slept terminal");
    }

    // Persistence identity (inv. 1): the pre-sleep journal is a byte-exact
    // PREFIX of the post-sleep journal, and the only appended bytes are
    // conhost's ConPTY-teardown mode resets (observed: ?9001l + ?1004l) —
    // escape sequences with ZERO visible text (the reader journals them
    // because mirror purity forbids dropping real conhost output; a daemon
    // shutdown simply exits before reading them). The sidecar is
    // byte-identical outright.
    {
        let jafter = std::fs::read(&jpath)?;
        anyhow::ensure!(
            jafter.len() >= jbytes.len() && jafter[..jbytes.len()] == jbytes[..],
            "pre-sleep journal is not a prefix of the post-sleep journal"
        );
        let suffix = &jafter[jbytes.len()..];
        anyhow::ensure!(
            suffix.len() <= 64,
            "sleep appended {} bytes to the journal (teardown resets are ~16)",
            suffix.len()
        );
        let mut stripped = Vec::new();
        let mut stripper = crate::strip::AnsiStripper::default();
        stripper.feed_bytes(suffix, &mut stripped);
        let text = String::from_utf8_lossy(&stripped);
        anyhow::ensure!(
            text.trim().is_empty(),
            "sleep appended VISIBLE journal content: {text:?}"
        );
    }
    anyhow::ensure!(
        std::fs::read(&spath)? == sbytes,
        "blocks sidecar changed across sleep"
    );
    // state.json carries the flag (what a reboot restores from).
    {
        let sj: serde_json::Value = serde_json::from_slice(&std::fs::read(state_path())?)?;
        let flag = sj["terminals"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["id"] == id.to_string())
            .and_then(|t| t["asleep"].as_bool());
        anyhow::ensure!(flag == Some(true), "state.json asleep flag: {flag:?}");
    }

    // Boot skip, both polarities: restart the daemon; the sibling
    // auto-restores, the asleep terminal stays down.
    let old_pid = info.pid;
    legacy.send(&C2D::Shutdown)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if legacy.recv().is_err() {
            break;
        }
    }
    let lock = crate::state::data_dir().join("daemon.lock");
    for _ in 0..50 {
        if !lock.exists() || std::fs::remove_file(&lock).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        std::process::Command::new(std::env::current_exe()?)
            .arg("--daemon")
            .creation_flags(DETACHED_PROCESS)
            .spawn()?;
    }
    let mut c2 = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            anyhow::ensure!(Instant::now() < deadline, "restarted daemon never came up");
            std::thread::sleep(Duration::from_millis(200));
            let fresh = std::fs::read(daemon_info_path())
                .ok()
                .and_then(|b| serde_json::from_slice::<DaemonInfo>(&b).ok())
                .is_some_and(|i| i.pid != old_pid);
            if !fresh {
                continue;
            }
            if let Ok(conn) = Conn::open() {
                break conn;
            }
        }
    };
    c2.snapshot_until(30, |s| {
        s.terminal(sib).is_some_and(|t| t.status == TermStatus::Running)
    })?;
    // Give the restore lanes a beat, then pin both polarities.
    std::thread::sleep(Duration::from_millis(1000));
    let st = c2.snapshot_until(5, |_| true).or_else(|_| {
        // No fresh broadcast pending — ask via a state-mutating no-op-free
        // path: reconnect for a Hello snapshot.
        Conn::open().and_then(|mut c| c.first_snapshot())
    })?;
    let t = st
        .terminal(id)
        .ok_or_else(|| anyhow::anyhow!("asleep terminal vanished across restart"))?;
    anyhow::ensure!(
        t.status == TermStatus::Dead && t.asleep,
        "boot restore did not skip the asleep terminal (status {:?}, asleep {})",
        t.status,
        t.asleep
    );
    {
        let needle = id.to_string();
        let respawned = crate::daemon::procinfo::snapshot_processes()
            .iter()
            .filter(|(_, _, exe)| exe.eq_ignore_ascii_case("powershell.exe"))
            .any(|(pid, ..)| {
                crate::daemon::procinfo::read_process_cmdline(*pid)
                    .is_some_and(|args| args.iter().any(|a| a.contains(&needle)))
            });
        anyhow::ensure!(!respawned, "boot restore respawned the asleep terminal");
    }

    // Wake: launch()-verbatim — hooked prompt back, history intact, anchors
    // re-mint. (Spec asks ≤5s to prompt; asserted at ≤10s here because the
    // suite runs on a live user machine — the measured value prints.)
    let master2 = master_token()?;
    let mut ctl2 = Conn::open_ctl(&master2, None)?;
    let mut rid2 = 6050u64;
    let wake_t0 = Instant::now();
    rid2 += 1;
    match ctl2.ctl(rid2, CtlRequest::Wake { id }, 20)? {
        CtlBody::Done => {}
        other => anyhow::bail!("Wake returned {other:?}"),
    }
    await_hooked_prompt(&mut ctl2, &mut rid2, id, 15)?;
    let wake_ms = wake_t0.elapsed().as_millis();
    println!("(wake→hooked-prompt {wake_ms}ms) ");
    anyhow::ensure!(wake_ms <= 10_000, "wake to prompt took {wake_ms}ms");
    let recs = ctl_read_blocks(&mut ctl2, &mut rid2, id)?;
    let pre_rec = recs
        .iter()
        .find(|r| r.cmd.contains("SLEEP_MARK_1"))
        .ok_or_else(|| anyhow::anyhow!("pre-sleep block lost across sleep/wake"))?;
    let pre_off = pre_rec.start_off;
    await_listing_status(&mut ctl2, &mut rid2, id, "running", 10)?;
    // Fresh attach: Replay→StreamPos→Blocks→…→ReplayAnchors with a hint for
    // the pre-sleep command (covers re-mint — history-parity machinery).
    std::thread::sleep(Duration::from_millis(600));
    let vb = attach_view(id, 120, 30, 20)?;
    anyhow::ensure!(
        vb.recs.iter().any(|r| r.start_off == pre_off),
        "attach Blocks full-sync lost the pre-sleep record"
    );
    anyhow::ensure!(
        vb.hints
            .iter()
            .any(|h| h.kind == ANCHOR_BLOCK && h.start_off == pre_off),
        "no ReplayAnchors block hint for the pre-sleep command (hints: {:?})",
        vb.hints.len()
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut c2, id);
    delete_terminal(&mut c2, sib);
    Ok(())
}

/// P-S2 `sleep_busy_gate`: an open block gates a no-force sleep with the
/// offender named; --force sleeps anyway and the dangling block closes
/// exit=None (reboot parity); Wake on a running sibling refuses not_asleep;
/// Run AND SendRaw against the asleep terminal refuse "asleep" (S9 — input
/// never wakes).
fn case_sleep_busy_gate() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut legacy = Conn::open()?;
    let _ = legacy.first_snapshot()?;
    let id = create_probe_terminal(&mut legacy, "__probe_sleep_busy__")?;
    let sib = create_probe_terminal(&mut legacy, "__probe_sleep_busy_sib__")?;
    let mut c = Conn::open_ctl(&master, None)?;
    let mut rid = 6100u64;

    // Open a long-running block.
    match ctl_run_retry(&mut c, &mut rid, id, "ping -t 127.0.0.1", None, 25)? {
        CtlBody::RunStarted { .. } => {}
        other => anyhow::bail!("ping Run returned {other:?}"),
    }
    {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let recs = ctl_read_blocks(&mut c, &mut rid, id)?;
            if recs
                .iter()
                .any(|r| r.cmd.contains("ping -t") && r.end_off.is_none())
            {
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "ping block never opened");
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    // Gate: no-force refuses, naming the command.
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::Sleep {
            id,
            force: false,
            force_self: false,
        },
        10,
    )? {
        CtlBody::Err { code, msg } if code == "busy" => {
            anyhow::ensure!(msg.contains("ping"), "busy msg lacks the command: {msg}");
        }
        other => anyhow::bail!("busy Sleep returned {other:?}"),
    }
    // Wake on the RUNNING sibling refuses not_asleep (never surprise-restart).
    rid += 1;
    match c.ctl(rid, CtlRequest::Wake { id: sib }, 10)? {
        CtlBody::Err { code, .. } if code == "not_asleep" => {}
        other => anyhow::bail!("Wake(running) returned {other:?}"),
    }
    // --force sleeps through the open block.
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::Sleep {
            id,
            force: true,
            force_self: false,
        },
        20,
    )? {
        CtlBody::Done => {}
        other => anyhow::bail!("forced Sleep returned {other:?}"),
    }
    await_listing_status(&mut c, &mut rid, id, "asleep", 10)?;
    // S9: input verbs refuse "asleep" — never auto-wake.
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::Run {
            id,
            cmd: "echo NO".into(),
            force: false,
            force_self: false,
            wait: None,
        },
        10,
    )? {
        CtlBody::Err { code, msg } if code == "asleep" => {
            anyhow::ensure!(msg.contains("wake"), "asleep msg should name the fix: {msg}");
        }
        other => anyhow::bail!("Run(asleep) returned {other:?}"),
    }
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::SendRaw {
            id,
            bytes: b"x".to_vec(),
            force_self: false,
        },
        10,
    )? {
        CtlBody::Err { code, .. } if code == "asleep" => {}
        other => anyhow::bail!("SendRaw(asleep) returned {other:?}"),
    }
    // Sleeping again refuses "asleep" (idempotence is a refusal, not a kill).
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::Sleep {
            id,
            force: true,
            force_self: false,
        },
        10,
    )? {
        CtlBody::Err { code, .. } if code == "asleep" => {}
        other => anyhow::bail!("double Sleep returned {other:?}"),
    }
    // Wake; the dangling ping block closed exit=None (the reboot-mid-command
    // shape) and the shell is interactive again.
    rid += 1;
    match c.ctl(rid, CtlRequest::Wake { id }, 20)? {
        CtlBody::Done => {}
        other => anyhow::bail!("Wake returned {other:?}"),
    }
    await_hooked_prompt(&mut c, &mut rid, id, 15)?;
    let recs = ctl_read_blocks(&mut c, &mut rid, id)?;
    let ping = recs
        .iter()
        .find(|r| r.cmd.contains("ping -t"))
        .ok_or_else(|| anyhow::anyhow!("ping record lost across forced sleep"))?;
    anyhow::ensure!(
        ping.end_off.is_some() && ping.exit.is_none(),
        "dangling block should close exit=None (end {:?}, exit {:?})",
        ping.end_off,
        ping.exit
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut legacy, id);
    delete_terminal(&mut legacy, sib);
    Ok(())
}

/// P-S3 `sleep_waiters_folder`: a folder sleep fails the target's non-Exit
/// waiters with code "asleep" while its Exit waiter resolves Exited (S11);
/// both members go down off ONE shared drain window (wall-clock bound);
/// WakeFolder staggers both back to hooked prompts; the folder's dead
/// (non-asleep) third member is untouched in both directions (S16).
fn case_sleep_waiters_folder() -> anyhow::Result<()> {
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut legacy = Conn::open()?;
    let _ = legacy.first_snapshot()?;
    let fname = "__probe_sleep_folder__";
    legacy.send(&C2D::CreateFolder {
        name: fname.into(),
    })?;
    let st = legacy.snapshot_until(10, |s| s.folders.iter().any(|f| f.name == fname))?;
    let fid = st.folders.iter().find(|f| f.name == fname).unwrap().id;
    let a = create_probe_terminal_in(&mut legacy, "__probe_slf_a__", Some(fid))?;
    let b = create_probe_terminal_in(&mut legacy, "__probe_slf_b__", Some(fid))?;
    let d = create_probe_terminal_in(&mut legacy, "__probe_slf_dead__", Some(fid))?;

    let mut c = Conn::open_ctl(&master, None)?;
    let mut rid = 6200u64;
    await_hooked_prompt(&mut c, &mut rid, a, 30)?;
    await_hooked_prompt(&mut c, &mut rid, b, 30)?;
    // The dead-not-asleep member (S16: dead means "died", not "shelved").
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::Kill {
            id: d,
            force_self: false,
        },
        10,
    )? {
        CtlBody::Done => {}
        other => anyhow::bail!("Kill returned {other:?}"),
    }
    legacy.snapshot_until(10, |s| {
        s.terminal(d).is_some_and(|t| t.status == TermStatus::Dead)
    })?;

    // Park two waiters on `a` from a separate controller conn: a BlockClose
    // that can never resolve (after u64::MAX) and an Exit.
    let mut w = Conn::open_ctl(&master, None)?;
    w.send(&C2D::Ctl {
        req_id: 1,
        req: CtlRequest::Wait {
            id: a,
            cond: WaitCond::BlockClose {
                after_off: u64::MAX,
            },
            timeout_ms: 60_000,
        },
    })?;
    w.send(&C2D::Ctl {
        req_id: 2,
        req: CtlRequest::Wait {
            id: a,
            cond: WaitCond::Exit,
            timeout_ms: 60_000,
        },
    })?;
    std::thread::sleep(Duration::from_millis(300)); // registration settles

    // Folder sleep (force — the CLI bulk spelling P-S3 pins): ONE shared
    // drain window, so the Done lands well inside a single 2s cap.
    let t0 = Instant::now();
    rid += 1;
    match c.ctl(
        rid,
        CtlRequest::SleepFolder {
            folder: fid,
            force: true,
        },
        20,
    )? {
        CtlBody::Done => {}
        other => anyhow::bail!("SleepFolder returned {other:?}"),
    }
    let wall = t0.elapsed();
    anyhow::ensure!(
        wall < Duration::from_secs(4),
        "folder sleep took {wall:?} — the drain window is not shared"
    );

    // Waiter outcomes (S11): BlockClose fails "asleep" BEFORE the kill;
    // Exit resolves truthfully.
    {
        let (mut saw_fail, mut saw_exit) = (false, false);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !(saw_fail && saw_exit) {
            match w.recv() {
                Ok(D2C::Ctl { req_id: 1, body }) => match body {
                    CtlBody::Err { code, .. } if code == "asleep" => saw_fail = true,
                    other => anyhow::bail!("BlockClose waiter got {other:?}"),
                },
                Ok(D2C::Ctl { req_id: 2, body }) => match body {
                    CtlBody::Waited {
                        hit: WaitHit::Exited { .. },
                    } => saw_exit = true,
                    other => anyhow::bail!("Exit waiter got {other:?}"),
                },
                _ => {}
            }
        }
        anyhow::ensure!(saw_fail, "BlockClose waiter never failed 'asleep'");
        anyhow::ensure!(saw_exit, "Exit waiter never resolved Exited");
    }

    await_listing_status(&mut c, &mut rid, a, "asleep", 10)?;
    await_listing_status(&mut c, &mut rid, b, "asleep", 10)?;
    anyhow::ensure!(
        listing_status(&mut c, &mut rid, d)? == "dead",
        "folder sleep touched the dead member"
    );

    // WakeFolder: both members return to hooked prompts (staggered); the
    // dead member stays dead (wake resurrects what sleep suspended, S16).
    rid += 1;
    match c.ctl(rid, CtlRequest::WakeFolder { folder: fid }, 10)? {
        CtlBody::Done => {}
        other => anyhow::bail!("WakeFolder returned {other:?}"),
    }
    // The wake is staggered on daemon worker lanes (Done returns before the
    // spawns land): wait for Running before asking for a hooked prompt.
    await_listing_status(&mut c, &mut rid, a, "running", 20)?;
    await_listing_status(&mut c, &mut rid, b, "running", 20)?;
    await_hooked_prompt(&mut c, &mut rid, a, 30)?;
    await_hooked_prompt(&mut c, &mut rid, b, 30)?;
    anyhow::ensure!(
        listing_status(&mut c, &mut rid, d)? == "dead",
        "folder wake resurrected the dead member"
    );
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut legacy, a);
    delete_terminal(&mut legacy, b);
    delete_terminal(&mut legacy, d);
    let _ = legacy.send(&C2D::DeleteFolder { id: fid });
    Ok(())
}

/// P-S4 `sleep_freeze_frame` (sleep-spec §17): the freeze-frame pipeline
/// end-to-end on a synthetic quiet alt-screen TUI that STAYS RUNNING (open
/// block — the `tc run claude` shape). (1) The S7 refinement lets the
/// no-force sleep pass despite the open block; (2) the pre-kill capture
/// lands in journals/<id>.frame, crc-valid, alt-flagged, holding the TUI's
/// row text; (3) a fresh attach replay carries the serialized underlay THEN
/// `?1049h` + the frame bytes, with ReplayAnchors hints skipped under the
/// overlay; (4) wake removes the sidecar, restores a hooked prompt, and the
/// dangling block closed exit=None.
fn case_sleep_freeze_frame() -> anyhow::Result<()> {
    ensure_isolated_daemon("sleep_freeze_frame")?;
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut legacy = Conn::open()?;
    let _ = legacy.first_snapshot()?;
    let id = create_probe_terminal(&mut legacy, "__probe_sleep_frz__")?;
    let mut c = Conn::open_ctl(&master, None)?;
    let mut rid = 6300u64;

    // A synthetic idle TUI: enter the alt screen, print rows, park. Runs via
    // a temp script so no escape bytes need to survive prompt quoting.
    let script = std::env::temp_dir().join("tc_probe_freeze_tui.ps1");
    std::fs::write(
        &script,
        concat!(
            "$e=[char]27\n",
            "[Console]::Write(\"$e[?1049h$e[2J$e[H\")\n",
            "[Console]::Write(\"FRZ ROW ONE`r`nFRZ ROW TWO`r`n\")\n",
            "[Console]::Write(\"$e[5;3HFRZ DEEP CELL\")\n",
            "Start-Sleep 120\n",
        ),
    )?;
    match ctl_run_retry(
        &mut c,
        &mut rid,
        id,
        &format!(
            "powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
            script.display()
        ),
        None,
        25,
    )? {
        CtlBody::RunStarted { .. } => {}
        other => anyhow::bail!("TUI Run returned {other:?}"),
    }
    // Alt screen live in the daemon mirror, rows drawn, block open.
    {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            rid += 1;
            let alt_live = match c.ctl(rid, CtlRequest::ReadScreen { id }, 10)? {
                CtlBody::Screen { lines, alt_screen, .. } => {
                    alt_screen && lines.iter().any(|l| l.contains("FRZ ROW ONE"))
                }
                other => anyhow::bail!("ReadScreen returned {other:?}"),
            };
            let block_open = ctl_read_blocks(&mut c, &mut rid, id)?
                .iter()
                .any(|r| r.cmd.contains("tc_probe_freeze_tui") && r.end_off.is_none());
            if alt_live && block_open {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "synthetic TUI never settled (alt={alt_live} open={block_open})"
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    // §17.5: quiet ≥ SLEEP_QUIET_MS ⇒ the open block does NOT gate. Busy
    // retries only ride out the quiet window (the TUI parks for 120s, so a
    // REGRESSED gate stays busy past the deadline and still fails here).
    std::thread::sleep(Duration::from_millis(3200));
    {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            rid += 1;
            match c.ctl(
                rid,
                CtlRequest::Sleep { id, force: false, force_self: false },
                20,
            )? {
                CtlBody::Done => break,
                CtlBody::Err { code, .. } if code == "busy" && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(500));
                }
                other => anyhow::bail!(
                    "no-force sleep of a quiet alt-screen TUI must pass (S7 refinement): {other:?}"
                ),
            }
        }
    }
    await_listing_status(&mut c, &mut rid, id, "asleep", 10)?;

    // The frame sidecar: present, decodable, alt-flagged, rows intact.
    let fpath = crate::daemon::frame::path(id);
    let f = crate::daemon::frame::decode(&std::fs::read(&fpath)?)
        .ok_or_else(|| anyhow::anyhow!("frame sidecar failed to decode"))?;
    anyhow::ensure!(f.alt, "frame must be alt-flagged");
    anyhow::ensure!(f.cols >= 2 && f.rows >= 2, "frame geometry {}x{}", f.cols, f.rows);
    let ftext = String::from_utf8_lossy(&f.bytes).into_owned();
    for needle in ["FRZ ROW ONE", "FRZ ROW TWO", "FRZ DEEP CELL"] {
        anyhow::ensure!(ftext.contains(needle), "frame lost {needle:?}: {ftext:?}");
    }

    // Attach replay: underlay first, then ?1049h + the frame; hints skipped.
    let vb = attach_view_tolerant(id, 120, 30, 20)?;
    let rtext = String::from_utf8_lossy(&vb.replay).into_owned();
    let alt_pos = rtext
        .find("\x1b[?1049h")
        .ok_or_else(|| anyhow::anyhow!("asleep attach replay carries no alt overlay"))?;
    anyhow::ensure!(alt_pos > 0, "the overlay must FOLLOW a serialized underlay");
    let frame_pos = rtext
        .rfind("FRZ DEEP CELL")
        .ok_or_else(|| anyhow::anyhow!("frame rows missing from the replay"))?;
    anyhow::ensure!(
        frame_pos > alt_pos,
        "frame rows must land after the alt enter (frame {frame_pos} vs alt {alt_pos})"
    );
    anyhow::ensure!(
        vb.hints.is_empty(),
        "hints must be skipped under a frame overlay (rows point into the hidden primary)"
    );

    // Wake: sidecar gone, hooked prompt back, dangling block closed honest.
    rid += 1;
    match c.ctl(rid, CtlRequest::Wake { id }, 20)? {
        CtlBody::Done => {}
        other => anyhow::bail!("Wake returned {other:?}"),
    }
    await_hooked_prompt(&mut c, &mut rid, id, 15)?;
    anyhow::ensure!(
        !fpath.exists(),
        "frame sidecar must be removed by the wake's success path"
    );
    let recs = ctl_read_blocks(&mut c, &mut rid, id)?;
    let tui = recs
        .iter()
        .find(|r| r.cmd.contains("tc_probe_freeze_tui"))
        .ok_or_else(|| anyhow::anyhow!("TUI block lost across sleep/wake"))?;
    anyhow::ensure!(
        tui.end_off.is_some() && tui.exit.is_none(),
        "dangling TUI block should close exit=None (end {:?}, exit {:?})",
        tui.end_off,
        tui.exit
    );
    ensure_no_new_panics(log0)?;
    let _ = std::fs::remove_file(&script);
    delete_terminal(&mut legacy, id);
    Ok(())
}

/// P-S5 `frame_corrupt_degrade` (sleep-spec §17.2): a primary-screen sleep
/// writes NO frame; a planted garbage sidecar and a truncated-real-encode
/// sidecar each degrade the asleep attach to the plain serialize_dead replay
/// (no `?1049h`), are REMOVED on first read, and never surface an error or
/// block the wake.
fn case_frame_corrupt_degrade() -> anyhow::Result<()> {
    ensure_isolated_daemon("frame_corrupt_degrade")?;
    let log0 = daemon_log_len();
    let master = master_token()?;
    let mut legacy = Conn::open()?;
    let _ = legacy.first_snapshot()?;
    let id = create_probe_terminal(&mut legacy, "__probe_frame_corrupt__")?;
    let mut c = Conn::open_ctl(&master, None)?;
    let mut rid = 6400u64;

    // Idle primary-screen sleep: no frame by design (v1 scope). The fresh
    // spawn's boot output can hold the quiet window open — ride it out.
    std::thread::sleep(Duration::from_millis(3200));
    {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            rid += 1;
            match c.ctl(
                rid,
                CtlRequest::Sleep { id, force: false, force_self: false },
                20,
            )? {
                CtlBody::Done => break,
                CtlBody::Err { code, .. } if code == "busy" && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(500));
                }
                other => anyhow::bail!("idle Sleep refused: {other:?}"),
            }
        }
    }
    await_listing_status(&mut c, &mut rid, id, "asleep", 10)?;
    let fpath = crate::daemon::frame::path(id);
    anyhow::ensure!(
        !fpath.exists(),
        "a primary-screen sleep must not write a frame sidecar"
    );

    // Plant garbage; the attach must degrade cleanly and remove it.
    std::fs::write(&fpath, b"PFRZ this is not a frame at all")?;
    let vb = attach_view_tolerant(id, 120, 30, 20)?;
    anyhow::ensure!(
        !vb.replay.windows(8).any(|w| w == b"\x1b[?1049h"),
        "corrupt sidecar must not produce an alt overlay"
    );
    anyhow::ensure!(!fpath.exists(), "corrupt sidecar removed on first read");

    // Truncated REAL encode (torn write shape): same degrade.
    let enc = crate::daemon::frame::encode(80, 24, true, b"\x1b[1;1HFRZ FAKE")
        .expect("tiny payload under cap");
    std::fs::write(&fpath, &enc[..enc.len() - 2])?;
    let vb2 = attach_view_tolerant(id, 120, 30, 20)?;
    anyhow::ensure!(
        !vb2.replay.windows(8).any(|w| w == b"\x1b[?1049h"),
        "truncated sidecar must not produce an alt overlay"
    );
    anyhow::ensure!(!fpath.exists(), "truncated sidecar removed on first read");

    // The wake is untouched by any of it.
    rid += 1;
    match c.ctl(rid, CtlRequest::Wake { id }, 20)? {
        CtlBody::Done => {}
        other => anyhow::bail!("Wake returned {other:?}"),
    }
    await_hooked_prompt(&mut c, &mut rid, id, 15)?;
    ensure_no_new_panics(log0)?;
    delete_terminal(&mut legacy, id);
    Ok(())
}

/// CLAUDE-SESSION BEACON end-to-end (attribution Layer 3, `claude_beacon`):
/// the staged fake claude prints the tcbeacon OSC to /dev/tty inside the
/// WSL stand-in — the exact byte path a consent-installed remote hook
/// script rides (pty → transport → ConPTY → journal → BlockScanner).
/// A) SessionStart(startup) upgrades the bare launch's Ambiguous inner_cli
///    to Explicit(<sid>) with ZERO probes, then the /clear analog
///    (SessionEnd(clear) + SessionStart(clear), NEW sid) switches the token
///    LIVE — the daemon.log carries both accepts;
/// B) sleep → wake resumes the SWITCHED conversation directly (`claude
///    --resume <new>` — Explicit skips the correlate leg entirely; no
///    "correlated ->", no "definitively ambiguous");
/// C) anti-spoof: a beacon emitted at a plain prompt (no open claude-family
///    block) is dropped — inner_cli stays untouched;
/// D) the remote installer (`install_remote` over the staged transport):
///    fresh install lands an executable ~/.tc/claude-hook.sh + merges the
///    hook entries into ~/.claude/settings.json non-destructively; a re-run
///    reports Already (idempotent). Pre-existing remote settings.json bytes
///    are restored verbatim afterwards.
fn case_claude_beacon() -> anyhow::Result<()> {
    let host = ssh_cli_env()?;
    // The staged fake claude must speak the beacon dialect (rr-stage4).
    let staged = run_wsl_sh("grep -q tcbeacon \"$HOME/bin/claude\" && echo OK || echo OLD")?;
    if !staged.contains("OK") {
        return Err(skip(
            "fake claude predates the beacon (re-run the rr-stage4 recipe)".into(),
        ));
    }
    // Beacon + rotate BEFORE launch: the fake claude then emits
    // SessionStart(startup, u1) → …3s… → SessionEnd(clear, u1) +
    // SessionStart(clear, u2) — the in-TUI /clear switch shape.
    run_wsl_sh("touch /tmp/tcprobe-beacon /tmp/tcprobe-rotate")?;
    let result = claude_beacon_body(&host);
    // ALWAYS clean, pass or fail (r2 roll: `ensure!`/`?` exits used to
    // bypass the old per-call cleanup closure, leaving the beacon flag to
    // poison every later fake-claude launch into Explicit-instead-of-
    // Correlated, plus a ~24h `sleep 86399` fake claude). ssh_cli_env's
    // defensive pre-clean is the second belt.
    let _ = run_wsl_sh(
        "rm -f /tmp/tcprobe-beacon /tmp/tcprobe-rotate; pkill -f 'sleep 86399'; exit 0",
    );
    result
}

/// The claude_beacon case body — every early return routes through the
/// wrapper's unconditional flag cleanup above.
fn claude_beacon_body(host: &str) -> anyhow::Result<()> {
    let host = host.to_string();
    let master = master_token()?;
    let log0 = daemon_log_len();
    let cwd = "/tmp/tcprobe-home/beacon";
    ssh_cli_wipe_store(cwd)?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 4900u64;
    let id = ssh_cli_setup(&mut c, &mut ctl, &mut rid, "__probe_beacon__", &host, cwd)?;
    c.send(&C2D::Input { id, bytes: b"claude\r".to_vec() })?;

    // ── A) startup beacon → Explicit(u1); clear-switch beacon → Explicit(u2).
    let store = ssh_cli_wait_store(cwd, 2, true, 60)?;
    run_wsl_sh("rm -f /tmp/tcprobe-rotate; exit 0")?;
    let sid_new = store[0].0.trim_end_matches(".jsonl").to_string(); // newest = post-"/clear"
    let sid_old = store[1].0.trim_end_matches(".jsonl").to_string();
    anyhow::ensure!(sid_new != sid_old, "rotate produced one file");
    ssh_cli_poll_state(30, |s| {
        s.terminals.iter().any(|t| {
            t.id == id
                && t.inner_cli.as_ref().is_some_and(|cli| {
                    cli.adapter == "claude"
                        && cli.confidence == CliConfidence::Explicit
                        && cli.resume_token.as_deref() == Some(sid_new.as_str())
                })
        })
    })?;
    let log = log_since(log0);
    // Accept-line shape: task #30 added the adapter word (the beacon verb is
    // shared with codex) — `tcbeacon claude session …`.
    anyhow::ensure!(
        log.contains(&format!("terminal {id}: tcbeacon claude session {sid_old} (source=startup)")),
        "startup beacon accept line missing"
    );
    anyhow::ensure!(
        log.contains(&format!("terminal {id}: tcbeacon claude session {sid_new} (source=clear)")),
        "clear-switch beacon accept line missing"
    );

    // ── B) sleep → wake resumes the SWITCHED conversation, zero probes. ──
    let wake_log0 = daemon_log_len();
    let size_new = store[0].1;
    c.send(&C2D::SleepTerminal { id })?;
    c.snapshot_until(30, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Dead && t.asleep)
    })?;
    c.send(&C2D::RestartTerminal { id })?;
    c.snapshot_until(60, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Running && !t.asleep)
    })?;
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    let want_cmd = format!("claude --resume {sid_new}");
    c.await_blocks(id, 60, |recs| {
        recs.iter().any(|r| r.cmd == want_cmd && r.end_off.is_none())
    })?;
    // The resumed fake claude appends to the SWITCHED transcript…
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let cur = ssh_cli_store_ls(cwd)?;
        if cur.iter().any(|(n, s)| n.starts_with(&sid_new) && *s > size_new) {
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "wake never appended to the switched transcript {sid_new}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    // …and the Explicit identity made the whole wake probe-free.
    let wake_log = log_since(wake_log0);
    anyhow::ensure!(
        !wake_log.contains("remote claude session correlated")
            && !wake_log.contains("definitively ambiguous"),
        "an Explicit beacon identity must skip the correlate leg entirely"
    );
    // The resume itself re-fires SessionStart(resume, u2) through the
    // beacon — same sid, change-gated to a no-op, but the exec-hook block
    // must exist for it to have been gated AT ALL rather than dropped
    // (hooks_live + open-CLI-block both true on the wake path).
    delete_terminal(&mut c, id);

    // ── C) anti-spoof: a beacon with NO open claude block is dropped. ──
    let spoof = Uuid::new_v4();
    let id2 = ssh_cli_setup(&mut c, &mut ctl, &mut rid, "__probe_beacon_spoof__", &host, cwd)?;
    let body = ctl_run_retry(
        &mut ctl,
        &mut rid,
        id2,
        &format!("printf '\\033]7717;tcbeacon;SessionStart;spoof;{spoof}\\007'"),
        Some(RunWait { timeout_ms: 30_000, tail_bytes: 4096 }),
        60,
    )?;
    anyhow::ensure!(
        matches!(body, CtlBody::RunDone { .. }),
        "spoof printf never ran: {body:?}"
    );
    // RunDone ⇒ the block closed ⇒ the beacon bytes were already scanned
    // (ingest is ordered). The printf block is not claude-family, so the
    // beacon must have been dropped: no accept line, no inner_cli.
    anyhow::ensure!(
        !log_since(log0).contains(&spoof.to_string()),
        "spoofed beacon must never be accepted"
    );
    let state: SharedState = serde_json::from_slice(&std::fs::read(state_path())?)?;
    anyhow::ensure!(
        state
            .terminals
            .iter()
            .find(|t| t.id == id2)
            .is_some_and(|t| t.inner_cli.is_none()),
        "spoofed beacon must not mint an inner_cli"
    );
    delete_terminal(&mut c, id2);

    // ── D) remote installer end-to-end over the staged transport. ──
    // Snapshot the remote settings.json + ~/.tc state for exact restore.
    let orig_b64 = run_wsl_sh(
        "base64 -w0 \"$HOME/.claude/settings.json\" 2>/dev/null || echo TC_ABSENT",
    )?;
    let orig_b64 = orig_b64.trim().to_string();
    let had_tc_dir = run_wsl_sh("[ -e \"$HOME/.tc\" ] && echo YES || echo NO")?
        .contains("YES");
    let restore_remote = || {
        let put_back = if orig_b64.contains("TC_ABSENT") {
            "rm -f \"$HOME/.claude/settings.json\"".to_string()
        } else {
            format!(
                "printf %s {orig_b64} | base64 -d > \"$HOME/.claude/settings.json\""
            )
        };
        let tc_clean = if had_tc_dir {
            "rm -f \"$HOME/.tc/claude-hook.sh\""
        } else {
            "rm -rf \"$HOME/.tc\""
        };
        let _ = run_wsl_sh(&format!("{put_back}\n{tc_clean}\nexit 0"));
    };
    let installed = crate::claude_hooks::install_remote("ssh", std::slice::from_ref(&host))
        .map_err(|e| {
            restore_remote();
            anyhow::anyhow!("install_remote failed: {e}")
        })?;
    anyhow::ensure!(
        installed == crate::claude_hooks::Outcome::Installed,
        "first install should be Fresh, got {installed:?}"
    );
    let check = run_wsl_sh(
        "[ -x \"$HOME/.tc/claude-hook.sh\" ] && head -1 \"$HOME/.tc/claude-hook.sh\"; \
         grep -c 'claude-hook.sh' \"$HOME/.claude/settings.json\"",
    )
    .inspect_err(|_| restore_remote())?;
    anyhow::ensure!(
        check.contains("#!/bin/sh"),
        "remote script missing/not executable: {check:?}"
    );
    anyhow::ensure!(
        check.lines().any(|l| l.trim().parse::<u32>().map(|n| n >= 2).unwrap_or(false)),
        "settings.json should reference the hook for both events: {check:?}"
    );
    // Idempotence against the REAL uploaded state.
    let again = crate::claude_hooks::install_remote("ssh", std::slice::from_ref(&host));
    restore_remote();
    anyhow::ensure!(
        again == Ok(crate::claude_hooks::Outcome::AlreadyInstalled),
        "re-install should be Already, got {again:?}"
    );
    // Flag/reaper cleanup lives in case_claude_beacon's wrapper — it runs
    // on EVERY exit path, not just success.
    ensure_no_new_panics(log0)?;
    Ok(())
}

/// F1 NESTED-SHELL CLAUDE BREADCRUMB (`ssh_nested_claude`, nested-resume
/// spec §7): the user's `ssh → sudo su → cd → claude` chain over the WSL
/// stand-in, both restore classes.
/// A) beacon-less episode: the hook-witnessed `sudo su` opens the
///    breadcrumb (nested_chain == ["sudo su"], inner_cli None) and a
///    beacon-LESS nested claude changes nothing (no hooks in there — no
///    guessing); sleep → wake restores SHELL-ONLY (I1: no `--resume` block,
///    "shell-only restore" in daemon.log) with the variant-C re-establish
///    preface riding the re-attach replay (I4: loss is never silent); one
///    real Run later the first token-checked pre retires the breadcrumb
///    (one-shot honesty, §2.5).
/// B) beacon episode: a fake ROOT claude prints the v2 tcbeacon
///    (hex(cwd)="/") ⇒ inner_cli{claude, Explicit, nested:true} +
///    chain.cli_cwd witnessed; a full daemon RESTART boot-restores with the
///    variant-A preface (`re-establish: sudo su; cd '/'; claude --resume
///    <sid>`), STILL no resume block, and the identity survives INTO the
///    restored session — the log ORDER (shell-only restore, THEN
///    hooked-prompt retirement) proves the §2c erase-on-failed-resume path
///    is unreachable (no resume was ever attempted).
/// C) anti-spoof: plain-prompt beacons stay covered by claude_beacon leg C;
///    the non-nested-overwrite/Claude-kind refusals are pinned unit-level.
/// Needs the ssh_cli_env staging + passwordless sudo in the distro; SKIPs
/// without. Root-side writes: NONE — the fake claude carries its own beacon
/// (byte-identical on the wire to a root-installed hook script), staged
/// under /tmp only.
fn case_ssh_nested_claude() -> anyhow::Result<()> {
    let host = ssh_cli_env()?;
    ensure_isolated_daemon("ssh_nested_claude")?;
    let sudo_ok = run_wsl_sh("sudo -n true >/dev/null 2>&1 && echo OK || echo NO")?;
    if !sudo_ok.contains("OK") {
        return Err(skip(
            "passwordless sudo unavailable in the default distro".into(),
        ));
    }
    let sid = Uuid::new_v4();
    // Stage under /tmp only: a QUIET nested claude (leg A) and a v2-beacon
    // claude (leg B; hex("/") = 2f). Both hold the foreground like the real
    // TUI would.
    let stage = format!(
        "mkdir -p /tmp/tc-nested-probe\n\
         cat > /tmp/tc-nested-probe/claude-quiet <<'EOF'\n\
         #!/bin/sh\n\
         sleep 300\n\
         EOF\n\
         cat > /tmp/tc-nested-probe/claude <<'EOF'\n\
         #!/bin/sh\n\
         printf '\\033]7717;tcbeacon;claude;SessionStart;startup;{sid};2f\\007' > /dev/tty 2>/dev/null || true\n\
         sleep 300\n\
         EOF\n\
         chmod +x /tmp/tc-nested-probe/claude-quiet /tmp/tc-nested-probe/claude\n"
    );
    run_wsl_sh(&stage)?;
    let result = ssh_nested_claude_body(&host, sid);
    // ALWAYS clean, pass or fail (the fake root claude outlives the pty —
    // root-owned, so the reap needs sudo -n; the stage dir must not linger).
    let _ = run_wsl_sh(
        "sudo -n pkill -f tc-nested-probe 2>/dev/null; rm -rf /tmp/tc-nested-probe; exit 0",
    );
    result
}

/// The ssh_nested_claude case body — every early return routes through the
/// wrapper's unconditional stage cleanup above.
fn ssh_nested_claude_body(host: &str, sid: Uuid) -> anyhow::Result<()> {
    let master = master_token()?;
    let log0 = daemon_log_len();
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let mut ctl = Conn::open_ctl(&master, None)?;
    let mut rid = 5400u64;
    let cwd = "/tmp/tcprobe-home/nested";
    let chain_open = |id: Uuid, secs: u64| {
        ssh_cli_poll_state(secs, |s| {
            s.terminals.iter().any(|t| {
                t.id == id
                    && t.nested_chain
                        .as_ref()
                        .is_some_and(|n| n.cmds == ["sudo su"])
            })
        })
    };

    // ── A) beacon-less nested episode → honest shell-only sleep/wake. ──
    let id = ssh_cli_setup(&mut c, &mut ctl, &mut rid, "__probe_nested_a__", host, cwd)?;
    c.send(&C2D::Input { id, bytes: b"sudo su\r".to_vec() })?;
    chain_open(id, 30).map_err(|e| e.context("breadcrumb never opened (leg A)"))?;
    let st: SharedState = serde_json::from_slice(&std::fs::read(state_path())?)?;
    anyhow::ensure!(
        st.terminals
            .iter()
            .find(|t| t.id == id)
            .is_some_and(|t| t.inner_cli.is_none()),
        "opening a nested shell must not mint an identity"
    );
    // Inside the hookless root shell: cd + a beacon-less claude — nothing
    // may change (raw-typed lines are invisible by design).
    c.send(&C2D::Input { id, bytes: b"cd /\r".to_vec() })?;
    std::thread::sleep(Duration::from_millis(400));
    c.send(&C2D::Input {
        id,
        bytes: b"/tmp/tc-nested-probe/claude-quiet\r".to_vec(),
    })?;
    std::thread::sleep(Duration::from_millis(1500));
    let st: SharedState = serde_json::from_slice(&std::fs::read(state_path())?)?;
    anyhow::ensure!(
        st.terminals
            .iter()
            .find(|t| t.id == id)
            .is_some_and(|t| t.inner_cli.is_none()),
        "a beacon-less nested claude must not mint an identity (zero accepts)"
    );
    let wake0 = daemon_log_len();
    c.send(&C2D::SleepTerminal { id })?;
    c.snapshot_until(30, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Dead && t.asleep)
    })?;
    c.send(&C2D::RestartTerminal { id })?;
    c.snapshot_until(60, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.status == TermStatus::Running && !t.asleep)
    })?;
    // The variant-C preface rides the re-attach replay (Session.preface).
    c.send(&C2D::Attach { id, cols: 120, rows: 30 })?;
    c.await_output(id, 30, |l| {
        l.contains("this terminal had a nested shell (sudo su)")
            && l.contains("re-establish it manually")
    })
    .map_err(|e| e.context("variant-C preface never rendered (leg A)"))?;
    let wake_log = log_since(wake0);
    anyhow::ensure!(
        wake_log.contains("shell-only restore (no auto-resume across a privilege boundary)"),
        "wake must log the shell-only restore"
    );
    anyhow::ensure!(
        !wake_log.contains("claude --resume"),
        "a bare nested chain must never compose a resume (I1)"
    );
    await_hooked_prompt(&mut ctl, &mut rid, id, 90)?;
    let recs = c.await_blocks(id, 30, |_| true)?;
    anyhow::ensure!(
        recs.iter().all(|r| !r.cmd.contains("--resume")),
        "no resume block may exist after a nested wake: {:?}",
        recs.iter().map(|r| &r.cmd).collect::<Vec<_>>()
    );
    // §2.5 one-shot: a real round-trip (token-checked pre) retires the chain.
    let body = ctl_run_retry(
        &mut ctl,
        &mut rid,
        id,
        "echo tc-nested-done",
        Some(RunWait { timeout_ms: 30_000, tail_bytes: 4096 }),
        60,
    )?;
    anyhow::ensure!(
        matches!(body, CtlBody::RunDone { .. }),
        "post-wake run failed: {body:?}"
    );
    ssh_cli_poll_state(20, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == id && t.nested_chain.is_none())
    })
    .map_err(|e| e.context("the first hooked prompt must retire the breadcrumb"))?;
    delete_terminal(&mut c, id);

    // ── B) v2-beacon episode → daemon restart → variant-A preface, no
    //       resume, identity survives into the restored session. ──
    let idb = ssh_cli_setup(&mut c, &mut ctl, &mut rid, "__probe_nested_b__", host, cwd)?;
    c.send(&C2D::Input { id: idb, bytes: b"sudo su\r".to_vec() })?;
    chain_open(idb, 30).map_err(|e| e.context("breadcrumb never opened (leg B)"))?;
    c.send(&C2D::Input { id: idb, bytes: b"cd /\r".to_vec() })?;
    std::thread::sleep(Duration::from_millis(400));
    c.send(&C2D::Input {
        id: idb,
        bytes: b"/tmp/tc-nested-probe/claude\r".to_vec(),
    })?;
    let sid_s = sid.to_string();
    ssh_cli_poll_state(60, |s| {
        s.terminals.iter().any(|t| {
            t.id == idb
                && t.inner_cli.as_ref().is_some_and(|cli| {
                    cli.adapter == "claude"
                        && cli.nested
                        && cli.confidence == CliConfidence::Explicit
                        && cli.resume_token.as_deref() == Some(sid_s.as_str())
                })
                && t.nested_chain
                    .as_ref()
                    .is_some_and(|n| n.cli_cwd.as_deref() == Some(std::path::Path::new("/")))
        })
    })
    .map_err(|e| e.context("nested beacon never minted the tagged identity"))?;
    anyhow::ensure!(
        log_since(log0).contains(&format!(
            "terminal {idb}: tcbeacon claude session {sid} tagged nested (source=startup)"
        )),
        "nested-beacon accept line missing"
    );

    // Graceful daemon restart with chain + nested identity persisted.
    let restart0 = daemon_log_len();
    c.send(&C2D::Shutdown)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if c.recv().is_err() {
            break;
        }
    }
    let lock = crate::state::data_dir().join("daemon.lock");
    for _ in 0..50 {
        if !lock.exists() || std::fs::remove_file(&lock).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        std::process::Command::new(std::env::current_exe()?)
            .arg("--daemon")
            .creation_flags(DETACHED_PROCESS)
            .spawn()?;
    }
    let mut c2 = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match Conn::open() {
                Ok(conn) => break conn,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(250))
                }
                Err(e) => return Err(e),
            }
        }
    };
    let _ = c2.first_snapshot()?;
    c2.snapshot_until(90, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == idb && t.status == TermStatus::Running)
    })?;
    // Variant-A preface: the exact copy-pasteable chain, uuid included.
    c2.send(&C2D::Attach { id: idb, cols: 120, rows: 30 })?;
    c2.await_output(idb, 30, |l| {
        l.contains("re-establish: sudo su; cd '/'; claude --resume") && l.contains(&sid_s)
    })
    .map_err(|e| e.context("variant-A preface never rendered (leg B)"))?;
    let mut ctl2 = Conn::open_ctl(&master, None)?;
    let mut rid2 = 5600u64;
    await_hooked_prompt(&mut ctl2, &mut rid2, idb, 90)?;
    let recs = c2.await_blocks(idb, 30, |_| true)?;
    anyhow::ensure!(
        recs.iter().all(|r| !r.cmd.contains("--resume")),
        "no resume block may exist after a nested boot restore: {:?}",
        recs.iter().map(|r| &r.cmd).collect::<Vec<_>>()
    );
    let rlog = log_since(restart0);
    anyhow::ensure!(
        rlog.contains("shell-only restore (no auto-resume across a privilege boundary)"),
        "boot restore must log the shell-only lane"
    );
    anyhow::ensure!(
        !rlog.contains(&format!("claude --resume {sid}")),
        "the nested identity must never reach a restore command (I1)"
    );
    // §2c non-erasure, structurally: the identity survived INTO the restored
    // session and retired at the first hooked prompt — restore line strictly
    // BEFORE the retirement line, and no resume failure anywhere between.
    let pos_restore = rlog
        .find("shell-only restore")
        .ok_or_else(|| anyhow::anyhow!("no restore line"))?;
    let pos_retire = rlog
        .find("nested-shell breadcrumb retired (hooked prompt returned)")
        .ok_or_else(|| anyhow::anyhow!("no retirement line — identity never survived the restore"))?;
    anyhow::ensure!(
        pos_restore < pos_retire,
        "retirement must come from the restored session's own prompt, after the restore"
    );
    ssh_cli_poll_state(20, |s| {
        s.terminals
            .iter()
            .any(|t| t.id == idb && t.nested_chain.is_none() && t.inner_cli.is_none())
    })?;
    delete_terminal(&mut c2, idb);
    ensure_no_new_panics(log0)?;
    Ok(())
}

/// Delete every `__probe_*` terminal currently in the daemon's state. Runs
/// before the suite (sweeps leftovers from prior/failed runs) and after every
/// case (so cleanup happens even when a case fails or times out) — probe
/// terminals must never pollute the user's real sidebar.
/// Screenshot rig for Shiro report #1 (see run()'s hidden-verb match): the
/// same real composition as `case_launcher_claude_cwd`, but the terminal
/// survives (demo name, not `__probe_`-prefixed) for a GUI capture.
fn case_claude_cwd_demo_create() -> anyhow::Result<()> {
    let dir = std::path::PathBuf::from(
        std::env::var("TC_PROBE_CLAUDE_DIR")
            .map_err(|_| anyhow::anyhow!("set TC_PROBE_CLAUDE_DIR to the chosen directory"))?,
    );
    std::fs::create_dir_all(&dir)?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;
    let (nt, _) = crate::gui::launcher::claude_dir_spec(&dir, None, &[])
        .expect("'claude' is a known tag");
    let name = nt.name.clone();
    c.send(&C2D::CreateTerminal { spec: nt })?;
    let state = c.snapshot_until(15, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == name && t.status == TermStatus::Running)
    })?;
    let meta = state.terminals.iter().find(|t| t.name == name).unwrap();
    anyhow::ensure!(meta.cwd == dir, "state cwd mismatch");
    println!("[claude_cwd_demo] created '{}' id={} cwd={}", name, meta.id, dir.display());
    Ok(())
}

/// Shiro report #1 end-to-end: the launcher's typed-path claude row picks
/// the session's working directory. The case drives the REAL composition fn
/// (`claude_dir_spec` — exactly what the row's activation sends) into a real
/// daemon spawn and proves the cwd at the PTY level. Contract with the
/// staged fake claude.exe: it prints `FAKECLAUDE_CWD=<process cwd>` and one
/// `FAKECLAUDE_ARG=<arg>` line per argv entry, then blocks on stdin (stays
/// Running until deleted).
fn case_launcher_claude_cwd() -> anyhow::Result<()> {
    if std::env::var("TC_PROBE_FAKE_CLAUDE").ok().as_deref() != Some("1") {
        return Err(skip(
            "needs a fake claude.exe FIRST on the daemon's PATH (opt in with \
             TC_PROBE_FAKE_CLAUDE=1 on such a rig)"
            .into(),
        ));
    }
    let dir =
        std::env::temp_dir().join(format!("tcprobe-claude-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let mut c = Conn::open()?;
    let _ = c.first_snapshot()?;

    // The REAL launcher composition — the typed-path claude row's activation.
    let (mut nt, sp) = crate::gui::launcher::claude_dir_spec(&dir, None, &[])
        .expect("'claude' is a known tag");
    anyhow::ensure!(
        sp.kind_tag == "claude" && sp.cwd == dir,
        "the recorded SpawnSpec must carry the chosen dir"
    );
    let sid = match &nt.kind {
        TermKind::Claude { session_id, .. } => *session_id,
        _ => anyhow::bail!("claude_dir_spec must compose a Claude-kind terminal"),
    };
    nt.name = "__probe_claude_cwd__".into(); // sweepable
    c.send(&C2D::CreateTerminal { spec: nt })?;
    let state = c.snapshot_until(15, |s| {
        s.terminals
            .iter()
            .any(|t| t.name == "__probe_claude_cwd__" && t.status == TermStatus::Running)
    })?;
    let meta = state
        .terminals
        .iter()
        .find(|t| t.name == "__probe_claude_cwd__")
        .unwrap();
    let id = meta.id;
    anyhow::ensure!(
        meta.cwd == dir,
        "the state (every cwd display surface reads it) must record the chosen dir"
    );

    // PTY-level proof: the launched process's OWN idea of its cwd, plus the
    // launch_command lane (a fresh `--session-id <sid>` composed per spawn).
    c.send(&C2D::Attach { id, cols: 160, rows: 40 })?;
    let want_cwd = format!("FAKECLAUDE_CWD={}", dir.display());
    let want_sid = format!("FAKECLAUDE_ARG={sid}");
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        anyhow::ensure!(
            Instant::now() < deadline,
            "fake claude never printed `{want_cwd}` + `{want_sid}` \
             (got {} bytes)",
            collected.len()
        );
        match c.recv() {
            Ok(D2C::Replay { id: rid, bytes }) | Ok(D2C::Output { id: rid, bytes })
                if rid == id =>
            {
                collected.extend_from_slice(&bytes);
                let text = String::from_utf8_lossy(&collected).replace("\r\n", "\n");
                if text.contains(&want_cwd) && text.contains(&want_sid) {
                    break;
                }
            }
            _ => {}
        }
    }
    delete_terminal(&mut c, id);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn sweep_probes() {
    let Ok(mut c) = Conn::open() else { return };
    let Ok(state) = c.first_snapshot() else { return };
    let mut deleted = false;
    for t in &state.terminals {
        if t.name.starts_with("__probe_") {
            let _ = c.send(&C2D::DeleteTerminal { id: t.id });
            deleted = true;
        }
    }
    if deleted {
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// r2 roll (probe policy): the daemon-killing/restarting cases target
/// whatever daemon THIS data dir's daemon.json names. Against the installed
/// daemon that is the user's live workspace — a hard TerminateProcess (or a
/// shutdown+respawn) plus flood/restore churn on real sessions. The standing
/// rule ("never probe the user's live daemon") was enforced only by operator
/// discipline; this makes it structural: such cases SKIP unless the data dir
/// is overridden (TC_DATA_DIR staging daemon) or the operator explicitly
/// opts in with TC_PROBE_LIVE=1.
fn ensure_isolated_daemon(what: &str) -> anyhow::Result<()> {
    if crate::state::data_dir_overridden()
        || std::env::var("TC_PROBE_LIVE").is_ok_and(|v| v == "1")
    {
        return Ok(());
    }
    Err(skip(format!(
        "{what} kills/restarts the daemon this data dir points at — refusing against \
         the installed daemon (run with TC_DATA_DIR staging, or TC_PROBE_LIVE=1 to override)"
    )))
}

pub fn run(case: Option<&str>) -> anyhow::Result<()> {
    // Probes are measurement tools: a hidden background probe gets parked on
    // E-cores under foreground load, corrupting flood/latency numbers on the
    // client side just like the daemon's own demotion did on the ingest side.
    crate::daemon::procinfo::set_high_qos(std::process::id());
    // Hidden screenshot helpers: run WITHOUT the sweeps (the demo terminal
    // must survive between invocations; `--probe sweep` cleans up).
    match case {
        Some("blocks_demo_create") => return case_blocks_demo_create(),
        // Screenshot rig (Shiro report #1): a Claude-kind terminal composed
        // through the REAL `claude_dir_spec` into $TC_PROBE_CLAUDE_DIR,
        // left alive (non-__probe_ name) so a GUI capture can show the cwd
        // chip + the fake claude's own FAKECLAUDE_CWD line.
        Some("claude_cwd_demo_create") => return case_claude_cwd_demo_create(),
        Some("blocks_demo_run") => return case_blocks_demo_run(),
        // Hidden byte-level diagnosis for the respawn seam (never swept in).
        Some("banner_diag") => {
            sweep_probes();
            let r = case_banner_diag();
            sweep_probes();
            return r;
        }
        Some("composer_demo_arm") => return case_composer_demo_arm(),
        // Hidden ops helper (perf-wave-3): gracefully stop the daemon this
        // data dir's daemon.json points at — Shutdown frame with the
        // request_shutdown linger, then wait out the lock release exactly
        // like --install. Lets lifecycle scripts (boot-restore, downtime
        // measurement) stop an isolated daemon through the REAL clean path
        // (output drain + journal flush) instead of TerminateProcess.
        Some("shutdown") => {
            let _ = crate::gui::ipc::request_shutdown();
            let lock = crate::state::data_dir().join("daemon.lock");
            for _ in 0..100 {
                if !lock.exists() || std::fs::remove_file(&lock).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            println!("[shutdown] daemon stopped");
            return Ok(());
        }
        // Hidden perf-measurement cases (report-only, never in the sweep).
        Some("perf_attach") => {
            sweep_probes();
            let r = case_perf_attach();
            sweep_probes();
            return r;
        }
        Some("perf_idle") => {
            sweep_probes();
            let r = case_perf_idle();
            sweep_probes();
            return r;
        }
        _ => {}
    }
    type ProbeCase = (&'static str, fn() -> anyhow::Result<()>);
    let cases: Vec<ProbeCase> = vec![
        ("basic", case_basic),
        ("restore", case_restore),
        ("dead_relaunch", case_dead_relaunch),
        ("dead_retry_manual", case_dead_retry_manual),
        ("remnant", case_remnant),
        ("banner", case_banner),
        ("folders", case_folders),
        ("backpressure", case_backpressure),
        ("resize_owner", case_resize_owner),
        ("peb", case_peb),
        ("tracker", case_tracker),
        ("resize_stress", case_resize_stress),
        ("resize_race", case_resize_race),
        ("journal_reap", case_journal_reap),
        ("replay_cap", case_replay_cap),
        ("keys", case_keys),
        ("paste_stuck_child", case_paste_stuck_child),
        ("latency", case_latency),
        ("flood", case_flood),
        ("blocks_roundtrip", case_blocks_roundtrip),
        ("blocks_restore", case_blocks_restore),
        ("blocks_antispoof", case_blocks_antispoof),
        ("blocks_compact_evict", case_blocks_compact_evict),
        ("blocks_stream_pos", case_blocks_stream_pos),
        ("blocks_text", case_blocks_text),
        ("blocks_rerun_gate", case_blocks_rerun_gate),
        ("blocks_hookless_silent", case_blocks_hookless_silent),
        ("composer_submit", case_composer_submit),
        ("composer_multiline", case_composer_multiline),
        ("composer_gate_replay", case_composer_gate_replay),
        ("cold_attach", case_cold_attach),
        ("restore_fidelity", case_restore_fidelity),
        ("width_mismatch_replay", case_width_mismatch_replay),
        ("attach_alt_flood", case_attach_alt_flood),
        ("compact_crash", case_compact_crash),
        ("boot_cover", case_boot_cover),
        ("reclaim_extract", case_reclaim_extract),
        ("history_cross_session", case_history_cross_session),
        ("ctl_scope", case_ctl_scope),
        ("ctl_run_wait", case_ctl_run_wait),
        ("ctl_busy_gate", case_ctl_busy_gate),
        ("ctl_read", case_ctl_read),
        ("wsl_hooks", case_wsl_hooks),
        ("wsl_composer_semantics", case_wsl_composer_semantics),
        ("wsl_nested_shell", case_wsl_nested_shell),
        ("wsl_hostile_prompt_command", case_wsl_hostile_prompt_command),
        ("wsl_restore", case_wsl_restore),
        ("cmd_hooks", case_cmd_hooks),
        ("cmd_restore", case_cmd_restore),
        ("ssh_bootstrap_local", case_ssh_bootstrap_local),
        ("ssh_reconnect", case_ssh_reconnect),
        ("ssh_cli_resume", case_ssh_cli_resume),
        ("ssh_cli_resume_fallback", case_ssh_cli_resume_fallback),
        ("ssh_cli_authdead", case_ssh_cli_authdead),
        ("claude_beacon", case_claude_beacon),
        ("ssh_nested_claude", case_ssh_nested_claude),
        ("codex_beacon", case_codex_beacon),
        ("cwd_broadcast", case_cwd_broadcast),
        ("history_parity", case_history_parity),
        ("history_parity_wsl", case_history_parity_wsl),
        ("sleep_roundtrip", case_sleep_roundtrip),
        ("sleep_busy_gate", case_sleep_busy_gate),
        ("sleep_waiters_folder", case_sleep_waiters_folder),
        ("sleep_freeze_frame", case_sleep_freeze_frame),
        ("frame_corrupt_degrade", case_frame_corrupt_degrade),
        ("launcher_claude_cwd", case_launcher_claude_cwd),
    ];

    let selected: Vec<_> = match case {
        None | Some("all") => cases,
        Some("sweep") => Vec::new(), // just clean up leftovers
        Some(name) => {
            let found: Vec<_> = cases.into_iter().filter(|(n, _)| *n == name).collect();
            anyhow::ensure!(!found.is_empty(), "unknown probe case: {name}");
            found
        }
    };

    // Clear any artifacts left by an earlier interrupted run.
    sweep_probes();

    let mut failures = 0;
    let mut skipped: Vec<String> = Vec::new();
    for (name, f) in selected {
        print!("[probe] {name} … ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let result = f();
        // Cleanup runs regardless of pass/fail so nothing leaks into the sidebar.
        sweep_probes();
        match result {
            Ok(()) => println!("PASS"),
            Err(e) => {
                // Environment-gated cases (P6 §12 discipline): a skip is
                // printed with its reason and counted separately — it never
                // masquerades as green.
                let msg = format!("{e:#}");
                match msg.strip_prefix("SKIP: ") {
                    Some(reason) => {
                        println!("SKIP({name}): {reason}");
                        skipped.push(name.to_string());
                    }
                    None => {
                        println!("FAIL: {msg}");
                        failures += 1;
                    }
                }
            }
        }
    }
    if failures > 0 {
        anyhow::bail!("{failures} probe case(s) failed");
    }
    if skipped.is_empty() {
        println!("[probe] all cases PASS");
    } else {
        println!(
            "[probe] all run cases PASS; {} SKIPPED (not passes): {}",
            skipped.len(),
            skipped.join(", ")
        );
    }
    Ok(())
}
