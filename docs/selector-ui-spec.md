# Terminal Selector UI — Redesign Spec (final, implementation-ready)

Target: C:\Terminal Control (egui 0.35 GUI + daemon, P0–P5 shipped). Scope: everything the
user touches to CREATE a terminal and to FIND/SWITCH between terminals — the titlebar "+ New
terminal" form modal, the Import modal, the sidebar tree/rail, the empty states, and the
dashboard's role in selection. Trigger: a blunt user verdict that the new-terminal selector
UI wasn't good enough.

**Dependency:** docs\p6-shells-spec.md (concurrent, planner-shells) defines WHAT can be
launched — shell kinds, WSL distros, ssh hosts, how they're enumerated, any TermKind/protocol
changes. THIS spec defines how those choices are PRESENTED. Wherever this doc says "shell
candidates", read "the list P6 provides". §11 states the exact interface assumed. If P6 isn't
merged first, the launcher ships with the degraded candidate set in §4.8 and picks up P6 data
with zero layout changes.

**Binding doctrine** (ux-doctrine, restated because every pixel here is judged by it):
zero dividers/hairlines/strokes anywhere; structure = spacing, background shifts, soft
shadows, hover-reveal; every drawn pixel must pass "a terminal with magic, not an app
wrapping a terminal"; mouse-first — every action has a visible clickable entry, hotkeys are
silent accelerators only, never shown in chrome, never the only path; no motion on functional
paths, bounded fades (≤120ms) only.

---

## 0. Before-state findings (staged GUI, isolated TC_DATA_DIR daemon, PrintWindow captures)

Findings below were verified against staged screenshots (empty first run, populated
comfortable/compact, rail, new-terminal modal, dashboard) and the code (line numbers =
src\gui\mod.rs @ time of writing).

### The creation flow (where the verdict lands)

- **F1 — Creating a terminal is a form.** `+ New terminal` opens `Modal::NewTerminal`
  (show_modal:3750): a 520px dialog with a kind row (Claude Code / PowerShell / Custom
  selectable-labels), then a 2-column Grid of Name / Folder ComboBox / Directory TextEdit /
  Args-or-Command, then Create/Cancel. Screenshot 05 reads as a settings form floating over
  the app — the single worst screenshot-test failure in the product. Minimum path to "just
  give me a shell": click + → visually parse 4–6 fields → click Create. Two clicks plus a
  form read for the most common action in the app.
- **F2 — Enter does not submit the form.** The NewTerminal arm never reads Enter (only
  `primary_button(...).clicked()`, mod.rs:3805); NewFolder/Rename do (`dialog_input` returns
  Enter). Typing a directory and pressing Enter does nothing.
- **F3 — Kind defaults to Claude** (`open_new_dialog` → `NewKind::Claude`, mod.rs:3690), and
  the kind list is a debug enum: no cmd, no WSL, no ssh; "Custom" = type a raw command line
  into a bare field.
- **F4 — Directory is a raw path TextEdit** prefilled with a single global `prefs.last_cwd`
  ("C:\" on first run). No recents, no validation, no browse — even though the app already
  KNOWS every terminal's cwd, live_cwd, and every block record's cwd. Highest-friction field
  in the flow, and the one the app has the most data to eliminate.
- **F5 — The new terminal is NOT selected after creation.** `apply_snapshot` auto-selects
  only when `selected.is_none()` (mod.rs:917). You create a terminal and stay parked on the
  old one; then you must find the new row in the sidebar and click it. Creation doesn't even
  finish the job.
- **F6 — No name feedback.** Name hint is "optional"; the auto-name (`default_name`:4759,
  "Shell · dir") is invisible until the row appears. Two shells in the same dir get IDENTICAL
  names — no uniquification.
- **F7 — Import is a separate, parallel creation flow** (titlebar icon → `Modal::Import`)
  that duplicates folder-picking and uses stock `ui.checkbox` + **`ui.separator()`**
  (mod.rs:3892) — a live hairline the doctrine stroke-sweep missed.
- **F8 — Folder placement is a stock ComboBox** (folder_picker:3720); creating INTO a folder
  from the sidebar exists only behind right-click → "New terminal here".

### The switching/browsing surfaces (good bones, specific failures)

- **F9 — NeedsYou rows JUMP to the top of their folder** (`sort_by_key(!NeedsYou, order)`,
  sidebar_tree:2257/2318) and jump back when viewed. At 20 terminals the row you aimed at
  moves out from under the pointer; muscle memory is impossible. The amber dot + titlebar
  pill already surface attention — the reorder is pure churn.
- **F10 — Folder row click zones are booby-trapped** (folder_row:2400-2410): left 24px =
  collapse, the NAME opens the folder dashboard (surprise — universal expectation is
  collapse), and on hover the terminal COUNT turns into a delete-✕ in the same pixels — a
  destructive control materializing exactly where your eye just was.
- **F11 — No drag. Reordering = right-click → "Move up" one step per menu round-trip;**
  moving into a folder = right-click → "Move to" → submenu. The daemon already supports
  arbitrary-position reorder in one message (`ReorderTerminal{delta}` clamps to any index,
  daemon\mod.rs:1111).
- **F12 — Rename is modal-only, context-menu-only.** No hover affordance, no double-click,
  no header-bar rename. For auto-named terminals (F6) rename is a core flow.
- **F13 — Dead terminals lie in the sidebar.** Second line shows `idle_label` = time since
  the GUI attached ("45s" on a box dead for days — screenshot 02/03, "ssh build box"). Their
  hover affordance is delete-✕ only; Restore hides in the context menu / header.
- **F14 — The rail (collapsed sidebar) has no create affordance and no identity** beyond
  hover tooltips (screenshot 04) — fine as a status strip, but it dead-ends the selector.
- **F15 — Empty sidebar state is dead text**: "Create one, or import your existing Claude
  sessions." — nothing clickable (mouse-first violation, sidebar_tree:2326).
- **F16 — Dashboard is entered by an unlabeled grid icon or by the folder-NAME click (F10)**;
  its header chip floats detached ("← Back All terminals 7 terminal(s)", screenshot 06);
  cards are uniform gray text with no cwd, no last-command/exit info, no actions — it browses
  but doesn't select any better than the sidebar it duplicates.
- **F17 — Titlebar carries five creation/navigation icons** (sidebar, +, folder, import,
  grid) with tooltip-only identity; import and grid are effectively invisible features.

**Diagnosis.** The sidebar's bones (two-line rows, activity dots, folders, hover-✕) are
right and stay. The creation flow is the failure: it is a form where it should be a single
click, a picker where it should already know the answer, and it doesn't even select what it
made. Secondary failures are the jumpy sort, the folder row's zones, no drag, no inline
rename, and dashboard/empty-state confusion.

---

## 1. Non-negotiable invariants

1. **Zero protocol changes.** Creation is `C2D::CreateTerminal{NewTerminal}` — already
   sufficient. Folder ops, reorder, rename: all existing messages. (P6 may add enumeration
   messages; that's P6's proto bump, not ours.)
2. **Doctrine binds every new pixel** (dividers/strokes = bug; hover-reveal; mouse-first;
   fades ≤120ms; no motion on functional paths — the drag ghost tracks the pointer raw).
3. **One floating surface at a time** (P4 §3.5 rule extended): launcher, search, blocks
   panel, history popup are mutually exclusive; modal beats all.
4. **The launcher never guesses filesystem state**: a typed path row appears only when the
   path exists (one `Path::is_dir` per debounced keystroke, §4.5); candidates are exactly
   reconstructable from state the GUI already holds.
5. **Destructive actions never sit on the hover path.** Delete moves behind the ⋯ menu
   (folders) or keeps its confirm modal (terminals).
6. **Instant create must be deterministic and legible**: same defaults every time
   (§3.1 sticky rule), auto-name previewed in the button tooltip, and the new terminal is
   SELECTED when it arrives.
7. **No new repaint loops**: the launcher repaints on input events only; candidate index
   builds at open-click time, never per steady frame.

---

## 2. Headline decisions

| # | Decision | One-line justification |
|---|---|---|
| D1 | Split the titlebar + into **click = instant spawn** / **chevron = launcher** | The most common action (another shell, same place) must cost one click, not a form |
| D2 | Replace the NewTerminal **form modal** with a **launcher palette** (type-to-filter list) | Lists are picked from, forms are filled in; every candidate is one click |
| D3 | Kill `Modal::NewTerminal` AND `Modal::Import` — claude-session import becomes a launcher section | One creation surface; F7's parallel flow and its separator die together |
| D4 | Auto-select the created terminal (track our own pending create) | F5 — creation should end with you IN the terminal |
| D5 | Auto-name + uniquify ("Shell · dir 2"), rename inline later | Names are metadata, not a creation gate |
| D6 | Sidebar sort is **stable** (order only) — drop the NeedsYou-first bump | F9 — the dot + pill already signal; rows must not move under the pointer |
| D7 | Folder row: whole row = collapse; hover reveals `+`, `⊞`, `⋯`; delete only inside ⋯ | F10 — expected affordance, and destructive off the hover path |
| D8 | **Drag** rows to reorder and drop into folders (2px accent insertion bar) | F11 — direct manipulation at 20 terminals; daemon already supports it |
| D9 | **Inline rename** (hover ✏ / double-click / header-name click), rename modals deleted | F12 — rename where the name is |
| D10 | Dead rows: sublabel "exited", hover shows ↻ Restore beside ✕ | F13 — stop showing fake idle time; put revival on the row |
| D11 | Empty states EMBED the launcher content (no modal, no dead text) | F15/first-run — the selector is the first thing a new user sees; make it the real one |
| D12 | Dashboard stays (browse/preview), entered by titlebar ⊞ + folder-row hover ⊞ only; cards gain cwd + last-command line | F16 — distinct job (glance), clearer entries; sidebar remains THE switcher |
| D13 | Titlebar sheds the folder + import icons (folder creation moves into sidebar hover/launcher footer; import lives in the launcher) | F17 — five icons → three (sidebar, split-+, ⊞) |
| D14 | Launcher slots into the focus chain as: modal > **launcher** > search > blocks = history > composer > grid | It's an app-level overlay; everything below it must not see its keys |
| D15 | All list interaction is mouse-complete; Up/Down/Enter/Esc are silent accelerators | Standing mouse-first doctrine |

---

## 3. Surface A — instant create (the split + button)

### 3.1 Behavior

- Titlebar keeps ONE creation control where "+ New terminal" sits today: a 116px ghost
  button with a split hit-test — main zone (icon+label) and a right chevron zone (~24px,
  chevron glyph fades in on button hover at the standard 0.12 `animate_bool`; at rest the
  button renders exactly like today — no new resting chrome).
- **Main-zone click ⇒ instant spawn**, zero UI: `NewTerminal` built from `Prefs.last_spawn`
  (kind tag + program + args + cwd — "sticky last choice"). First run ever: PowerShell in
  the user's home dir. Every successful create (instant OR launcher) overwrites `last_spawn`.
  Sticky beats "always pwsh" because this user's real default may become `claude` or a WSL
  distro; repetition is the pattern instant-create exists to serve.
- Auto-name: `default_name(kind, cwd)` then `uniquify_name` — if the name exists among
  current terminals, append ` 2`, ` 3`, … (first free suffix). Pure fn, unit-tested.
- Folder: `None` from the titlebar; the folder's id from a folder-row `+` (§5.3).
- Tooltip on the main zone announces exactly what a click does:
  `New terminal — PowerShell · C:\Terminal Control` (kind · cwd from last_spawn). The
  tooltip IS the preview (F6).
- **Chevron-zone click ⇒ launcher palette** (§4). Right-click anywhere on the button also
  opens the launcher (secondary path, not the only one — mouse-first holds).
- Empty-state central button and sidebar first-run block route to the launcher (§6), not to
  instant spawn — a first-time user should see their options once.

### 3.2 Auto-select on create (D4, fixes F5)

`App.pending_create: Option<(String, Instant)>` — the name we just asked for, stamped. In
`apply_snapshot`, if `pending_create` matches a terminal id we don't yet know (new id whose
meta.name == pending name, newest `order` wins), `select_terminal(it)` + clear. 5s expiry
(daemon refused / raced rename ⇒ silently stop retargeting). Selection races (user clicks a
different row before the snapshot lands) resolve in the user's favor: a manual
`select_terminal` clears `pending_create`.

---

## 4. Surface B — the launcher palette (replaces the form modal + import modal)

### 4.1 Placement & shell

- `egui::Area::new(Id::new("launcher")).order(Order::Foreground)`, anchored under the
  titlebar, horizontally centered on the central panel (pivot `CENTER_TOP`, y = titlebar
  bottom + 8px). Width 560px (clamped to window − 32). Max height: min(62% of central
  height, content).
- Visual: `SURFACE` fill, `CornerRadius::same(10)`, soft shadow (the blocks-panel/editor
  shadow), **zero strokes**. Open/close = 90ms opacity fade (existing `tc-dialog-fade`
  mechanism, under the 120ms budget); no slide, no scale.
- Click-outside closes (exempt the + button rect — `launcher_btn_rect`, the
  `blocks_btn_rect` press/release pattern). Esc closes one layer. Mutually exclusive with
  search / blocks panel / history popup (close them on open, and vice versa).

### 4.2 Anatomy (top to bottom)

```
┌──────────────────────────────────────────────────────────┐
│  ❯ type to filter — shells, folders, paths, sessions     │   ← query row, 36px
│                                                          │
│  SUGGESTED                                               │   ← 11px TEXT_FAINT section label
│  ▸ PowerShell            C:\Terminal Control          ↩  │   ← 34px rows
│  ▸ Claude                C:\QuipShot                     │
│                                                          │
│  SHELLS                                                  │
│  ▸ PowerShell 7                                          │
│  ▸ Windows PowerShell                                    │
│  ▸ cmd                                                   │
│  ▸ Ubuntu-22.04                        WSL               │
│  ▸ build-box                           ssh               │
│                                                          │
│  RECENT DIRECTORIES                                      │
│  ▸ C:\Terminal Control                 3 terminals       │
│  ▸ C:\QuipShot\src                                       │
│                                                          │
│  CLAUDE SESSIONS                                         │
│  ▸ "fix the overlay DPI bug…"          C:\QuipShot · 2h  │
│                                                          │
│  ▸ Custom command…                                       │
│                                                          │
│  in: (no folder) ▾                    Enter creates  ·  Esc │  ← footer lane, 28px
└──────────────────────────────────────────────────────────┘
```

(The box border above is diagrammatic only — the real surface is fill + shadow.)

- **Query row**: accent `❯` glyph + borderless `TextEdit` (stable id `Id::new("launcher_q")`),
  13px, hint text as shown, `SURFACE` background (no darker input well — the palette IS the
  input). No search icon, no pill.
- **Section labels**: 11px semibold TEXT_FAINT, 12px top padding, 4px bottom — spacing only,
  never a rule line.
- **Rows** (34px): 16px kind glyph (reuse `Icon::Terminal`; new tiny glyphs `Icon::Shell`,
  `Icon::Distro`, `Icon::Ssh`, `Icon::ClaudeSpark`, `Icon::Dir` — painted, D35 style) +
  13px TEXT label + right-aligned 11px TEXT_MUTED secondary (source tag / cwd / age).
  Hover: `OV_HOVER` fill rounded 6 (exact terminal_row treatment). Selected (keyboard):
  `ACCENT_SUBTLE` fill + the 2px accent left bar — identical grammar to the sidebar's
  selected row, so "selected" means one thing app-wide. The `↩` return-glyph hint renders
  ONLY on the keyboard-selected row, 11px TEXT_FAINT (silent-accelerator display, not
  chrome; drop it entirely if it reads noisy in review — open question Q4).
- **Footer lane** (28px, no fill — quiet like the composer strip): left = folder target chip
  `in: <name> ▾` (TEXT_SECONDARY, brightens on hover, click opens a small stock popup menu of
  folders + "(no folder)" + "New folder…"); right = faint static hints `Enter creates · Esc`
  in TEXT_FAINT 11px. The folder chip is pre-set when the launcher was opened from a folder
  row's ⋯ ("New terminal here…" §5.3).

### 4.3 Candidate model (all client-side, built at open)

```rust
// src/gui/launcher.rs (new, egui-free core — unit-testable)
pub enum CandKind {
    Shell(ShellChoice),         // from P6's gui/shells.rs enumerators (§11)
    RecentDir { cwd: PathBuf }, // spawn default shell there
    TypedDir { cwd: PathBuf },  // §4.5, validated
    ClaudeSession { session_id: Uuid, cwd: PathBuf, preview: String, modified: SystemTime },
    ClaudeNew,                  // "New Claude session" (in query-matched cwd or last_spawn cwd)
    Custom,                     // expands the inline custom editor (§4.6)
    Suggestion { spec_idx: usize }, // recent (kind+cwd) combos
}
pub struct Candidate { pub kind: CandKind, pub label: String, pub secondary: String,
                       pub label_lc: String, pub secondary_lc: String }
pub fn build(state: &SharedState, blocks: &HashMap<Uuid, BlockList-recs-view>,
             shells: &[ShellChoice], sessions: &[import::FoundSession],
             recents: &[SpawnSpec]) -> Vec<Candidate>;
pub fn filter(cands: &[Candidate], query: &str) -> Vec<u32>; // history.rs token-AND rules
pub fn uniquify_name(base: &str, taken: &[&str]) -> String;
```

- **Suggested** (query empty only): the last 3 distinct `SpawnSpec`s (kind+cwd combos) from
  `Prefs.recent_spawns` (ring of 8, §10). First entry == what instant-create would do.
- **Shells**: P6's list verbatim, P6's order. Secondary tag = source ("WSL", "ssh", version).
- **Recent directories**: union of every terminal's `cwd` + `live_cwd` + block-rec `cwd`s
  (the history corpus already in `App.blocks`), deduped, MRU by block `started_ms` /
  terminal presence, capped 12. Secondary = "N terminals" when >1 terminal lives there.
- **Claude sessions**: `import::scan()` minus already-imported ids (existing logic from
  `open_import_dialog`, reused verbatim), top 8 by mtime; secondary = `cwd · age`. Row
  action = the Import spec (`already_launched: true`). A `ClaudeNew` row ("New Claude
  session") sits first in the section.
- **Custom command…** : always last.
- Empty query shows: Suggested / Shells / Recent directories (first 5) / Claude sessions
  (first 3) / Custom. Non-empty query: sections keep their order but show ALL matches;
  section labels hide when a section has no hits.
- Filter: `history::filter`'s exact rules (tokenized AND-substring over label+secondary,
  case-insensitive, no fuzz) — command-recall predictability precedent (P4 D8).

### 4.4 Activation

Click a row (or Enter on the keyboard-selected row) ⇒ build `NewTerminal`:
- Shell/RecentDir/TypedDir/Suggestion ⇒ kind per candidate (P6 tags decide `TermKind` —
  the P6 spec owns any new TermKind variants; without P6: Shell/Custom as today), cwd per
  candidate (Shell rows use last_spawn's cwd; directory rows use the row's dir with
  last_spawn's shell), auto-name + uniquify, folder = footer chip, `already_launched: false`.
- ClaudeSession ⇒ exactly today's Import spec per session (name from preview, F7's flow).
- One activation closes the launcher, sends `CreateTerminal`, sets `pending_create` (§3.2),
  updates `last_spawn` + `recent_spawns`. NO multi-select (the old Import checkbox list is
  deliberately dropped — clicking three sessions three times costs the same clicks as three
  checkboxes + a button, with zero extra UI; bulk import remains possible by re-opening,
  and the section re-filters out imported ones each open).

### 4.5 Typed paths

When the query parses as an absolute path (`[A-Za-z]:\` or `\\`), debounce 250ms then one
`Path::is_dir` check; if it exists, prepend a `TypedDir` row: `Open shell in C:\typed\path`.
No spinner, no error text — a nonexistent path simply produces no row (refuse-over-guess;
the other sections still filter normally so typos degrade gracefully).

### 4.6 Custom command row

Click expands IN PLACE (below the row, palette grows ≤72px) two borderless fields —
`program` (hint: "program") and `args` (hint: "arguments") — plus a `Run ▸`-styled accent
text-button "Create". Enter in either field creates. This replaces the old Custom kind +
its form fields; `split_args` (mod.rs:4787) is reused for args. Collapses on Esc (one Esc =
collapse, second = close palette).

### 4.7 What is deleted

`Modal::NewTerminal`, `NewTermDialog`, `NewKind`, `Modal::Import`, `ImportDialog`,
`folder_picker`, the titlebar folder-icon and import-icon buttons, and the import modal's
`ui.separator()` (F7). `Modal::NewFolder`, rename modals → see D9/§5.4 (rename modals also
deleted; NewFolder modal stays — it's one text field and rare).

### 4.8 Degraded mode (P6 absent)

Shells section = PowerShell (`powershell.exe`) + cmd (`cmd.exe`, TermKind::Custom) + New
Claude session. Everything else identical. The section is data-driven; P6 swaps the data.

---

## 5. Surface C — the sidebar

Keep: two-line rows, activity dots (Working pulse / Idle / NeedsYou amber / Dead ring),
compact + comfortable densities, burst badges, folders with counts, rail collapse, footer
cluster, ✕-with-confirm on terminal rows. These pass the doctrine and daily use; the row
anatomy is untouched. Changes:

### 5.1 Stable order (D6, fixes F9)

`sidebar_tree` sorts by `t.order` alone (both folder groups and loose). Delete the
`activity_of != NeedsYou` sort key. The NeedsYou signal remains: amber dot, amber left edge
bar, burst badge, titlebar "N waiting" pill (which already cycles through waiters on click).

### 5.2 Terminal row hover actions (extends the existing hover-✕)

On hover (existing `hover_t` gate), right side shows, right-to-left: `✕` (delete, confirm
modal — unchanged), `✏` (inline rename, §5.4), and for Dead rows `↻` (Restore =
`C2D::RestartTerminal`) — ↻ takes the slot nearest the text so revival is the easy target.
22px hit-rects, `draw_icon` glyphs, TEXT_MUTED → TEXT on per-icon hover, danger tint only on
✕. Burst badge/unread yields while hovered (exactly today's rule). Compact rows: same
targets; the row is 30px, three 22px targets fit the right edge.
Dead-row sublabel: `exited` (static, TEXT_MUTED) instead of the lying idle timer (F13) —
no persisted death-time exists and we don't add state for cosmetics; "exited" is honest.

### 5.3 Folder row rework (D7, fixes F10)

- Whole row click (chevron OR name) = toggle collapse. The chevron stays as the visual cue.
- Hover reveals, right-aligned in the count's space: `+` (instant create IN this folder —
  §3.1 defaults with `folder: Some(id)`), `⊞` (folder dashboard), `⋯` (menu). Count shows
  at rest, actions on hover — same swap grammar as terminal rows.
- `⋯` menu (stock context-menu, also on right-click): New terminal here… (opens LAUNCHER
  with the footer chip preset), Rename (inline, §5.4), Move up / Move down, Delete folder
  (danger-tinted, confirm modal — unchanged).
- The hover-✕-delete on the folder row is DELETED (destructive off the hover path, I-5).

### 5.4 Inline rename (D9, fixes F12)

`App.renaming: Option<(RenameTarget, String)>` where `RenameTarget = Term(Uuid) |
Folder(Uuid)`. While active, the row's name galley is replaced by a borderless TextEdit
(same 13px font, same x offset, `SURFACE_2` fill rounded 4 behind the text only), value
preseeded, all text selected, focused on the open frame (the `memory.request_focus`
open-frame pattern — LOW-9). Enter/blur commits (`C2D::RenameTerminal/RenameFolder`,
whitespace-trimmed, empty ⇒ cancel), Esc cancels. Entry points: hover ✏, double-click the
row name, context-menu Rename, and clicking the NAME in the terminal header bar (header
name gets `Sense::click` + hover brighten + "Rename" tooltip). While renaming:
`overlay_open` is true for the selected terminal's card (the composer/grid must not fight
for keys — §8). `Modal::RenameTerminal/RenameFolder` are deleted.

### 5.5 Drag to reorder / drag into folder (D8, fixes F11)

- **Arming:** `Sense::click_and_drag` on rows; drag arms at >6px vertical travel (clicks
  and context menus unaffected). While armed: `CursorIcon::Grabbing`, the source row dims
  to 40% opacity, and a **ghost** (name + dot only, `SURFACE_2` fill rounded 6, the standard
  soft shadow, 80% opacity) tracks the pointer raw — no easing, ever.
- **Drop targets**, computed from the y-positions of the rows painted this frame (the
  sidebar already lays them out linearly; collect `(y_range, DropSlot)` while painting):
  - Between two rows ⇒ insertion: a 2px × row-width ACCENT bar in the 2px gap (signal, not
    decoration — the selection-bar grammar).
  - On a folder header (or anywhere in a COLLAPSED folder's row) ⇒ move-into: folder row
    gets `ACCENT_SUBTLE` fill while hovered as a target.
  - The rail accepts no drops (collapse the drag with no-op on release there).
- **Release:** compute target folder + index. Wire:
  1. folder changed ⇒ `C2D::MoveTerminal{id, folder}`;
  2. then `C2D::ReorderTerminal{id, delta}` with `delta = target_idx − current_idx_in_group`,
     where `current_idx_in_group` is computed CLIENT-side by replicating the daemon's
     group ordering (filter by folder, sort by `order` — daemon\mod.rs:1114 does exactly
     this, so the client's replica is exact). Two messages, same daemon thread, processed
     in order — no race. Optimistically apply nothing; the snapshot round-trip is <10ms
     and reorder isn't latency-critical.
- **State:** `App.drag: Option<DragState{ id, from: Option<Uuid>, armed: bool }>` + this
  frame's `Vec<(Rangef, DropSlot)>`. Escape or release outside the sidebar cancels.
- Folder reordering by drag is OUT of scope (folders are few; ⋯ Move up/down stays) — churn
  control.

### 5.6 Rail additions (F14)

A `+` glyph button (28px cell, `footer_glyph` styling) pinned at the rail's TOP, above the
first dot: click = instant create (§3.1, no folder); right-click/chevron-less — the launcher
needs width, so the rail's + never opens it (tooltip says "New terminal — <kind · cwd>").
Everything else in the rail is unchanged.

---

## 6. Surface D — empty states & the dashboard's role

### 6.1 First-run / empty central (D11, fixes F15)

When `state.terminals.is_empty()`, the central panel embeds the LAUNCHER CONTENT inline
(not a popup, not a modal): centered column, max 560px wide, starting at 24% height —
query row + Shells + Claude sessions + Custom sections, same row rendering, same activation
(this is `launcher::build` + the same painter fns hosted in a plain `ui` instead of an
Area). Heading above it: 15px semibold TEXT "Start a terminal", 12px gap. No primary
button, no dead text. The sidebar empty block becomes a single ghost_button "New terminal"
that focuses the embedded query (one clickable, zero prose).
When terminals exist but none is selected (rare transient), keep today's icon + "No
terminal selected" but the button opens the launcher.

### 6.2 Dashboard (D12, fixes F16)

Keeps its job: glanceable previews (the "not just terminals" product direction), scoped
all/folder. Selection remains one click on a card. Changes, minimal by design:
- Entries: titlebar ⊞ (unchanged) + folder-row hover ⊞ (§5.3). Folder-NAME click no longer
  goes here (F10). No other churn — it already passes the doctrine (fills, no strokes).
- Header strip spans the full central width (today's Frame shrinks to content — screenshot
  06's floating chip); label `All terminals · 7` / `QuipShot · 3` (no "terminal(s)").
- Card meta line (under the name): `cwd · last command` (cwd middle-ellipsized; last
  closed block's cmd from `App.blocks`, TEXT_FAINT 11px, omitted when absent). Dead cards:
  hover reveals a `↻ Restore` ghost text-button bottom-right.
- NOT adding: kill buttons, live thumbnails, per-card menus — the dashboard is a viewport,
  not a control panel (churn control; the user said selector, and the sidebar is the
  selector).

---

## 7. Visual language (exact values)

All colors are existing tokens (mod.rs:37-83) — zero new colors, zero strokes anywhere.

| Element | Spec |
|---|---|
| Launcher surface | `SURFACE` fill, radius 10, shadow: existing editor/blocks-panel shadow, width 560, anchored titlebar+8 |
| Launcher fade | 90ms opacity in/out via `animate_bool_with_time`; no translate/scale |
| Query row | 36px; `❯` in ACCENT 13px mono; TextEdit 13px proportional, hint TEXT_FAINT; no fill behind the field |
| Section label | 11px semibold, TEXT_FAINT, letter-spacing default, 12px above / 4px below |
| Row | 34px; glyph 16px at x+12 (TEXT_MUTED); label 13px TEXT at x+36; secondary 11px TEXT_MUTED right-aligned −12; hover `OV_HOVER` r6; kb-selected `ACCENT_SUBTLE` + 2px ACCENT left bar (inset 3, rounded 1) |
| Footer lane | 28px, no fill; folder chip TEXT_SECONDARY 12px, hover→TEXT; hints TEXT_FAINT 11px right |
| Custom expansion | +72px; two TextEdits 13px, `SURFACE_2` fill r4 behind text only; "Create" = ACCENT text, brightens on hover (Run ▸ grammar) |
| Split + button | today's ghost_button metrics; chevron zone 24px right, glyph TEXT_MUTED→TEXT, fades in with the button's existing hover animation |
| Row hover actions | 22px hit rects, `draw_icon` at shrink(7); TEXT_MUTED→TEXT; ✕ keeps DANGER_HOVER + red 30-alpha hover pill; spacing 0px between (adjacent cells) |
| Inline rename field | same font/pos as the name galley; `SURFACE_2` r4 padded 4×2; no stroke |
| Drag ghost | name+dot on `SURFACE_2` r6 + shadow, 80% opacity, pointer-locked (offset = grab point), zero animation |
| Insertion bar | 2px × available row width, ACCENT, rounded 1, drawn in the inter-row gap |
| Folder drop highlight | `ACCENT_SUBTLE` full-row fill r6 while a drag hovers it |
| Rail + | 28px cell, `Icon::Plus` 14px TEXT_MUTED→TEXT, hover `OV_HOVER` r6 |
| Dashboard header | full-width `SURFACE` strip 40px (header_bar grammar); title 15px semibold; count TEXT_MUTED 12px |
| Card meta line | 11px TEXT_FAINT at y+36 (title line moves to y+50 when both present; preview stays y+54 grid) |

Animation budget, exhaustively: launcher 90ms fade; hover states = existing 0.12
`animate_bool`; everything on drag/create/select paths moves 0ms. Nothing else animates.

---

## 8. Focus & input routing (load-bearing — position in the P3/P4 chain)

Priority (top wins): **modal > launcher > search > blocks panel = history popup >
inline-rename > composer > grid.**

- `overlay_open` (terminal_card, mod.rs:3105) gains `|| self.launcher.is_some() ||
  self.renaming.is_some()` — the composer's every-frame focus re-request (mod.rs:3144) and
  the grid's `focused` gate (mod.rs:3186) then both stand down automatically; this is the
  same slot search/history already occupy, no new mechanism.
- Launcher open-frame focus: `ctx.memory_mut(|m| m.request_focus(Id::new("launcher_q")))`
  at the click that opens it (LOW-9 pattern — a fast keystroke must not leak to the PTY or
  the composer draft).
- Key consumption INSIDE the launcher, before its TextEdit is shown (the P3/P4
  consume-before-show pattern, `input_mut().consume_key`): ArrowUp/ArrowDown (move `sel`,
  clamp; scroll via `vertical_scroll_offset` steering — `show_rows` can't scroll_to an
  unrendered row, P4 gotcha), Enter (activate selected), Esc (collapse custom-expansion
  first, else close + return focus: if the selected terminal's composer is in Compose,
  `want_focus = true` — the history popup's exact Esc contract, mod.rs:1529).
- Mutual exclusion writes both ways: opening launcher closes search/blocks/history;
  `select_terminal` closes the launcher (selection changed under it ⇒ its folder chip and
  Run-target assumptions are stale — history popup precedent).
- Inline rename: plain TextEdit focus; Enter/Esc consumed by it naturally (it's the only
  focused widget; grid is gated by overlay_open). Clicking anywhere else = blur = commit.
- Drag never touches keyboard focus; Esc-cancel of a drag is read only while
  `drag.is_some()` and consumed.
- The embedded empty-state launcher needs NO gating: no grid, no composer exists then.

---

## 9. File-by-file plan

**NEW `src/gui/launcher.rs`** (~350 lines, egui-free core + egui view)
- Types/fns from §4.3: `Candidate`, `CandKind`, `SpawnSpec { kind_tag, program, args, cwd }`,
  `build`, `filter`, `uniquify_name`, `spec_for(candidate, last_spawn, folder) -> NewTerminal`.
- `LauncherState { query: String, sel: usize, cands: Vec<Candidate>, hits: Vec<u32>,
  folder: Option<Uuid>, custom_open: bool, custom_prog: String, custom_args: String,
  typed_dir: Option<(String, Option<PathBuf>, Instant)> }`.
- Pure unit tests co-located (§12.1).

**`src/gui/mod.rs`**
- App fields: `launcher: Option<LauncherState>`, `launcher_btn_rect: Option<Rect>`,
  `pending_create: Option<(String, Instant)>`, `renaming: Option<(RenameTarget, String)>`,
  `drag: Option<DragState>`.
- Prefs: add `last_spawn: Option<SpawnSpec>`, `recent_spawns: Vec<SpawnSpec>` (ring 8),
  `#[serde(default)]` both — old gui.json loads clean; `last_cwd` kept (seeds first
  `last_spawn` cwd), delete its other uses.
- `titlebar`: split + button (main/chevron zones via two `ui.interact` rects over one
  painted button); DELETE folder-icon + import-icon buttons; keep sidebar toggle + ⊞.
- `instant_create(folder: Option<Uuid>)`: build spec from prefs → send → pending_create.
- `launcher_ui(ctx)`: the Area + §4 content; called from `ui()` after panels (the
  show_modal slot); candidates built on open (and rebuilt on `blocks_stamp` drift while
  open, 500ms debounce — history popup pattern).
- `sidebar_tree`: stable sort (delete the NeedsYou key); folder_row rework (§5.3 —
  `FolderAction` becomes `{None, ToggleCollapse, NewHere, Dashboard, Menu}`); collect
  drop slots; empty-sidebar block → single ghost_button.
- `terminal_row`: hover action cluster (↻/✏/✕), inline-rename branch, drag arming +
  ghost + slot collection.
- `header_bar`: name click → inline rename (reuse the same renaming state).
- `empty_state`: embed launcher content (§6.1).
- `dashboard` / `dashboard_card`: full-width header, count label, meta line, dead-card
  hover Restore.
- `show_modal`: DELETE NewTerminal/Import/Rename* arms (+ their structs/enums + folder_picker);
  KEEP NewFolder + both ConfirmDelete arms.
- `apply_snapshot`: pending_create resolution (§3.2).
- `select_terminal`: `self.launcher = None; self.pending_create = None;` added.
- `Icon`: add `Shell, Distro, Ssh, ClaudeSpark, Dir, Rename (pencil), Restore (↻ arc —
  reuse Icon::Rerun), Chevron (reuse ChevronDown)`.

**`src/gui/import.rs`** — unchanged (scan reused by launcher).
**`src/gui/shells.rs`** — P6's module (enumerators + `ShellChoice`); the launcher only
calls it at open. Not this spec's diff.
**`src/gui/composer.rs`, `term_view.rs`, `term_backend.rs`** — untouched.
**Daemon / protocol / tc** — untouched by THIS spec (P6 owns proto 5 / `shell_cfg`; the
launcher passes `shell_cfg` through P6's mapping, adding nothing).

Estimated diff: mod.rs −250/+520, launcher.rs +350. No new dependencies (no rfd/native
file dialog — Q3).

---

## 10. State & persistence

- `Prefs` additions (§9) — serde-default, forward+backward compatible.
- `SpawnSpec.kind_tag`: string tag (`"pwsh" | "powershell" | "cmd" | "claude" | "wsl:<distro>"
  | "ssh:<host>" | "custom"`) — STRING, not enum, so P6 kinds deserialize without lockstep
  releases; `spec_for` maps unknown tags → refuse (drop the suggestion row) rather than
  guess a program.
- Nothing else persists: launcher/rename/drag state is transient; candidate index rebuilt
  per open (click-time cost, I-7).

## 11. P6 interface (now concrete — p6-shells-spec.md §9 is the contract)

P6 defines `gui/shells.rs::ShellChoice { family: ShellFamilyTag, label, detail,
is_default, degraded_note, fields: Vec<ChoiceField> }`, enumerated at dialog-open (WSL
distros from the Lxss registry, ssh hosts from ~/.ssh/config — no polling, inv. 7 there),
plus the exact `NewTerminal` mapping per family (including the appended
`shell_cfg: Option<ShellCfg>` field @ proto 5). The launcher consumes it as follows:

- **Shells section rows = `ShellChoice`s verbatim**: `label` is the row label, `detail`
  the right-aligned secondary, `degraded_note` appended to the row tooltip ("cmd — no
  exit codes"), `is_default` sorts first within the section.
- **Rows whose `fields` need no user input create instantly on click** (Pwsh, Cmd, a
  concrete distro with default shell, a config-listed ssh host with default hooks):
  every `ChoiceField` has a P6-defined default (Cwd ⇒ launcher's cwd logic §4.4;
  WslShell ⇒ options[0]; RemoteHooks ⇒ default_on). Instant beats a mandatory detail step.
- **A hover-revealed `⋯` on rows carrying optional fields** (WSL shell choice, ssh hooks
  toggle) expands the row IN PLACE — the §4.6 custom-expansion mechanism — rendering
  exactly the `ChoiceField`s P6 declares: `WslShell` = three text-chips (bash·zsh·fish,
  chosen chip in ACCENT), `RemoteHooks` = a text toggle with P6's one-line explanation,
  `SshHostFreeform` = one borderless TextEdit (this row — "ssh to…" — always expands,
  it has no default host). Enter/Create-click creates.
- **Creation submit** uses P6 §9's mapping verbatim (Wsl ⇒ Shell kind + `wsl.exe -d …`,
  Cmd ⇒ `cmd.exe`, Ssh ⇒ `ssh.exe … host` + `shell_cfg.remote_hooks`); the launcher
  never re-derives program/args itself. `shell_cfg` rides the appended NewTerminal field.
- **Family glyphs**: P6 puts dimmed `wsl`/`cmd`/`ssh` text suffixes in sidebar rows; the
  launcher uses the same text-glyph grammar for its section rows (no new icon set needed
  beyond §9's list — drop `Icon::Distro/Ssh` if the text suffix reads cleaner in review).

If P6 lands after this spec: §4.8 degraded set ships first; the Shells section is
data-swappable and the `⋯` expansion simply has no rows to attach to.

## 12. Tests

### 12.1 Unit (cargo test, launcher.rs + mod.rs)
1. `uniquify_name`: base free ⇒ unchanged; taken ⇒ " 2"; " 2" taken ⇒ " 3"; case-exact.
2. `build`: recents deduped by (kind_tag, cwd) MRU-first; recent-dirs union of cwd/live_cwd/
   block-cwds, MRU, cap 12, "N terminals" secondary; claude sessions minus already-imported;
   suggested == recent_spawns head; section order stable.
3. `filter`: token-AND, case-insensitive, label+secondary, empty query = identity (mirror
   of history::filter tests).
4. `spec_for`: shell row uses last_spawn cwd; dir row uses row dir + last_spawn shell;
   claude session ⇒ already_launched=true + session id; unknown kind_tag ⇒ None.
5. Drop-slot math (pure fn `drop_slot(y, &slots) -> DropSlot`): between-rows index vs
   folder-target vs none; delta computation replicates daemon group ordering (fixture with
   mixed folders, asserts single-message delta lands the exact index).
6. `pending_create` resolution: matching new terminal selected; expiry; manual selection
   cancels.
7. Rerun-style truth table for launcher key consumption order (custom_open ⇒ Esc collapses
   before close).

### 12.2 Probes
None new — creation round-trip is already probed (`basic`, `folders`), and every launcher
activation exits through the same `C2D::CreateTerminal`. No daemon behavior changes (I-1).

### 12.3 Interactive checklist (staged GUI, isolated daemon; never touch the user's)
1. Titlebar + click ⇒ terminal appears AND is selected, named "Shell · dir" (uniquified on
   repeat); tooltip predicted exactly what spawned.
2. + chevron ⇒ launcher fades in ≤90ms, query focused; a fast first keystroke lands in the
   query (not the PTY, not the composer) — repeat with an ARMED composer visible.
3. Type `term` ⇒ sections filter live; Up/Down walk, Enter creates, Esc closes and the
   armed composer regains focus (caret blinking in the cover row).
4. Open launcher from a folder ⋯ "New terminal here…" ⇒ chip preset; created terminal lands
   in that folder, selected.
5. Type `C:\Terminal Control` ⇒ "Open shell in…" row appears; type garbage path ⇒ no row,
   no error chrome.
6. Claude section: rows match ~/.claude/projects leftovers; activating one imports (resume
   on first launch) and it disappears from the section next open.
7. Custom command… expands inline; `ping -t 127.0.0.1` creates a Custom terminal running it.
8. Sidebar: NeedsYou terminal keeps its position (dot+pill only); hover a dead row ⇒ ↻
   restores in place; ✏ renames inline (Enter commits, Esc cancels, empty cancels);
   double-click name renames; header-bar name click renames.
9. Drag a row within a folder ⇒ accent insertion bar tracks; drop reorders exactly there.
   Drag onto a collapsed folder ⇒ folder highlights, drop moves into it. Esc mid-drag
   cancels. Click (no 6px travel) still selects; right-click still menus.
10. Folder row: click anywhere toggles collapse; hover shows + ⊞ ⋯; delete only inside ⋯
    (confirm modal).
11. Empty first run: central shows the embedded launcher; creating from it works; sidebar
    ghost button focuses the query.
12. Dashboard: entered only via ⊞s; header spans full width; cards show cwd · last cmd;
    dead card hover-Restore works; click selects.
13. Doctrine sweep: screenshot every new surface — zero hairlines/strokes anywhere
    (launcher, footer chip menu excepted as stock popup? NO — restyle check: the folder
    chip popup must use the same strokeless menu styling the context menus already have).
14. 20-terminal soak: launcher open <5ms build (log once with TC_PERF_STAGES if in doubt);
    no per-frame candidate rebuilds (add a debug assert counter during review).

## 13. Open questions (defaults pre-chosen — implementer proceeds without asking)

| # | Question | Default |
|---|---|---|
| Q1 | Should instant-create ever open the launcher instead (e.g. first run)? | Yes only when `recent_spawns` is empty AND terminals exist is false — i.e. true first run; otherwise never surprise the click |
| Q2 | `import::scan` synchronously at open — fast enough? | Yes (128KiB/head-read per session, dozens of sessions ⇒ <50ms); if a profile shows worse, move to a one-shot thread + fill-in repaint, layout unchanged |
| Q3 | Native folder-browse dialog in the launcher? | No — typed paths + recents cover it; a native dialog drags in a dependency and a foreign-styled window (doctrine). Revisit only on user ask |
| Q4 | Keep the `↩` hint on the kb-selected row? | Keep; drop silently if the doctrine screenshot review reads it as chrome |
| Q5 | Folder chip when the launcher opens from the titlebar while a folder's terminal is selected | `(no folder)` — global entry means global create; folder-scoped creation has its own entries (folder + / ⋯) |
| Q6 | Persist dead-since time to replace the "exited" static label with "exited · 2d"? | Not now — needs meta/state change; revisit with any future state.json touch (note in that PR) |
| Q7 | Rail: should + open the launcher on right-click? | No — rail is a minimal strip; expand the sidebar for the full selector |

## 14. Explicit DO-NOTs

1. **DO NOT add any protocol variant, field, or proto bump** — everything here rides
   existing messages; P6 owns its own wire changes (I-1).
2. **DO NOT keep the form modal "as an advanced fallback"** — two creation surfaces is how
   F7 happened; the launcher + custom expansion covers every old field (name is post-hoc
   rename; folder is the chip; directory is recents/typed; args is custom/claude row via
   P6 profile data).
3. **DO NOT animate the drag ghost, the insertion bar, or content shifts on drop** — the
   falling-motion incident (P3) proved eased motion on functional paths reads as lag.
4. **DO NOT put delete on any hover-reveal cluster of a FOLDER row** (I-5); terminal-row ✕
   keeps its confirm modal.
5. **DO NOT re-sort the sidebar by activity** — stable order is the decision (D6); if
   attention needs more surfacing later, strengthen the pill, not the ordering.
6. **DO NOT let launcher keys reach the composer/grid** — overlay_open + open-frame focus +
   consume-before-show are ALL required (each guards a different frame — P3 Bug-1 and P4
   LOW-9 are the incident history).
7. **DO NOT scan filesystems per keystroke** — one debounced `is_dir` for typed paths;
   candidates build at open only.
8. **DO NOT draw a single Stroke** — insertion indicator is a filled 2px bar; drop
   highlight is a fill; the launcher has no border; the folder-chip popup reuses the
   strokeless menu style.
9. **DO NOT guess unknown kind_tags** into a program to spawn (refuse row-less) — wrong
   spawn is worse than a missing suggestion (refuse-over-guess).
10. **DO NOT run a second GUI against the user's daemon for verification** — staged
    TC_DATA_DIR daemon + scratch exe copies only (standing ops rule).

## 15. Implementation order (compile-green at each step)

1. launcher.rs core (types, build/filter/uniquify/spec_for) + unit tests — no UI.
2. Prefs additions + `instant_create` + split + button + `pending_create` auto-select.
   (Ship-checkpoint: instant create alone already kills 80% of F1.)
3. Launcher Area UI (query/sections/footer/custom expansion) + focus routing + deletion of
   NewTerminal/Import modals + titlebar icon shed.
4. Sidebar: stable sort, folder-row rework, hover clusters, inline rename (+ header-bar
   rename), rail +.
5. Drag & drop (slots, ghost, wire).
6. Empty-state embed + dashboard tweaks.
7. Checklist pass on a staged GUI + doctrine screenshot sweep.
