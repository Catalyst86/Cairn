# Crash-only domain supervision (v0)

Realizes DESIGN.md pillar 8 ("crash-only, live-updatable, spill-free components"). A
ring-3 fault terminates **just that domain**; the kernel and every other domain keep
running. Kernel wiring only; cap-core byte-unchanged. Single-CPU, IRQs-off discipline.

## Mechanism
- **Ring detection** (`interrupts.rs::faulted_in_ring3`): `frame.code_segment.rpl() ==
  Ring3`. A ring-0 fault is a real kernel bug → stays FATAL (halt). The #PF/#GP/#UD
  handlers terminate a ring-3 fault; #DF stays fatal (on its IST).
- **terminate-and-switch** (`sched.rs::terminate_current` → `jump_to_task`): the dead
  task is ABANDONED (not saved) and we switch into the earliest-deadline runnable peer.
  `jump_to_task` is the *restore half* of the 4b `block_and_switch` — `mov rsp,next;
  pop 15 GP; iretq` — with **NO swapgs**. This is sound because a ring-3 exception enters
  the handler with **GS=user(0)** (exceptions don't auto-swapgs) and IF=0 (interrupt
  gate), and every switch target expects active GS=user(0) (ring3/idle run there; a
  4b-blocked task re-swaps at its `resume_point`). It NEVER `iretq`s back to the faulting
  instruction (that would re-fault forever). The abandoned exception frame on the IST/
  rsp0 stack is harmless — the CPU reloads RSP from the IST entry / TSS.rsp0 on the next
  entry.
- **`reap_domain(domain, task)`** (`capspace.rs`): **revoke the dead domain's authority**
  by clearing its CapTable (`DOMAINS.tables[d] = EMPTY_TABLE`) — its CPtrs vanish, while
  the underlying objects persist for whoever else holds caps (so only this domain loses
  authority, not the shared object). Then **scrub every `ENDPOINTS[oid]`** where the dead
  task was the parked peer (`peer_task == dead → EP_EMPTY`) so a surviving partner never
  rendezvous-wakes a dead task.
- **Teardown order** (all under IF=0, single CPU, no lock held across the switch): mark
  the slot `!present/!runnable/!blocked` + `num--` (pick_next/roll_deadlines now skip it)
  → `reap_domain` → `roll_deadlines` + `pick_next` → `set_kernel_stack` +
  `set_current_domain` → `jump_to_task(next)`.

## Demo + boot-log proofs (verified in QEMU)
A `faulter` ring-3 task (its own domain, admitted FIRST so it dies early) does one real
`M_ALLOC` then `ud2`:
- `crash-only: admitted faulter (domain4) as task 1 …`
- `ring3 syscall #1: … method=1 => status=0 …` (it ran in ring 3)
- `domain 4 (task 1) terminated: #UD rip=0x420025 … — crash-only: kernel survives`
- then the **4b endpoint rendezvous completes normally** (server parks → client
  E_SEND+grant → server resumes `recv_cptr=3` → moved cap works → MOVE proof) — proving
  the kernel survived and the fault was isolated to the one domain. No panic/halt; the
  run quiesces.

## Known v0 gaps / deferred (4c / later)
- **Survivor liveness (one-directional scrub)**: `reap_domain` clears endpoints the DEAD
  task was parked on, but a *survivor* already parked **waiting for** the dead task blocks
  forever (its slot's `peer_task` is itself, so reap can't identify it). Safety is fine (no
  corruption) — it's a liveness gap. Proper fix needs endpoint peer-tracking / IPC timeouts
  / domain restart. Not exercised by the current demo (the faulter is no one's IPC partner).
- **Restart / self-healing**: re-admit a fresh instance of a terminated domain (fresh
  CapTable + re-granted caps, reusing the freed task slot), with a restart-count policy
  (cap restarts so a crash-loop is reaped); client checkpoint caps. The slot + machinery
  are ready (terminate frees the slot; admit_user re-fills it).
- **Resource accounting on death**: the faulter's `M_ALLOC`'d frame leaks on termination
  (frames aren't tracked per-domain yet; tied to the "return a Memory CPtr" item so
  frames can be reclaimed on reap).
- Notification-waiter scrub (once blocking `N_WAIT` exists in 4c).
- Per-task page-table teardown (v0 ring-3 tasks share one address space at distinct VAs).
- SMP (per-CPU run queues + real locks).
