# Pulse — design docs index

These are **phased implementation plans**, written before each phase shipped.
Read them for the *why* behind a subsystem; read the code for the *current*
truth. In particular:

> **Proto version and probe-case counts in every spec are as-of-writing.**
> They pin the numbers at plan time (e.g. P2 "proto 1→2, 18→22 cases", P6
> "proto 4→5, suite 36"). The live surface is whatever `src/protocol.rs`
> (the `Proto` constant / D2C+C2D enums) and `src/probe.rs` (the `--probe all`
> registry) currently say — at the time of this note, **proto 11**; the case
> count lives in `src/probe.rs`'s module header. Do not treat a spec's
> number as current.

Specs and their subsystems:

| Spec | Subsystem | Notable supersessions (see the spec's own header/notes) |
|---|---|---|
| `p2-blocks-ui-spec.md` | GUI block records + panel | — |
| `p3-composer-v1-spec.md` | composer gate/state machine | settle=ZERO; D2 → cover system; RawReason::Asleep |
| `p4-composer-v2-spec.md` | typeahead reclaim, history popup | — |
| `p5-controller-api-spec.md` | `tc` controller verbs | Sleep/Wake/ReportCliSession appended post-P5 |
| `p6-shells-spec.md` | WSL/ssh/cmd families | **D13 / DO-NOT 10** ssh auto-reconnect → shipped (proto 10, `reconnect.rs`) |
| `qol-spec.md` | selection/clipboard/paste QOL | — |
| `sleep-spec.md` | per-terminal hibernation | — |
| `ssh-drop-spec.md` | sftp file drop | §8 transport pure fns hoisted to `src/ssh_transport.rs` (D4) |
| `selector-ui-spec.md` | terminal launcher | — |
| `remote-cli-resume-spec.md` | remote CLI resume/attribution | beacon (Layer 3) short-circuits most correlate legs |
| `controller-api.md` | **user/agent-facing** `pulse-ctl` reference | kept current — this is the doc agents read |
