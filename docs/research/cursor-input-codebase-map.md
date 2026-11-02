# Cursor-Anchored Voice Input -- Codebase Map

Status: research note for `docs/CURSOR-INPUT-METHOD.md` (plan), 2026-11-02
Scope: read-only study of the tree at `G:\GitHub\VocalCode`. Every claim below
cites `file:line` from the actual source. Where the plan is wrong, see section 12.

Line counts here are total lines including blank lines (`(Get-Content f).Count`),
not `Measure-Object -Line` (which skips blanks).

---

## 0. Orientation: what lives where (and two plan mistakes)

| Layer | Crate / file | Role |
| --- | --- | --- |
| State machine | `vocalcode-core/src/engine.rs` (3387) | the only dictation state machine |
| Event bus | `vocalcode-core/src/trigger_bus.rs` (658) | bounded, callback-safe queue |
| Config schema | `vocalcode-core/src/config.rs` (1833) | TOML schema, versioning, bounds |
| Global input | `vocalcode-platform/src/hotkey.rs` (3956) + `hook_windows.rs` (411) | hooks, capture, trigger matching |
| Focus/identity | `vocalcode-platform/src/foreground.rs` (197) | app id of the foreground window |
| Text delivery | `vocalcode-platform/src/inject.rs` (3379) | UIA focus token + typing/clipboard |
| Floating surfaces | `vocalcode-app/src/overlay.rs` (1616) + `overlay.html` (274); `control_bar.rs` (1789) + `control_bar.html` (118) | indicator / interactive capsule |
| UI-to-engine mailbox | `vocalcode-app/src/dictation_control.rs` (430) | `Bridge`: Start/Stop/Cancel |
| Engine thread + event loop | `vocalcode-app/src/main.rs` (9594) | consumes the bus, publishes `Outcome` |
| Settings UI host | `vocalcode-app/src/webui.rs` (11757) + `webui.html` (7264) | tao loop, config IPC, page |

Two corrections to the plan's section 4 table, established below:

1. **`vocalcode-app/src/workflows.rs` is not the dictation lifecycle.** It is
   per-app cleanup/diagnostics preferences stored in
   `personalization/workflows.json` (`workflows.rs:1-2`, `:124-146`). The
   lifecycle is `Engine::handle` in `vocalcode-core/src/engine.rs:858`.
2. **`dictation_control.rs` is not the lifecycle either.** It is a
   generation-bound request mailbox that *emits* `TriggerEvent`s
   (`dictation_control.rs:1-2`, `:166-170`).

---

## 1. Global mouse observation

### 1.1 The hooks that exist

`vocalcode-platform/src/hook_windows.rs` installs **both** hooks on one thread:

- `SetWindowsHookExW(WH_KEYBOARD_LL, ...)` -- `hook_windows.rs:279-283`
- `SetWindowsHookExW(WH_MOUSE_LL, ...)` -- `hook_windows.rs:284-288`
- a 1 s thread timer as a heartbeat -- `hook_windows.rs:292-297`
- a watchdog thread that posts `WM_QUIT` on stall/shutdown --
  `hook_windows.rs:313-354`

`run` is the whole contract:

```rust
pub(crate) fn run(
    handler: impl FnMut(&EventType, bool) -> bool + Send + 'static,
    installed: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), String>          // hook_windows.rs:220-225
```

The handler is `FnMut(&EventType, injected: bool) -> bool`, where `true` means
**consume** (`hook_windows.rs:14-19`, `:110-134`).

### 1.2 What the mouse callback actually receives

`mouse_proc` (`hook_windows.rs:184-216`):

- Casts `lparam` to `MSLLHOOKSTRUCT` at `hook_windows.rs:188`, reads
  `mouse.flags & LLMHF_INJECTED` at `:189`, and reads `mouse.mouseData >> 16`
  for X1/X2 at `:200`.
- **It never reads `mouse.pt`** (the screen position) or `mouse.time`.
- Mapping (`:190-207`):
  - `WM_LBUTTONDOWN/UP` -> `ButtonPress/Release(Button::Left)`
  - `WM_RBUTTON*` -> `Right`, `WM_MBUTTON*` -> `Middle`
  - `WM_XBUTTONDOWN/UP` -> `Button::Unknown(high word)` = 1 for X1, 2 for X2
  - everything else (`WM_MOUSEMOVE`, wheel) -> `None`, straight to
    `CallNextHookEx` (`:207`, `:215`)
- Consumption is per event: `if dispatch(&event, injected) { return 1; }`
  (`:209-213`), otherwise `CallNextHookEx(...)` (`:215`). Returning `1` from a
  `WH_MOUSE_LL` proc is what suppresses delivery to the target window (the same
  trick as rdev's grab, `vendor/rdev/src/windows/grab.rs:42-48`).

The module doc is explicit: *"Mouse motion and wheel never reach the handler -- a
1000 Hz mouse is filtered before any lock is taken"* (`hook_windows.rs:17-19`).
`dispatch_grabbed` re-filters defensively via `physical_event`
(`hotkey.rs:1143-1149`, `:1424-1434`).

### 1.3 Does the vendored rdev report button-down with position?

**No, and it does not matter on Windows, because rdev's grab is not used there.**

- `EventType::ButtonPress(Button)` / `ButtonRelease(Button)` carry **no
  coordinates** (`vendor/rdev/src/rdev.rs:230-243`). Only `MouseMove { x, y }`
  has a position.
- rdev's Windows decoder has a `get_point` helper that reads
  `MSLLHOOKSTRUCT.pt` (`vendor/rdev/src/windows/common.rs:35-38`) but calls it
  **only** for `WM_MOUSEMOVE` (`:77-83`); the button arms at `:63-76` discard it.
- rdev's `grab` can suppress (`vendor/rdev/src/windows/grab.rs:34-48`) and it
  attaches `Event::name = None` in this fork (`:22-28`).
- On Windows, VocalCode does **not** use `rdev::grab`; `run_backend` calls
  `crate::hook_windows::run` (`hotkey.rs:1020-1074`). The rdev-grab path is the
  non-Windows fallback (`hotkey.rs:1085-1119`).

So the plan's risk #2 ("confirm the vendored rdev reports button-down with
position") is answered: not via rdev, but trivially available from the
`MSLLHOOKSTRUCT` that `mouse_proc` already holds. Adding
`let (x, y) = (mouse.pt.x, mouse.pt.y);` at `hook_windows.rs:188` is the entire
"get position with the click" problem.

### 1.4 Trigger dispatch: hook -> dictation start

```
mouse_proc (hook_windows.rs:184)
  -> dispatch(event, injected)                        :110
       -> HANDLER.lock() -> handler(event, injected)  :115-122  (closure from hotkey.rs:1060)
            -> dispatch_grabbed(...) -> bool          hotkey.rs:1127
                 - physical_event(event)              :1147 / :1424
                 - matching_action(input, talk, send, teach)  :1355-1360 / :1651
                 - tx.try_send(TriggerEvent::TalkPressed(id))  :1376-1380
                 - hook.active.insert(id, (action, injected))  :1382
```

- Trigger identity: `TriggerId::new(TriggerSource::Mouse, 0,
  mouse_control(button))` (`hotkey.rs:1503-1511`, `:1527-1534`); left/right/middle
  get `0x100/0x101/0x102`, `Button::Unknown(n)` gets `n`.
- Matching: `mouse_matches` (`hotkey.rs:1737-1748`) accepts **only**
  `Button::Unknown(1|2)` for `Trigger::MouseButton(X1|X2)`; left/right/middle
  never match any trigger.
- The engine receives `TalkPressed(TriggerId)` / `TalkReleased(TriggerId)` and
  starts/stops (`engine.rs:860`, `:942`).
- The same bus carries `HandsFreeStart` (explicit UI start) and `ForceStop` /
  `Cancel` (controls) -- `traits.rs:64-93`.

### 1.5 Single vs double click today

**For the left button: no.** No click timing, no history, no
`WM_LBUTTONDBLCLK` handling, and left clicks are not bindable at all:

- `capture_code` maps left/right/middle to `("mouse_click", false)` --
  deliberately unbindable (`hotkey.rs:1624-1629`).
- `mouse_matches` returns false for them (`hotkey.rs:1737-1748`), so
  `matching_action` is `None`, so `dispatch_grabbed` returns `false` and the
  click passes through untouched (`hotkey.rs:1372-1374`).

**For a bound X1/X2: no click-count state either**, but there is an *existing
double-tap state machine* that a cursor feature must not fight:

- `Engine` holds `double_tap_lock: bool`, `last_tap: Option<Instant>`, and
  `DOUBLE_TAP_WINDOW = 500 ms` (`engine.rs:101-104`, `:193`).
- A hold-mode press shorter than `tap_threshold()` (150-400 ms, `:369-371`) is
  discarded on release and remembered (`:942-951`); a second press within 500 ms
  then latches hands-free (`:899-905`).
- It is enabled only when `talk_mode == "hold" && double_tap_lock`
  (`main.rs:5106-5108`), and `double_tap_lock` defaults to `true`
  (`config.rs:541-543`).

Consequence: on a bound X2 in hold mode, "double-click" already means
*tap-then-press -> latch*, not *toggle a cursor session*.

---

## 2. Dictation lifecycle

### 2.1 Exact entry points

| Function | Signature | Location |
| --- | --- | --- |
| `Engine::handle` | `(&mut self, ev: TriggerEvent) -> Result<Outcome>` | `engine.rs:858` |
| `Engine::will_start` | `(&self, ev: TriggerEvent) -> bool` | `engine.rs:1025-1032` |
| `Engine::will_finish` | `(&self, ev: TriggerEvent) -> bool` | `engine.rs:1043-1062` |
| `Engine::force_stop` | `(&mut self) -> Result<Outcome>` (stops + transcribes + injects) | `engine.rs:652-654` |
| `Engine::force_stop_to_history` | `(&mut self) -> Result<Outcome>` (never injects) | `engine.rs:662-674` |
| `Engine::force_cancel` | `(&mut self) -> Result<Outcome>` (discards) | `engine.rs:714-722` |
| `Engine::finish` (private) | `(&mut self) -> Result<Outcome>` | `engine.rs:1073-1223` |
| `Bridge::submit` | `(&self, action: Action, snapshot: Snapshot, ready: bool, recording: bool, now: Instant) -> Result<(), &'static str>` | `dictation_control.rs:57-64` |
| `Bridge::take` | `(&self, current: Snapshot, ready: bool, recording: bool, enabled: bool, now: Instant) -> Option<Dispatch<'_>>` | `dictation_control.rs:130-137` |

`Action` is `{ Start, Stop, Cancel }` (`dictation_control.rs:17-21`) and maps to
`HandsFreeStart(CONTROL_ID)`, `ForceStop`, `Cancel`
(`dictation_control.rs:166-170`; `CONTROL_ID = TriggerId::synthetic(0x444f_434b)`,
`:11`).

### 2.2 Hold vs toggle vs hands-free

Three different things, easy to conflate:

1. **hold vs toggle** -- `Config::talk_mode: String` `"hold" | "toggle"`
   (`config.rs:492-500`), published into `status.talk_latched: Arc<AtomicBool>`
   (`main.rs:2756-2758`, `:4769-4771`) and read by the engine through
   `latch: Arc<AtomicBool>` (`engine.rs:44`, `:1010-1012`).
2. **latched per utterance** -- `active_latched` is set at start from
   `hands_free || latched() || double_tap` (`engine.rs:905`, `:937`) and decides
   whether the release that started the recording also ends it (`:955-962`).
3. **hands-free start** -- `TriggerEvent::HandsFreeStart` always latches and is
   idempotent while recording: `if hands_free && self.recording { return
   Ok(Outcome::Idle) }` (`engine.rs:861-864`). **A second double-click therefore
   cannot "stop" via `HandsFreeStart`; it must send `ForceStop`.**

### 2.3 State machine shape

Fields that constitute the state (`engine.rs:40-112`):

```
recording: bool                 // microphone open, ASR owns the utterance
recording_since: Option<Instant>
active_latched: bool            // this utterance ends on the next press, not release
pressed: HashSet<TriggerId>     // physical controls currently down for this utterance
started_by: Option<TriggerId>
last_tap: Option<Instant>       // discarded hold-mode tap, for double-tap latch
latch: Arc<AtomicBool>          // config talk_mode
```

plus the pre-engine bookkeeping in `HookState` (`hotkey.rs:700-743`):
`physical_down`, `active`, `captured_down`, `pending_modifier`, `page_down`.

Transitions of interest:

- start: `engine.rs:860-941` -> `injector.begin_utterance()` (`:921`),
  `audio.start()` (`:925`), `recording = true` (`:930`), `Outcome::Listening`.
- stop+deliver: `finish()` (`:1073`), which stops capture first (`:1082`), then
  decodes, cleans, applies rules/writing, and calls `inject_dictation` (`:1176`).
- cancel: `force_cancel()` (`:714`).
- lost key-up safety net: `recording_limit_expired()` (`:851-856`) +
  `force_stop_to_history()` (`:662`).
- UI feedback is computed *before* `handle`: `will_start` / `will_finish` are
  called at `main.rs:5079-5080` so cues and the overlay are not late.

### 2.4 Where a new module plugs in

The engine is a single owner on a dedicated thread:

- thread spawn: `main.rs:3894-3895`
- blocking receive: `rx.recv_timeout(input_wait)` at `main.rs:4987`, with
  `DICTATION_POLL_INTERVAL = 80 ms` while recording (`main.rs:2806`,
  `:4974-4977`)
- dispatch: `main.rs:5010-5234`, `engine.handle(ev)` at `:5138`
- the UI mailbox is consulted **before** the bus event is handled, and only for
  `TriggerEvent::Wake` (`main.rs:4990-5009`):

```rust
let control_dispatch = if matches!(input, Ok(TriggerEvent::Wake)) {
    status.dictation_control.take(
        overlay.snapshot(),
        status.ready.load(Ordering::Acquire) && !status.shutdown.load(Ordering::Acquire),
        engine.is_recording(),
        active_config.desktop_control,        // <-- the gate
        Instant::now(),
    )
} else { /* ... */ };
```

**This is the correct plug-in point for a programmatic start/stop**, and it is
exactly what `control_bar.rs` uses (`control_bar.rs:643`, `:166-170`). Two
hazards:

1. The `enabled` argument is literally `active_config.desktop_control`
   (`main.rs:4997`). With cursor mode on and the desktop control bar off,
   `Bridge::take` refuses a Start (`dictation_control.rs:158`). Reuse requires
   either a second `Bridge` consulted in the same block, or changing that gate
   to `desktop_control || cursor_input`.
2. `Bridge` is a single-slot mailbox with `REQUEST_TTL = 750 ms`
   (`dictation_control.rs:10`, `:150-165`) and a `busy` latch; two independent
   producers sharing one `Bridge` reject each other (`:77-83`; test
   `duplicate_clicks_have_one_slot_and_do_not_toggle`, `:335-346`). A cursor
   controller should own its **own** `Bridge` instance (cheap:
   `Bridge::default()` + `connect(tx.clone())`, cf. `main.rs:3827`).

Alternative (worse): send `TriggerEvent`s directly on `TriggerEventSender`
(`trigger_bus.rs:170-252`). `ForceStop`/`Cancel` take the emergency path when
contended (`trigger_bus.rs:215-251`), which is safe, but a `HandsFreeStart` sent
this way bypasses the snapshot/TTL/`ready` guards the `Bridge` provides.

### 2.5 Delivery and its guards

`EnigoInjector` (`inject.rs:1635-1641`) implements `TextInjector`
(`inject.rs:1908-1980`):

- `begin_utterance()` snapshots `current_focus()` but deliberately does **not**
  pin it (`:1909-1920`).
- `inject_text()` resolves the delivery target when the words are ready:
  `delivery()` = `select_current_delivery_target(recording_start,
  focused_text_target())` (`:1681-1687`, `:161-172`) -- the current safe target
  wins over the recording-start one.
- If the target is not a text target, text goes to the clipboard and
  `VocalCodeError::Diverted` is returned (`:1953-1956`); the user sees a notice
  (`main.rs:5192-5194`).
- Typing vs paste: default is synthesised typing; `paste_insert` switches to the
  clipboard round trip (`inject.rs:1941-1951`; config `:427-435`).
- Focus is re-validated before each chunk and before the paste chord
  (`ensure_focus` `:1662-1665`, `:1710`, `:1749`).

Guards that prevent injection into the wrong window:

- `FocusToken` on Windows is `{process_id, foreground_window, runtime_id}`
  (`inject.rs:60-66`), built only when the UIA element's root HWND equals
  `GetForegroundWindow()` (`windows_focus_candidate_is_safe` `:184-192`,
  `:298-310`) and re-checked against a second `GetFocusedElement()` +
  `CompareElements` (`:311-324`).
- `validate_focus` refuses when the expected target is unknown or changed
  (`:146-159`).
- The UIA element is rejected if `CurrentIsPassword()` (`:534-536`) or if
  `ValuePattern.CurrentIsReadOnly()` (`:541-547`).
- `foreground.rs` supplies only the *application* identity (exe filename,
  lowercased, `:96-105`); it does not gate injection by itself.

---

## 3. Overlay / indicator window

### 3.1 Creation

`Overlay::new` (`overlay.rs:366-484`):

- tao `WindowBuilder` -- no custom Win32 class is registered by VocalCode; tao
  registers and owns the class. Properties: undecorated, transparent,
  always-on-top, not resizable, `with_focused(false)`, `with_focusable(false)`,
  `with_visible(false)`, initial logical size `172 x 34` (`:371-381`; sizes at
  `:55-62`).
- Windows post-creation native styling (`:384-403`):
  - `window.set_ignore_cursor_events(true)` (`:388`) -> `WS_EX_TRANSPARENT`
    (asserted in the hidden smoke, `overlay.rs:603`).
  - `GWL_EXSTYLE` rewritten to `(style & !WS_EX_APPWINDOW) | WS_EX_TOOLWINDOW |
    WS_EX_NOACTIVATE` (`:390-402`).
- WebView: `wry` with `include_str!("overlay.html")` (`:412-415`); on Windows the
  document is pre-marked `class="solid"` and the background colour is opaque
  `(13,12,11,255)` because WebView2 will not composite a transparent window
  (`:416-445`; comment `:405-409`).
- macOS: reparented into a non-activating `NSPanel` at screen-saver level with
  `setIgnoresMouseEvents: true` (`:447-458`, `:1067-1119`).

### 3.2 Positioning -- hard-coded, screen-anchored, not configurable

- `position_bottom_centre(window, frame)` (`overlay.rs:809-840`) puts it bottom
  centre of the monitor of the **foreground window** (Windows path) or the
  focused screen (macOS, `:989-1026`).
- Windows path `position_on_windows_work_area` (`:1131-1196`):
  `MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTONEAREST)`
  (`:1147-1153`), `GetMonitorInfoW(...).rcWork` (`:1158-1165`),
  `GetDpiForMonitor(MDT_EFFECTIVE_DPI)` (`:1167-1175`), then
  `set_outer_position(PhysicalPosition::new(x, y))` (`:1194`).
- The only "configurability" is avoidance: `keep_clear_of(ScreenRect)`
  (`:542-550`) and `origin_clear_of` (`:1233-1265`) lift the capsule above the
  desktop control bar.
- `place()` is called on show and on frame change only (`:554-560`, `:669`,
  `:723`). **There is no per-tick repositioning anywhere.**

Constants: `BOTTOM_MARGIN = 8.0` (`:64`), `WINDOW_W = 172.0` / `WINDOW_H = 34.0`
(`:55-58`), `MINI_WINDOW_W = 64.0` / `MINI_WINDOW_H = 22.0` (`:60-62`).

### 3.3 How the HTML receives state

Host -> page: `webview.evaluate_script(...)` with generated JS, one function per
concern:

- `vcPhase('idle'|'recording'|'transcribing'|'learning'|'notice')`
  (`overlay.rs:254-263`, `:266-273`; page `overlay.html:239-259`)
- `vcStyle('classic'|'mini')` (`:659-668`; page `:226-230`)
- `vcLang(pref)` (`:791-794`; page `:214-220`)
- `vcNotice(text)` -- JSON-encoded, never `innerHTML` (`:257-262`; page
  `:235-237`)
- `vcLevel(0.0..1.0)` -- deduplicated to 1/1000 (`overlay.rs:179-196`,
  `:693-697`)

Page -> host: a single bounded IPC message, display metrics only:

```rust
fn parse_scale_message(body: &str) -> Option<u32>   // overlay.rs:753-766
// {"type":"scale","ratio_milli":500..=16000}, body <= 128 bytes
```

wired at `overlay.rs:439-443`; the page sends it from `reportPageScale()`
(`overlay.html:262-271`). Everything else is `connect-src 'none'` under a strict
CSP (`overlay.html:5`).

`Phase` is packed into one `AtomicU64`: **3 bits for the phase, the rest a
revision counter** (`overlay.rs:108-138`):

```rust
((old & 7) != phase as u64).then(|| ((old & !7).wrapping_add(8)) | phase as u64)
```

So at most **8 phases**, and `Snapshot { revision, phase }` is what binds a
control click to one session (`overlay.rs:112-116`; used by
`dictation_control.rs:150-165` and `control_bar.rs:601-604`).

### 3.4 What it would take to (a) anchor to arbitrary coordinates at ~30 Hz

Feasible, but it is **not** a small extension of the current show/hide model:

1. **Visibility rule.** Today the window is hidden whenever the display is
   `Phase::Idle` (`Display::wants_window`, `overlay.rs:244-250`; hide at
   `:644-691`). A persistent dot needs a different rule (or a new `Style`).
2. **Reposition call.** Per tick,
   `SetWindowPos(hwnd, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE)` in
   **physical** pixels -- exactly what `control_bar.rs:792-814` already does.
   `tao`'s `set_outer_position` also works but runs tao's DPI conversion;
   `SetWindowPos` is cheaper and avoids a resize.
3. **Tick source.** `Overlay::tick` is only called from the UI loop
   (`webui.rs:1929`), whose default cadence is 180 ms
   (`webui.rs:1645-1646`: `ControlFlow::WaitUntil(now + 180ms)`). The only
   existing mechanism to shorten it is `ControlBar::next_wake`
   (`control_bar.rs:865-874`, applied at `webui.rs:1861-1863`). A dot needs the
   same: `next_wake -> now + 33ms` while cursor mode is on.
4. **Window size.** 172x34 (or 64x22 mini) is not a 10 px dot; the native window
   must be resized and re-rounded (`CreateRoundRectRgn` at `overlay.rs:737-748`,
   `control_bar.rs:804-811`), or a new window built.
5. **Position state.** `Overlay` has no field for a desired position; add one and
   compare before calling `SetWindowPos` (the control bar's `last_bounds`
   pattern, `control_bar.rs:457`, `:792`).

### 3.5 What it would take to (b) add new visual states

- `Phase` is `#[repr(u8)]` with 4 variants and a 3-bit pack
  (`overlay.rs:83-106`). `Armed`/`Blocked`/`Delivering` would fit numerically,
  **but** `Phase` is also the control-authority gate:
  `dictation_control::permitted` only allows Start in `Phase::Idle |
  Phase::Learning` (`dictation_control.rs:30-35`), and `control_bar`'s
  `active(phase)` / `surface_for` treat anything not Recording/Transcribing as
  idle (`control_bar.rs:106-108`, `:338-354`). Adding variants silently changes
  control-bar permissions unless every match arm is updated.
- `overlay.html` is a class-toggle machine (`body.on`, `body.notice`,
  `html.mini`, `html.solid`; `overlay.html:239-259`) with `display:none` toggles
  per element. Adding states is mechanical: add a class + CSS, add the case in
  `vcPhase`, and add the string in `phase_name` (`overlay.rs:266-273`) and
  `Display::script` (`:254-263`).
- A separate `CursorVisual` enum (not `Phase`) avoids the permission coupling
  entirely and is the safer design.

### 3.6 Which is the better base: `overlay.rs` or `control_bar.rs`?

**`control_bar.rs` is the better base for a cursor-following dot**, by a wide
margin:

| Capability | `overlay.rs` | `control_bar.rs` |
| --- | --- | --- |
| Per-frame physical positioning | no (`place()` on show only, `:554`) | yes (`SetWindowPos`, `:792-814`) |
| Frame cadence control | no | `next_wake()` (`:865-874`) |
| Arbitrary rectangular window sizing | only 2 presets (`:313-327`) | `Surface::size()` + per-frame size (`:257-273`, `:743`) |
| Rounded-region repaint | only on resize (`:726-749`) | every bounds change (`:804-811`) |
| Structured JSON state push | 4 ad-hoc `evaluate_script` calls | one `window.vocalcodeControl({...})` payload (`:825-842`) |
| Richer visual states | 4 phases | `data-phase`/`data-status`/`data-menu`/`data-expanded` + CSS vars (`control_bar.html:47-64`) |
| Structured IPC in | one message type | 5 message types with `deny_unknown_fields` (`:46-104`) |
| Non-activation hardening | `WS_EX_NOACTIVATE` | plus `SetWindowSubclass` returning `MA_NOACTIVATE`/`PA_NOACTIVATE` (`:420-440`) |
| Debug hidden-layout probe | `inspect_hidden_layout` (`:566-616`) | `inspect_hidden_layout` + `inspect_native_contract` + `inspect_composited_window` (`:896-1048`) |
| Click-through | yes (`WS_EX_TRANSPARENT`) | no (it is interactive by design) |

Caveat: the control bar is interactive (it must receive clicks), while the dot
must not. Take `control_bar.rs`'s geometry/motion/payload machinery and keep
`overlay.rs`'s `set_ignore_cursor_events(true)` + `WS_EX_TRANSPARENT`. The
control bar also owns the desktop-control *feature* (visibility rules, snooze,
hover, menu); do not copy that half.

---

## 4. Editable-target detection

### 4.1 What already exists

**Yes, for the *focused* element, in `inject.rs`.** `focused_text_target()`
(`inject.rs:527-557`) is precisely the conservative predicate the plan wants:

```rust
// windows, inject.rs:533-556 (condensed)
if element.CurrentIsPassword()?.as_bool() { return None }
if let Ok(value) = element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
    if value.CurrentIsReadOnly()?.as_bool() { return None }
    return Some(token)
}
let control_type = element.CurrentControlType()?;
if control_type == UIA_EditControlTypeId || control_type == UIA_DocumentControlTypeId {
    return Some(token)
}
element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId).ok().map(|_| token)
```

Supporting pieces:

- COM setup + `CoCreateInstance(CUIAutomation)` per call, MTA:
  `with_windows_automation` (`inject.rs:230-263`).
- `GetFocusedElement` + root-HWND ancestry walk with a 256-ancestor bound:
  `with_windows_focused_using` (`:266-331`), `windows_native_window_ancestor`
  (`:194-214`).
- Runtime-id capture: `copy_runtime_id` (`:338-375`).
- Value read-back with the same gates: `focused_editable_value` (`:640-681`).

macOS equivalent: `focused_text_target` (`inject.rs:559-614`) checks
`AXSubrole == AXSecureTextField` and `AXRole in {AXTextField, AXTextArea,
AXComboBox}`, with a 0.25 s messaging timeout (`:606`).

### 4.2 The gaps for a *point*-based probe

1. `focused_text_target` is **private** (`fn`, `inject.rs:527`) and asks
   `GetFocusedElement`, not "what is under this point". UIA does offer
   `IUIAutomation::ElementFromPoint`, but nothing in the tree calls it.
2. There is **no window-class heuristic anywhere** (no `GetClassName` outside
   the Raw Input window registration, `input_windows.rs:213`).
3. There is **no UI Automation call outside `inject.rs`**: `foreground.rs` is
   plain Win32 (`GetForegroundWindow`, `GetWindowTextW`,
   `QueryFullProcessImageNameW`; `foreground.rs:31-106`), and `hid_input.rs` is
   HID descriptor decoding (`hid_input.rs:1-6`) with nothing about text targets.
4. `focused_text_target()` has **no Windows timeout**. macOS sets
   `AXUIElementSetMessagingTimeout(0.20/0.25)` (`foreground.rs:129`,
   `inject.rs:606`); Windows UIA has no equivalent bound in this code, so a hung
   provider blocks the caller. Calling that at 30 Hz from the UI thread is a
   hang risk (see section 8b/8c).
5. `CoCreateInstance` per call (`inject.rs:260-261`) costs on the order of a
   millisecond; at 30 Hz that is measurable. Cache the `IUIAutomation` in a
   thread-local (or a long-lived worker) instead.

### 4.3 Identity checks that exist

| Check | Where | What it is |
| --- | --- | --- |
| foreground app id | `foreground.rs:27-29`, `:96-105` | exe filename, lowercased (`app_id`), plus `process_id`, `display_name`, bounded `window_title` (512 units, `:17`) |
| foreground HWND identity | `inject.rs:277-286`, `:311-324` | `GetForegroundWindow()` compared before and after the UIA walk |
| focused element identity | `inject.rs:62-66`, `:296-329` | `(process_id, foreground_window, runtime_id)` |
| password/read-only | `inject.rs:534-547` | UIA properties |
| app profile matching | `workflows.rs:105-122` | exact exe filename / bundle id, never titles |

**No hash of the foreground window exists**; the plan's phrase "hash of
foreground window" has no counterpart in the code.

Useful accident: `foreground_application()` returns `None` when
`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` fails or the path is empty
(`foreground.rs:73-95`). A higher-integrity (elevated) foreground process fails
that call from a non-elevated VocalCode, so `foreground_app_id() == None` is a
usable "cannot identify, likely elevated" signal for the `Blocked` state.

### 4.4 Is the `windows` crate a dependency, with UIAutomation features?

Yes, but **only in `vocalcode-platform`**:

```toml
# vocalcode-platform/Cargo.toml:41-52
windows = { version = "0.61.3", features = [
    "Win32_Foundation", "Win32_Media_Audio", "Win32_System_Com",
    "Win32_System_Com_StructuredStorage", "Win32_System_Variant",
    "Win32_System_DataExchange", "Win32_System_Memory", "Win32_System_Ole",
    "Win32_UI_Accessibility", "Win32_UI_WindowsAndMessaging",
] }
```

- `Win32_UI_Accessibility` is the feature that carries `IUIAutomation`,
  `CUIAutomation`, `UIA_*Id`, `IUIAutomationElement`,
  `IUIAutomationTextPattern`, `IUIAutomationValuePattern` (used at
  `inject.rs:528-531`, `:642-645`).
- `windows-sys` (0.61.2) is present in both crates, but **neither enables
  `Win32_UI_Accessibility`** on it (`vocalcode-platform/Cargo.toml:53-68`,
  `vocalcode-app/Cargo.toml:77-99`), and the COM interfaces used here
  (`IUIAutomation`, `IUIAutomationElement`, ...) come from the `windows` crate
  projection, not `windows-sys`.
- **`vocalcode-app` has no `windows` dependency at all** -- only `windows-sys`
  (`vocalcode-app/Cargo.toml:73-99`). So any UIA code must live in
  `vocalcode-platform` (which is where the plan puts `pointer.rs`) or the app
  must add the dependency. Adding it to the app is the wrong move: it duplicates
  a COM/`IUIAutomation` stack the platform crate already owns.
- `vocalcode-app` **does** already enable `Win32_UI_HiDpi` and `Win32_UI_Shell`
  (`vocalcode-app/Cargo.toml:97-98`), and `vocalcode-platform` does **not**
  enable `Win32_UI_HiDpi`. `GetCursorPos`/`MonitorFromPoint` live in
  `Win32_UI_WindowsAndMessaging` (already on in both).

---

## 5. Config and settings

### 5.1 Serialization and version

- Format: **TOML** on disk (`vocalcode.toml`), JSON across the WebView IPC
  (`config_snapshot_for_page`, `webui.rs:2191-2225`).
- `Config` is `#[derive(Serialize, Deserialize)] #[serde(default, remote =
  "Self")]` (`config.rs:392-394`); the manual impls exist so that *omitted* keys
  from an older release get that release's implied value (`config.rs:387-391`,
  `:553-590`, `restore_pre_v5_defaults` `:612-630`).
- Current version: **`CONFIG_VERSION: u32 = 5`** (`config.rs:545-547`).
- `config_version` field: `#[serde(default)] pub config_version: u32`
  (`config.rs:521-524`) -- absent means 0.
- Forward-compat rule, enforced in two places: a document with
  `config_version > CONFIG_VERSION` is **refused** rather than migrated --
  `accept_supported_config` (`main.rs:1109-1126`) and
  `reject_future_config_version` (`main.rs:1132-1153`).
  `persist_config_if_current` refuses to overwrite one (`main.rs:1236-1260`).
- `BUILDING.md:74-79` explains the practical consequence: once a build writes v5,
  paid VocalCode 1.2.1 and earlier refuse to start with that folder.

### 5.2 How to add a section without breaking older builds

The pattern is documented in the source itself, on `WritingConfig`:

> "Kept as its own `[writing]` table: an older build that does not know it
> ignores the table instead of refusing the whole settings file, which keeps an
> update reversible." -- `config.rs:14-19`

And there is a test proving it: `writing_is_opt_in_reversible_and_resolves_per_app`
deserializes the full v5 document into `struct Older { language: String }`
(`config.rs:1069-1074`) -- i.e. `Config` has **no** `deny_unknown_fields`, so
unknown tables are ignored by older readers.

Therefore, for cursor input:

1. **Do not bump `CONFIG_VERSION`.** A bump to 6 makes *today's* v5 builds
   refuse the user's folder -- the exact regression `BUILDING.md:74-79` warns
   about, and the reason `accept_supported_config` exists. A purely additive
   field needs no bump: it changes no existing key's meaning
   (`config.rs:545-546`: "Bump when an existing key changes meaning").
2. Add a nested `#[serde(default)]` struct (copy `WritingConfig` verbatim,
   `config.rs:20-51`), and put it **last** in `Config` so it serializes as a
   trailing table (`config.rs:535-539`).
3. Add `#[serde(default)]` on the field so pre-existing files read it as the
   `Default` impl.
4. Validate in `Config::validate_bounds` (`config.rs:657-723`), using
   `bounded_config_text` (`config.rs:922`) for any string enum, exactly like
   `overlay_style` (`config.rs:678-682`) and `desktop_control_edge`
   (`config.rs:683-688`).
5. Add a migration branch only if a *default* must differ for old files; "off by
   default" needs none (`Config::migrate`, `config.rs:732` onward).

### 5.3 The exact struct/serde pattern to copy

```rust
// vocalcode-core/src/config.rs:20-51 (shape, condensed)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WritingConfig {
    pub commands: bool,
    pub style: String,
    pub app_styles: std::collections::BTreeMap<String, String>,
}

impl Default for WritingConfig {
    fn default() -> Self {
        Self {
            commands: false,
            style: "formal".to_string(),
            app_styles: Default::default(),
        }
    }
}

impl WritingConfig {
    pub fn validate(&self) -> Result<(), String> {
        if crate::writing::Style::parse(&self.style).is_none() {
            return Err("writing style must be formal, casual or very_casual".into());
        }
        Ok(())
    }
}
```

and on `Config`:

```rust
/// Last, so it serialises as the trailing `[writing]` table.
#[serde(default)]
pub writing: WritingConfig,        // config.rs:535-539
```

with `writing: WritingConfig::default()` in `impl Default for Config`
(`config.rs:1031`) and `self.writing.validate()?` in `validate_bounds`
(`config.rs:698`).

### 5.4 Settings UI: where and how

Host: `webui.rs` (tao loop, `run()` at `:1342`; page loaded with
`include_str!("webui.html")` at `:1517`). Page: `webui.html`.

The complete pattern for **one boolean toggle plus two dropdowns**, following
the existing `desktop_control` trio:

**(1) Config field** -- `config.rs` (as above).

**(2) Parse + validate on save** -- `webui.rs:3990-4004`:

```rust
if let Some(value) = v.get("desktop_control").and_then(Value::as_bool) {
    next.desktop_control = value;
}
if let Some(value) = v.get("desktop_control_edge").and_then(Value::as_str) {
    if !matches!(value, "bottom" | "left" | "right") {
        return Err("unknown desktop-control edge".into());
    }
    next.desktop_control_edge = value.to_string();
}
```

plus a `matches!` check alongside `overlay_style` at `webui.rs:4068-4073`.

**(3) Initial payload** -- the big `json!` at `webui.rs:4167-4246`, with the
Windows-only extras appended afterwards at `:4237-4244`:

```rust
initial["desktop_control"] = serde_json::json!(c.desktop_control);
initial["desktop_control_edge"] = serde_json::json!(c.desktop_control_edge);
initial["desktop_control_available"] = serde_json::json!(cfg!(windows));
```

**(4) Status/result snapshot** -- `config_snapshot_for_page`,
`webui.rs:2191-2225` (so a rollback reconciles the new keys too).

**(5) Markup** -- `webui.html:1083-1084`:

```html
<div class="row" id="desktopControlRow" hidden>
  <div class="k">Desktop control bar <span class="state-badge">Beta</span><small>...</small></div>
  <button type="button" class="tg" id="desktopControl" aria-label="Desktop control bar" aria-pressed="false"></button>
</div>
<div class="row" id="desktopControlEdgeRow" hidden>
  <div class="k">Control bar position<small>...</small></div>
  <select id="desktopControlEdge" aria-label="Control bar position">
    <option value="bottom">Bottom</option><option value="left">Left</option><option value="right">Right</option>
  </select>
</div>
```

**(6) Behaviour** -- `webui.html:5026` and `:5177-5178`:

```js
function bindToggle(id,field,changed){const el=document.getElementById(id);
  el.onclick=()=>{cfg[field]=!cfg[field];el.classList.toggle("on",cfg[field]);
  el.setAttribute("aria-pressed",String(!!cfg[field]));save();if(changed)changed(!!cfg[field]);};}

bindToggle("desktopControl","desktop_control",function(){renderConfigState();});
document.getElementById("desktopControlEdge").onchange=function(event){cfg.desktop_control_edge=event.target.value;save();};
```

**(7) Render back into the controls** -- `webui.html:5726-5731`:

```js
[["liveCaption","live_caption"],["noiseFilter","noise_filter"],["desktopControl","desktop_control"], ...]
  .forEach(function(pair){var el=document.getElementById(pair[0]);if(el){var on=!!cfg[pair[1]];
    el.classList.toggle("on",on);el.setAttribute("aria-pressed",String(on));}});
document.getElementById("desktopControlRow").hidden=!cfg.desktop_control_available;
document.getElementById("desktopControlEdgeRow").hidden=!cfg.desktop_control_available||!cfg.desktop_control;
document.getElementById("desktopControlEdge").value=cfg.desktop_control_edge||"bottom";
```

**(8) Saving** is debounced 250 ms and revision-checked by the host: `save()` at
`webui.html:5791-5795`, `flushConfigSave` at `:5777-5790`, host handler in
`webui.rs` (`"save"`/`"load"` ops).

**(9) Translations**: `webui.html` carries a large en/zh/es/fr/de dictionary
keyed on the **English source string** (e.g. `:1711`, `:2224`); new UI text
needs entries in all five or it silently stays English.

**Note:** the plan says the settings UI lives in "webui.rs / webui.html" --
correct, but `webui.rs` is 11 757 lines of host + tests; the actual markup is
`webui.html:1073-1132` and the bindings are around `webui.html:5022-5060` and
`:5726-5752`.

---

## 6. Windows-specific build facts

### 6.1 `windows` / `windows-sys` features

- `vocalcode-platform` `windows` 0.61.3: `Win32_Foundation`,
  `Win32_Media_Audio`, `Win32_System_Com`,
  `Win32_System_Com_StructuredStorage`, `Win32_System_Variant`,
  `Win32_System_DataExchange`, `Win32_System_Memory`, `Win32_System_Ole`,
  **`Win32_UI_Accessibility`**, `Win32_UI_WindowsAndMessaging`
  (`vocalcode-platform/Cargo.toml:41-52`).
- `vocalcode-platform` `windows-sys` 0.61.2: `Win32_Foundation`,
  `Win32_Media_Audio`, `Win32_Devices_HumanInterfaceDevice`, `Win32_Graphics_Gdi`,
  `Win32_System_DataExchange`, `Win32_System_LibraryLoader`,
  `Win32_System_Memory`, `Win32_System_SystemInformation`,
  `Win32_System_Threading`, `Win32_UI_Input`,
  `Win32_UI_Input_KeyboardAndMouse`, `Win32_UI_Input_XboxController`,
  `Win32_UI_WindowsAndMessaging` (`:53-68`).
  **No `Win32_UI_HiDpi` here** -- the overlay/control-bar DPI calls come from the
  app crate instead.
- `vocalcode-app` `windows-sys` 0.61.2: `Win32_Foundation`,
  `Win32_Globalization`, `Win32_Graphics_Gdi`, `Win32_Security`,
  `Win32_Security_Authorization`, `Win32_Security_Cryptography`,
  `Win32_Storage_FileSystem`, `Win32_System_IO`, `Win32_System_JobObjects`,
  `Win32_System_Memory`, `Win32_System_Com`, `Win32_System_Pipes`,
  `Win32_System_ProcessStatus`, `Win32_System_Registry`,
  `Win32_System_SystemInformation`, `Win32_System_Threading`,
  `Win32_UI_WindowsAndMessaging`, **`Win32_UI_HiDpi`**, `Win32_UI_Shell`
  (`vocalcode-app/Cargo.toml:77-99`).
- `vocalcode-meeting` also has its own `windows-sys` block
  (`vocalcode-meeting/Cargo.toml:32-33`).

### 6.2 Native libraries downloaded at build time

`sherpa-onnx = { ..., features = ["shared"] }`
(`vocalcode-platform/Cargo.toml:19`) -> `sherpa-onnx-sys`'s `shared` feature ->
`download-binaries` (`vendor/sherpa-onnx-sys/Cargo.toml:57-69`).

- `vendor/sherpa-onnx-sys/build.rs` extracts a pinned archive into `OUT_DIR`
  under a budget/marker policy (`build.rs:300-400`), adds link search paths
  (`:414-427`), and links the extracted libraries through `link_lib`
  (`:36-40`).
- Because `is_dynamic`, it **copies** every runtime asset to three places
  (`build.rs:542-579`):
  - `target/<profile>/` (next to the exe)
  - `target/<profile>/deps/`
  - `target/<profile>/examples/`
  using `copy_file` -> `publish_file_replacing`, which replaces on every build
  (`build.rs:52-64`).
- The Windows DLL set is named in CI and packaging: `onnxruntime.dll`,
  `onnxruntime_providers_shared.dll`, `sherpa-onnx-c-api.dll`,
  `sherpa-onnx-cxx-api.dll`
  (`.github/workflows/community-ci.yml:151`, `packaging/community/windows.ps1:54`).
- macOS equivalents: `libonnxruntime.dylib`, `libsherpa-onnx-c-api.dylib`,
  `libsherpa-onnx-cxx-api.dylib` (`packaging/community/release.py:79`), with
  rpaths set in `.cargo/config.toml:41-61`.
- Build-time env: `LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'`
  (`.cargo/config.toml:17`); `SHERPA_LIB_PATH` / `SHERPA_LIB_PROFILE` /
  `SHERPA_STATIC_CRT` are honoured by `build.rs:433-457`.

**So yes: the build produces a non-portable exe with four DLLs beside it.**
`BUILDING.md:28-31` says so explicitly.

### 6.3 `--build-info`

Implemented inline in `main`, before any logging/device/model/data work
(`main.rs:5921-5942`):

```rust
if args.len() == 2 && args[1] == "--build-info" {
    println!("{}", serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "edition": if community::ENABLED { "community" } else { "legacy" },
        "identity": if community::RELEASE_IDENTITY { "release" } else { "development" },
        "data_directory": community::DATA_DIR_NAME,
        "update_manifest": UPDATE_MANIFEST_URL,
    }));
    return Ok(());
}
```

`identity` is compile-time (`VOCALCODE_RELEASE_IDENTITY`, `BUILDING.md:66-70`).

---

## 7. Testing patterns

### 7.1 How hidden smoke examples are written

The five UI-surface smoke examples relevant here are `vocalcode-app/examples/*smoke*.rs` binaries, each a **standalone binary**
that pulls in only the modules it needs via `#[path]`:

- `indicator_hidden_smoke.rs` (132) -- `#[path = "../src/notice.rs"] mod notice;`
  and `#[path = "../src/overlay.rs"] mod overlay;` (`:3-10`), then
  `overlay::Overlay::new(&event_loop, temp_dir)` (`:26`) and
  `indicator.inspect_hidden_layout(...)` per case (`:108-111`).
- `control_bar_hidden_smoke.rs` (186) -- includes `control_bar.rs`,
  `dictation_control.rs`, `overlay.rs` (`:3-14`), builds the bar (`:31-33`), and
  drives `inspect_hidden_layout` (`:165-169`) plus `inspect_native_contract`
  (`:87`).
- `control_bar_ipc_smoke.rs` (371) -- real IPC: connects a `Bridge` to a real
  `trigger_event_channel` (`:247-249`) and asserts which `TriggerEvent` a click
  produced (`:299-302`).
- `control_bar_composition_smoke.rs` (152) -- the only **visible** probe, gated on
  an explicit `--visible-owned-backdrop` flag (`:24-27`); creates an owned
  `STATIC` backdrop window and samples pixels with `GetPixel`/`BitBlt`
  (`:87-131`); uses `bar.inspect_composited_window(x, y)` (`:90`, impl
  `control_bar.rs:1032-1048`).
- `webui_hidden_smoke.rs` (157) -- builds its own hidden tao window + wry
  webview around `include_str!("../src/webui.html")` (`:26-42`) and calls
  `window.vocalcodeInit(config)` with a synthetic JSON fixture (`:87-100`).

Every one is gated `#[cfg(all(windows, not(test), debug_assertions))]` with an
empty `main` otherwise (e.g. `indicator_hidden_smoke.rs:3`, `:132-133`), and each
creates its own temp WebView profile with `pid` + `jiff::Timestamp` because it
cannot use the test-only `TempDir` (`indicator_hidden_smoke.rs:18-23`).

Run commands are documented in `BUILDING.md:106-112`.

### 7.2 Is there a test helper for a hidden WebView window?

**No.** `vocalcode-app/src/test_support.rs` (255) contains only `TempDir`
(`:24-84`), `assert_directory_released` (`:92-104`), `e2e_fixture` (`:129-133`)
and `copy_tree` (`:136-147`). It is also `#[cfg(test)]`-only in `main.rs:32-33`,
so examples cannot even import it.

The reusable pieces are the `#[cfg(debug_assertions)]` methods on the production
types:

- `Overlay::inspect_hidden_layout(mini, phase, notice, language, on_result)`
  (`overlay.rs:564-616`) -- returns native flags (`no_activate`,
  `click_through`, `not_in_taskbar`, `rounded`) plus page geometry.
- `ControlBar::inspect_native_contract()` (`control_bar.rs:896-918`),
  `inspect_hidden_layout` (`:920-1001`), `inspect_hidden_zoom` (`:1005-1011`),
  `inspect_hidden_sample` (`:1015-1027`), `inspect_composited_window`
  (`:1032-1048`), `inspect_hidden_interaction` (`:1050-1115`).

### 7.3 Existing mouse simulation / window-position assertions

- **No mouse simulation anywhere in the product tests.** The only synthetic
  input test is keyboard: `vocalcode-platform/tests/keyboard_capture_live.rs`
  uses `rdev::simulate` for `KeyPress/KeyRelease` and is `#[ignore]`d
  (`:41-49`). `vendor/rdev/tests/{grab,listen_and_simulate}.rs` are upstream
  rdev tests, not part of this workspace's QA story.
- **No test asserts a real window's screen position.** Positioning is tested
  purely as arithmetic on pure functions:
  - `bottom_centre_in_work_area` -- `overlay.rs:1582-1614`
    (`windows_indicator_stays_above_a_bottom_taskbar`,
    `..._uses_the_remaining_area_beside_a_side_taskbar`,
    `..._handles_negative_monitor_coordinates`,
    `..._clamps_pathological_work_areas_without_overflow`)
  - `origin_clear_of` -- exercised from `control_bar.rs:1660-1760`
  - `keep_clear_area` / `bounds` -- `control_bar.rs:360-418`, tests around
    `:1650-1760`
- The composition smoke *does* read real pixels of a real window at real
  coordinates (`control_bar_composition_smoke.rs:96-130`) -- the only precedent
  for asserting where something actually drew.

### 7.4 What a `cursor_dot_hidden_smoke` example would need

1. `#[path = "../src/cursor_dot.rs"] mod cursor_dot;` plus whatever it depends
   on (`overlay.rs` for `ScreenRect`/`Snapshot`, `dictation_control.rs` if it
   uses `Bridge`). `#[path]` modules resolve `crate::` against the *example*
   crate, so any `crate::foo` reference inside the included module must also be
   included -- this is why `control_bar_hidden_smoke.rs:3-14` lists three
   modules.
2. A hidden tao window + wry webview (copy `webui_hidden_smoke.rs:26-42`), or a
   new `inspect_hidden_*` hook on the dot type.
3. A debug-only method that, without showing the window, reports:
   `IsWindowVisible == 0`, `WS_EX_NOACTIVATE`, `WS_EX_TRANSPARENT`,
   `WS_EX_TOOLWINDOW`, `GetForegroundWindow()` unchanged, and the *computed*
   position for a list of synthetic cursor points across monitors/DPI.
4. Cases: a grid of points including negative coordinates (left/above primary),
   a 100 %/150 %/200 % monitor, points on the taskbar edge, and the idle/armed
   transition. Because the real cursor cannot be moved in a hidden test, feed
   positions through an injected `Fn() -> (i32, i32)` (a "cursor source" trait),
   so the arithmetic is testable exactly like `bottom_centre_in_work_area`.
5. If it must prove the window really moved, use `GetWindowRect` /
   `GetWindowPlacement` rather than a screenshot -- the composition smoke shows
   how to do a visible variant behind an explicit flag
   (`control_bar_composition_smoke.rs:24-27`).

---

## 8. Open questions from the plan

### (a) Double-click without stealing it from the target app

**Feasible to observe, impossible to hide from the host app.** Evidence:

- The LL hook sees `WM_LBUTTONDOWN`/`UP` with `pt` and `time` in
  `MSLLHOOKSTRUCT` (`hook_windows.rs:188`), and passes them through unless the
  handler returns `true` (`:209-215`). Clicks can therefore be counted *without*
  consuming them.
- But `WM_LBUTTONDBLCLK` is generated by the target window's own class
  (`CS_DBLCLKS`); the LL hook never receives it, and by the time the second
  `WM_LBUTTONDOWN` arrives the first click has already been delivered. A
  double-click on a word in an editor **will** select the word. There is no
  non-suppressing way to prevent that.
- Suppressing the first click for up to `GetDoubleClickTime()` (default 500 ms)
  and replaying it if no second click arrives is the only way to "own" the
  gesture; it delays every click in the system and is exactly the "swallow a
  click" the plan forbids (section 3.1 of the plan).
- Nothing in the tree reads `GetDoubleClickTime`, `SM_CXDOUBLECLK`, or tracks
  click times (repo-wide search: no matches).
- The engine's existing `DOUBLE_TAP_WINDOW` (`engine.rs:193`) is a *keyboard
  tap* concept with a different threshold (release-to-press, 500 ms) and is
  gated on `double_tap_lock && talk_mode == "hold"` (`engine.rs:899-905`).

**Verdict:** the plan's "dot-only rule" (double-click *on the dot*, not anywhere
on the field) is not merely "safer" -- it is the only version that does not
interfere with the host app, because the dot is `WS_EX_TRANSPARENT` and therefore
never receives clicks either. The dot-only rule still needs the global hook to
attribute a click to the dot's rectangle (hit-test the dot's own screen rect
against the click point), and the host app will still see the double-click if the
dot overlaps a text field. Recommendation: require the click point to be inside
the dot's rect **and** the target under that exact point to be non-editable, or
accept that editors will word-select.

### (b) Pointer position cheaply at ~30 Hz

**Yes, but not on the thread the plan names.** There is no "platform timer
thread" in this codebase:

| Thread | Cadence | Evidence |
| --- | --- | --- |
| UI/tao loop | 180 ms idle, shortened by `ControlBar::next_wake` | `webui.rs:1645-1646`, `:1861-1863` |
| engine thread | event-driven, 80 ms while recording / 450 ms maintenance | `main.rs:2806`, `:4974-4977` |
| hook thread | Win32 message loop, 1 s heartbeat `WM_TIMER` | `hook_windows.rs:292-297`, `:362-387` |
| audio callback | device rate (fast), read by the UI | `main.rs:6305`, `overlay.rs:624` |

Three viable sources, best first:

1. **Capture the position inside `mouse_proc` itself.** Every mouse event already
   arrives on the hook thread; store `(x, y)` in a `static AtomicU64`
   (`(x as u32 as u64) << 32 | y as u32 as u64`) before any lock, for
   `WM_MOUSEMOVE` *and* button edges. Cost: two instructions per event, no mutex,
   no dispatch, no extra thread, and it stays under the 20 ms slow-callback
   warning (`hook_windows.rs:61`, `:124-132`). The hook is always installed while
   the app runs (`hotkey.rs:1059`).
   Caveat: the source-contract test asserts the callback region contains no
   `log::` (`hotkey.rs:2055-2062`), so keep it allocation- and log-free.
2. **`GetCursorPos` on a dedicated 33 ms thread.** Cheap (microseconds), but a
   new thread, it samples rather than reacts, and it cannot report clicks.
3. **Shorten the UI loop to 33 ms while cursor mode is on.** Simplest to wire
   (`ControlFlow::WaitUntil` at `webui.rs:1646`), but it wakes the whole UI loop
   (tray tooltip diffing, config locks, meeting checks) 30x/s, which conflicts
   with M4's "no measurable idle CPU cost". If used, gate it on
   `cursor_input && (pointer moved || state != Idle)`.

Positions from both `MSLLHOOKSTRUCT.pt` and `GetCursorPos` are **physical screen
coordinates**; the existing Windows positioning code already works in physical
coordinates deliberately (`overlay.rs:1126-1130`, `:1180-1194`).

### (c) Per-monitor DPI for a tiny window

**Feasible with the existing machinery**, with one gotcha:

- The codebase already does the right thing on Windows: convert the monitor's
  effective DPI to a scale (`GetDpiForMonitor(MDT_EFFECTIVE_DPI)`,
  `overlay.rs:1167-1175`; same at `control_bar.rs:717-730`), keep the whole
  calculation in physical desktop pixels (`overlay.rs:1126-1130`), and place with
  `set_outer_position(PhysicalPosition)` (`overlay.rs:1194`) or `SetWindowPos`
  (`control_bar.rs:795-803`).
- Text/UI scale is a **separate** factor from monitor DPI on Windows: WebView2's
  `devicePixelRatio` includes the accessibility text scale, so both products
  divide it back out (`windows_text_scale`, `overlay.rs:768-771`;
  `additional_text_scale`, `control_bar.rs:1118-1120`, tests `:1555-1558`). A dot
  with no text does not need this, which is one reason a dot is easier than a
  capsule.
- Gotcha: after moving a window to a monitor with a different DPI, Windows sends
  `WM_DPICHANGED` and tao resizes the window to the suggested rect. A fixed-size
  dot must re-assert its size every frame (as `control_bar.rs:792-814` does) or
  it will grow/shrink on monitor crossings.
- No app-level DPI-awareness call exists in the repo (no
  `SetProcessDpiAwareness*` anywhere); tao 0.35 is expected to set
  per-monitor-v2 itself. This is the one item that cannot be verified from the
  tree and should be confirmed at runtime with a two-monitor test (M1's exit
  criterion already requires it).
- DPI is also why a 10 px dot should be specified in **logical** px and
  converted with the *monitor's* scale at paint time, not the window's current
  scale (which lags one move) -- see the comment at `overlay.rs:1176-1185`.

### (d) Hiding over elevated windows / secure desktop / fullscreen

- **Elevated windows:** the dot can be *shown* (a topmost `WS_EX_NOACTIVATE`
  window is not blocked by UIPI), but *arming* must fail closed:
  - `foreground_application()` returns `None` when `OpenProcess` fails or the
    image path is empty (`foreground.rs:73-95`) -- the normal outcome for a
    higher-integrity foreground process from a non-elevated app. Use
    `foreground_app_id().is_none()` as a `Blocked` signal.
  - UIA from a non-elevated process cannot read an elevated provider, so
    `focused_text_target()`-style checks return `None` (`inject.rs:533-556`).
  - Injection is blocked by the same UIPI boundary, so refusing to arm is
    consistent with delivery.
  - Nothing in the tree currently detects elevation explicitly (no
    `TokenElevation`, no `IsProcessElevated`).
- **Secure desktop / lock screen:** low-level hooks are per desktop; while
  Winlogon's secure desktop is active the default desktop is not receiving
  input, so the hook is silent and the dot is not visible. Nothing in the code
  detects it. The codebase acknowledges the concept only as a cause of a lost
  key-up ("secure desktop, lock screen", `engine.rs:656-661`), and handles it by
  refusing to inject (`force_stop_to_history`). A cursor feature should treat
  "no input for N seconds + `GetForegroundWindow() == NULL`" as `Blocked`; no
  existing helper does this.
- **Fullscreen apps:** already solved and reusable --
  `foreground_is_fullscreen(hwnd, monitor)` (`control_bar.rs:1122-1140`)
  excludes our own pid and minimized windows, and requires the foreground rect
  to cover the monitor; used to hide the idle bar at `control_bar.rs:708-714`.
- **Exclusive-fullscreen / raw-input games:** out of scope for detection; the
  fullscreen check above is the only lever, and the plan's "hide, do not
  hook-fight" is right.

---

## 9. File inventory and line counts (largest relevant files)

| File | Lines | Notes |
| --- | ---: | --- |
| `vocalcode-app/src/webui.rs` | 11 757 | settings host + tao loop + tests |
| `vocalcode-app/src/main.rs` | 9 594 | engine thread, event loop, config IO |
| `vocalcode-app/src/webui.html` | 7 264 | settings page (5 languages inline) |
| `vocalcode-platform/src/hotkey.rs` | 3 956 | hooks, capture, trigger matching |
| `vocalcode-core/src/engine.rs` | 3 387 | dictation state machine |
| `vocalcode-platform/src/inject.rs` | 3 379 | UIA focus + typing/paste |
| `vocalcode-core/src/config.rs` | 1 833 | TOML schema + migrations |
| `vocalcode-app/src/control_bar.rs` | 1 789 | interactive capsule (Windows) |
| `vocalcode-app/src/overlay.rs` | 1 616 | passive indicator |
| `vocalcode-core/src/trigger_bus.rs` | 658 | bounded event queue |
| `vocalcode-app/src/workflows.rs` | 472 | per-app prefs (not lifecycle) |
| `vocalcode-app/src/dictation_control.rs` | 430 | UI to engine mailbox |
| `vocalcode-platform/src/hook_windows.rs` | 411 | WH_KEYBOARD_LL + WH_MOUSE_LL |
| `vocalcode-platform/src/hid_input.rs` | 402 | HID descriptor decoding (macOS) |
| `vocalcode-app/src/overlay.html` | 274 | indicator document |
| `vocalcode-app/src/test_support.rs` | 255 | TempDir only (no WebView helper) |
| `vocalcode-platform/src/foreground.rs` | 197 | app identity only |
| `vocalcode-app/examples/control_bar_ipc_smoke.rs` | 371 | real IPC probe |
| `vocalcode-app/examples/control_bar_hidden_smoke.rs` | 186 | hidden layout probe |
| `vocalcode-app/examples/webui_hidden_smoke.rs` | 157 | settings page probe |
| `vocalcode-app/examples/control_bar_composition_smoke.rs` | 152 | visible pixel probe |
| `vocalcode-app/examples/indicator_hidden_smoke.rs` | 132 | indicator probe |
| `vocalcode-app/src/control_bar.html` | 118 | capsule document |
| `vendor/rdev/src/windows/common.rs` | 127 | `MSLLHOOKSTRUCT` decode |
| `vendor/rdev/src/windows/grab.rs` | 74 | suppress-or-pass grab |

---

## 10. TODOs / notes about mouse, cursor, overlay positioning, pointer

There are **no** product TODOs about cursor/pointer positioning. The only
in-tree mentions:

- `vendor/sherpa-onnx-sys/build.rs:581` -- `TODO: add rpath for Android and iOS`
  (unrelated).
- `vendor/rdev/src/linux/grab.rs:21` -- `TODO The x, y coordinates are currently
  wrong !! Is there mouse acceleration` (Linux only; not used on Windows).
- `docs/CURSOR-INPUT-METHOD.md` -- the plan itself; it is the only document in
  the repo that mentions cursor/pointer/editable targets.
- `BUILDING.md:114` -- "These exercise layout, native window flags and
  programmatic IPC, **not real mouse input**...", an explicit warning about the
  limits of the hidden smoke tests.
- `overlay.rs:2-33` and `:552-560`, `:796-808` -- the only long-form notes on
  overlay positioning; they document *why* it is bottom-centre of the foreground
  monitor and why `current_monitor()` was the bug.
- `control_bar.rs:381-382` -- "Preserve the tiny peek control under the pointer,
  NOT a recording button" (the closest thing to a pointer-position note).

---

## 11. Recommended seams (concrete, in dependency order)

1. **Position capture** -- `vocalcode-platform/src/hook_windows.rs:184-216`:
   store `(pt.x, pt.y)` from `MSLLHOOKSTRUCT` into a new
   `static MOUSE_POSITION: AtomicU64` for `WM_MOUSEMOVE` and button edges,
   before `dispatch`. Expose `pub fn cursor_position() -> Option<(i32, i32)>`
   from `vocalcode-platform`. No lock, no log, no dispatch (keeps
   `hotkey.rs:2055-2062` green).
2. **Click edges with position and time** -- same proc: on `WM_LBUTTONDOWN`/`UP`
   store `(pt, time)` too (the fields are already there: `MSLLHOOKSTRUCT` at
   `hook_windows.rs:188`), so a double-click policy needs no second hook and no
   rdev change. Do **not** consume.
3. **Point-based editable probe** -- `vocalcode-platform/src/inject.rs`: make
   the `focused_text_target`-equivalent logic reusable (e.g.
   `pub fn text_target_at_point(x: i32, y: i32) -> Option<TextTarget>`), using
   `IUIAutomation::ElementFromPoint` plus the exact gates at `:534-556`. Reuse
   `with_windows_automation` (`:230-263`) but cache the `IUIAutomation` in a
   thread-local; keep it off the hook thread and off the UI thread (worker +
   timeout + generation token, like `CorrectionMonitor`).
4. **Cursor controller** -- new `vocalcode-app/src/cursor_input.rs` owning: its
   own `Bridge` (`Bridge::default()` + `connect(tx)`), a `CursorVisual` enum
   (not `Phase`), debounce/hysteresis, and the double-click policy. It must be
   consulted in `main.rs:4990-5009` next to `status.dictation_control.take(...)`.
5. **Dot surface** -- copy `control_bar.rs`'s geometry/motion/payload machinery
   (`Bounds`, `SetWindowPos`, `next_wake`, JSON payload) into a small
   `cursor_dot.rs`, but take `overlay.rs:388-402`'s click-through styling
   (`set_ignore_cursor_events(true)` + `WS_EX_NOACTIVATE|WS_EX_TOOLWINDOW`).
   Position with `SetWindowPos(..., SWP_NOSIZE|SWP_NOACTIVATE)` in physical px.
6. **Cadence** -- add a `next_wake` for the dot and apply it at
   `webui.rs:1861-1863`, next to the control bar's; only while cursor mode is
   enabled and the pointer has moved.
7. **Config** -- `[cursor_input]` table (do **not** bump `CONFIG_VERSION`), plus
   the 8-step settings pattern in section 5.4.
8. **Tests** -- `cursor_dot_hidden_smoke.rs` with an injectable cursor source;
   arithmetic tests mirroring `overlay.rs:1582-1614`; and an `#[ignore]`d live
   probe (mirroring `keyboard_capture_live.rs`) for real hook + UIA behaviour.

---

## 12. Where the plan is wrong or over-optimistic

1. **"Vendored rdev reports button-down with position" (section 4 table, risk
   #2).** False. `ButtonPress(Button)` has no coordinates
   (`vendor/rdev/src/rdev.rs:236`), and rdev's Windows decoder drops `pt` for
   buttons (`vendor/rdev/src/windows/common.rs:63-76`). The capability exists in
   VocalCode's *own* hook (`hook_windows.rs:188`), unused. Net effect: the work
   is smaller than the plan fears, but the stated reason is wrong, and the rdev
   path is not even the Windows path (`hotkey.rs:1020-1026`).
2. **"`workflows.rs` is the dictation lifecycle" (section 4 table).** It is
   per-app cleanup/diagnostics preferences (`workflows.rs:1-2`, `:124-146`). The
   lifecycle is `Engine::handle` (`engine.rs:858`) plus the mailbox
   (`dictation_control.rs:57`, `:130`) plus `main.rs:5010-5234`.
3. **"Cursor polling belongs on the platform timer thread" (section 5).** There
   is no such thread. The options are the hook callback (best), a new 33 ms
   thread, or the 180 ms UI loop (`webui.rs:1645-1646`). The plan should name the
   hook callback and drop the "timer thread" fiction.
4. **"Reposition the overlay through the existing IPC path" (section 5).** There
   is no positioning IPC. Host to page is `evaluate_script` only; page to host is
   a single bounded `{"type":"scale"}` message (`overlay.rs:753-766`).
   Repositioning is a native `SetWindowPos`/`set_outer_position`, not IPC. What
   *is* reusable is the control bar's `SetWindowPos` (`control_bar.rs:792-814`).
5. **"Add an anchor mode (`Screen | Cursor`) to `overlay.rs`" (section 5).**
   Workable but the wrong base: the overlay hides itself whenever the phase is
   Idle (`overlay.rs:244-250`), has only two fixed sizes (`:313-327`), repositions
   only on show (`:554-560`), and packs the phase into 3 bits that are also the
   control-permission key (`overlay.rs:118-138`, `dictation_control.rs:30-35`).
   `control_bar.rs` already has every needed mechanism (section 3.6).
6. **"~70 % of the plumbing is already present" (section 4).** True for
   *delivery* and *settings*; false for the four things the feature actually is:
   pointer tracking (0 %), point-based UIA probing (the *focused*-element version
   exists, the point version does not), per-frame window positioning (0 %), and a
   click policy (0 %). A more honest split is ~50 %, with all of the new work in
   those four items.
7. **"Double-click toggles dictation" as if it were free (section 3.2, M3).**
   The host app will also see the double-click (word select); `WM_LBUTTONDBLCLK`
   never reaches the hook. The plan's own "dot-only rule" mitigates but does not
   eliminate this. Also, the *stop* half cannot be `HandsFreeStart` (idempotent
   while recording, `engine.rs:861-864`) -- it must be `ForceStop` via the
   `Bridge`, and `Bridge` is single-slot with a 750 ms TTL
   (`dictation_control.rs:10`, `:77-83`), so the cursor controller needs its own
   instance and the `main.rs:4997` gate must be widened.
8. **"`Esc` cancels" (section 3.2).** No global `Esc` path exists; `Cancel` is
   produced only by the control bar's button (`dictation_control.rs:169`).
   `Escape` is also a *capture* key (`hotkey.rs:1617`) and a bindable control
   (`hotkey.rs:1521`), so a global Esc must be observed without consuming and
   must not disturb binding capture (`hotkey.rs:1308-1345`).
9. **Settings version (section 7 risk #6).** The plan is right to worry but
   should be decisive: **do not bump** `CONFIG_VERSION` (`config.rs:547`). An
   additive `#[serde(default)]` table is already the documented reversible
   pattern (`config.rs:14-19`, test `:1069-1074`); a bump would make today's v5
   builds refuse the folder (`main.rs:1109-1118`).
10. **"Perceived latency: a 100 ms probe is fine" (section 7 risk #8).** With the
    current COM pattern (`CoCreateInstance` per call, `inject.rs:260-261`) and no
    Windows UIA timeout, a point probe can be *far* worse than 100 ms on a busy
    provider, and it can block whatever thread runs it. Budget for a cached
    `IUIAutomation` on a worker with a hard timeout and last-known-good state,
    or M2 will fail on Word/VS Code.
11. **Minor:** the plan calls the second surface a "dev tree" implementation
    (section 4). It is a shipping, opt-in Windows feature
    (`config.desktop_control`, `webui.html:1083`; `webui.rs:1836-1869`), not
    dev-only.
