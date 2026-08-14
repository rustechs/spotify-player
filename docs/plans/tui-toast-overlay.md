# Implementation Plan: In-TUI toast overlay for mutation results

**Status:** Draft
**Approval authority:** human review (Interview Output 2026-08-14, depth B)
**Activation authority:** human; `Authorized phases` must be set before `implement-plan`. This document's merge does not authorize implementation, deploy, or tracker mutations.
**ADR(s):** none — this repository has no `docs/decision/` ADR process; durable choices are the Interview Output recorded in the planning session (purpose, overlay vs `PopupState`, FIFO+peek, sticky errors, placement, triggers, `esc`, config, overflow).
**Epic / execution unit:** none
**Linear project:** none (non-epic; no named project policy in this repo)
**Primary Linear issue:** pending creation via `gen-tickets` after the plan is Active — do not create tickets from this Draft
**Material cutover:** no — additive TUI overlay and optional config keys with defaults; no production data store, traffic split, or enablement ritual
**Cutover plan dependency:** none
**Routine deployment phase:** none — merge into the repo's default branch is the ship for this CLI app; no post-merge environment action
**Supersedes:** none
**Superseded by:** none
**Target repo:** aome510/spotify-player (workspace `/home/avolkov/git/spotify-player`)
**Execution mode:** manual
**Phase 0 gate:** human
**Maximum Phase 0 rounds:** 3
**Authorized phases:** (unset until activation)
**Context strategy:** current context (`feat/toast-notifications`)
**Scope:** In: non-modal toast queue on `UIState`, render in main-content lower-right, enqueue from mutating `ClientRequest` results plus clipboard/copy-link, config/theme/docs/tests. Out: desktop `notify` changes, playback-poll/Get*/search toasts, `PlayerRequest` toasts, Cargo feature flag, daemon/CLI rendering.

## 1. Observable outcome and invariants

### End-to-end outcome

In the interactive TUI, a mutating action (queue/library/playlist/like/follow/create playlist, copy link, clipboard-open) shows a short non-modal toast: success auto-dismisses after `toast_success_timeout_secs` (default 3); errors stay until `ClosePopup` (`esc`) when no popup is open. Multiple toasts FIFO with cap 10 (drop newest, `tracing::warn!`); UI shows the current body plus a stacked-card peek of the next. Desktop track-change notify is unchanged. Daemon/CLI never render toasts.

### Blast-radius invariants

| Affected contract | Existing behavior | Characterization test | Allowed change |
|---|---|---|---|
| `PopupState` / `has_focused_popup` | Modal overlays; Search unfocused; `new_page` clears popup | Existing popup key tests if any; manual ActionList+esc | Toasts are **not** a `PopupState` variant; page change does **not** clear the toast queue |
| Playback render order | Playback drawn first; leftover rect is main UI; nothing paints on playback (issue #498) | `split_rect_for_playback_window` still returns leftover rect; toast clip tests | Toasts draw only inside leftover content rect, after `render_main_layout` |
| Desktop `notify` | Playback-change OS notifications behind `notify` feature | No edits to `notify_new_playback` | Unchanged |
| Get*/poll errors | `tracing::error!` only (`Failed to retrieve current playback`, etc.) | Handler still logs; no toast enqueue for those variants | Unchanged |
| `ClosePopup` / `esc` | When `ui.popup.is_some()`, Search/PlaylistCreate often return unhandled and **global** `ClosePopup` sets `ui.popup = None`; list popups consume `ClosePopup` in the popup handler | Unit test: Search/PlaylistCreate/ActionList still open → esc clears popup, toast queue unchanged; `popup` None → dismiss current toast | Replace the global `ui.popup = None` arm with `close_popup_or_dismiss_toast`; do not add a second dismiss on the `popup.is_none()` page path |
| Default keymap / commands | `ClosePopup` remains `esc`; no new default binding | Keymap table + README command row unchanged except toast mention in ClosePopup desc if needed | Optional desc tweak: close popup, else dismiss toast |
| Logging | Failures use `tracing::error!("{err:#}")` | Handler still logs on failure/timeout | Also enqueue error toast for mutating variants |
| Daemon | `state.is_daemon`; no `ui::run` | Enqueue skipped when `is_daemon` | Bounded no-op |

## 2. Phase 0 — risk-reduction portfolio

| Assumption | Consequence if false | Promising leads | Discriminating validation | Alternate probe | Pass/fail threshold |
|---|---|---|---|---|---|
| Leftover rect after `render_playback_window` is the only safe toast clip | Overlaying `frame.area()` paints on cover art (#498) | Read `split_rect_for_playback_window`; render toast with that rect, not full frame | Unit-test a pure `toast_area(content: Rect) -> Rect` that stays inside `content` (x+width ≤ content.right, y+height ≤ content.bottom) | Manual TUI with `image` feature | Pass: helper never returns a rect outside content; fail: any overlap with playback outer rect |
| Mutating vs fetch `ClientRequest` can be a closed `fn is_toastable(&self) -> bool` | Missed variant toasts Get* or skips a mutation | Exhaustive match on `ClientRequest` in `request.rs` | Compile-time exhaustive match; table test of each variant | Clippy `-- -D warnings` on the match | Pass: every variant listed; Get*/Player/Search/Lyrics/Restart = false; listed mutations = true |
| Success timeout can use `Instant` + existing UI refresh (`app_refresh_duration_in_ms`, default 32) | Need a dedicated timer thread | `ToastQueue::expire_due(now)` called from `ui::run` **before** `terminal.draw` | Unit test: success toast with `expires_at` in the past is popped; error has `expires_at: None` and is not popped | — | Pass: fake clock tests; fail: success never expires or error expires |
| Global `ClosePopup` is the right single hook for toast dismiss | Adding dismiss only when `popup.is_none()` misses Search/PlaylistCreate (they fall through to global `ui.popup = None`); adding it unconditionally on global ClosePopup while a popup is set would dismiss the toast instead of Search | Read `handle_key_sequence_for_search_popup` / create-playlist (no `ClosePopup`); list popups handle it locally; global arm `event/mod.rs` ~872 | Replace that arm with `close_popup_or_dismiss_toast`: if `popup.is_some()` clear popup only; else dismiss current toast. Tests: Search still open → queue unchanged; no popup → toast dismissed | Grep `Command::ClosePopup` | Pass: both cases; fail: Search+esc pops toast |
| Cap-10 drop-newest does not drop the **visible** toast | User loses the sticky error on screen | `push` refuses insert when `len() >= 10` without touching index 0 | Unit test: 10 queued, 11th dropped, current still first | — | Pass: current unchanged; warn path invoked (test hook or count) |

### Phase 0 evidence and review

#### Round 1

##### Evidence inventory

| Assumption | Critical sub-claims | Evidence gathered | Outcome | Coverage & proxy risk | Validation confidence | Remaining work |
|---|---|---|---|---|---|---|
| Leftover-rect clip | Toast never uses full `frame.area()`; playback outer reserved | Untested (pre-implementation). Static: `ui/mod.rs` `render_application` draws playback first then shrinks `rect`; `playback.rs` `split_rect_for_playback_window` | Untested | Static layout only | Low | Prove in Phase 0 of `implement-plan` via `toast_area` tests + read of split helper |
| Toastable match | Exhaustive; Player/Get* excluded | `client/request.rs` variant list inventoried in this plan | Untested | Inventory is a proxy until the match exists | Medium | Prove with exhaustive match + table test |
| UI-loop expiry | 32ms poll enough; no extra thread | `ui/mod.rs` sleep `app_refresh_duration_in_ms` | Untested | Assumes clock monotonic in tests | Medium | Prove with fake `Instant` in queue tests |
| ClosePopup path | Search/PlaylistCreate fall through to global ClosePopup; list popups consume it | Reviewer B round 1: search/create do not handle `ClosePopup`; global arm clears popup | Partial | Dispatch re-read; tests absent | Medium | Prove `close_popup_or_dismiss_toast` including Search still-open case |
| Drop-newest | Visible toast stable | Interview Q13 (b) | Untested | Policy locked; code absent | Medium | Prove push/overflow tests |

**Round summary:** Planning-time static reads support the approach; no runtime evidence yet. Phase 0 of implementation must fill this inventory before production enqueue/render.

##### Review

**Gate:** human
**Verdict:** pending

> **Phase 0 status — pending.**

## 3. Existing patterns and ownership

| Concern | Searches/files read | Existing anchor | Candidate decision | Owner/disposition |
|---|---|---|---|---|
| UI overlay state | `state/ui/mod.rs`, `state/ui/popup.rs` | `UIState.popup: Option<PopupState>`; `new_page` clears popup | New `UIState.toasts: ToastQueue` in `state/ui/toast.rs`; never a popup variant | create |
| Render order | `ui/mod.rs` `render_application`; `ui/playback.rs` `render_playback_window` | Playback first; leftover rect; popups then main layout | After `render_main_layout`, overlay toasts on the **same leftover rect** passed into main layout (not `frame.area()`) | extend `ui/mod.rs` + new `ui/toast.rs` |
| Theme styles | `config/theme.rs` `ComponentStyle` + `docs/config.md` Component Styles | Optional fields + `Theme::like()` accessors | Add `toast_success` / `toast_error`; defaults green / red + bold | extend |
| App config | `config/mod.rs` `AppConfig`; `enable_notify` pattern | serde defaults in `Default` impl | `enable_toast: bool` default true; `toast_success_timeout_secs: u64` default 3 | extend |
| Client mutations | `client/request.rs`, `client/mod.rs` `handle_request`, `client/handlers.rs` | Success: log duration; fail/timeout: `tracing::error!` only | After mutating handle: success toast; on Err/timeout: error toast with `{err:#}` / timeout text; skip if `!enable_toast` or `is_daemon` | extend |
| Copy / clipboard | `event/mod.rs` `Action::CopyLink`; `event/clipboard.rs`; `OpenSpotifyLinkFromClipboard` | `execute_copy_command`; invalid link `tracing::warn!` | Success/fail toasts on those paths | extend |
| Event dismiss | `event/mod.rs` `Command::ClosePopup`; `event/popup.rs` search/create vs list | Search/PlaylistCreate fall through to global `ui.popup = None`; list popups handle `ClosePopup` themselves | Replace **only** the global `ClosePopup` arm with `UIState::close_popup_or_dismiss_toast` (popup Some → clear popup, leave toasts; None → dismiss current toast). Do not hook the page/`popup.is_none()` branch | extend |
| Tests | `state/queue.rs` module tests; `ui/mod.rs` `drain_while_ready` | Inline `#[cfg(test)]` | Toast queue + area + close-or-dismiss tests in `state/ui/toast.rs` (and thin event helper test) | create |
| Docs | `README.md` Features; `docs/config.md`; `examples/app.toml`; command table `ClosePopup` | User-visible config must be documented (`CLAUDE.md`) | Document toast config, theme keys, esc behavior | extend |
| Feature flags | `Cargo.toml` `notify` optional | OS notify is optional | **No** new feature; always compiled | keep |

## 4. Execution phases and units

| Unit | Deliverable | Authority ref | Files/areas | Depends on | First failing test | Green + regression verification | Effort |
|---|---|---|---|---|---|---|---|
| P0 | Layout clip + toastable match + expiry/overflow/close helpers proven | Plan §2 | `state/ui/toast.rs` (types+tests), read-only playback split | — | `cargo test -p spotify_player toast -- --nocapture` fails: no such module | Same + clippy | S |
| U1 | `ToastQueue`: kind, message, `expires_at`, FIFO, peek, cap 10 drop-newest, `expire_due` | Interview q4–q6, q13 | `spotify_player/src/state/ui/toast.rs`, export from `state/ui/mod.rs` | P0 types | `toast_queue_drops_newest_at_cap` does not compile / fail | `cargo test -p spotify_player toast_queue` | S |
| U2 | Config + theme keys + defaults | Interview q11–q12 | `config/mod.rs`, `config/theme.rs`, `examples/app.toml` | — | serde roundtrip test or compile fail on new fields | `cargo test` + clippy | S |
| U3 | Wire `UIState.toasts`; call `expire_due(Instant::now())` in `ui::run` before draw; render overlay lower-right of the leftover content rect (the `rect` after playback split, not `frame.area()`) with peek sliver | Interview q3, q5, q8 | `state/ui/mod.rs`, `ui/mod.rs` `run` + `render_application`, `ui/toast.rs` | U1, U2 | `toast_area_stays_inside_content` fails until helper exists | unit tests + lint | M |
| U4 | Enqueue from mutating client results + copy/clipboard; skip Get*/Player/daemon/disabled | Interview q2, q9 | `client/request.rs` `is_toastable` (`RestartIntegratedClient` behind `#[cfg(feature = "streaming")]`), `client/handlers.rs`, `event/mod.rs` CopyLink + clipboard-open | U1, U2 | Table test `is_toastable` fails on wrong variants | `cargo test is_toastable` + lint | M |
| U5 | Global `ClosePopup` arm: popup present → clear popup only; else dismiss toast | Interview q10 | `event/mod.rs` `handle_global_command` ClosePopup arm only; `state/ui/mod.rs` helper | U1 | `close_popup_or_dismiss_toast` tests fail (Search still-open case required) | those tests + lint | S |
| U6 | README, `docs/config.md` (options + component styles), ClosePopup command note | Interview q12; `CLAUDE.md` docs | `README.md`, `docs/config.md` | U2 | Doc review (no automated doc gate) | human read of tables | S |

> **Phase N status — pending.**

`ClientRequest` toastable (true): `AddPlayableToQueue`, `AddAlbumToQueue`, `AddPlayableToPlaylist`, `DeleteTrackFromPlaylist`, `ReorderPlaylistItems`, `AddToLibrary`, `DeleteFromLibrary`, `CreatePlaylist`. False: all `Get*`, `Search`, `GetLyrics`, `Player(_)`, `RestartIntegratedClient`. Clipboard/copy-link are event-thread toasts, not `ClientRequest`.

Success copy: short past-tense (“Added to queue”, “Copied link”, “Created playlist”). Error copy: `Failed to …: {err:#}`.

**U4 handler wiring:** `start_client_handler` moves `request` into `handle_request`. Clone the request (or snapshot `is_toastable` + success/error strings) **before** the timeout call. On `Ok(())` enqueue success if toastable; on `Err` / timeout enqueue error toast **and keep** the existing `tracing::error!` lines. Skip enqueue when `!enable_toast` or `state.is_daemon`.

**U4 copy-link / clipboard:** `Action::CopyLink` uses `execute_copy_command(...)?` today (failure only hits `Failed to handle terminal event`). Clipboard-open uses `get_clipboard_content()?` and `tracing::warn!` for invalid URLs. Catch these on the event thread; enqueue success/failure toasts locally. Do not rely on the event-loop error path.

**U2 config:** Do **not** copy `#[cfg(feature = "notify")]` from `enable_notify`. `enable_toast` and `toast_success_timeout_secs` are always compiled, with `Default` / serde defaults like other ungated `AppConfig` fields so existing `app.toml` keeps working.

**U3 expiry:** `ui::run` currently locks, draws, sleeps. Call `ui.toasts.expire_due(Instant::now())` on that lock **before** `terminal.draw`.

**Navigation:** `new_page` and `PreviousPage` already clear popup only; they must not clear `toasts`. Add a unit assert that `new_page` leaves the queue intact.

## 5. Test strategy

### TDD and coverage contract

- **Coverage baseline command/result:** unavailable — no tarpaulin/llvm-cov/coverage job in CI (`.github/workflows/ci.yml` is fmt/clippy/test only). Do not invent a percentage.
- **Coverage completion gate:** every behavior in the table below has a named test; `./scripts/lint.sh` and `cargo test --no-default-features --features rodio-backend,media-control,system-audio-visualization,image,notify,fzf` pass; no test weakened.

| Behavior/requirement | Test level and path | RED command and expected failure | GREEN/regression command | Coverage expectation |
|---|---|---|---|---|
| FIFO + peek of next | unit `state/ui/toast.rs` | `cargo test toast_queue_peek -- --exact` fail (module missing / assert) | same after U1 | enqueue 2, `current` first, `peek` second |
| Success expires, error does not | unit | `cargo test toast_queue_expire_due` fail | same | fake `now` past `expires_at` |
| Cap 10 drop newest | unit | `cargo test toast_queue_drops_newest_at_cap` fail | same | 11th rejected; len 10; front unchanged |
| `enable_toast` false skips push | unit | `cargo test toast_queue_respects_enable_flag` fail | same | helper no-ops |
| Toast rect inside content | unit `ui/toast.rs` or `state/ui/toast.rs` | `cargo test toast_area_stays_inside_content` fail | same | several content sizes including tiny |
| `is_toastable` exhaustive | unit `client/request.rs` | `cargo test client_request_is_toastable` fail | same | one assert per variant |
| ClosePopup vs popup | unit on `UIState` helper | `cargo test close_popup_or_dismiss_toast` fail | same | Search/ActionList still open → toast unchanged; no popup → toast dismissed |
| Config defaults | unit or compile `AppConfig::default` | fields missing → compile fail | `cargo test` / clippy | `enable_toast == true`, timeout 3 |

### Realism target

Level 5 (pure functions + in-process state) for queue, clip, toastable, dismiss. Level 1 TUI is human-only (ratatui overlay + cover art). No higher automated level is feasible without a terminal snapshot harness this repo does not have.

### Happy-path integration

| Behavior | Systems composed | Environment | Command/evidence |
|---|---|---|---|
| Mutation success toast | event → `ClientRequest` → handler → `ToastQueue` → UI draw | developer TUI | Manual: add to queue, see toast, wait 3s, gone |
| Mutation error toast | same with forced API failure | developer TUI | Sticky until esc |
| Peek | two rapid mutations | developer TUI | Card sliver of next |

### Edge-case and failure matrix

| Scenario | Boundary/failure | Expected behavior | Test level | Environment | Command |
|---|---|---|---|---|---|
| Empty queue | no toast | no overlay | unit | local | `toast_queue` empty draw skip |
| Tiny terminal | content smaller than toast | clamp/skip draw; no panic | unit | local | `toast_area` min size |
| Popup + sticky error | ActionList open | esc closes popup (list handler); toast remains | unit + human | local | helper + TUI |
| Search / PlaylistCreate + sticky error | those popups do not handle `ClosePopup` | global esc clears popup via `close_popup_or_dismiss_toast`; toast remains | unit | local | Search still-open case |
| Queue full | 11th mutation | drop newest; `tracing::warn!` | unit | local | cap test |
| Playback poll error | `GetCurrentPlayback` fail | log only | unit `is_toastable` | local | variant false |
| Daemon | `is_daemon` | no enqueue | unit/handler guard | local | skip when daemon |
| Handler timeout | 30s mutating request | error toast + existing error log | code path; timeout hard to unit | local | same enqueue helper as Err |
| `new_page` | navigate | toasts persist | unit on `new_page` | local | assert queue len unchanged |
| Concurrent workers | two mutating tasks | `UIState` mutex serializes pushes | code review; parking_lot Mutex | — | no extra test if all pushes take `ui.lock()` |

### Human-only validation

| Gate | Why not automated | Exact procedure | Expected evidence | Rollback |
|---|---|---|---|---|
| Stacked-card peek | no screenshot harness | Add two items to queue quickly; confirm one body + sliver | Operator note on PR | revert overlay |
| Cover art | needs `image` + terminal | Play track with cover; toast must not duplicate/glitch playback image | Operator note | keep clip helper |
| Theme | visual | success green / error red vs `theme.toml` overrides | Operator note | default styles |

## 6. Temporary scaffolding

| Scaffold | Purpose | Maintained value | Cleanup checkpoint | Proposed disposition |
|---|---|---|---|---|
| None planned | — | — | — | — |

## 7. Fallbacks and replan triggers

| Blocker/signal | Evidence | Recovery or next investigation | Amend plan / replace plan / supersede ADR |
|---|---|---|---|
| Toast on leftover rect still hits cover (protocol draws outside block) | #498-class glitch in human image test | Move toast to top-right of content (still leftover rect); never overlay playback outer | Amend placement (keep leftover-rect invariant) |
| Exhaustive match broken by new `ClientRequest` variant | compile fail | Add variant to `is_toastable` with test | Amend trigger table |
| Success toasts too noisy | user feedback | Default `enable_toast` stays on; document disable | Amend copy/triggers only with new interview |
| Drop-newest hides a fresh error behind 10 old successes | Interview-accepted risk | Do not silently switch to drop-oldest | New interview / plan amend if product changes |

## 8. Traceability

| Authority requirement | Artifact/unit | Verification |
|---|---|---|
| Failures + confirmations; not desktop notify | U4; no `notify_new_playback` edits | grep notify; `is_toastable` |
| Separate `UIState` overlay | U1, U3 | type not in `PopupState` |
| FIFO + stacked-card peek | U1, U3, human peek | tests + human gate |
| Success timeout; sticky error | U1, U2 | expire tests; `expires_at: None` |
| Content lower-right; not on playback | U3, P0 | `toast_area` |
| Mutating actions only | U4 | table test |
| esc: popup first, else toast | U5 | helper tests including Search still-open (global ClosePopup arm only) |
| Always compiled; `enable_toast` default on; timeout secs | U2, U6 | defaults + docs |
| Cap 10 drop newest + warn | U1 | overflow test |
| Theme keys + docs + unit tests + lint | U2, U6, §5 | CI commands |
| Keep `tracing::error!` | U4 | handlers still log |
| Daemon no render | U4 skip enqueue | `is_daemon` guard |

## 9. Primary Linear issue

- **Identity:** pending `gen-tickets` after Active
- **Reconciliation state:** pending preview
- **Desired title:** In-TUI toasts for mutation success and failure
- **High-level description:** Implement `docs/plans/tui-toast-overlay.md`. In: toast queue overlay, mutating-request enqueue, config/theme/docs/tests. Out: desktop notify, Get*/playback-poll toasts, feature flag.

### Adapted children/subtasks

| Child/subtask | Outcome and phase coverage | Dependency/gate | Branch/environment | Authority required |
|---|---|---|---|---|
| `phase-0-toast-helpers`: Prove clip, toastable, expiry, overflow, close-or-dismiss | P0 | plan Active | `feat/toast-notifications` | implement-plan after activation |
| `toast-queue-overlay`: Queue + render + config/theme | U1–U3 | P0 pass | same | same |
| `toast-enqueue-dismiss-docs`: Client/event enqueue, esc, docs | U4–U6 | U1–U3 | same | same |

## 10. Execution checklist and outcomes

- [ ] Required prototype evidence accepted and folded into plan, or not triggered
- [ ] Exactly one primary Linear issue linked
- [ ] Phase 0 evidence gathered
- [ ] Phase 0 human/independent review approved
- [ ] Pattern inventory reconciled after Phase 0
- [ ] Each repository implementation phase completed; any routine post-merge deployment phase is separately pending or evidence-backed
- [ ] Every behavior-changing unit has recorded RED evidence from before its production edit and GREEN evidence afterward
- [ ] Happy-path integration passes
- [ ] Edge-case matrix passes
- [ ] Blast-radius invariants pass
- [ ] Configured line/branch coverage meets repository thresholds and has not decreased from the recorded baseline (N/A — no coverage metric; requirement-to-test table is the gate)
- [ ] No test, assertion, coverage threshold, or coverage exclusion was weakened to make the change pass
- [ ] Human-only gates completed or explicitly pending
- [ ] Material cutover decision and any cutover-plan dependency recorded
- [ ] Each material cutover dependency records expected identity fields, compatibility, and the freshness method; actual exact identities and fresh readiness evidence are deferred until after commit/build/merge as applicable and are required only before cutover Ready approval/execution
- [ ] Scaffolding disposition decided
- [ ] Validation outcomes recorded
