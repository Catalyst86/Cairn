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

## Increment 4b — NEXT: Endpoints (sync rendezvous) + block/wake + 2nd ring-3 task
- **Scheduler block/wake primitive:** `block_current(reason)` (mark not-runnable,
  yield) + `unblock(task)` (mark runnable). The hard part; single-CPU, IRQs-off, must
  cooperate with EDF `pick_next`/`roll_deadlines` and never lose a wakeup.
- **Notification blocking wait** (`N_WAIT`): block until pending≠0, then return+clear.
  One waiter/object in v0 (record waiter task id beside `NOTIFY`).
- **Endpoint** (kind 5): `E_SEND`/`E_RECV` synchronous rendezvous. Sender blocks until
  a receiver is waiting (and vice-versa); transfer up to N scalar words. Optional
  **capability transfer**: move the cap in the syscall `xfer` slot (currently stubbed
  in `syscall_dispatch`) from sender→receiver domain table iff the endpoint cap has
  `GRANT_CAP`. This is where the `xfer != CPTR_NULL` branch gets implemented.
- **Two ring-3 tasks** (client + server) over one endpoint → the first real IPC demo.
- Good Claude×Grok split: Grok drafts the rendezvous state machine; Claude integrates
  the block/wake into EDF, wires cap-transfer, reviews unsafe, drives verify.
- Add Kani harnesses for the new wiring where it has model-checkable logic.

## Deferred (carry-overs, unchanged)
Return a Memory **CPtr** (not a raw frame) from `M_ALLOC` so `M_FREE` can be
ownership-checked (until then `M_FREE` is refused — see capspace). Multi-waiter
endpoints/notifications; fair co-scheduling on equal EDF deadlines; SMP (per-CPU run
queues + real locks — today's cap tables assume single-CPU/IRQs-off).
