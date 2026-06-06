# Cairn — Session Resume / Handoff

**Read this first, then `DESIGN.md`, `docs/CAP_ABI.md`, and the `cairn-os` memory.**
Cairn is a from-scratch, capability-based OS for James's HPE ProLiant x86-64 server,
built as a **Claude Code × Grok Build** collaboration. Repo: `C:\Users\danie\Desktop\Cairn`.

## ⏭ Next session — start here
1. **Confirm the clean boot** still works: `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/cairn-go-kernel.sh`
   — expect the crash-only **self-healing loop**: `crash-only: admitted faulter …` → `domain 4
   (task 1) terminated: #UD rip=0x420025 — crash-only: kernel survives` → `supervisor: RESTARTED
   domain 4 … (1 restart(s) left)` → terminated → `RESTARTED … (0 left)` → terminated →
   `supervisor: domain 4 reaped — restart budget exhausted`; FOLLOWED BY the full 4b endpoint
   rendezvous (`ep: domain2 E_RECV resumed … recv_cptr=3`, MOVE proof `cptr=1 => status=1`) plus
   the `perdomain:`/`notify:`/`GRANT_CAP gate` lines — **no panic/halt** anywhere (≈66 serial
   lines: 3 terminations, 2 restarts).
2. **Phase 2 is COMPLETE** (domains, EDF, portal IPC, crash-only supervision + restart). The
   Roadmap's major next is **Phase 3 — zero-kernel I/O + object store:** PCIe enum, virtio-blk/
   net, direct queue mapping, a log-structured object store + extent caps; objects survive
   reboot. Big, greenfield (good Claude×Grok split). Start with a design panel + a thin vertical
   slice (e.g. virtio-blk read/write of one extent via a DeviceQueue cap mapped into a domain).
   - **Or small polish before Phase 3** (all in `docs/CRASH_ONLY.md` / `docs/PORTAL_IPC.md`
     "deferred"): survivor-liveness scrub (a survivor parked waiting for a dead task blocks
     forever — needs endpoint peer-tracking / IPC timeouts); per-domain frame+object reclamation
     on death (currently bounded-leak); blocking `N_WAIT`; multi-waiter endpoint queues.
3. **cap-core stays byte-unchanged** (the regression gate; `git diff HEAD -- crates/` must be
   empty). NOTE: `kani-proofs.sh` HANGS on the frame-alloc proofs (a prior session left stale
   `cbmc` running — kill any `cbmc`/`cargo-kani`/`kani-driver` by NAME first: `pkill -9 cbmc`,
   NOT `-f` which self-matches the kill command). cap-core's 4 proofs already pass; to re-confirm
   run ONLY `cargo kani -p cap-core --features kani`. At handoff the last FEATURE commit is
   crash-only RESTART `a1b37bf` (HEAD itself is the RESUME-update commit after it; run
   `git log --oneline -12`).

## Status (Phase 2: crash-only domain supervision + restart) ✅
- ✅ **Restart / self-healing** (commit `a1b37bf`; `supervisor.rs`): `terminate_current` calls
  `supervisor::on_domain_death(domain)` (after reap, before pick_next) which, for a registered
  restartable domain with budget left, decrements and **re-admits a fresh instance** (fresh caps
  into the reaped domain table, reused slot, pages persist). Budget 0 ⇒ stays reaped. Re-entrancy
  sound (terminate holds only a raw SCHED ptr; admit_user's transient `&mut` doesn't alias).
  Focused adversarial review: 0 confirmed findings. Verified: faulter `#UD` → RESTARTED (budget
  2→1→0) → reaped, kernel + endpoint demo alive throughout (3 terminations, 2 restarts, 0 faults).
- ✅ **A ring-3 fault terminates just that domain; the kernel + other domains live on**
  (commit `b5362c3`; `docs/CRASH_ONLY.md`). DESIGN.md pillar 8. Implemented directly (it
  extends the 4b block/wake machinery) + put through a 4-dimension adversarial review (2
  findings fixed; the terminate/GS asm drew ZERO).
  - `interrupts.rs`: `faulted_in_ring3` (`code_segment.rpl()==Ring3`) routes ring-3
    `#PF`/`#GP`/`#UD` to `terminate_ring3`; ring-0 faults stay FATAL.
  - `sched.rs::terminate_current`: frees the slot (`num--`), `capspace::reap_domain`, picks the
    earliest-deadline peer, and `jump_to_task`s in — NEVER returning to the fault. `jump_to_task`
    = the restore HALF of `block_and_switch` (mov rsp; pop15; iretq) with **NO swapgs** (a ring-3
    exception is already at GS=user(0); every target expects user(0)). `MAX_TASKS` 4→5.
  - `capspace.rs::reap_domain`: revokes the dead domain's authority (clears its CapTable; shared
    objects persist for other holders) + scrubs `ENDPOINTS[oid]` where the dead task was the
    parked peer. `MAX_DOMAINS` 4→5.
  - **Verified in QEMU:** a faulter domain `M_ALLOC`s then `ud2` → `domain 4 terminated: #UD …
    kernel survives`, then the full 4b endpoint rendezvous completes — fault isolated, no halt.
  - **Review fixes:** HIGH — the kernel-stack-overflow guard check now gates on `!USER_MODE` so a
    ring-3 task reading the guard VA can't masquerade as an overflow and halt the kernel (DoS).
    MED (documented v0 gap) — the endpoint scrub is one-directional (a survivor parked *waiting
    for* a dead task blocks forever; needs peer-tracking/timeouts/restart → 4c).
  - **Deferred (later):** per-domain frame/object reclamation on death (bounded-leak; tied to the
    Memory-CPtr redesign), survivor-liveness scrub (a survivor parked *waiting for* a dead task
    blocks forever — needs peer-tracking/timeouts). cap-core byte-unchanged. (Restart/self-healing
    is DONE — see the block above.)

## Status (Phase 2: blocking endpoint IPC + scheduler block/wake — step 4b) ✅
- ✅ **Synchronous Endpoint IPC across two ring-3 domains, with block/wake + cap-transfer**
  (commit `cffbc81`; `docs/PORTAL_IPC.md`). **Designed via a judged 4-way design panel, then
  put through a 5-dimension adversarial review** (7 findings, all one root cause — cap-transfer
  error/sentinel handling — fixed; the block/wake asm + lost-wakeup logic drew ZERO findings).
  - **Scheduler block/wake** (`sched.rs`): `block_current` parks the running task *inside its
    syscall* and switches to the earliest-deadline peer; `unblock` stages the IPC result then
    clears an INDEPENDENT `blocked` gate (deliver-before-unblock). One new naked routine
    `block_and_switch` saves the blocked task as the **exact same 20-u64 frame** `timer_isr`/
    `build_task_frame_inner` use (resumable by the timer OR a peer's switch via one identical
    pop+iretq tail), resuming at a kernel `resume_point` (kernel CS/SS, IF=0). **GS-neutral**
    paired `swapgs` keeps active GS correct for every incoming task. `pick_next` skips blocked;
    `roll_deadlines` won't re-arm a blocked task. Lost-wakeup-free by IF=0-whole-syscall atomicity.
  - **Endpoint** (`capspace.rs`): `E_SEND`/`E_RECV` single-waiter rendezvous (fast handoff if the
    partner is parked, else block); state in `ENDPOINTS[oid]` (mirrors `NOTIFY`). One scalar
    message word + optional **capability transfer** via the syscall `xfer` slot, two-key gated
    (`GRANT_CAP` channel + `DELEGATE` moved cap), MOVE = verified `delegate(all)` + clear source,
    **atomic with the rendezvous** (a failed grant aborts both, never reports Ok). `reply_cptr`
    returns in rdx via `PER_CPU.reply_cptr@16`. Single `CPTR_NULL` sentinel; `xfer` range-checked.
  - **Verified in QEMU:** server parks on E_RECV → client E_SEND rendezvous wakes it → server
    RESUMES inside the syscall (`recv_cptr=3`) → moved cap works in domain 2 (`M_ALLOC=>Ok`) →
    client's granted-away slot ⇒ `ErrBadCPtr` (MOVE) → `E_SEND` w/o GRANT_CAP ⇒ `ErrRights`. 4a
    proofs intact; no faults. **cap-core byte-unchanged** (4 Kani proofs are the regression gate).

## Status (Phase 2: per-domain CapTables + Notification async IPC — step 4a) ✅
- ✅ **Per-domain CapTables + Notification IPC live** (portal-IPC step 4a; `docs/PORTAL_IPC.md`).
  `capspace.rs` now holds `MAX_DOMAINS` per-domain CapTables (`DOMAINS`, **const static-init**
  to avoid a boot-stack temporary — the original overflow bug class), `CURRENT_DOMAIN` +
  `set_current_domain` (scheduler hook), `cap_invoke_in(domain,..)` routing (the verified
  `CapTable::invoke` is still in the live path, plus a method-specific rights layer), and
  `delegate_from_root` — the **first live use of cap-core's verified `delegate`** (I3:
  child.rights ⊆ parent.rights). New **Notification** object: `N_SIGNAL` (OR bits into
  `NOTIFY[oid]`, needs INVOKE|WRITE) / `N_POLL` (read+clear, needs INVOKE|READ); payload lives
  in capspace because cap-core's `ObjectMeta` is frozen. The **live ring-3 task now runs in its
  OWN domain (1)** with an INVOKE-only Memory cap **delegated** from root (its syscalls use
  `cptr=0` in its own table). `sched.rs` gained `Task.domain`, `admit_user(..,domain,..)`, and a
  per-switch `set_current_domain`. **Verified in QEMU:** `perdomain: domain2 ALLOC via
  delegated(INVOKE-only) cap => Ok; root CPtr 1 in domain2 => ErrBadCPtr` (isolation);
  `notify: domain2 SIGNAL=>Ok POLL(no READ)=>ErrRights; root POLL=>Ok bits=0b101 then 0b0`
  (cross-domain async IPC, rights enforced); ring-3 8× `M_ALLOC` via its delegated cap; no
  faults. **cap-core byte-unchanged** (4 Kani proofs still hold). Claude-led integration.
- ✅ **Ring-3 adversarial review folded in + hardening** (commit `6dda246`). One HIGH, confirmed,
  reachable-from-ring-3 finding fixed: **`M_FREE` arbitrary-frame free** (privesc) — an untrusted
  frame number went straight to `deallocate_frame` with no ownership check; now **refused
  (`ErrMethod`)** until `M_ALLOC` returns a Memory CPtr the kernel can ownership-check (boot-log
  proof `cap_invoke(M_FREE, 0x100) => ErrMethod`). Rest of the review = the Roadmap's documented
  deferrals (raw-frame leak, sysret canonical-RIP guard, IF-during-syscall, NMI paranoid-entry +
  SMEP/SMAP, fair co-scheduling, fatal ring-3 faults). Cosmetic: all 6 `function_casts_as_integer`
  lints rewritten to `fn as *const () as …`; warnings 13→8 (rest are forward-scaffolding dead code).

## Status (Phase 2: ring-3 userspace making real cap_invoke syscalls)
- ✅ **`syscall`/`sysret` + ring 3 + first userspace `cap_invoke`.** `syscall.rs` arms the
  MSRs (STAR/LSTAR/SFMASK/KERNEL_GS_BASE + EFER.SCE|NXE) and holds the naked `syscall_entry`
  stub (swapgs → per-task kernel-stack switch → CAP_ABI↔SysV marshal → `syscall_dispatch` →
  `cap_invoke` → anti-leak zero → `sysretq`). `gdt.rs` reordered for STAR (kCS 0x08, kSS 0x10,
  uDS 0x18, uCS 0x20, TSS 0x28) with a **mutable TSS** whose `rsp0` the scheduler rewrites per
  switch. `sched.rs` gained `admit_user` + ring-3 frames (`build_task_frame_inner`) + per-switch
  `set_kernel_stack`. `paging.rs::map_user_page` maps W^X USER pages; `user.rs` is the ring-3
  blob. **Verified in QEMU:** a CPL-3 task issued **8 real `cap_invoke(M_ALLOC)` syscalls**
  (distinct frames 0x109000…0x110000), each a full ring3→syscall→cap-core→sysret round-trip; a
  revoked TimeSlice cap is still denied admission. Grok built `map_user_page` + `user.rs`; Claude
  built the GDT/MSR/asm/scheduler integration. cap-core byte-unchanged.
## Status (Phase 2: EDF scheduler with time-capabilities running)
- ✅ **EDF scheduler + time-capabilities (DESIGN.md pillar 6).** `sched.rs` now selects
  the earliest-deadline runnable task (`pick_next`) off **calibrated real-time deadlines**;
  `apic.rs` calibrates the APIC timer against PIT channel 2 to a **1 ms tick** (`TICK_NS`).
  Tasks are **admitted only by presenting a live `TimeSlice` capability** (`capspace::{mint_timeslice,
  admit_check,revoke_timeslice}` over *unchanged* verified cap-core — `sched::admit` gates on it).
  Verified in QEMU: `APIC calibrated: ~62k ticks/ms`; 3 periodic tasks (T=2/5/13 ms) admitted;
  a **revoked TimeSlice cap is denied admission (`ErrRevoked`** — live I2); steady CPU share
  **fast 86% / med 10% / slow 3%** (deadline-ordered, ≠ round-robin's ~33/33/33). Built with
  Grok (greenfield calibration + EDF data/policy + cap gate) + Claude (schedule_tick wiring,
  admit, verify). Then **adversarially reviewed** (4-dimension find→verify workflow) and 5
  latent defects fixed: new-task + ISR stack-alignment (SysV RSP%16==8), EDF idle-floor
  starvation, deadline over-spacing for D>T, calibration timeout/fallback, and removal of the
  EDF-incompatible `spawn`. Context-switch frame layout unchanged; the ISR gained one
  `and rsp,-16` alignment instruction.
- ✅ **Preemptive round-robin scheduler.** `kernel/src/sched.rs`: ring-0 kernel tasks
  with per-task stacks, context switch via a **naked timer ISR** (saves full register
  set, calls `schedule_tick` to pick the next task, `iretq`s into it). Boot registers the
  idle boot thread as task 0, spawns demo tasks A + B; the APIC timer round-robins all
  three. Verified: `[task A]`/`[task B]` interleave at ticks 1,4,7,… / 2,5,8,… (gap of 3
  = round-robin over {boot, A, B}), 119+ ticks, no fault. Policy is trivial round-robin —
  EDF deadline-ordering only changes `pick_next` (Grok's lane, next). Single-CPU; SCHED is
  touched only with IRQs off (no lock — a lock would deadlock the ISR vs a preempted holder).
- ✅ **APIC periodic timer (Phase 2 foundation).** `kernel/src/apic.rs` brings up the
  **xAPIC via MMIO** (TCG does NOT emulate x2APIC), masks the legacy PIC, and runs a
  periodic timer on vector `0x20`; the ISR bumps `apic::TICKS` + EOIs. Boot log shows
  `timer tick #1..#5, #100`. The LAPIC MMIO page (`0xFEE00000`) is mapped explicitly
  (`paging::map_mmio`, no-cache) since Limine's HHDM doesn't cover it. Uncalibrated for
  now (raw reload count); real time units arrive with the scheduler.
- ✅ **keystone boots cleanly in QEMU** (Phase 0/1): serial → switch to a **512 KiB
  guard-paged kernel stack** → GDT/IDT → frame allocator → 1 MB kernel heap → #BP
  exception recovery.
- ✅ **Stack-overflow hardening:** `kmain` switches RSP to a dedicated `KERNEL_STACK`
  with an **unmapped guard page**; the #PF handler runs on its **own IST stack** and
  reports `KERNEL STACK OVERFLOW` on a guard hit. Verified by a forced-overflow self-test
  (caught loudly, no silent corruption). This is the durable fix for the bug class below;
  the 1 MiB Limine `StackSizeRequest` now only covers the tiny pre-switch prologue.
- ✅ **cap-core formally verified** — 4 Kani proofs, 343 checks, 0 failures
  (I2 revocation, I3 no-amplification, I4 invoke-requires-right, encode round-trip).
- ✅ **`cap_invoke` is LIVE** (`kernel/src/capspace.rs` driving the verified cap-core).
  Boot log: `init_root => cptr=0`, `cap_invoke(ALLOC,0) => Ok frame=0x100000` then
  `0x101000`, `demo_revoke_then_invoke => ErrRevoked` — the verified I2 epoch-revocation
  invariant running in the real kernel.
- ✅ **The paging "map-then-unmap" bug is FIXED** (root cause + fix below).
- ⏳ `frame-alloc` Kani proofs are NOT confirmed — `kani-proofs.sh` HANGS on them (see the top
  note); cap-core's 4 proofs DO pass. Re-confirm cap-core alone: `cargo kani -p cap-core --features kani`.

## THE PAGING BUG — SOLVED (was: "map-then-unmap")
**The previous diagnosis was WRONG.** It was NOT an unmapped `.bss` tail and NOT
frame-allocator aliasing of a live page table — both were red herrings.

**Real root cause: a kernel-stack overflow into Limine's page tables.** Limine's default
~64 KiB boot stack sits immediately above its own page tables (in QEMU: stack top
~`0x1ff92790`; tables at phys `0x1ff7f000`–`0x1ff82fff`). In debug builds the by-value
construction of the 8 KiB `ObjectTable` static (capspace `OBJECTS`) cascades several
full-struct `memcpy`s (~90 KiB of stack), overflowing the boot stack DOWN through
L1/L2/L3/L4 and zeroing them. The first write to `OBJECTS` (not yet cached in the TLB)
then page-faulted **not-present**, while the CPU kept executing off stale TLB entries —
which is exactly why the #PF handler saw "L4 reads 0": the table genuinely was zero.

**How it was proven (GDB stub, as planned in the old handoff):**
- Normal-context page-table walk: Limine maps ALL of `.bss`; `OBJECTS` was present the
  whole time; **zero frames** were allocated before the fault → both old hypotheses dead.
- QEMU `monitor xp` (physical read, bypasses the broken translation) at the fault: the
  entire page-table region L1–L4 read back as zeros.
- GDB **hardware watchpoint** on the OBJECTS L1 PTE caught the clobber: `compiler_builtins`
  `memcpy`, write dest `0x1ff7f1b8` (inside the tables), RSP `0x1ff7c3f0` (below all four).

**The fix:** request a 1 MiB boot stack via Limine `StackSizeRequest::new(1 MiB)` — a
`.requests` static in `kernel/src/main.rs`; `kmain` reports whether Limine honored it
(`Boot stack: 1024 KiB (Limine request honored)`). Removed `premap_bss` (it was a no-op
built on the false premise). The `#PF` `manual_map` mapper stays only as a defensive
backstop. Building keystone's own page tables is now OPTIONAL polish, not a blocker.

**Debug tooling now in place (reusable, in `C:\WSL\`):** `gdb` installed in WSL1.
- `cairn-gdb.sh [cmds]` — boots the EXISTING `cairn.iso` under a frozen QEMU
  (`-S -gdb tcp::1234`), runs `gdb -batch -x <cmds>` (default `cairn-gdb.cmds`), then
  dumps serial. Reuses the iso so addresses match the last `cairn-go-kernel.sh` run.
- `cairn-gdb.cmds` — break at the #PF handler (by raw address), dump CR2/CR3 + page
  tables physically via `monitor xp`.
- `cairn-gdb-wp.cmds` — `set language c`, then a HW `watch` on a PTE to catch the clobber.
- Gotchas: with the kernel ELF loaded, gdb is in **Rust mode** → `set language c` before
  any C-typed `watch`/`x`; break by **raw address** (`break *0xADDR` from `nm`) since Rust
  symbols are mangled; use `monitor xp /Ngx <phys>` to read tables when translation is dead.

## Dev environment (CRITICAL — WSL1, not WSL2)
- **WSL2 is BROKEN on this Windows host** (VM rootfs extraction hangs). We use **WSL1**:
  Ubuntu 24.04 imported as `--version 1` to `C:\WSL\UbuntuWSL1`. Runs as root. apt works
  (shares Windows networking). Good for Rust + QEMU (TCG, no KVM) + Kani + gdb.
- **Invoke WSL from the PowerShell tool**, e.g. `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/<script>.sh`.
  Do NOT use the Bash tool for this — git-bash mangles `/mnt/c` paths into `C:/Program Files/Git/mnt/...`.
- **PowerShell 5.1 mangles embedded double-quotes** passed to native exes (git, grok).
  Use `--prompt-file` for grok and here-strings WITHOUT `"` for git commit messages.
- Toolchain: rustup nightly + `rust-src`, `qemu-system-x86` 8.2.2, `ovmf`, `xorriso`,
  Kani 0.67 (`cargo kani`), `gdb` 15.1. Limine 9.6.7 binary at `/root/limine`.
- Repo lives at `C:\Users\danie\Desktop\Cairn` (canonical — EDIT HERE). Scripts rsync it
  to `~/cairn` in WSL, excluding `target/` so incremental builds persist.

## Build / boot / verify commands
- **Build + boot (fast loop):** `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/cairn-go-kernel.sh`
  (rsyncs `kernel/` only, force-relinks, builds Limine BIOS ISO, runs QEMU 20 s, serial →
  `/root/cairn-serial.log` and stdout).
- **GDB debug:** `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/cairn-gdb.sh [/mnt/c/WSL/<cmds>]`
  (reuses the last-built `cairn.iso`; gdb output via `set logging` → `/root/cairn-gdb*.log`).
- **Full rebuild (kernel + crates):** `.../cairn-rebuild.sh` ; **whole pipeline:** `.../cairn-go.sh`.
- **Kani proofs:** `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/kani-proofs.sh` (slow — cap-core
  took ~26 min; runs `cargo kani -p cap-core --features kani` and `-p frame-alloc`). **WARNING:**
  it currently HANGS on the `frame-alloc` proofs and leaves stale `cbmc` running — for a quick
  regression check run only `cargo kani -p cap-core --features kani`, and reap stragglers with
  `pkill -9 cbmc` (by NAME — `-f` self-matches the kill command).
- Filter serial output in PowerShell with `... | Select-String -Pattern "..."` (no `grep`).
- Helper scripts in `C:\WSL\`: cairn-go-kernel.sh, cairn-rebuild.sh, cairn-go.sh,
  kani-proofs.sh, kani-setup.sh, limine-setup.sh, cairn-gdb.sh, cairn-gdb.cmds,
  cairn-gdb-wp.cmds, investigate-fault.sh, nm-sections.sh, api-dump*.sh.

## Kernel build specifics (so you don't rediscover them)
- Target: built-in **`x86_64-unknown-none`** + rustflags in `kernel/.cargo/config.toml`
  (`code-model=kernel`, `relocation-model=static`, `link-arg=-Tlinker.ld`). NOT a custom
  `.json` target (Rust gates those behind `-Zjson-target-spec` and the format keeps changing).
- Needs `#![feature(abi_x86_interrupt)]` (kernel) and `generic_const_exprs` (frame-alloc).
- Deps: `limine = 0.6.3` (0.3/0.4 are YANKED), `x86_64 = 0.15.4`, `spin`, `linked_list_allocator`,
  `bitflags`. limine 0.6 API: `MemmapRequest`, `StackSizeRequest::new(size)`, `.response()`,
  markers at crate root, `memmap::Entry.type_` / `MEMMAP_USABLE`. x86_64 0.15:
  `GlobalDescriptorTable::append`, `Segment` trait for `CS::set_reg`, `Cr2::read()` → Result.
- `linker.ld`: `.got` placed in `.data` BEFORE `.bss`; `.bss` page-aligned and LAST, with
  `__bss_start`/`__bss_end` symbols. Segment now has FileSiz `0xc0` ≪ MemSiz (clean NOBITS
  `.bss` tail) and Limine maps the whole thing — confirmed by page-table walk. Limine base
  revision negotiates to 3 (crate requests 6 → `is_supported()` false; made non-fatal).
  Limine config uses v9 syntax (`/Entry` + `kernel_path: boot():/boot/keystone`). cargo does
  NOT track `linker.ld`, so the boot script `touch`es a source file to force a relink.
- GDT must reload SS/DS/ES to our data segment (Limine leaves stale selectors → #GP on iretq).
- **Boot stack: request it explicitly** via `StackSizeRequest` — Limine's default boot stack
  is small (~64 KiB) and adjacent to its page tables; deep call chains overflow it (see bug above).

## Collaboration model (Claude × Grok)
- **Grok** (xAI CLI at `C:\Users\danie\.grok\bin\grok.exe`) writes greenfield Rust into the
  repo: `& "$env:USERPROFILE\.grok\bin\grok.exe" --prompt-file <path> --cwd "$env:USERPROFILE\Desktop\Cairn" --always-approve --permission-mode bypassPermissions --disable-web-search --max-turns N`.
  Model `grok-build` does NOT support `--effort`; needs `--max-turns >= 8`.
- **Claude** orchestrates, reviews Grok's `unsafe`, drives the build/boot/verify loop and
  (later) the real-hardware loop, keeps proofs green, builds the management-plane UI.

## Roadmap (Phase 2 COMPLETE → Phase 3 next)
APIC timer ✅ → preemptive round-robin scheduler ✅ → EDF policy + time-caps ✅ →
ring 3 + syscall + first userspace cap_invoke ✅ → ring-3 hardening (M_FREE gate) ✅ →
per-domain CapTables + Notification async IPC ✅ (step 4a) → portal IPC endpoints (sync
rendezvous) + scheduler block/wake + cap-transfer + 2nd ring-3 task ✅ (step 4b) →
crash-only domain supervision (ring-3 fault terminates the domain, not the kernel) ✅ →
crash-only restart / self-healing (supervisor re-admits under a budget) ✅ (Phase 2 COMPLETE) →
**Phase 3 — zero-kernel I/O + object store (PCIe/virtio + extent caps) — NEXT** →
Ring-3 follow-ups (deferred, see commits): fair co-scheduling (the demo now co-schedules
faulter+client+server across domains and works, but equal EDF deadlines are still broken by
lowest-index — no round-robin among equal deadlines), return a Memory CPtr (not a raw frame) to
ring 3 — which also unblocks the real `M_FREE` ownership check (M_FREE is currently *refused*)
and per-domain frame reclamation on death, sysret canonical-RIP guard once user RIP is
attacker-influenced, re-enable IF mid-syscall for blocking IPC, NMI paranoid-entry + SMEP/SMAP.
Follow-ups deferred from EDF: per-task budget *enforcement* (preempt on overrun; v0 only
accounts), deadline-miss policy beyond finish-late, calibration accuracy on real HW,
admission utilization check (Σ Cᵢ/Tᵢ≤1), and a way to revoke an *already-admitted* task's
TimeSlice without cap-core in the ISR hot path.
Phase 4 (network-boot onto the real HPE ProLiant via James's existing iPXE server; see the
`studio-server-access` memory) → Phase 5 (confidential boot + beautiful management plane).
Keep adding Kani proofs per component; finish the `frame-alloc` proofs. Building keystone's
own page tables is now optional hardening, no longer blocking.

## Server (not needed until Phase 4)
HPE ProLiant, currently OFF. iLO `192.168.99.2` (web, user Administrator, reachable only with
laptop Ethernet in the POE switch). OS over Wi-Fi: `ssh james@studio.local` (key-only,
`~/.ssh/id_ed25519`). Details in the `studio-server-access` memory.
