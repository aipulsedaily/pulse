# QOL — everyday terminal conveniences — Implementation Spec (final, implementation-ready)

Target: C:\Terminal Control (Rust daemon + egui 0.35 GUI + tc.exe, proto 7 at research
time). **This feature set is GUI-only: ZERO wire changes, zero daemon changes, zero
protocol coordination needed** — every action rides existing verbs (`C2D::Input`,
`C2D::CreateTerminal`) or is pure view state. sidebar-p2 and sleep-impl may append
protocol variants freely; this spec never touches protocol.rs.

User mandate (verbatim): **"selecting and right click no context menu not copy nothing —
use one subagent to research what QOL features are missing NOT BLOAT but actual things
people use everyday"** + **"yes big one no drag drop image support"**. The user drags
QuipShot screenshots into Claude Code terminals all day — local drag-drop is the headline
alongside the right-click menu. SSH drag-drop (remote upload) is SPLIT OUT to #26; this
spec only leaves it a clean hook point (§4.6).

All file:line references are at research time; sidebar-p2 is concurrently editing
gui/mod.rs / composer.rs — cite-by-function at merge, and put new pure logic in NEW files
(§9) to minimize churn collisions.

Ordered: audit → ranking → invariants → designs (P1 → P2 → P3) → file-by-file →
tests/staging → open questions → DO-NOTs.

---

## 0. Audit — what exists, what doesn't (evidence-verified)

| # | Candidate | Verdict | Evidence |
|---|---|---|---|
| A1 | Terminal-area right-click menu | **MISSING** | `process_input` handles ONLY `PointerButton::Primary` (term_view.rs:629); the grid response never gets `.context_menu()` — the only context menus in the app are sidebar folder/terminal rows (mod.rs:3147, 3604) and split-+ (mod.rs:4068). Right-click on the grid today does literally nothing |
| A2 | Right/middle-click for TUI apps (mouse reports) | **MISSING** | `MouseButton` enum = `{LeftButton=0, LeftMove=32}` only (term_backend.rs:54) — mouse-mode apps have never received a right or middle click; `mouse_report` (term_backend.rs:1685) is already button-code-generic |
| A3 | Local drag-drop | **MISSING** | Zero hits for `dropped_files`/`hovered_files` in the tree. The events already arrive: egui-winit 0.35 fills `raw_input.hovered_files`/`dropped_files` from `WindowEvent::HoveredFile/DroppedFile` (egui-winit lib.rs:440,457), winit's Windows `drag_and_drop` attribute defaults TRUE (winit platform_impl/windows/mod.rs:49), and our `ViewportBuilder` (mod.rs:6554) never disables it. Nothing consumes them — files dropped on the window today vanish silently. **winit's DroppedFile carries NO cursor position** (drop_handler.rs emits path only) — routing must not be position-based |
| A4 | Ctrl+C copy-or-interrupt | EXISTS | `Event::Copy` → selection ⇒ synthesized copy, else interrupt chord (term_view.rs:591-600); composer intercepts first while armed (composer.rs:1829) |
| A5 | Ctrl+Shift+C/V, Ctrl/Shift+Insert, Shift+Delete | EXISTS (de facto) | egui-winit folds them: `is_copy_command` = `command && C` — shift NOT excluded, so Ctrl+Shift+C ≡ Ctrl+C, ditto V/X/Insert/Delete (egui-winit lib.rs:1394-1410). The explicit bindings.rs:196-197 rows are unreachable vestige (those chords never arrive as Key events; and Key events short-circuit at the win32_input branch, term_view.rs:564, before the table). **Quirk, accepted:** Ctrl+Shift+C with no selection interrupts exactly like Ctrl+C — the fold erases the shift bit before we see it |
| A6 | Right-click-paste (WT convention) | MISSING | superseded — menu wins (§3, decision R2) |
| A7 | Middle-click paste | **MISSING** | no `PointerButton::Middle` arm anywhere in term_view.rs |
| A8 | Copy-on-select | **MISSING** | selection release (term_view.rs:745-750) copies nothing; no pref exists |
| A9 | Double-click word / triple-click line select | EXISTS | `response.double_clicked()` ⇒ `SelectionType::Semantic`, `triple_clicked()` ⇒ `Lines` (term_view.rs:736-742). Word boundaries = alacritty default `SEMANTIC_ESCAPE_CHARS` = `,│`|:"' ()[]{}<>\t` — **includes `:`**, so double-click splits `C:\Users\…` and `https://…` at the colon. We own the `term::Config` (term_backend.rs:267) — tunable, §6.4 |
| A10 | URL detection / Ctrl+click | EXISTS | Ctrl+hover resolves the link on-demand, cached per (cell, display_offset) (term_view.rs:351-363) — already the on-demand hit-test design, no per-frame scan; underline paints on the hovered match (render, term_view.rs:1281-1282); Ctrl+click opens via `open::that_detached` (term_view.rs:747-749, 819-833); regex = alacritty's URL pattern incl. https/http/file/mailto/git/ssh/ftp (mod.rs:840-843). Gaps (§6.5): no PointingHand cursor on link hover; covered rows hit-test the GRID while painting synthesized text (`❯ cwd cmd`) — column mismatch possible |
| A11 | Paste safety | **MISSING** | `paste()` writes immediately, `\n`→`\r` sanitized, bracketed iff app mode (term_view.rs:801-811) — a multi-line paste into PS 5.1 (never BRACKETED_PASTE) executes every line instantly. The composer path is already safe (multi-line drafts buffer visibly); the raw grid path is the risk |
| A12 | Scroll-to-bottom on keypress | EXISTS | `write_and_pin` pins every key/paste/IME write to bottom (term_view.rs:783-787, 810) |
| A13 | Ctrl+wheel font zoom | EXISTS **with a bug** | zoom: mod.rs:5579-5589 via `zoom_delta()`. But egui-winit ALWAYS emits the raw `MouseWheel` event too (lib.rs:918-948) and term_view's wheel arm has no modifier guard (term_view.rs:613) — **Ctrl+wheel zooms AND scrolls the grid** (or ships arrow keys under ALTERNATE_SCROLL+alt-screen). One-line fix, §6.3 |
| A14 | Clear scrollback | **MISSING** | no `clear_history` call in the tree; `Grid::clear_history` is pub in alacritty 0.26 (grid/mod.rs:383) |
| A15 | Duplicate terminal | **MISSING** | all machinery exists (`C2D::CreateTerminal{spec}` protocol.rs:33, `uniquify_name` launcher.rs:255, `shell_cfg` on NewTerminal); no UI entry |
| A16 | Open cwd in Explorer | **MISSING** | `live_cwd` persisted (state.rs:277), `open` crate already a dep (term_view.rs:831); no UI entry |
| A17 | Find (search) | EXISTS | bar search toggle creates `SearchState::new()` (mod.rs:3783); menu adds an entry point only |
| A18 | Rerun-last | PARTIAL | per-block Re-run exists on the hover toolbar, blocks panel, and history popup (`can_rerun` mod.rs:1921, `rerun_block` mod.rs:1939); no "last command" one-click |
| A19 | Unfocused-terminal attention | PARTIAL | NeedsYou latch + amber pill + `attention_flashed` taskbar flash exist (mod.rs:593, 1529-1541). The toast COMPONENT belongs to #26 — this spec consumes it later, builds nothing (§8.4) |
| A20 | Open-new-terminal-in-same-cwd | PARTIAL | launcher Recent-dirs section + folder "New terminal here…" (mod.rs:3149) exist; Duplicate (§7.1) covers the per-terminal gesture — no separate feature |

---

## 1. Ranking (daily value) + rejected

**P1 (the user's two headliners):**
1. §3 Terminal-area right-click context menu (Copy/Paste/Select all/Find/Open cwd/
   Rerun last/Clear scrollback) — the verbatim complaint.
2. §4 Local drag-drop → quoted path insertion (the QuipShot→Claude workflow).

**P2 (used weekly-to-daily, small, rides P1 machinery):**
3. §5 Paste safety (multi-line into non-bracketed raw shell).
4. §7.1 Duplicate terminal (sidebar menu).
5. §6.3 Ctrl+wheel scroll-while-zoom fix + §6.4 word-boundary `:` tuning + §6.5 link
   polish (cursor, covered rows).

**P3 (conventions that complete the story):**
6. §6.1 Middle-click paste.
7. §6.2 Copy-on-select (pref, default off).
8. §3.4 Right/middle mouse-report forwarding to MOUSE_MODE apps.

**Rejected (fails "used daily?" — one line each):**

| Candidate | Why rejected |
|---|---|
| Themes / appearance UI | the unified text theme was hand-curated app-wide; a picker is chrome, not terminal |
| Split panes | Phase-B-class layout work, explicitly deferred by the roadmap, not QOL |
| Profiles/settings UI | Prefs has 6 fields; the launcher IS the profile surface |
| OSC 8 explicit hyperlinks | near-zero emitter coverage in Windows shells vs regex detection already shipped |
| Copy as HTML/RTF | nobody pastes terminal output into Word daily |
| Broadcast-input-to-all | dangerous, niche, contradicts never-guess |
| Bell sound / visual bell | NeedsYou pill + taskbar flash already cover it |
| Command palette | launcher + history popup + blocks panel already cover every verb |
| Scrollback search count badge etc. | search works; polish without new capability |
| Separate "new terminal in same cwd" shortcut | Duplicate (§7.1) is the same gesture with a better name |

---

## 2. Non-negotiable invariants

1. **ZERO wire changes**: no protocol.rs, state.rs persistence, or daemon edits anywhere
   in this spec (the one state.rs item — nothing — was verified; Prefs is GUI-local
   JSON). No proto coordination with sidebar-p2/sleep needed; merge order free.
2. **UX doctrine** (ux-doctrine.md): mouse-first (the menu IS the visible entry point for
   copy/paste — hotkeys stay silent accelerators), zero dividers/strokes (menus use the
   existing `menu_item_style` SURFACE_4 grammar, mod.rs:6084 — call it at the top of
   EVERY menu/submenu closure), hover-reveal, "terminal with magic" screenshot test
   (the drop tint is a whole-surface wash + text, no border).
3. **Pointer never disarms the composer** (P3 bug-fix-2 contract): opening the menu,
   clicking menu items, and dropping files are pointer acts — they must never call
   `on_raw_input`, never consume the prompt episode, never blur Compose. Only bytes
   headed for the PTY do that, and they already do it centrally (mod.rs:4538-4545).
4. **Never guess**: a drop routes to the SELECTED terminal only (winit gives no drop
   position — A3); untranslatable/remote paths are refused with a visible explanation,
   never silently mangled; menu items disable (dim) rather than hide-and-surprise.
5. **Copy is the §6 synthesized copy**: every copy surface (menu Copy, Select all→Copy,
   copy-on-select, Ctrl+C/Ctrl+Insert) goes through `TermBackend::selection_text()`
   (term_backend.rs:1608) — copy matches paint, covers synthesize, spacers blank.
6. **Paste parity for drops**: dropped-path bytes take the exact `paste()` path
   (bracketed iff `TermMode::BRACKETED_PASTE`, `write_and_pin` bottom-pin) — a drop is a
   paste the OS typed for you.
7. **Mirror purity / journal untouched**: everything here is GUI view state. Clear
   scrollback clears the LOCAL ring only (§7.2) — the daemon mirror, journal, and blocks
   sidecar never hear about it.
8. **Zero idle cost**: no new per-frame scans. Link hit-test stays on-demand (already
   is); drop code runs only when `raw.hovered_files/dropped_files` is non-empty; the
   menu closure runs only while open.

---

## 3. P1a — Terminal-area right-click context menu

### 3.1 Decisions

| # | Decision | Justification |
|---|---|---|
| R1 | Attach the menu to the term_view grid response in `terminal_card` (mod.rs), not inside term_view | the items need App context (clipboard, search state, `live_cwd`, `send`, blocks, modals); term_view stays App-free like the block toolbar |
| R2 | **Right-click-paste loses; menu wins.** No paste-on-right-click even with a selection | one gesture = one meaning; the menu carries Paste one hover away; WT itself moved from paste-default to menu-default (their `experimental.rightClickContextMenu` → default in Terminal 1.19+); mouse-first doctrine wants a VISIBLE copy/paste entry and this is it |
| R3 | **MOUSE_MODE && !shift ⇒ right-click forwards to the app; Shift+right-click always opens the menu.** Otherwise menu always opens | WT precedent ("menu wins unless the app captures"); mirrors the existing left-click rule verbatim (term_view.rs:726 `MOUSE_MODE && !click_mods.shift`) so the shift-override convention is ONE rule app-wide; forwarding needs A2's two enum variants and nothing else |
| R4 | Menu never touches selection state | selection code only reacts to Primary (term_view.rs:629) — right-click preserving the selection is true by construction; menu Copy needs exactly that |
| R5 | Items disable (dim) via `ui.add_enabled`, never vanish | stable chrome doctrine (F3): the menu shape is learnable; a vanished item reads as a bug |
| R6 | Menu actions route Paste through the same mode-aware router as drops (§4.3) | Compose ⇒ draft, Raw ⇒ PTY with the §5 safety gate — one router, one truth |

### 3.2 Menu contents (top to bottom)

| Item | Enabled when | Action (all existing machinery) |
|---|---|---|
| Copy | `backend.selection_text().is_some()` | `copy_selection` (term_view.rs:813) — §6 synthesis |
| Paste | status Running (presented; asleep/dead ⇒ dim) | route_paste(id, clipboard text): Compose ⇒ `ComposerState::insert_dropped_text` (§4.5); Raw ⇒ `paste()` through the §5 gate |
| Select all | grid non-empty (always true) | `TermBackend::select_all()` — `Selection::new(SelectionType::Lines, Point::new(Line(-(history_size as i32)), Column(0)), Side::Left)` + `update()` to (bottommost_line, last column). Copy stays a second, deliberate click (WT parity) |
| — separator — | | |
| Find | always | extract the bar toggle body (mod.rs:3783) into `App::open_search()`; call it |
| Open cwd in Explorer | `resolve_local_cwd(t).is_some()` (§3.3) | `open::that_detached(dir)` |
| Rerun last: `<cmd>` | `can_rerun(id)` && last CLOSED rec exists (blocks map) | `rerun_block(id, last_rec.start_off)` (mod.rs:1939); cmd middle-ellipsized to ~32 chars via the existing `ellipsize` helpers |
| — separator — | | |
| Clear scrollback | history_size > 0 | `App::clear_scrollback(id)` (§7.2) |
| Copy on select ✓ | always (toggle row) | flips `Prefs.copy_on_select`, `save_prefs()` (§6.2) — the only visible entry point a settings-UI-free app has |

Everything terminal-LIFECYCLE (Kill/Restore/Wake/Rename/Move/Delete/Sleep) stays in the
sidebar row menu and top bar — the grid menu is content-scoped; two menus with disjoint
vocabularies never fight.

### 3.3 `resolve_local_cwd(t) -> Option<PathBuf>` (shared with §7.1)

| Family / namespace | Result |
|---|---|
| Win namespace (pwsh, cmd, claude-kind, custom) | `live_cwd` else `meta.cwd` |
| WslShell{distro} (posix cwd) | `\\wsl.localhost\<distro>\<path with / → \>` — Explorer opens WSL UNC natively |
| WSL cwd under `/mnt/<drive>/…` | translate back to `<Drive>:\…` (inverse of `wsl_mnt_path`) — nicer than the UNC form |
| Ssh{..} | None (dim the item) — no local directory exists |

### 3.4 Mechanics

- **term_view** (`process_input`): add `PointerButton::Secondary` + `Middle` arms. Under
  `MOUSE_MODE && !click_mods.shift`: `backend.mouse_report(MouseButton::RightButton /
  MiddleButton, …)` for press AND release (SGR release is 'm', legacy release is the
  existing `3 + mods` — both already handled by `mouse_report`, term_backend.rs:1708-1727)
  and set `out.rclick_forwarded = true` for the frame. Middle outside MOUSE_MODE:
  `out.middle_paste = true` (§6.1). New enum variants: `MiddleButton = 1`,
  `RightButton = 2` (xterm button codes; `LeftMove = 32` untouched).
- **mod.rs terminal_card**: after `term_view::show`, when `!out.rclick_forwarded`, call
  `resp.context_menu(|ui| { menu_item_style(ui); … })`. egui opens it on secondary click
  on that response and manages position/close-on-click-outside; menu widgets live in an
  egui menu layer, so their clicks never reach the grid's raw-event input path (the
  blocks-panel/launcher precedent). Gating the *call* on `rclick_forwarded` means a
  forwarded click never opens the menu, and an already-open menu stays manageable by egui.
- **Focus**: opening/closing the menu is pointer-only — invariant 3. A Compose composer
  keeps re-requesting focus every frame while `!overlay_open` (composer contract); the
  menu is NOT added to `overlay_open` (it is self-contained like the block toolbar; the
  TextEdit keeping keyboard focus while a menu is open is fine — menu items are
  pointer-driven). After "Paste into draft", composer focus is already held.
- **Dashboard / empty central**: no grid response ⇒ no menu (nothing to attach to).

### 3.5 Family/mode edge table

| Situation | Right-click behavior |
|---|---|
| Hooked shell at armed prompt (cover painted) | menu; Paste ⇒ draft append; episode untouched |
| Raw shell / claude REPL (alt, no MOUSE_MODE) | menu; Paste ⇒ PTY (bracketed for claude — it sets BRACKETED_PASTE) through §5 gate |
| vim/mc with `mouse=a` (MOUSE_MODE) | forwarded as button-2 press/release; Shift+right-click ⇒ menu |
| MOUSE_MODE app, selection made with Shift+drag | Shift+right-click ⇒ menu with Copy enabled |
| Dead terminal (frozen grid) | menu; Copy/Select all/Find/Open cwd work; Paste/Rerun/Clear dim (Clear works — view state — keep enabled; Paste/Rerun dim) |
| Asleep terminal (sleep-spec: frozen frame) | same as Dead row — Paste dimmed satisfies sleep inv. 5 (no input path can wake/spawn) |
| Right-click during a left-drag selection | egui delivers the secondary press; drag continues (Primary state machine untouched); menu opens on release frame — harmless |
| Scrolled up (display_offset > 0) | menu opens; Copy of a scrolled selection works (selection survives scrolling by design) |

Unit tests: menu-enabled-state table (pure fn `menu_gates(status, has_sel, can_rerun,
local_cwd, history) -> MenuGates`); `select_all` range over history+screen incl. a
synthesized-cover row (copy text matches paint); RightButton/MiddleButton SGR + legacy
encodings golden (`\x1b[<2;…M`, release `m`; legacy 32+2, release 32+3).

---

## 4. P1b — Local drag-drop (the headline)

### 4.1 Decisions

| # | Decision | Justification |
|---|---|---|
| D1 | Consume `raw.dropped_files` in `App::ui` and route to the **selected** terminal, only when `CentralView::Terminal` is showing | winit Windows drops carry no cursor position (A3) — the selected terminal IS the visible grid the user is aiming at; dashboard/empty views ignore drops (nothing is "the terminal") |
| D2 | Insert QUOTED PATHS as paste-semantics bytes; never upload, never read file contents | that is what WT/terminals do and exactly the Claude Code workflow — claude resolves local paths itself |
| D3 | Routing: **Compose ⇒ append to draft; anything else ⇒ PTY bytes** | the composer is stationary input furniture — text lands where typing would land; raw/TUI (claude) gets the paste path (bracketed under claude's BRACKETED_PASTE ⇒ atomic, exactly like pasting the path today) |
| D4 | Family-aware quoting (§4.4); WSL gets `/mnt/<drive>` translation via the existing `wsl_mnt_path` (bootstrap.rs:246, pub, same crate) plus `\\wsl.localhost\<distro>\…` → in-distro absolute paths | a Windows path is a syntax error to bash; the translation function is already golden-tested |
| D5 | ssh terminals: **refuse with an explanation, insert nothing** (v1). The refusal renders DURING the hover (tint label: "remote upload lands with #26"), so the no-op drop is pre-explained | a local path is meaningless on the remote host — inserting it would be a silent lie (never-guess); #26 replaces exactly one match arm with the upload pipeline (§4.6) |
| D6 | Drop-target visual: full-grid ACCENT wash (~6% alpha) + one centered TEXT_MUTED line while `hovered_files` non-empty | doctrine: surface shift + text, no border; the label doubles as the preview/refusal surface since hover paths ARE available (egui `HoveredFile.path`) |
| D7 | Multi-file: space-separated, one trailing space | shell argument convention; trailing space lets the user keep typing (WT behavior) |
| D8 | Drops are exempt from the §5 paste gate | paths never contain newlines; a drop is a deliberate single-line insertion |

### 4.2 Intake (mod.rs, `ui()` — runs only when the fields are non-empty)

```rust
let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());  // tint + label
let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter()
    .filter_map(|f| f.path.clone()).collect());                  // route once
if !dropped.is_empty() { self.route_file_drop(dropped); }
```

egui clears `dropped_files` next frame; multiple files arrive in one frame (winit emits
one event per file into the same pump). `hovered_files` clears on HoveredFileCancelled /
drop (egui-winit lib.rs:450,458).

### 4.3 `route_file_drop(paths)` — the single router (also serves menu-Paste, §3, and middle-click, §6.1, for the mode decision)

```
selected terminal? CentralView::Terminal? presented Running?      — else: no-op
family = shell_family(&t.kind, &t.program, &t.args)               (state.rs:138)
family == Ssh          ⇒ REFUSE (hover label already explained)   [#26 hook: this arm]
per file: translate + quote per §4.4; None (untranslatable) ⇒ skip, count
insert = joined + trailing space
composer mode == Compose (composers map)  ⇒ st.insert_dropped_text(&insert)
else                                       ⇒ paste-semantics bytes via the existing
                                             paste() path → C2D::Input (mod.rs:4579
                                             write path, so on_raw_input fires — the
                                             bytes ARE PTY input, routing is truthful)
```

Note the asymmetry with invariant 3: a drop that lands in the DRAFT is a pointer act
(no episode consumed); a drop that goes to the PTY is real input and takes the normal
raw-input path — same as any paste.

### 4.4 Translation + quoting table (pure fns in new `src/gui/drop.rs`, golden-tested)

| Family | Translation | Quoting |
|---|---|---|
| Pwsh | none | always single-quote, internal `'` → `''` (PS: `"` interpolates `$`, backtick escapes — single quotes are the only inert form) |
| Cmd | none | wrap in `"…"` iff contains space/`&^%()` etc.; `"` is illegal in Win paths so no escape needed |
| WslShell{distro} | `C:\…` → `wsl_mnt_path` ⇒ `/mnt/c/…`; `\\wsl.localhost\<distro>\rest` and `\\wsl$\<distro>\rest` (case-insensitive, matching THIS terminal's distro) ⇒ `/rest` with `\`→`/`; other UNC / mismatched distro ⇒ None (skip + label counts it) | bash single-quote, internal `'` → `'\''` |
| Ssh | — | refused (D5) |
| Other (claude-kind, Custom, hookless) | none | WT-style: bare when `[A-Za-z0-9_\-.:\\/]+` only, else wrap in `"…"` — claude/CLIs tokenize both |

Directories drop the same as files (a path is a path).

### 4.5 Composer append — `ComposerState::insert_dropped_text(&mut self, s: &str)`

Append to `draft` (insert `" "` separator if draft is non-empty and doesn't end with
whitespace), set the existing `caret_to_end` one-frame flag (P4 machinery). NOT
`insert_history` (that stashes/replaces — wrong semantics; a drop composes INTO the
command being typed). Works identically while post-submit buffering (draft is live).

### 4.6 The #26 ssh hook point (design constraint, not implementation)

`route_file_drop`'s family match has exactly one Ssh arm. v1 body: mark
`drop_refused_until: Option<Instant>` (drives the label for ~2s if someone drops without
hovering long enough to read). #26 replaces the body with its upload pipeline (scp/sftp
+ progress + toast) and inherits: the paths Vec, the terminal id, the composer-vs-raw
mode decision, and the tint/label surface. Nothing else in this spec may branch on Ssh —
one seam, one replacement.

### 4.7 Drop-target visual (term_view paint, doctrine-compliant)

While `hovering && CentralView::Terminal`: painter.rect_filled(grid_rect, 0,
ACCENT.gamma_multiply(0.06)) painted AFTER grid shapes (over text, under nothing — it is
the top wash), plus one centered line, TEXT_MUTED, 13px:

| State | Label |
|---|---|
| Compose armed | `add to command — 2 files` (count from hovered_files) |
| Raw/TUI | `insert path` / `insert 2 paths` |
| Wsl, all translatable | `insert as /mnt/… path` |
| Wsl, some untranslatable | `2 of 3 translate to /mnt/…` |
| Ssh | `remote upload lands with #26` |
| Dead/Asleep | `terminal is not running` |

No strokes, no panel, no icon. The label IS the refusal surface (D5) — after a refused
drop nothing further appears in v1 (#26's toast upgrades this).

### 4.8 Edge table

| Edge | Behavior |
|---|---|
| Drop while dashboard/empty central | ignored (D1); tint never painted |
| Drop on the sidebar area | routes to selected terminal (no position info — documented; the tint over the grid says where it will land) |
| Drop while composer post-submit buffering | draft append (mode is Compose during the window — P3 typeahead design) |
| Drop on claude mid-response (alt, output flowing) | PTY paste, bracketed — identical to pasting a path mid-response today; claude buffers it as typed input |
| Drop on Dead/Asleep terminal | refused with label; zero bytes, zero spawns (sleep inv. 5) |
| Path with `'` into pwsh/wsl | quoted correctly (goldens); path with `"` cannot exist on Win filesystems; posix-origin `"` paths (via \\wsl.localhost) still single-quoted for bash |
| Mixed translatable/untranslatable multi-drop into WSL | translatable inserted, others skipped, label counted it at hover |
| GUI unfocused during OS drag | OLE delivers hover/drop regardless of focus; routing unchanged (selected terminal is still the visible one) |
| Files > ~50 dropped | insert all (it is text); PENDING_CAP-class limits don't apply — a shell line of 50 paths is the user's own choice |

Unit tests (drop.rs pure): quoting goldens per family incl. `'`-bearing paths; wsl
translation incl. `\\wsl$` + `\\wsl.localhost` + mismatched distro ⇒ None; router table
(family × mode × status ⇒ Draft/Pty/Refuse) as a pure-fn walk.

---

## 5. P2 — Paste safety (raw path only)

### 5.1 Decisions

| # | Decision | Justification |
|---|---|---|
| P1 | Gate fires only on the RAW grid paste path when `!BRACKETED_PASTE` && (post-sanitize line count > 1 \|\| bytes > 5000) | bracketed paste is the app declaring "I handle pastes atomically" (claude, bash readline, TUIs) — warning there would nag the user's heaviest daily flow; PS 5.1 (never bracketed) executing every `\r` immediately is THE accident class; 5000-byte large-paste bar is WT's precedent |
| P2 | Composer paste is exempt structurally | multi-line pastes into the TextEdit buffer VISIBLY and submit line-by-line only on explicit Enter (P3 design) — already the safe surface |
| P3 | Confirm = existing modal pattern (`Modal::ConfirmPaste{id, text}`), preview first 3 lines + `+N more lines`, buttons **Paste** / Cancel, checkbox "Don't warn again" ⇒ `Prefs.paste_warn = false` (serde-default true) | ConfirmDeleteTerminal's exact shape; a timing-window "paste twice to confirm" is hidden state the doctrine forbids (sleep-spec S8's argument) |
| P4 | Middle-click paste and menu Paste share the gate (they call the same route) | one gate, no bypass surface |
| P5 | On confirm, bytes ship through the ORIGINAL paste() encoding decision (re-checked mode at send time) | mode may have flipped during the modal; encode at send, not at capture |

### 5.2 Mechanics

`paste()` (term_view.rs:801) gains a caller-supplied gate: term_view emits
`out.paste_pending: Option<String>` instead of writing when the gate trips (it knows
mode + text; the pref rides in as a new `show()` param bundled into a small
`ViewOpts{copy_on_select, paste_warn}` struct to stop parameter creep). mod.rs converts
`paste_pending` into the modal; modal-confirm calls `App::send_paste(id, text)` which
re-runs the encoding (bracketed check) and ships via the standard write path (composer
`on_raw_input` fires — it is PTY input). Byte-order note: a same-frame key+paste loses
ordering only when the modal interposes — acceptable by construction (the user is
answering a dialog).

Unit tests: gate table (lines × bytes × bracketed × pref); modal-confirm encodes
bracketed iff mode at confirm time.

---

## 6. P2/P3 — Clipboard & mouse convention completions

### 6.1 Middle-click paste (P3)

Outside MOUSE_MODE: middle-click RELEASE over the grid ⇒ clipboard text through the §4.3
mode router (Compose ⇒ draft, Raw ⇒ PTY via §5 gate). Under MOUSE_MODE && !shift ⇒
forward as button 1 (§3.4). X11/WT-familiar; 10 lines on top of P1 machinery. Clipboard
read: egui 0.35 exposes paste only as events — reuse eframe's clipboard via
`ctx.send_viewport_cmd`? NO — simplest correct: egui-winit's clipboard is not directly
readable from App; instead synthesize by pushing `egui::Event::Paste` is not possible
either. Use `window.clipboard`? Decision: read via the `arboard`-free path we already
have — eframe exposes nothing; ADD the tiny `clipboard-win` crate? **No new deps
mandate-side; instead**: middle-click paste is implemented by injecting a synthetic
`egui::Event::Paste` is unavailable — so middle-click paste ships ONLY IF a zero-dep
clipboard read exists: `windows` crate (already a dep) `Win32_System_DataExchange`
OpenClipboard/GetClipboardData(CF_UNICODETEXT) — ~30 lines, feature-add
`"Win32_System_DataExchange"` + `"Win32_System_Memory"` to the existing windows dep.
That is the plan of record; if the implementer finds it disproportionate, drop
middle-click paste alone (it is the lowest-ranked item) — do not add a clipboard crate
for it.

### 6.2 Copy-on-select (P3, pref default OFF)

`Prefs.copy_on_select: bool` (serde-default false; save via `save_prefs()` mod.rs:876).
In `process_input`, at the three selection-commit edges — Primary release while
`vs.dragging` with a non-empty selection, double-click, triple-click — call
`copy_selection` when the pref is set. The §6-synthesis invariant makes this exact.
Visible entry point: the menu toggle row (§3.2) — the app has no settings UI and
doctrine demands a clickable path. Interaction with "output clears selection at
offset 0": copy already happened at mouse-release — a later clear is fine.

### 6.3 Ctrl+wheel guard (P2, one line)

term_view.rs:613 `MouseWheel` arm: destructure `modifiers` from the event and skip when
`modifiers.command` (the event carries its own modifiers — do not re-read ctx state).
Today Ctrl+wheel zooms (mod.rs:5588) AND scrolls/ships `ESC O A/B` under alt-screen —
verified emit-side (egui-winit always pushes the raw event, lib.rs:941).

### 6.4 Double-click word boundaries (P2, one line + tests)

Set `semantic_escape_chars` at the ONE Config construction site (term_backend.rs:267) to
the alacritty default MINUS `:` — i.e. `,│`|"' ()[]{}<>\t`. Effect: double-click
selects `C:\Users\alice\shot.png` and `https://x.dev/a?b=1` whole (today both split at
the colon); cost: `key: value` double-click on the key grabs `key:` (trailing colon) —
trivially edited, and the path/URL win is the user's daily gesture. `│` stays (box-drawing
walls); quotes/brackets stay (they delimit words). Unit test: semantic selection over a
seeded grid row picks the full path/URL.

### 6.5 Link polish (P2)

1. PointingHand cursor while `hovered_link.is_some()` (`ctx.set_cursor_icon` in show(),
   after the hover resolve at term_view.rs:352-363) — currently the only unlinked-looking
   link affordance is the underline.
2. Covered rows: suppress link hover/click when the row is a healthy pres cover or the
   armed cover row (`backend.cur_blank_line` + covers list — the same suppression set
   selection/search already use). The hit-test reads GRID text while the row paints
   synthesized `❯ cwd cmd` — a column-mismatched underline/click on a covered row is a
   wrong-cover-class bug; suppression is refuse-over-guess. URLs inside a covered row
   remain reachable via the raw view (select/scroll — covers drop under selection).

### 6.6 The clipboard matrix (documentation of record — no code)

| Chord | Today (verified) | Notes |
|---|---|---|
| Ctrl+C | copy if selection else interrupt | term_view.rs:591; composer intercepts while armed (composer.rs:1829) |
| Ctrl+Shift+C | identical to Ctrl+C | egui fold erases shift (lib.rs:1400); no-selection ⇒ interrupt — accepted quirk, cannot distinguish |
| Ctrl+V / Ctrl+Shift+V / Shift+Insert | paste | fold (lib.rs:1406-1410); §5 gate added |
| Ctrl+Insert | copy | fold (lib.rs:1403) |
| Ctrl+X / Shift+Delete | interrupt-style Cut (0x18/win32) | term_view.rs:604 |
| bindings.rs:196-197 Copy/Paste rows | dead code | may be deleted or kept as documentation; they can never fire (A5) |

---

## 7. P2 — Misc everyday

### 7.1 Duplicate terminal (sidebar row context menu, after "Rename")

Build `NewTerminal` from the row's meta and `send(C2D::CreateTerminal{spec})`:
- `name`: `uniquify_name(&t.name, taken)` (launcher.rs:255).
- `kind`: **Claude ⇒ fresh `session_id: Uuid::new_v4()`**, `extra_args` cloned — NEVER
  copy the pinned id (two terminals resuming one session id is the corruption the
  Ambiguous-adapter machinery exists to prevent); Shell/Custom cloned verbatim.
- `program`/`args`: cloned.
- `cwd`: Win namespace ⇒ `live_cwd` else `meta.cwd` (duplicate means "where it is NOW");
  Wsl posix ⇒ `live_cwd` verbatim (launch `--cd` accepts posix); Ssh ⇒ empty (remote
  $HOME — ssh rows persist empty cwd by design; edge documented).
- `shell_cfg`: cloned (carries remote_hooks opt-out); `folder`: same folder;
  `already_launched: false`.
- Auto-select via the existing `pending_create` name-match (launcher machinery).
- Do NOT touch `last_spawn`/`recent_spawns` — a duplicate is not a launcher choice.

Covers "open new terminal in same cwd" (A20) — one gesture, obvious name, mouse-first.

### 7.2 Clear scrollback — VIEW-ONLY (recommended and pre-decided)

`TermBackend::clear_scrollback_view()`:
1. `scroll_to_bottom()`, then `term.grid_mut().clear_history()` (pub, alacritty
   grid/mod.rs:383).
2. Prune GUI anchoring state exactly like the existing ED3 rule (drop-don't-drift):
   anchors/covers with `line < 0` dropped, `prompt_end` kept iff `line >= 0`,
   jump_flash cleared.
3. Invalidate stored search matches (the history-drift invalidation path).
4. Selection cleared (`clear_selection` — coordinates died).

**Journal/daemon implications, documented honestly:** the daemon mirror (2000-line
history), journal, and blocks sidecar are untouched — a reopen/reattach resurrects the
scrollback via serialized replay + ReplayAnchors. That is the deliberate v1 contract:
"clear" is a view gesture (declutter now), not data destruction. A daemon-side truncate
would have to reconcile mirror purity (history rows conhost still repaints on resize),
journal `base` bookkeeping, blocks sidecar offsets, and preface reconstruction — a
whole-subsystem change for a gesture users invoke to tidy a view. Rejected for v1;
sketched as Q4 if "it came back after reboot" ever surfaces as a complaint.

### 7.3 Rerun-last / Find / Open-cwd

All menu rows over existing machinery — specified in §3.2; no additional design.

### 7.4 Attention toast — DEFERRED to #26 wholesale

NeedsYou pill + taskbar flash already exist (A19). #26 builds the shared toast
component (it needs one for upload progress/failures); the attention toast then becomes
a consumer of that component. Building an interim toast here would be the exact
duplicate-component churn the mandate forbids. No work in this spec.

---

## 8. (reserved — merged into §7; numbering kept stable for review cross-refs)

---

## 9. File-by-file plan

| File | Changes |
|---|---|
| **`src/gui/drop.rs` (NEW)** | pure: `quote_for_family`, `translate_wsl`, `RouteVerdict` router table, label text builder + all goldens/unit tests. New file = no sidebar-p2 churn contact |
| `src/gui/term_view.rs` | Secondary/Middle arms (+forwarding, +`out.rclick_forwarded`/`middle_paste`); wheel modifier guard (§6.3); paste gate emit (`out.paste_pending`); copy-on-select at commit edges; link cursor + covered-row suppression (§6.5); drop tint+label paint (§4.7); `ViewOpts` param struct |
| `src/gui/term_backend.rs` | `MouseButton::{MiddleButton=1, RightButton=2}`; `select_all()`; `clear_scrollback_view()`; semantic_escape_chars at Config (§6.4) |
| `src/gui/mod.rs` (sidebar-p2 territory — coordinate at merge, additions are localized) | grid `resp.context_menu` closure + `menu_gates` (§3); `route_file_drop` + intake (§4.2); `Modal::ConfirmPaste` + `send_paste`; `open_search()` extraction; `resolve_local_cwd`; `clear_scrollback(id)`; Duplicate row in the sidebar terminal menu (§7.1); `Prefs.copy_on_select`/`paste_warn` (serde-default); middle-paste clipboard read (§6.1, windows-crate feature add) |
| `src/gui/composer.rs` (sidebar-p2 territory) | `insert_dropped_text()` (§4.5) — one small method |
| `Cargo.toml` | windows features += `Win32_System_DataExchange`, `Win32_System_Memory` (§6.1 only — drop if middle-paste is dropped) |
| `src/protocol.rs`, `src/state.rs`, daemon/* | **NOTHING** |
| `docs/qol-spec.md` | this file |

---

## 10. Tests & staging

Cargo (all pure/sim — no daemon needed):
- drop.rs: quoting goldens (pwsh `'`-escape, cmd spaces, bash `'\''`, Other bare-vs-quoted),
  wsl translation (drive, `\\wsl$`, `\\wsl.localhost`, mismatch⇒None, UNC⇒None),
  router table (family × mode × status).
- term_view/term_backend: mouse-report goldens for buttons 1/2 (SGR + legacy, press +
  release); select_all range walk incl. cover synthesis; clear_scrollback prune table
  (anchors/covers/prompt_end/selection); wheel guard (ctrl-wheel ⇒ no scroll, no arrows);
  semantic word-select over seeded path/URL rows; copy-on-select commit-edge walk.
- mod-level pure fns: `menu_gates` table; paste-gate table (§5); `resolve_local_cwd`
  table; duplicate-spec builder (fresh claude uuid asserted ≠ source, shell_cfg cloned,
  prefs untouched).

Probes: none required (zero daemon changes). Existing suite must stay green — the only
shared surface touched is term_view input, covered by the sim_frame/composer tests.

Staging (visual verification, the scratch-tree recipe — staged-gui rig with
`raw_input_hook` + TC_DEMO_POS parked off-screen, knobs NEVER in the main tree):
- Menu: rclick verb → PrintWindow — SURFACE_4 hover rows, dim states (dead terminal,
  no selection), zero strokes.
- Drop: the raw_input_hook gains `hoverfiles <paths>` / `dropfiles <paths>` verbs that
  set `raw_input.hovered_files/dropped_files` directly (OLE cannot be synthesized while
  the workstation is in use — SendInput rule) — tint label per §4.7 states, then the
  draft/PTY landing shot. ssh terminal via TC_SSH_VIA_WSL for the refusal label.
- Paste modal: clipboard seeded, Ctrl+V into a raw PS 5.1 prompt → modal shot; confirm →
  lines land; "don't warn again" → pref persisted.
- Acceptance bar (the user's workflow): drop a PNG from Explorer onto a claude terminal
  → quoted path appears in claude's input box; onto an armed pwsh composer → path in the
  draft; Enter runs.

---

## 11. Open questions (defaults chosen — implementer may proceed on defaults)

| # | Question | Default |
|---|---|---|
| Q1 | Menu also on the composer strip / sidebar empty space? | NO — grid menu is content-scoped; strip has its own affordances; keep vocabularies disjoint |
| Q2 | "Copy on select" row in the menu vs hidden pref | menu row (only mouse-first surface that exists); drop the row if a settings surface ever ships |
| Q3 | Middle-click paste worth the Win32 clipboard read? | YES if ~30 lines as sketched; DROP alone otherwise (§6.1) — never add a clipboard crate for it |
| Q4 | Daemon-side scrollback truncate ("clear forever") | OUT (§7.2) — revisit only on real user complaint; requires mirror/journal/sidecar reconciliation design |
| Q5 | Forward wheel as mouse-report buttons 64/65 to MOUSE_MODE apps (currently only ALTERNATE_SCROLL arrows) | OUT — no observed daily need; note kept here as the known gap |
| Q6 | Drop onto a sidebar ROW routing to that terminal | impossible without drop position (winit gives none); revisit only if winit grows OLE position events |
| Q7 | `paste_warn` large-paste threshold | 5000 bytes (WT parity); not configurable v1 |
| Q8 | Menu item to copy the hovered block's output (menu-dup of the toolbar) | NO — hover toolbar + blocks panel already serve it; menu stays ≤ 9 rows |

---

## 12. DO-NOTs (each protects a probe-pinned or doctrine behavior)

1. **DO NOT let menu open/close or drops call `on_raw_input` or consume the prompt
   episode** — pointer-never-disarms is the P3 bug-fix-2 contract; only bytes headed for
   the PTY are raw input (and those already route through the central write path).
2. **DO NOT copy on right-click or auto-copy the Select-all** — copy is always an
   explicit act (menu Copy / Ctrl+C / the opt-in pref); surprise clipboard writes
   clobber user state.
3. **DO NOT paste on right-click** (R2) — one gesture, one meaning; the menu carries
   Paste.
4. **DO NOT bypass `selection_text()` with `term.selection_to_string()` anywhere** —
   raw grid text diverges from painted covers/spacers; copy-matches-paint is a shipped
   invariant with tests.
5. **DO NOT scan for URLs at render time** — the on-demand cached hit-test (A10) is the
   shipped perf design; a per-frame regex over the viewport re-introduces exactly the
   cost it avoided.
6. **DO NOT position-route drops** — winit delivers no drop coordinates on Windows;
   any position heuristic (last pointer pos) is stale the moment OLE captures the mouse.
7. **DO NOT translate or "helpfully" localize paths for ssh terminals** — refuse (D5);
   the #26 seam is one match arm, keep it that way.
8. **DO NOT warn on bracketed-paste multi-line pastes** — that nags the claude REPL (the
   user's heaviest daily surface) to protect apps that already protect themselves.
9. **DO NOT touch the daemon mirror/journal for clear-scrollback** — view-only (§7.2);
   feeding the mirror anything conhost didn't emit violates mirror purity and corrupts
   resize reflow/DSR forever.
10. **DO NOT duplicate a Claude terminal's pinned session id** (§7.1) — mint a fresh
    uuid; a shared `--resume` id is the wrong-session class the tracker refuses to guess
    about.
11. **DO NOT add strokes/borders to the menu, tint, or modal** — SURFACE_4 hover fills
    via `menu_item_style` in EVERY closure (submenus derive style from ctx, each needs
    the call — the shipped menu-hover fix).
12. **DO NOT ship the staging knobs** (`hoverfiles`/`dropfiles` verbs, TC_DEMO_*) in the
    main tree — scratch-tree only; env-gated read-only diagnostics are the only
    permanent knobs.
