# Portal IPC + per-domain CapTables (Phase 2, step 4)

Realizes DESIGN.md pillar 9 ("everything is a `cap_invoke`") and the Roadmap item
**"portal IPC (endpoints + notify caps) + per-domain CapTables."** Built on the
*frozen, Kani-verified* `cap-core` (crates/cap-core) — this is **kernel wiring only**;
cap-core is never modified. cap-core already defines the `Endpoint`, `Notification`,
and `CapTable` object kinds and the verified `delegate` (I3: child.rights ⊆
parent.rights), `mint`, `invoke`, `lookup`, epoch `revoke`.

## Design constraints inherited from cap-core (do not fight these)
- **No object payload in cap-core.** `ObjectMeta` is `{kind, epoch, present}` only.
  Object *state* lives in the kernel next to the table — the frame allocator backs
  `Memory`, and a per-object word backs `Notification` (`NOTIFY[object_id]`). An
  `Endpoint`'s rendezvous/queue state lives in the kernel the same way.
- **`Status` is a fixed subset** (`Ok, ErrBadCPtr, ErrRevoked, ErrType, ErrRights,
  ErrMethod`) — no `ErrWouldBlock`/`ErrNoReceiver`. IPC-specific outcomes are
  expressed by *blocking* (the caller is descheduled, not given an error) or mapped
  onto an existing code; we do **not** add codes to cap-core.
- **Fixed capacities:** 256 cap slots / domain, 1024 objects, all `no_heap`.

## Per-domain CapTables (model)
Each protection domain (a scheduler task) owns its own `CapTable`. A CPtr is an index
into the *running domain's* table, so the same integer names different authority in
different domains and one domain cannot reference another's caps (CAP_ABI §1, seL4
CSpaces). Domain 0 is the root/kernel domain. A domain gets a cap by **mint** (root)
or by **delegate** from another domain with a rights subset. The running domain is
tracked in `capspace::CURRENT_DOMAIN`, written by the scheduler on every context
switch, so a ring-3 `syscall` resolves its CPtr against its own table automatically.

## Increment 4a — DONE (this commit): per-domain CapTables + Notification (async)
- `capspace`: `MAX_DOMAINS` CapTables (`DOMAINS`, const static-init → no boot-stack
  temporary, mindful of the original stack-overflow bug); `CURRENT_DOMAIN` +
  `set_current_domain` (scheduler hook); `cap_invoke_in(domain, …)` routing, with the
  verified `CapTable::invoke` still in the live path plus a method-specific rights
  layer (Notification SIGNAL→WRITE, POLL→READ); `delegate_from_root`;
  `create_notification`; `N_SIGNAL`/`N_POLL` over a `NOTIFY[object_id]` pending word.
- `sched`: `Task.domain`; `admit_user(… , domain, …)`; `schedule_tick` calls
  `set_current_domain` per switch.
- The **live ring-3 task now runs in its own domain (1)** with a Memory cap
  *delegated* (INVOKE-only) from root — first live use of verified `delegate`.
- Boot-log proofs: cross-domain delegate (I3 subset), CPtr isolation between domains,
  ring-3 `M_ALLOC` via its delegated cap, and a Notification signalled by one domain +
  polled by another with rights enforced (signal-only cap cannot poll).

## Increment 4b — DONE: Endpoints (sync rendezvous) + block/wake + 2nd ring-3 task
Designed via a judged 4-way design panel (run `wke6z5oxd`), then implemented and put
through a 5-dimension adversarial review (run `w3fa74g69`).

- **Scheduler block/wake (`sched.rs`):** `block_current(reason, oid)` parks the running
  task *inside its syscall* and switches to the earliest-deadline peer; `unblock(task,
  status, reply, reply_cptr)` stages the IPC result then clears `blocked`. The one new
  naked routine `block_and_switch` saves the blocked task as the **exact same 20-u64
  frame** `timer_isr`/`build_task_frame_inner` use (resumable by EITHER the timer OR a
  peer's switch via one identical pop+iretq tail), resuming at a kernel `resume_point`
  with kernel CS/SS and IF=0. **GS-neutral**: a paired `swapgs` (out before the switch,
  in at `resume_point`) keeps active GS = what every incoming task expects (this fixed a
  fatal flaw the panel found in a competing design). `Task` gains an INDEPENDENT
  `blocked` gate (pick_next skips it; roll_deadlines won't re-arm it) + per-task IPC
  staging. Lost-wakeup-free by IF=0-for-the-whole-syscall atomicity + deliver-before-
  unblock ordering.
- **Endpoint** (kind 5, `capspace.rs`): `E_SEND`/`E_RECV` single-waiter rendezvous
  (fast handoff if the partner is parked, else block). State in `ENDPOINTS[oid]` (mirrors
  `NOTIFY`). One scalar message word + optional **capability transfer** via the syscall
  `xfer` slot, gated on `GRANT_CAP` (channel) AND `DELEGATE` (the moved cap). Transfer =
  verified `delegate(all)` + clear source slot (MOVE), and is **atomic with the
  rendezvous** — a failed grant aborts both parties with the error, never reports Ok.
  `reply_cptr` flows back in rdx via `PER_CPU.reply_cptr@16`. cap-core byte-unchanged.
- **Two ring-3 tasks** (`user.rs`/`main.rs`): client (domain 1) sends `0xCA11` + grants a
  Memory cap; server (domain 2) blocks on recv, resumes with the message + moved cap, and
  `M_ALLOC`s via it. Boot-log proofs: server parks → client rendezvous → server resumes
  (`recv_cptr=3`) → moved cap works in domain 2 → client's granted-away slot ⇒ ErrBadCPtr
  (MOVE) → GRANT_CAP-less send ⇒ ErrRights. No faults.
- **Review fixes folded in:** `do_cap_transfer` now returns `Result` and the rendezvous
  aborts atomically on a failed grant (was: silently Ok); the single `CPTR_NULL` (0xffff)
  sentinel replaces the ambiguous `0` for both `xfer` and `reply_cptr` (slot 0 is now an
  ordinary transferable slot); `xfer` is range-checked before parking.

## Deferred to 4c (and later)
- **Blocking `N_WAIT`** on Notifications (the block/wake primitive trivially extends to
  it via an `NWAITER[oid]` slot + waking from `notify_signal`).
- Multi-waiter endpoints/notifications (FIFO sender/receiver queues); v0 returns
  `ErrRights` to a second waiter in a direction.
- A cap-core `move` primitive (GRANT without requiring DELEGATE on the moved cap).
- Directed hand-off on unblock (switch straight to a woken earlier-deadline partner);
  re-anchor a long-blocked task's stale EDF deadline at unblock (fairness).
- Bulk payloads beyond one scalar word (via shared Memory caps).
- Return a Memory **CPtr** (not a raw frame) from `M_ALLOC` so `M_FREE` can be
  ownership-checked (until then `M_FREE` is refused). SMP (per-CPU run queues + real
  locks — today's cap/sched state assumes single-CPU/IRQs-off). Re-enabling IF mid-syscall
  (would replace the IF=0 atomicity argument with a real critical section).
