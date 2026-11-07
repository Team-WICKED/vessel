# Vessel web UI — design

Track M7 implements this. The mockups in this directory are the visual
source of truth; this file is the contract: why the UI is shaped this way,
how it maps onto the protocol, and what must not be improvised.

The product is an **Agentic Development Environment**: one seat driving
AI coding harnesses (Claude Code, Codex, Gemini, OpenCode) as sessions on
several of the operator's machines (nodes). It is a control room, not a
marketing site and not a chat app with a sidebar bolted on.

Mockups: `fleet.html`, `sessions.html`, `session-structured.html`,
`session-terminal.html`, `new-session.html`, `fleet-sync.html`,
`devices.html`. Shared: `tokens.css` (the token contract), `app.css`
(mockup composition — do not copy it wholesale into the app).

Settings (node book, theme) is a dialog on every screen (`#settings`),
not a seventh route. The TypeScript app may give it its own route; the
information architecture below says where it lives.

---

## 1. Principles

1. **The daemon is the product; the UI is a client.** Every label, status
   and action maps to a protocol type (`SessionInfo`, `NodeInfo`,
   `AgentEvent`, fleet generation, device scopes). Do not invent a second
   vocabulary. If the wire has `Interrupted`, the badge says interrupted.
2. **Calm density.** Zed's restraint, Linear's clarity. Information is
   close together because the operator is comparing nodes, not browsing a
   landing page. Padding is a 4px grid, not a marketing 80px section.
3. **Dark is the design.** Light is a re-derived second theme, never an
   inversion of the dark canvas. Depth is layered near-blacks and 1px
   hairlines, not drop shadows.
4. **One accent.** Function blue (`--accent`) is the only colour that
   means "this is selected / this is a link / this has focus". Status
   greens, ambers and reds are reserved for process and reach state.
   Syntax colours appear inside code, diffs and the terminal, not as
   decorative chips.
5. **Monospace is a material.** It marks things a machine produced or a
   person could type: ids, paths, models, versions, generations, prompts,
   code, the terminal. Human sentences — navigation, button labels, empty
   states, explanations — are Pretendard.
6. **Attribution is visible.** A session created by a person and a
   session created by an agent on another node are not the same row with
   a different icon. Agent-created sessions nest under their creator.
   That is ADR 0013 made visible, not a nice-to-have tree widget.
7. **Live, never silent.** A lagged subscription, a draining node, a
   reconnecting attach are first-class states with copy, not spinners
   that hope. Nothing is skipped silently (`session.subscribe` already
   forbids it).
8. **Taste over chrome.** No gradients, no glows, no card grids of
   identical feature tiles, no 3D device frames, no looping typing
   animation. If a control is not needed this minute, it is in a menu.

### Why this, not a chat shell

The obvious template for "talk to an agent" is a single transcript and a
prompt box. Vessel's job is fleet-shaped: which machine, which account,
who started this, can I move it, is the member still on generation 48.
A chat shell hides the fleet. The session view is allowed to feel like a
conversation; the rest of the app must feel like an IDE's debug console.

---

## 2. Information architecture and navigation

### 2.1 Places

| Place | What it is | Primary objects |
| --- | --- | --- |
| **Fleet** | Every node the book knows, plus live facts from each daemon | node, reach, role, harnesses, profiles, live count |
| **Sessions** | Live and recent sessions, one node or all | session, creator tree, status, mode |
| **Session** | One session, terminal or structured | timeline or PTY, actions |
| **New session** | Create dialog | node, harness, profile, model, cwd, mode |
| **Sync** | Fleet configuration as the home publishes it | generation, members, conflicts, MCP, secrets (names only) |
| **Devices** | Pairing and revocation, per the selected node | device, scopes, invite link |
| **Settings** | This client | node book, theme |

There is no "home dashboard" distinct from Fleet. Fleet *is* the home.

### 2.2 Chrome

```
┌─ topbar 40px ─────────────────────────────────────────────────────────┐
│ mark  Vessel    breadcrumb                 search     New    device   │
├─ sidebar 240px ─┬─────────────────────────────────────────────────────┤
│ Fleet           │                                                     │
│   laptop   3    │  main                                               │
│   gpu-box  2    │                                                     │
│   build-server 1│                                                     │
│   studio   —    │                                                     │
│ Sessions        │                                                     │
│ Sync            │                                                     │
│ Devices         │                                                     │
│ ─────────────── │                                                     │
│ Settings        │                                                     │
└─────────────────┴─────────────────────────────────────────────────────┘
```

- **Sidebar, not a top-tab app.** Nodes are a hierarchy the operator
  lives in. Putting them in a top bar forces a second click every time.
  Zed and Linear both keep the collection on the left.
- **Nodes are children of Fleet**, not a separate "Machines" app. Clicking
  **Fleet** shows every node. Clicking a node name scopes the main pane
  to that node (its detail on Fleet, its sessions on Sessions, its
  devices on Devices, its applied generation on Sync).
- **Sessions in the sidebar is "all nodes".** It is the unscoped list.
  A node row in the sidebar is a scope, not a different product area.
- **Badge on a node row** is live session count (`SessionStatus` starting
  or running). Offline nodes show an em dash, not `0`, so they do not
  look idle-but-fine.
- **Topbar breadcrumb** repeats the scope: `Fleet / gpu-box / 3f2a`. It
  is the only place a session id appears in the chrome, so the operator
  can copy it without digging.
- **Search** (`/` or `⌘K`) is a command palette over nodes, sessions,
  commands. The mockups show the field; they do not implement results
  (no JS). The app should.
- **New** always opens the create dialog with the current node
  preselected (all-nodes → the book's default node).
- **Device chip** in the topbar is *this client*: its label, and a
  reach summary (`direct` / `ssh` / `relay`). It is not the Devices
  page. Clicking it opens Settings.

### 2.3 Routes (for the implementer)

Suggested hash-free routes. Names are UI, ids are protocol.

```
/                         → /fleet
/fleet                    all nodes
/fleet/:node              one node (book name or NodeId)
/sessions                 all nodes
/sessions?node=:node      scoped
/sessions/:id             redirect to the right view
/sessions/:id/structured
/sessions/:id/terminal
/sync
/sync?node=:node
/devices                  default: the selected or default node
/devices?node=:node
/settings                 optional; mockups use #settings
```

Creating a session is a modal over the current route, query
`?new=1`, not a dead-end page. `new-session.html` exists so the dialog
can be opened on its own in a browser.

### 2.4 Session hierarchy

`SessionInfo.created_by` (protocol 5) is either the owner/device or an
agent `{ node, session }`. The Sessions list is a **tree**:

- Root rows: created by a person (this device, or another paired device).
- Child rows: created by that session's agent, on any node.
- Depth is at most 3 (ADR 0013). Indent 16px per level, with a 1px
  connector in `--border-subtle`.
- A child can live on a different node than its parent. The node column
  is therefore always shown, even inside a scoped list, whenever any
  visible row's node differs from the scope.
- Collapsing a parent hides descendants. Default: expanded if any
  descendant is live.

Do not flatten this into a "Created by" column with a raw id. The tree
*is* the point of a fleet.

### 2.5 Which node is "the" node

Devices, pairing, drain/stop, and the node book entry are per node.
Sync conflicts are per member. The selected node is:

1. the node in the URL, else
2. the last node the operator selected this session, else
3. the book's `default_node`, else
4. the local node if the book has one.

The sidebar highlights that node. Switching it does not leave the
current place (Sync stays Sync).

---

## 3. Layout grid

A 12-column grid inside `main`, not including the sidebar.

| Breakpoint | Sidebar | Main | Columns | Gutter |
| --- | --- | --- | --- | --- |
| ≥ 1280px | 240px fixed | fluid | 12 | 16px |
| 1100–1279 | 52px icon rail | fluid | 12 | 16px |
| 768–1099 | hidden, drawer | fluid | 8 | 12px |
| < 768 | hidden, drawer | fluid | 4 | 8px |

Page padding inside main: 16px (12px below 768). Max content width is
the pane, not a centred 720px reading column — this is an app.

### 3.1 Common page frames

**Fleet.** Main is one column: a 32px summary strip, then a full-width
node table. Selecting a row opens a 320px inspector (`col` 10–12 on
wide screens) with harnesses, profiles, reach, last error.

**Sessions.** Filter bar 40px, then a table. No inspector; a row click
opens the session.

**Structured session.**

```
header 40px
┌───────────────  minmax(0, 1fr)  ──────────────┬─ 320px ─┐
│ timeline                                      │ inspect │
│                                               │ plan    │
│                                               │ files   │
│                                               │ meta    │
├───────────────────────────────────────────────┤         │
│ permission (if any), then composer 88–120px   │         │
└───────────────────────────────────────────────┴─────────┘
```

The inspector hides below 1100px (toggle `⌘I`). The composer is sticky.

**Terminal session.** Header 40px, reconnecting banner if needed, then
the terminal consumes all remaining height. No inspector.

**Dialogs** (new session, settings, pair, confirm revoke): 480px wide,
centred, 16px padding, dimmer `--overlay` at 48% black (dark) / 32%
(light). New session can grow to 560px because of the model list.

**Sync.** Two stacked sections, not tabs: Sources (home) then Members.
Conflicts, when any exist, pin to the top of Members as a list, not a
toast.

### 3.2 Spacing scale

| Token | px | Use |
| --- | --- | --- |
| `--s-1` | 4 | icon gaps, badge padding |
| `--s-2` | 8 | compact row padding, inline gaps |
| `--s-3` | 12 | default control padding |
| `--s-4` | 16 | pane padding, dialog padding |
| `--s-5` | 24 | section gaps |
| `--s-6` | 32 | page-level gaps (rare) |
| `--s-7` | 48 | empty-state vertical rhythm |

Row heights: sidebar item 32px, table row 40px, topbar 40px, composer
min 72px. Touch targets below 768px inflate to 44px; on desktop we
keep 32px because this is a pointer-first operator tool, with visible
focus for keyboard.

---

## 4. Design tokens

All tokens live in `tokens.css` as CSS custom properties on `:root`
(dark) and `:root[data-theme="light"]` (light). The mockups also honour
`#theme-light:checked` via `:has()`, so the Settings toggle works
without JavaScript.

Do not introduce a third palette. Status and syntax draw from the
tables below.

### 4.1 Colour — dark (primary)

| Token | Hex | Role |
| --- | --- | --- |
| `--bg-base` | `#0A0B0D` | app canvas |
| `--bg-sidebar` | `#0C0D10` | sidebar, slightly lifted so the rail is a layer |
| `--bg-surface` | `#101216` | tables, timeline, inputs |
| `--bg-elevated` | `#16181D` | popovers, dialogs, inspector, topbar |
| `--bg-hover` | `#1A1D24` | row hover |
| `--bg-active` | `#1E2430` | selected row (tinted toward accent) |
| `--border-subtle` | `#1E2127` | default hairline |
| `--border-strong` | `#2A2E37` | focused well, emphasised split |
| `--text-primary` | `#E6E8EB` | titles, primary copy, input text |
| `--text-secondary` | `#9BA1AC` | body, descriptions |
| `--text-tertiary` | `#8A919C` | meta, timestamps, column headers |
| `--text-disabled` | `#5C6370` | disabled controls (not small running text) |
| `--accent` | `#82AAFF` | selection, links, focus, "this" |
| `--accent-muted` | `#82AAFF22` | selected row wash, badge fill |
| `--online` | `#3ECF8E` | serving, running, applied |
| `--warn` | `#E0A066` | draining, pending permission, conflict, starting |
| `--danger` | `#F07178` | failed, revoke, kill |
| `--interrupt` | `#A8B0C0` | interrupted (neutral, not an error) |

Never `#000000` or `#FFFFFF` on large surfaces.

### 4.2 Colour — light (re-derived)

| Token | Hex | Role |
| --- | --- | --- |
| `--bg-base` | `#F3F4F6` | canvas |
| `--bg-sidebar` | `#EEF0F3` | sidebar |
| `--bg-surface` | `#F7F8FA` | tables, timeline |
| `--bg-elevated` | `#FBFBFC` | dialogs, topbar, inspector |
| `--bg-hover` | `#EBEEF2` | row hover |
| `--bg-active` | `#E4EAF8` | selected row |
| `--border-subtle` | `#E2E5EA` | hairline |
| `--border-strong` | `#C9CED6` | focus well |
| `--text-primary` | `#1A1C20` | titles |
| `--text-secondary` | `#4E5560` | body |
| `--text-tertiary` | `#5E6570` | meta (12px+) |
| `--text-disabled` | `#8A919C` | disabled |
| `--accent` | `#2F62D4` | same role as dark, darker so it passes on light |
| `--accent-muted` | `#2F62D41A` | selected wash |
| `--online` | `#1F7A4D` | |
| `--warn` | `#9A5B12` | |
| `--danger` | `#C03948` | |
| `--interrupt` | `#5E6570` | |

### 4.3 Contrast (WCAG AA, verified)

Text smaller than 18px needs 4.5:1. All running text in this UI is 12px
or 13px, so every pair below is measured at that bar.

| Pair | Ratio | Notes |
| --- | --- | --- |
| dark `--text-primary` on `--bg-base` | 16.0 | |
| dark `--text-secondary` on `--bg-base` | 7.6 | |
| dark `--text-tertiary` on `--bg-base` | 6.2 | `#8A919C`, not `#6B7280` (4.07, fails) |
| dark `--text-tertiary` on `--bg-surface` | 5.9 | |
| dark `--accent` on `--bg-surface` | 8.2 | |
| dark `--online` on `--bg-surface` | 9.4 | |
| dark `--danger` on `--bg-surface` | 6.6 | |
| dark `--warn` on `--bg-surface` | 8.4 | |
| dark code comment `#7A8190` on `--bg-surface` | 4.8 | `#5C6370` fails at 3.1 |
| light `--text-primary` on `--bg-base` | 15.5 | |
| light `--text-secondary` on `--bg-base` | 6.9 | |
| light `--text-tertiary` on `--bg-base` | 5.4 | |
| light `--accent` on `--bg-base` | 5.0 | |
| light `--online` / `--danger` / `--warn` on `--bg-base` | ≥ 4.9 | |

Do not use `--text-disabled` for timestamps or column headers. Disabled
is for controls that cannot be used.

Status is never colour alone: every badge has a word (`running`,
`relay`, `conflict`). Dots in the sidebar repeat the word in
`aria-label`.

### 4.4 Syntax (code, diffs, terminal)

Used inside tool-call JSON, markdown code, diffs and the terminal. Not
as UI decoration.

| Token | Dark | Light |
| --- | --- | --- |
| `--syn-keyword` | `#C792EA` | `#7A3DB3` |
| `--syn-string` | `#C3E88D` | `#3B6D11` |
| `--syn-func` | `#82AAFF` | `#2F62D4` |
| `--syn-number` | `#F78C6C` | `#B5471C` |
| `--syn-comment` | `#7A8190` | `#5E6570` |
| `--syn-const` | `#FFCB6B` | `#8A5A00` |
| `--syn-error` | `#F07178` | `#C03948` |
| `--diff-add-bg` | `#3ECF8E18` | `#1F7A4D14` |
| `--diff-del-bg` | `#F0717818` | `#C0394814` |

### 4.5 Radii

| Token | px | Use |
| --- | --- | --- |
| `--r-1` | 2 | badges, tiny chips |
| `--r-2` | 4 | buttons, inputs, rows |
| `--r-3` | 6 | menus, tool-call cards |
| `--r-4` | 8 | dialogs, composer |
| `--r-5` | 12 | inner frame (terminal) |
| `--r-6` | 16 | outer frame (terminal) |

The terminal uses the concentric pair 16 / 12 with a 4px gap
(inner = outer − gap). Nothing else is framed that way. Tables and
timelines are not cards-inside-cards.

### 4.6 Elevation

Dark: no shadows. A raised surface is `--bg-elevated` plus
`--border-subtle`. An optional `inset` highlight `rgba(255,255,255,0.04)`
on the top edge of dialogs only.

Light: a single shadow on dialogs and menus,
`0 8px 24px rgba(26, 28, 32, 0.08), 0 0 0 1px var(--border-subtle)`.
Rows and tables still use hairlines, not shadows.

`--overlay`: `rgba(0,0,0,0.48)` dark, `rgba(26,28,32,0.32)` light.

### 4.7 Motion

| Token | Value |
| --- | --- |
| `--ease` | `cubic-bezier(0.16, 1, 0.3, 1)` |
| `--d-1` | `120ms` |
| `--d-2` | `180ms` |
| `--d-3` | `240ms` |

Animate `opacity` and `transform` only. Dialogs: fade + 8px rise.
Inspector and sidebar: width, not a bounce. Tool-call expand: the
`<details>` disclosure, no extra animation. Live status dots do not
pulse unless the state is `starting` or `reconnecting`, and even then
the pulse is opacity 1 → 0.45 → 1 over 1.6s, stopped under
`prefers-reduced-motion`.

---

## 5. Typography

### 5.1 Families

```css
--font-sans: Pretendard, -apple-system, BlinkMacSystemFont, "Segoe UI",
             sans-serif;
--font-mono: Consolas, "JetBrains Mono", "Cascadia Mono", Pretendard, monospace;
```

**Mandatory host rule, applied everywhere:**

- Pretendard is first in every `font-family`, including the mono stack
  (after the real monospace faces, before the generic `monospace`).
- Never put a bare `monospace` or `serif` first.
- Never use Gulim, Dotum, Batang, or any family that renders as a pixel
  font on Korean Windows Chrome.
- Mockups load Pretendard from the jsDelivr CDN
  (`pretendard@v1.3.9` static). **The app must self-host Pretendard**
  (woff2, `font-display: swap`, subset Korean + Latin). Do not keep the
  CDN in production.

Two families only. No display serif, no third mono.

Ligatures off on `--font-mono` (`font-variant-ligatures: none`) so
paths and flags do not turn into unfamiliar glyphs.

### 5.2 Scale

| Token | Size / line / weight | Family | Use |
| --- | --- | --- | --- |
| `--t-title` | 16px / 24 / 600 | sans | page title in main |
| `--t-body` | 13px / 20 / 400 | sans | UI copy, markdown body |
| `--t-label` | 12px / 16 / 500 | sans | buttons, nav, tabs |
| `--t-meta` | 12px / 16 / 400 | sans | timestamps, helper text |
| `--t-mono` | 12px / 20 / 400 | mono | ids, paths, models, versions |
| `--t-mono-lg` | 13px / 22 / 400 | mono | composer, tool I/O |
| `--t-term` | 13px / 20.8 (1.6) / 400 | mono | terminal |
| `--t-kicker` | 11px / 16 / 500 | sans | column headers, overlines (uppercase + 0.06em) |

Headlines heavier than 600 are forbidden. Markdown in the timeline may
use 600 for `h3`/`h4` equivalents; there is no `h1` inside a session.

Composer and terminal stay 13px so they match the operator's editor,
not the 12px chrome.

---

## 6. Component inventory

Every component below is in `app.css` under the same class name. States
to implement: default, hover, active, focus-visible, disabled,
loading, error, empty.

### 6.1 Buttons

| Class | Look | Use |
| --- | --- | --- |
| `.btn` | hairline, `--text-primary`, transparent | default |
| `.btn.primary` | `--text-primary` inverted onto a light fill (`#E6E8EB` dark / `#1A1C20` light) | one primary per view (Create, Send, Pair) |
| `.btn.danger` | `--danger` hairline and text; solid `--danger` with light text only on the confirm step | Kill, Revoke |
| `.btn.ghost` | no border, tertiary text | icon buttons in headers |
| `.btn.sm` | 28px tall, 12px type | table row actions |

Primary is **not** filled with `--accent`. A solid accent rectangle
would steal the colour from selection and focus (see Principles).

Icon + label. Icon-only buttons need `aria-label`. Gap 6px. Radius
`--r-2`. Disabled at 40% opacity, `pointer-events: none`.

### 6.2 Inputs

Text fields, selects, the composer. Surface fill, 1px `--border-subtle`,
focus: 1px `--accent` (not a glow). Height 32px. Placeholder
`--text-tertiary`. Labels sit above, `--t-meta`, never inside.

Selects use the native control styled; the app may replace with a
listbox later. Model picker is a listbox grouped by provider, not a
flat 40-item `<select>` once routing exists.

Checkbox / radio: 14px square/circle, accent when on.

### 6.3 List rows and tables

`.table` is a CSS grid or `<table>` with hairline row borders, no
vertical rules, no zebra. Hover `--bg-hover`. Selected `--bg-active`
plus a 2px accent bar on the left.

Columns align: status (dot + word), name (sans), machine facts (mono),
counts (mono, right). Primary click target is the row. Row actions
appear on hover and always in the accessibility tree.

Sidebar `.nav-item`: 32px, 12px label, live count in mono on the right.
Current item: `--bg-active` and accent bar.

### 6.4 Status badges

`.badge` + modifier. Height 18px, padding 0 6px, radius `--r-1`,
12px / 500, background 12% of the status colour, text the status
colour. Word always present.

| Badge | Colour | Protocol |
| --- | --- | --- |
| `online` / `serving` | `--online` | node reachable, `NodeState::Serving` |
| `draining` | `--warn` | `NodeState::Draining` |
| `offline` | `--text-tertiary` | book entry, no connection |
| `starting` | `--warn` | `SessionStatus::Starting` |
| `running` | `--online` | `SessionStatus::Running` |
| `exited` | `--text-tertiary` | `Exited` (include code if any) |
| `failed` | `--danger` | `Failed` |
| `interrupted` | `--interrupt` | `Interrupted` |
| `home` | `--accent` | fleet role |
| `member` | `--text-secondary` | fleet role |
| `direct` / `ssh` / `relay` | secondary, mono | how this client reaches it |
| `conflict` | `--warn` | member has `.vessel-incoming` |
| `terminal` / `structured` | secondary | `SessionMode` |
| `stale` | `--warn` | applied generation < home |

### 6.5 Timeline items (structured)

A vertical 1px rail in `--border-subtle`, 12px from the left. Each
item is a row with a 8px glyph on the rail:

| Event (`AgentEvent`) | Glyph | Body |
| --- | --- | --- |
| `TurnStarted` | filled accent tick | the prompt, mono 13px, on `--bg-elevated` |
| `UserMessageChunk` | same | resume replay; muted, labelled "replayed" |
| `MessageChunk` | empty circle | markdown, sans 13px, max 78ch |
| `ThoughtChunk` | dashed circle | collapsed `<details>`, tertiary |
| `ToolCall` | square by `ToolKind` | `.tool` card, §6.6 |
| `ToolCallUpdate` | (same card, in place) | do not append a second card |
| `Plan` | checklist | inspector, and a compact strip in-timeline |
| `PermissionRequested` | amber diamond | §6.7; also sticky above composer |
| `PermissionResolved` | check or x | one line under the request |
| `TurnEnded` / `TurnFailed` | rule | reason in meta |
| `Ready` | meta | harness session id, mono |
| `HarnessLog` | meta | stderr, mono, tertiary; error colour if it looks like a login failure |

User and agent are not two chat bubbles on opposite sides. This is a
log, left-aligned, like an editor's output channel.

Markdown: Pretendard, 13/20. Inline code and fences use `--font-mono`
on `--bg-elevated` with a hairline. Headings inside a turn start at
the `h3` equivalent (14px / 600). Lists and blockquotes get a 1px
accent-left only for blockquotes.

### 6.6 Tool-call cards

`<details class="tool">`. Summary: kind icon, title, status badge,
location path (mono, truncated from the left). Body: input then output,
each a labelled `<pre>`. Default **collapsed** when `completed` or
`failed`; **open** when `pending` or `in_progress`.

Kind icons (stroke, 14px): read, edit, delete, move, search, execute,
think, fetch, other. Colour follows status, not kind — kind is the
shape, status is the colour. Otherwise eight rainbow tools fight the
accent rule.

Diffs (`ToolCallContent::Diff`): line-numbered, add/delete backgrounds
from the syntax table, `select-none` on the gutter. Horizontal scroll,
no wrap.

JSON input/output: 12px mono, comment colour for keys if you highlight;
plain is acceptable in v1. Copy button on the summary, `aria-label="Copy tool output"`.

### 6.7 Permission prompts

A pending `PermissionRequested` is the most important thing on the
screen.

- In-timeline card, `--warn` hairline, listing the tool title and the
  harness's options in order (`AllowOnce`, `AllowAlways`, `RejectOnce`,
  `RejectAlways` — use the harness labels, not our own).
- Sticky bar above the composer, same actions, so it cannot scroll off.
- First answer wins (protocol). After `PermissionResolved`, both copies
  freeze showing the chosen option and who (this device / another
  follower). Later clicks error with conflict — show a toast, do not
  hide the history.
- Composer `disabled` while a permission is pending, with helper text
  "Answer the permission to continue".

### 6.8 Terminal frame

Outer `.term-frame` radius 16, inner `.term` radius 12, 4px gap, both
hairlines, inner `--bg-base`. Chrome 32px: node · session prefix ·
cols×rows · encoding. The PTY itself is full-bleed inside the inner
frame.

Font: `--font-mono` at 13px / 1.6. Cursor a 2px box in `--accent`.
Selection `--accent-muted`. Do not paint the whole screen green.

The mockup is a `<pre>` with a few coloured spans. The app uses
xterm.js (or equivalent) with the token colours as its theme.

### 6.9 Dialogs

`.dialog` 480–560px, `--bg-elevated`, radius `--r-4`, padding `--s-4`.
Header 13px / 600, close ghost button. Footer right-aligned, ghost
Cancel then primary. `role="dialog"` `aria-modal="true"`
`aria-labelledby`. Focus trapped in the app; mockups only set
autofocus on the first field.

Dimmer click and Escape cancel, unless a pairing invite is live and
uncopied — then confirm.

### 6.10 Toasts

Bottom-right, 320px, stack up. 4s for success, stay for error until
dismissed. `--bg-elevated` + hairline + 3px status bar on the left.
`role="status"` for success, `role="alert"` for error. One toast in
the devices mockup (device revoked).

### 6.11 Empty, loading, error

| State | Treatment |
| --- | --- |
| Empty list | 48px top margin, 16px stroke icon, title 13/600, one sentence secondary, one primary action |
| Loading | 3 skeleton rows, 12px high, `--bg-hover`, no shimmer under reduced motion; otherwise a 1.2s opacity pulse |
| Error | banner at the top of main, `--danger` hairline, message from the protocol, Retry |
| Offline node | the node still lists; sessions pane empty-state "gpu-box is offline" with last-seen |
| Reconnecting | 28px bar under the session header, `--warn`, "Reconnecting to gpu-box · last byte 4s ago". Terminal stays visible, frozen. On resume, the bar becomes a 1.2s "Caught up from seq 1842" status then leaves |

Empty copy is specific ("No sessions on studio") not generic
("Nothing here yet").

---

## 7. Interaction details

### 7.1 Keyboard

The operator is in this UI for hours. Shortcuts are first-class.

| Keys | Action |
| --- | --- |
| `⌘K` or `/` | command palette (focus search) |
| `⌘N` | new session |
| `⌘Enter` | send prompt (structured) |
| `Escape` | close dialog, blur composer, collapse palette |
| `j` / `k` | next / previous row (when not in an input) |
| `Enter` | open the selected row |
| `⌘.` | kill the open session (confirm if running) |
| `⌘⇧C` | copy session id |
| `⌘I` | toggle inspector |
| `⌘B` | toggle sidebar |
| `[` / `]` | collapse / expand tool calls in view |
| `g f` | Fleet |
| `g s` | Sessions |
| `g y` | Sync |
| `g d` | Devices |
| `1`–`9` | select nth node in the sidebar |
| `⌘1` / `⌘2` | structured composer vs (future) editor pane — reserved |

Show the shortcut in menus and as a `kbd` hint on empty states. Do not
show a cheatsheet overlay on first run.

### 7.2 Live updates

- Node presence and session lists: the client already has typed RPC
  and `session.subscribe`. Rows patch in place; they do not flicker a
  reload.
- A new session inserts at the top of its tree position, 180ms fade.
- Status badge changes instantly; do not animate colour.
- Structured timeline appends events as they arrive. `MessageChunk`
  concatenates into the open agent bubble. `ToolCallUpdate` mutates
  the existing card.
- Sequence numbers (`EventSeq`) are the resume key. If the client is
  told `Lagged { resume_from }`, show reconnecting and call
  `session.read` from that seq — never pretend the gap did not happen.
- Multi-follower: permission answers and kills from another device
  appear as they are broadcast. A small meta line "answered on laptop"
  is enough.

### 7.3 Reconnecting

Three layers, because three things fail independently:

1. **This client's link to a node** (direct dropped, falling back to
   relay, or SSH restarting). Topbar device chip turns warn:
   `gpu-box · reconnecting (relay)`.
2. **The session attach** (PTY or structured subscribe). Session
   banner, §6.11.
3. **The node itself draining or gone.** Node badge `draining` /
   `offline`. Creating a session is disabled with the protocol's
   `Unavailable` message.

Fallback order is the product's: direct (3s), then relays, then (if
the book has one) SSH. The UI names the route in use, not the list of
attempts, unless the operator opens the node inspector.

### 7.4 Session actions

Available on both modes, from the session header overflow and from
the list row:

| Action | Notes |
| --- | --- |
| Kill | confirm; `session.kill` |
| Move profile | structured only, between turns; list profiles of the same harness that are logged in |
| Copy to node | `session.export` / `import`; pick node, profile, cwd; optional start |
| Copy id | |
| Open log | future; not in mockups |

Move and copy are dialogs, not inline selects, because they have
consequences (the conversation continues elsewhere).

### 7.5 New session

Fields in order, because each one filters the next:

1. **Node** — book names, with online/offline. Offline nodes disabled.
2. **Harness** — those the node discovered. If structured is
   unavailable, the mode toggle still shows structured but disabled
   with the install hint from `StructuredSupport::Unavailable`.
3. **Profile** — that harness's profiles on that node (`claude`,
   `claude-work`, …). Default profile labelled.
4. **Model** — routed model ids. Until Phase 3 ships, this is the
   harness's own list plus a placeholder "node default". Group by
   provider; show which node would serve a local model.
5. **Working directory** — text, placeholder the node's home, with a
   recent list.
6. **Mode** — structured (default when available) or terminal.

Create is the only primary button. The dialog does not close until
the RPC returns; the button shows a spinner. Failure sits inline,
not only as a toast.

---

## 8. Accessibility

- **Focus.** `:focus-visible` is a 2px `--accent` ring, offset 2px,
  on every control. Do not remove outlines. Mouse click does not
  leave a ring (`:focus-visible` only).
- **Hit targets.** 32px desktop, 44px below 768px for the primary
  controls (nav, composer send, dialog buttons).
- **Landmarks.** `role="banner"` topbar, `role="navigation"` sidebar,
  `role="main"`, inspector `role="complementary"`, composer is a
  `form`. Skip link "Skip to sessions" is the first focusable node.
- **Lists.** Session tree is `role="tree"` / `treeitem` with
  `aria-level`, `aria-expanded`. Flat tables stay `<table>` with
  `th scope="col"`.
- **Live regions.** Reconnecting banner and incoming permission:
  `aria-live="assertive"`. New log lines in the timeline:
  `aria-live="polite"` on the list, not on every chunk (or the reader
  is flooded). Toasts as in §6.10.
- **Contrast.** §4.3. Status never colour-only.
- **Reduced motion.** `prefers-reduced-motion: reduce` disables the
  starting/reconnecting pulse, skeleton shimmer, dialog rise (opacity
  only), and any type-in. Instant state, no delay.
- **Theme.** Honour `prefers-color-scheme` on first run if the
  operator has not chosen. The Settings toggle writes the preference.
  Do not flash dark-then-light: set the class before paint.
- **Language.** UI copy is English, matching the CLI. `lang="en"` on
  `<html>`. Pretendard still covers Korean in session markdown and
  paths.
- **Terminal.** xterm must expose a text alternative (the live buffer
  as a visually-hidden `aria-live="off"` log the operator can reach
  with a "Screen reader buffer" toggle, default off). Structured mode
  is the accessible default for that reason.

---

## 9. Mapping onto the protocol

So M7 does not have to guess names.

| UI | Protocol / store |
| --- | --- |
| Node name | `NodeInfo.name` (book name if it differs, book wins for display) |
| Node id (inspector) | `NodeInfo.id` |
| Version | `NodeInfo.version` |
| OS / arch | `NodeInfo.os` `arch` |
| Serving / draining | `NodeInfo.state` |
| Online | client connection state, not a daemon field |
| Reach | the route this client used: direct, SSH, relay |
| Home / member | fleet membership; home is the node that publishes |
| Sync generation | fleet manifest generation the home is on; member's applied generation from `fleet.status` |
| Conflict | local file kept, incoming at `<name>.vessel-incoming` |
| Harness | `HarnessKind` |
| Profile | `ProfileName`; current is `SessionInfo.profile` |
| Mode | `SessionMode` |
| Model | `SessionMode::Structured.model` |
| Cwd | `SessionSpec.cwd` |
| Status | `SessionStatus` (+ turn from last `TurnStarted`/`TurnEnded`) |
| Created | `SessionInfo.created_at` |
| Creator | `SessionInfo.created_by` / `origin` / `via` |
| Timeline | `AgentEvent` in `EventSeq` order |
| Permission | `PermissionId` + `PermissionOption` |
| Devices | `device.list`; scopes `read`, `operate`, `admin` |
| Pairing link | `vessel://pair#…` (fragment; never sent to a server) |
| Kill / move / copy | `session.kill`, `session.move_profile`, export/import |

Secret values never appear in the UI. Sync may list secret *names*.
Pairing links in the mockup are fake; the app renders a real QR of
the returned link and expires it in the same 10 minutes as the CLI.

---

## 10. Sample data used in the mockups

Generic, no real hostnames.

| Node | Role | Reach | State | Live |
| --- | --- | --- | --- | --- |
| `laptop` | home | direct | serving | 3 |
| `gpu-box` | member, gen 48 | relay | serving | 2 |
| `build-server` | member, gen 47 (stale) | ssh | draining | 1 |
| `studio` | member, gen 48 | direct | offline | — |

Profiles: `claude` (default), `claude-work`, `codex`, `gemini`,
`opencode`. Models shown as routed ids (`anthropic/claude-sonnet-4.5`,
`local/qwen2.5-coder-32b` on gpu-box).

A three-deep agent tree on Sessions: a person on laptop starts
`3f2a`, its agent starts `a91c` on gpu-box, that agent starts `c07e`
on build-server.

---

## 11. What the mockups will not do

- No JavaScript. Disclosure is `<details>`, theme is a checkbox,
  dialogs are either a dedicated page or `:target`.
- No real xterm, markdown renderer, or QR encoder.
- No command-palette results.
- Settings is a dialog, not a route.

M7 should replace those with the real controls, keeping the tokens,
density, and vocabulary.

---

## 12. Decisions worth keeping

- **Purpose and intent:** a control room for a personal fleet of
  harnesses, readable in a glance, operable from the keyboard, honest
  about reach and attribution.
- **Existing constraints:** protocol 6 is already shipped; the UI must
  not rename statuses, scopes, or fleet conflict behaviour. Pretendard
  first is a host-wide font rule.
- **Alternatives considered:** a chat-first shell (hides the fleet); a
  card-grid node dashboard (too marketing, too little data); filling
  primary buttons with the accent (kills the accent's quiet jobs);
  a top-tab layout (nodes are a hierarchy, they belong on the left).
- **Chosen:** dark-first, sidebar-scoped, tree of sessions, syntax as
  the status palette, concentric frame only on the terminal.
- **Trade-offs:** denser than consumer chat, so new users will need
  the empty states to be plain-spoken. Light theme is real work, not
  a filter, because comment/meta greys fail if inverted.
