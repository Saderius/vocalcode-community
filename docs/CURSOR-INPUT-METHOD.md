# Cursor-Anchored Voice Input — Project Plan

Status: proposal · Branch: `feat/cursor-input-method` · Date: 2026-11-02

This document is the goal statement for this fork. It is written to be edited:
the "Open questions" section is expected to shrink as the codebase study lands.

---

## 1. The idea in one sentence

Turn VocalCode from *"hold a shortcut, then speak"* into a **pointer-native input
method**: a small dot lives at the mouse cursor, morphs into a voice-input control
when the pointer is over a text field, and a **double-click on the field toggles
dictation** — no keyboard chord, no separate invocation step.

## 2. Why this is worth building

Today the flow is: *click into the field → move hand to keyboard → press the talk
key → speak → text appears.* The first and third steps are in different mental
modes. The pointer is already where the user's attention is; the trigger should be
there too.

Windows' own voice typing has the same friction: it is bound to a chord, and it
does not know or care where you are about to type. A cursor-anchored control can:

- arm itself **only** over targets we already trust (see §5), so it cannot type
  into the wrong window;
- collapse "invoke" and "aim" into one gesture;
- stay out of the way everywhere else — it is a dot, not a bar.

## 3. Product spec (v0)

### 3.1 The cursor dot

- An opt-in mode, `Cursor input mode`, in **Settings → Dictation**. Off by default.
- A ~10 px dot offset below-right of the pointer, following it.
- Visual states:

| State | Meaning | Look |
| --- | --- | --- |
| `Idle` | pointer is not over an editable target | dim, low-contrast dot |
| `Armed` | pointer is over a target we would accept text in | accent-coloured ring |
| `Listening` | capturing speech | dot morphs into a small live waveform |
| `Transcribing` | audio closed, waiting for text | slow pulse |
| `Delivering` | injecting text | brief fill |
| `Blocked` | target is elevated/secure/fullscreen | dot hidden or slashed |

- Hard constraints: never take focus, never swallow a click, never appear over the
  lock screen / secure desktop / UAC prompt, and never appear inside an elevated
  window's client area (we cannot legally interact with it — UIPI).

### 3.2 Toggle gesture

- **Double-click on an editable target** starts dictation; **double-click again**
  (or `Esc`, or a click on a non-editable area) stops and commits.
- The existing hold-to-talk shortcut stays and remains the default trigger. Cursor
  mode is additive, and the two must not fight over the same microphone session.

### 3.3 "Recognising the current input"

The phrase "recognition of currently possible input, as text" resolves to two
separate problems, and only the first is new:

1. **Target detection** — is the thing under the pointer an editable text target?
   This drives `Armed`. It must be conservative: a false positive means text could
   land somewhere unexpected.
2. **Speech recognition** — already solved by the existing engine; unchanged here.

### 3.4 Delivery

Reuse the existing injection path. Text must land at the caret of the field that
was double-clicked, and the existing focus/target checks stay in force.

---

## 4. What already exists (confirmed by reading the tree)

| Need | Where it lives today |
| --- | --- |
| Global mouse observation | `vocalcode-platform/src/hook_windows.rs` (vendored `rdev`); `MouseExtra::X1/X2` are already first-class triggers |
| Target trust / focus checks | `vocalcode-platform/src/foreground.rs` |
| Text insertion | `vocalcode-platform/src/inject.rs`, `hid_input.rs`, `input_windows.rs` |
| Floating indicator window | `vocalcode-app/src/overlay.rs` + `overlay.html` — already click-through and no-activate |
| Second floating surface (dev tree) | `vocalcode-app/src/control_bar.rs` + `control_bar.html` |
| Dictation lifecycle | `vocalcode-app/src/workflows.rs`, `dictation_control.rs` |
| Settings + UI | `vocalcode-core/src/config.rs`, `vocalcode-app/src/webui.rs` + `webui.html` |

So roughly 70% of the plumbing is already present. The genuinely new work is:
pointer tracking, editable-target probing, cursor-anchored overlay positioning, and
a click/double-click policy that does not break the host application.

---

## 5. Architecture sketch

New modules:

- **`vocalcode-platform/src/pointer.rs`**
  - cursor position + monitor + DPI (`GetCursorPos`, `MonitorFromPoint`, per-monitor v2);
  - global click observation (extend the existing hook, do not add a second hook);
  - `editable_target_at(point) -> Option<Target>` using UI Automation
    (`UIA_IsTextPatternAvailable`, `ValuePattern`, `TextPattern`) with an MSAA
    fallback, plus the existing foreground-identity checks as a gate.
- **`vocalcode-app/src/cursor_input.rs`**
  - the state machine from §3.1;
  - debounce and hysteresis for arming (pointer jitter must not flicker the dot);
  - the double-click policy and cancel rules;
  - owns "start/stop dictation" calls into the existing workflow, not a parallel path.

Modified:

- `overlay.rs` / `overlay.html` — add an *anchor* mode (`Screen` | `Cursor`) and the
  new visual states; the waveform is CSS/canvas, not a new window.
- `config.rs` — new `CursorInput` section; must follow the existing settings-version
  rules (a settings bump has forward-compatibility consequences — see `BUILDING.md`).
- `webui.rs` / `webui.html` — the toggle plus the two policy choices.

Threading rule: cursor polling belongs on the platform timer thread at a modest rate
(~30 Hz is enough for a dot), never a busy loop on the UI thread. Reposition the
overlay through the existing IPC path.

---

## 6. Milestones

| # | Milestone | Exit criteria |
| --- | --- | --- |
| **M0** | Toolchain and a reproducible local build | `cargo build -p vocalcode-app --release --locked --features community` succeeds; `vocalcode-app.exe --build-info` reports `"identity": "development"` |
| **M1** | Dot follows the pointer | Setting on → dot tracks the cursor across monitors and DPIs; clicks pass through untouched; setting off → nothing rendered |
| **M2** | Editable-target arming | Dot arms over Notepad, a browser `<input>` and `contenteditable`, VS Code, Word; stays idle over desktop, taskbar, and elevated windows. Log-only, no injection. |
| **M3** | Double-click toggle | Double-click an armed target → dictation starts; second double-click → stops and injects at the caret. `Esc` cancels. |
| **M4** | Polish | Waveform, blocked states, fullscreen/lock handling, multi-monitor and DPI changes, no flicker, no measurable idle CPU cost |
| **M5** | Ship-quality | Settings UI, docs, hidden smoke test, manual QA matrix, no regression in existing hotkey tests |

## 7. Risks and open questions

1. **Double-click semantics.** In editors a double-click selects a word. We must
   decide whether the toggle intercepts double-clicks *anywhere* on an editable
   target or only *on the dot itself*. The dot-only rule is safer and preserves the
   host app's behaviour; it is also the recommended default. **Decision needed.**
2. **Click-through vs. hit-testing.** The overlay is `WS_EX_TRANSPARENT`, so it never
   receives clicks — by design. All click detection must come from the global hook.
   Confirm the vendored `rdev` reports button-down with position and without
   stealing the event.
3. **UIPI / elevation.** Elevated windows are invisible to UI Automation from a
   non-elevated process. The dot must hide rather than lie.
4. **Games / exclusive fullscreen / raw-input apps.** Hide; do not hook-fight.
5. **IME and composition.** Injection during an active composition can corrupt text;
   reuse whatever guard `inject.rs` already has.
6. **Settings version bump.** `BUILDING.md` warns that a newer config format makes
   older builds refuse to start. Keep the new section additive if at all possible.
7. **Branding / licence.** AGPL-3.0-only. A redistributed fork must not present as
   the upstream project — see `BRANDING.md` before any release.
8. **Perceived latency.** Arming must feel instant when the pointer stops moving;
   a 100 ms probe is fine, a 400 ms probe is not.

## 8. Verification

- Build + `--build-info` identity check.
- Hidden smoke examples (`control_bar_hidden_smoke`, `indicator_hidden_smoke`,
  `webui_hidden_smoke`) extended with a cursor-anchor case.
- `cargo test --workspace --locked --features vocalcode-app/community`.
- Manual matrix: Notepad, Word, VS Code, Chrome `<input>`, Chrome `contenteditable`,
  Terminal, Explorer rename, an elevated Notepad, a fullscreen game, and a
  multi-monitor DPI change.

## 9. Non-goals for v0

- No ASR/model changes, no cloud, no meeting-feature changes.
- Not a replacement for the hotkey; not a general-purpose "AI at the cursor" widget.
- No per-app configuration.

## 10. Environment status (this machine, 2026-11-02)

| Component | Status |
| --- | --- |
| Git | present |
| Node 26 / Python 3.11 | present (meets `BUILDING.md` minimums) |
| WebView2 Runtime | present |
| Windows SDK 10.0.22621 | present |
| **Rust toolchain 1.97.1** | **missing** |
| **MSVC C++ Build Tools / `cl.exe`** | **missing** |
| **LLVM / libclang** | **missing** |
| Installed VocalCode | to be confirmed |

M0 is therefore a real task, not a formality.
