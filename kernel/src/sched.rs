//! Preemptive EDF (earliest-deadline-first) scheduler for ring-0 kernel tasks.
//!
//! Owns the task table, per-task stacks, the context switch, and the timer ISR
//! that drives preemption. The policy is EDF over calibrated real-time deadlines
//! ([`pick_next`] + [`roll_deadlines`]); a task is admitted to the run queue only
//! by presenting a `TimeSlice` capability ([`admit`], "time is a capability").
//!
//! ## How a switch works
//! The APIC timer fires the naked [`timer_isr`], which saves the interrupted
//! task's full register set, calls [`schedule_tick`] with the saved-context
//! pointer, sets RSP to the next task's saved-context pointer, restores its
//! registers, and `iretq`s into it. A task's "saved context" is just a region of
//! its own stack holding 15 pushed GP registers followed by the CPU's `iretq`
//! frame (RIP/CS/RFLAGS/RSP/SS) — identical in layout for a preempted task and a
//! freshly [`admit`]ted one, so the same restore path starts both.
//!
//! Single-CPU only: the scheduler state is touched with interrupts disabled (in
//! the ISR) or before `sti`, so no lock is needed (and a lock would deadlock the
//! ISR against a preempted holder). SMP will need per-CPU run queues + care.

use core::arch::naked_asm;
use core::sync::atomic::Ordering;

const MAX_TASKS: usize = 5;
const TASK_STACK_SIZE: usize = 64 * 1024; // 64 KiB per spawned task

#[derive(Clone, Copy)]
struct Task {
    // Existing fields — DO NOT REORDER. `rsp` must stay first (the only field the
    // context-switch path cares about); `present` second.
    rsp: u64,
    present: bool,
    // EDF metadata — touched only with interrupts off, inside schedule_tick.
    runnable: bool,
    period_ns: u64,
    rel_deadline_ns: u64,
    abs_deadline_ns: u64, // the EDF sort key; idle task uses u64::MAX
    budget_ns: u64,
    // diagnostics
    activations: u64,
    deadline_misses: u64,
    // Kernel-stack top for this task: where a syscall / privilege-changing interrupt
    // from ring 3 lands (TSS.rsp0 + PerCpu.kernel_rsp0). For a ring-0 task this is
    // also where it runs; for a ring-3 task its run stack is a separate user stack.
    kstack_top: u64,
    // Protection domain id (index into capspace per-domain CapTables). A ring-3 task
    // runs in its own domain so its CPtrs resolve against its own table; ring-0/idle
    // use domain 0 (root). The scheduler publishes this to capspace on each switch.
    domain: u16,
    // --- block/wake for portal IPC (4b) ---
    // `blocked` is an INDEPENDENT gate from `runnable`: roll_deadlines re-arms
    // `runnable` every period, so a task parked inside a blocking syscall must be kept
    // off the run queue by `blocked` until its rendezvous partner wakes it. pick_next
    // skips blocked tasks; roll_deadlines will not re-arm a blocked task's `runnable`.
    blocked: bool,
    block_reason: u8, // diagnostic: why parked (BLOCK_RECV / BLOCK_SEND)
    wait_ep: u16,     // diagnostic: object id being waited on
    // IPC result staged by the waker BEFORE it clears `blocked` (deliver-before-unblock,
    // so the result is visible before pick_next can ever select the woken task). Read by
    // the resumed syscall to return to ring 3; `ipc_reply_cptr` is a transferred CPtr.
    ipc_status: u64,
    ipc_reply: u64,
    ipc_reply_cptr: u16,
}

impl Task {
    const EMPTY: Task = Task {
        rsp: 0,
        present: false,
        runnable: false,
        period_ns: 0,
        rel_deadline_ns: 0,
        abs_deadline_ns: u64::MAX,
        budget_ns: 0,
        activations: 0,
        deadline_misses: 0,
        kstack_top: 0,
        domain: 0,
        blocked: false,
        block_reason: 0,
        wait_ep: 0,
        ipc_status: 0,
        ipc_reply: 0,
        ipc_reply_cptr: 0,
    };
}

struct Scheduler {
    tasks: [Task; MAX_TASKS],
    num: usize,
    current: usize,
}

static mut SCHED: Scheduler = Scheduler {
    tasks: [Task::EMPTY; MAX_TASKS],
    num: 0,
    current: 0,
};

/// Per-task stacks. Index 0 is unused (the boot thread runs on the main kernel
/// stack); spawned tasks use 1.. . 16-aligned so initial frames are aligned.
#[repr(C, align(16))]
struct TaskStack([u8; TASK_STACK_SIZE]);
static mut TASK_STACKS: [TaskStack; MAX_TASKS] =
    [const { TaskStack([0; TASK_STACK_SIZE]) }; MAX_TASKS];

/// Register the currently-running boot thread as task 0. Call once, before
/// enabling interrupts.
pub fn init() {
    // SAFETY: single-CPU, interrupts still disabled; sole initializer of SCHED.
    unsafe {
        let s = &mut *core::ptr::addr_of_mut!(SCHED);
        s.tasks[0] = Task {
            rsp: 0,
            present: true,
            runnable: true,
            abs_deadline_ns: u64::MAX,
            kstack_top: crate::stack_top(), // boot thread runs on the main kernel stack
            ..Task::EMPTY
        };
        s.num = 1;
        s.current = 0;
    }
}

/// Build the initial saved-context frame for a new task at the top of
/// `TASK_STACKS[i]` and return its saved-context pointer — exactly what
/// `timer_isr` restores from: 15 zeroed GP registers (r15..rax) followed by the
/// CPU `iretq` frame (RIP/CS/RFLAGS/RSP/SS). Used by `admit` so the verified
/// restore path starts every task identically.
///
/// SAFETY: caller holds exclusive access (IRQs off, single CPU) and `i` is a free
/// slot in `1..MAX_TASKS`.
unsafe fn build_task_frame_inner(
    i: usize,
    entry_va: u64,
    user: bool,
    user_rsp: u64,
    initial_rdi: u64,
) -> u64 {
    let (cs, ss) = if user {
        crate::gdt::user_selectors() // ring 3 (0x23, 0x1b)
    } else {
        crate::gdt::kernel_selectors() // ring 0 (0x08, 0x10)
    };
    let top = task_kstack_top(i);
    let frame_ptr = top - 20 * 8;
    let f = frame_ptr as *mut u64;
    for k in 0..15 {
        f.add(k).write(0);
    }
    f.add(9).write(initial_rdi); // RDI at entry (Memory cptr for ring-3 tasks; 0 else)
    f.add(15).write(entry_va); // RIP
    f.add(16).write(cs as u64); // CS (RPL 3 => iretq returns to ring 3)
    f.add(17).write(0x202); // RFLAGS: IF=1 (preemptible) + reserved bit
    // RSP-8 so the entry begins with RSP%16==8 (SysV; iretq pushes no return addr).
    f.add(18).write(if user { user_rsp - 8 } else { top - 8 });
    f.add(19).write(ss as u64); // SS
    frame_ptr
}

/// Ring-0 task frame (runs on its own kernel stack). Delegates to the shared builder
/// so the 20-u64 layout is byte-identical for ring-0 and ring-3 tasks.
unsafe fn build_task_frame(i: usize, entry: extern "C" fn() -> !) -> u64 {
    build_task_frame_inner(i, entry as *const () as u64, false, 0, 0)
}

/// 16-aligned top of task `i`'s kernel stack (its rsp0 + ring-0 run stack).
unsafe fn task_kstack_top(i: usize) -> u64 {
    (core::ptr::addr_of_mut!(TASK_STACKS[i]) as usize as u64 + TASK_STACK_SIZE as u64) & !0xf
}

/// Find a free task slot in `1..MAX_TASKS`. SAFETY: caller holds exclusive access.
unsafe fn free_slot(s: &Scheduler) -> Option<usize> {
    (1..MAX_TASKS).find(|&i| !s.tasks[i].present)
}

/// Admit a **periodic** kernel task to the EDF run queue, gated by a TimeSlice
/// capability. The cap at `cptr` must validate (type == TimeSlice, live epoch,
/// INVOKE right) via the verified cap-core path — realizing DESIGN.md pillar 6
/// "time is a capability": no live cap, no CPU. Times are in nanoseconds. Returns
/// the task index, or `None` if the cap is invalid/revoked or the table is full.
pub fn admit(
    entry: extern "C" fn() -> !,
    cptr: u16,
    period_ns: u64,
    rel_deadline_ns: u64,
    budget_ns: u64,
) -> Option<usize> {
    // Periodic tasks only: a 0 period never re-arms in roll_deadlines and would let a
    // stale deadline monopolize the CPU. Reject the degenerate input.
    if period_ns == 0 {
        return None;
    }
    // Capability gate (verified): reject unless a live, correctly-typed, INVOKE-righted
    // TimeSlice cap exists. A revoked cap returns ErrRevoked here (live I2 invariant).
    if crate::capspace::admit_check(cptr) != cap_core::table::Status::Ok {
        return None;
    }
    // SAFETY: single-CPU, interrupts disabled during setup; identical frame bytes to
    // spawn (shared builder), published once complete.
    unsafe {
        let s = &mut *core::ptr::addr_of_mut!(SCHED);
        let i = free_slot(s)?;
        let frame_ptr = build_task_frame(i, entry);
        let now = now_ns();
        s.tasks[i] = Task {
            rsp: frame_ptr,
            present: true,
            runnable: true,
            period_ns,
            rel_deadline_ns,
            abs_deadline_ns: now.saturating_add(rel_deadline_ns),
            budget_ns,
            activations: 0,
            deadline_misses: 0,
            kstack_top: task_kstack_top(i),
            domain: 0, // ring-0 kernel task runs in the root domain
            ..Task::EMPTY
        };
        s.num += 1;
        Some(i)
    }
}

/// Admit a **ring-3 (userspace)** periodic task. Same TimeSlice-capability gate as
/// [`admit`], but the task runs at CPL 3: it starts at `user_entry_va` on its own
/// `user_stack_top`, with the Memory capability `mem_cptr` placed in RDI so its first
/// instruction can `cap_invoke` it. Its kernel stack (`TASK_STACKS[i]`) is used only
/// for the iretq frame, syscalls, and preempting interrupts (TSS.rsp0).
#[allow(clippy::too_many_arguments)]
pub fn admit_user(
    user_entry_va: u64,
    user_stack_top: u64,
    mem_cptr: u16,
    cptr: u16,
    domain: u16,
    period_ns: u64,
    rel_deadline_ns: u64,
    budget_ns: u64,
) -> Option<usize> {
    if period_ns == 0 {
        return None;
    }
    if crate::capspace::admit_check(cptr) != cap_core::table::Status::Ok {
        return None;
    }
    // SAFETY: single-CPU, interrupts off during setup; the ring-3 frame uses the same
    // verified 20-u64 layout (user CS/SS + user RSP), published once complete.
    unsafe {
        let s = &mut *core::ptr::addr_of_mut!(SCHED);
        let i = free_slot(s)?;
        let frame_ptr = build_task_frame_inner(i, user_entry_va, true, user_stack_top, mem_cptr as u64);
        let now = now_ns();
        s.tasks[i] = Task {
            rsp: frame_ptr,
            present: true,
            runnable: true,
            period_ns,
            rel_deadline_ns,
            abs_deadline_ns: now.saturating_add(rel_deadline_ns),
            budget_ns,
            activations: 0,
            deadline_misses: 0,
            kstack_top: task_kstack_top(i),
            domain,
            ..Task::EMPTY
        };
        s.num += 1;
        Some(i)
    }
}

/// Called by [`timer_isr`] with the interrupted task's saved-context pointer;
/// returns the next task's saved-context pointer. Also advances the global tick
/// counter and acknowledges the interrupt (EOI) before returning.
extern "C" fn schedule_tick(current_rsp: u64) -> u64 {
    // SAFETY: invoked only from the naked ISR with interrupts disabled (single
    // CPU), so exclusive access to SCHED is guaranteed.
    unsafe {
        crate::apic::TICKS.fetch_add(1, Ordering::Relaxed);
        crate::apic::eoi();

        let s = &mut *core::ptr::addr_of_mut!(SCHED);
        if s.num <= 1 {
            set_kernel_stack(s.tasks[s.current].kstack_top); // defensive (no ring-3 task yet)
            crate::capspace::set_current_domain(s.tasks[s.current].domain);
            return current_rsp; // nothing else to run; resume the same task
        }
        let now = now_ns();
        s.tasks[s.current].rsp = current_rsp; // save outgoing context

        // EDF: release any periodic jobs whose deadline has passed, then run the
        // present+runnable task with the earliest absolute deadline (idle floor if
        // none). Pure function of SCHED state; no allocation, no cap-core, no lock.
        roll_deadlines(s, now);
        let n = pick_next(s);
        s.current = n;
        // Point TSS.rsp0 + PerCpu.kernel_rsp0 at the incoming task's kernel stack so a
        // syscall or privilege-changing interrupt from it lands on ITS stack (never a
        // stale/other task's). Load-bearing for ring-3 correctness.
        set_kernel_stack(s.tasks[n].kstack_top);
        // Publish the incoming task's protection domain so its ring-3 syscalls resolve
        // CPtrs against its own per-domain CapTable.
        crate::capspace::set_current_domain(s.tasks[n].domain);
        s.tasks[n].rsp
    }
}

/// Update both kernel-stack-top references used on a ring3→ring0 entry.
/// SAFETY: single-CPU, interrupts off (called only from schedule_tick).
#[inline]
unsafe fn set_kernel_stack(ktop: u64) {
    crate::gdt::set_rsp0(ktop);
    core::ptr::addr_of_mut!(crate::syscall::PER_CPU.kernel_rsp0).write(ktop);
}

/// Naked APIC-timer ISR: save the full register context, switch stacks via
/// [`schedule_tick`], restore the next task's context, and `iretq` into it.
/// Installed at the timer vector with `set_handler_addr` (it must run on the
/// interrupted task's stack, so it uses no IST).
#[unsafe(naked)]
pub unsafe extern "C" fn timer_isr() {
    naked_asm!(
        // Save GP registers (rax first => highest addr; r15 last => at rsp).
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // rdi = saved-context pointer (captured BEFORE aligning). Then align rsp to 16
        // so schedule_tick is entered ABI-correctly (rsp%16==8 after the call's push) —
        // the interrupted task's rsp may be 8 mod 16. rax = next task's saved-context ptr;
        // we overwrite rsp with it afterwards, so the alignment scratch is discarded.
        "mov rdi, rsp",
        "and rsp, -16",
        "call {tick}",
        "mov rsp, rax",
        // Restore in reverse order (r15 first).
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        "iretq",
        tick = sym schedule_tick,
    )
}

// ---------------- block / wake (portal IPC 4b) ----------------
//
// A task can BLOCK *inside a syscall* (endpoint rendezvous with no partner) and later
// RESUME to finish the syscall and `sysret`. The mechanism (vetted by an adversarial
// design panel) is "twin-frame": a blocked task is saved as the SAME 20-u64 frame the
// timer ISR uses, so it is indistinguishable to the restore path from a preempted
// ring-0 task and is resumable by EITHER the timer OR a peer's block switch with one
// identical pop+iretq tail. There are now THREE producers of that frame layout that
// MUST stay in sync: `build_task_frame_inner`, `timer_isr`'s save, and
// `block_and_switch` below (f[0]=r15 .. f[14]=rax, f[15..20]=RIP/CS/RFLAGS/RSP/SS).

/// Block reasons (diagnostic only).
pub const BLOCK_RECV: u8 = 1;
pub const BLOCK_SEND: u8 = 2;

/// The one new naked routine. Save the caller's context as a standard 20-u64 frame
/// (resumes at `resume_point`, kernel CS/SS, IF=0), store the saved-context pointer
/// through `save_slot` (rdi), then switch to `next_rsp` (rsi) via the proven
/// pop+iretq tail. Entered mid-syscall with IF=0 and kernel GS active.
///
/// GS-NEUTRAL: a `swapgs` BEFORE the switch sets active GS to user(0) — matching every
/// frame the restore tail can land on (ring-3=0, idle=0, blocked-resume re-swaps) — and
/// a `swapgs` at `resume_point` re-establishes kernel GS. Two swapgs per block/resume
/// (even), so per-syscall swapgs parity stays at the proven entry+exit pair.
///
/// SAFETY: called only from `block_current` with IRQs off, single CPU, kernel GS active,
/// and no live Rust `&mut` aliasing the saved-context slot.
#[unsafe(naked)]
unsafe extern "C" fn block_and_switch(save_slot: *mut u64, next_rsp: u64) {
    naked_asm!(
        // rdi = save_slot, rsi = next_rsp (both preserved through the pushes below).
        "mov rax, rsp", // R0: points at our return address (the resume `ret` pops it)
        // iretq tail of the saved frame (high addresses): kernel SS/CS, IF=0, RIP=resume.
        "push 0x10",            // SS  = kernel data
        "push rax",             // RSP = R0
        "push 0x2",             // RFLAGS (IF=0, reserved bit set)
        "push 0x8",             // CS  = kernel code
        "lea rax, [rip + 2f]",
        "push rax",             // RIP = resume_point
        // 15 GP regs in timer_isr order (rax pushed first/highest .. r15 last/lowest).
        // rbx/rbp/r12-r15 carry block_current's live locals; caller-saved slots are dead.
        "push rax", "push rbx", "push rcx", "push rdx", "push rsi", "push rdi",
        "push rbp", "push r8", "push r9", "push r10", "push r11", "push r12",
        "push r13", "push r14", "push r15",
        "mov [rdi], rsp", // *save_slot = saved-context pointer (rdi never modified above)
        "swapgs",         // GS-neutral: active GS -> user(0) before switching away
        "mov rsp, rsi",   // switch to the next task's saved context (rsi never modified)
        // Restore tail (own copy; byte-identical to timer_isr's).
        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10", "pop r9",
        "pop r8", "pop rbp", "pop rdi", "pop rsi", "pop rdx", "pop rcx", "pop rbx",
        "pop rax",
        "iretq",
        "2:",     // resume_point: reached via iretq from a later restore; GS=user(0), IF=0
        "swapgs", // GS-neutral: re-establish kernel GS for the resuming syscall
        "ret",    // pop return address (rsp = R0) -> back into block_current
    )
}

/// Park the current (running) task and switch to the earliest-deadline runnable peer
/// (idle floor guarantees a choice). Returns ONLY after a waker has staged this task's
/// IPC result, cleared `blocked`, and the scheduler later resumes it. MUST be called
/// with interrupts disabled (mid-syscall, IF=0) and with NO cap-table / endpoint lock
/// held (the woken partner re-locks them).
pub fn block_current(reason: u8, ep_oid: u16) {
    debug_assert!(
        !x86_64::instructions::interrupts::are_enabled(),
        "block_current requires IF=0 (the lost-wakeup/two-source proof depends on it)"
    );
    let sched = core::ptr::addr_of_mut!(SCHED);
    // SAFETY: single-CPU, IRQs off; raw-pointer access only, with NO long-lived `&mut`
    // held across the asm switch (the asm writes the saved-ctx ptr through `save_slot`).
    unsafe {
        let cur = (*sched).current;
        (*sched).tasks[cur].blocked = true;
        (*sched).tasks[cur].block_reason = reason;
        (*sched).tasks[cur].wait_ep = ep_oid;

        let now = now_ns();
        roll_deadlines(&mut *sched, now);
        let n = pick_next(&*sched);
        (*sched).current = n;
        let ktop = (*sched).tasks[n].kstack_top;
        set_kernel_stack(ktop);
        crate::capspace::set_current_domain((*sched).tasks[n].domain);

        let next = (*sched).tasks[n].rsp;
        debug_assert!(next != 0, "resuming a task with no saved context");
        let save_slot = core::ptr::addr_of_mut!((*sched).tasks[cur].rsp);
        // No Rust reference is live across this call (all reads above were via raw ptr).
        block_and_switch(save_slot, next);
        // ON RESUME: control returns here. `blocked` was cleared by the waker and our
        // ipc_* fields hold the rendezvous result (read via take_ipc_result by the caller).
    }
}

/// Wake `task`: stage its IPC result, THEN clear `blocked` (deliver-before-unblock — the
/// result is published before pick_next can ever select the woken task). Does not switch;
/// EDF picks the woken task on a later tick. MUST be called with IRQs off.
pub fn unblock(task: usize, status: u64, reply: u64, reply_cptr: u16) {
    if task >= MAX_TASKS {
        return;
    }
    let sched = core::ptr::addr_of_mut!(SCHED);
    // SAFETY: single-CPU, IRQs off; raw scalar writes. Result-before-runnable ordering.
    unsafe {
        (*sched).tasks[task].ipc_status = status;
        (*sched).tasks[task].ipc_reply = reply;
        (*sched).tasks[task].ipc_reply_cptr = reply_cptr;
        (*sched).tasks[task].blocked = false;
    }
}

/// Index of the running task (its protection domain == its capspace table).
pub fn current_task() -> usize {
    // SAFETY: single-CPU, IRQs off when called from the syscall path; scalar read.
    unsafe { (*core::ptr::addr_of!(SCHED)).current }
}

/// Protection-domain id of the currently-running task (diagnostics for fault handlers).
pub fn current_domain_id() -> u16 {
    // SAFETY: single-CPU, IRQs off; scalar reads.
    unsafe {
        let s = &*core::ptr::addr_of!(SCHED);
        s.tasks[s.current].domain
    }
}

/// Read the IPC result staged for task `i` by its waker (status, reply, reply_cptr).
pub fn take_ipc_result(i: usize) -> (u64, u64, u16) {
    if i >= MAX_TASKS {
        return (0, 0, 0);
    }
    // SAFETY: single-CPU, IRQs off; scalar reads.
    unsafe {
        let t = &(*core::ptr::addr_of!(SCHED)).tasks[i];
        (t.ipc_status, t.ipc_reply, t.ipc_reply_cptr)
    }
}

// ---------------- crash-only termination (domain supervision) ----------------
//
// A ring-3 fault terminates just that task; the kernel and other domains live on. The
// faulting task is ABANDONED (not saved) and we switch into the next runnable task.
// `jump_to_task` is the restore HALF of `block_and_switch` with NO swapgs: a ring-3
// exception enters with GS=user(0) (exceptions don't auto-swapgs) and IF=0 (interrupt
// gate), and every switch target expects active GS=user(0) (ring3/idle run there; a
// 4b-blocked task re-swaps at its resume_point) — so the GS-neutral invariant holds.

/// Load `next_rsp` and restore that task via the proven 15-pop + iretq tail. Never
/// returns (the caller's — the dead task's — context is discarded).
///
/// SAFETY: called only from `terminate_current` with IRQs off, single CPU, GS=user(0)
/// (post ring-3 exception), and `next_rsp` a valid saved 20-u64 context.
#[unsafe(naked)]
unsafe extern "C" fn jump_to_task(next_rsp: u64) -> ! {
    naked_asm!(
        "mov rsp, rdi", // rdi = next_rsp; discard the faulted task's stack entirely
        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10", "pop r9",
        "pop r8", "pop rbp", "pop rdi", "pop rsi", "pop rdx", "pop rcx", "pop rbx",
        "pop rax",
        "iretq", // GS already user(0) — no swapgs needed (see module note above)
    )
}

/// Terminate the currently-running (faulted) ring-3 task and switch to the earliest-
/// deadline runnable peer (idle floor guarantees a choice). Frees the task slot,
/// **revokes the dead domain's authority** + scrubs its endpoint parking
/// (`capspace::reap_domain`), then jumps into the next task — NEVER returning to the
/// faulting instruction. Call ONLY for a ring-3 fault (GS=user(0), IF=0).
pub fn terminate_current() -> ! {
    let sched = core::ptr::addr_of_mut!(SCHED);
    // SAFETY: single-CPU, IRQs off (interrupt-gate handler); raw access, no &mut across
    // the asm switch; the dead task is discarded so nothing is saved for it.
    unsafe {
        let cur = (*sched).current;
        let domain = (*sched).tasks[cur].domain;

        // Free the slot so pick_next/roll_deadlines skip it.
        (*sched).tasks[cur].present = false;
        (*sched).tasks[cur].runnable = false;
        (*sched).tasks[cur].blocked = false;
        if (*sched).num > 0 {
            (*sched).num -= 1;
        }

        // Revoke the dead domain's caps (clear its CapTable) and scrub any endpoint
        // where it was the parked peer (so no survivor wakes a dead task).
        crate::capspace::reap_domain(domain, cur as u16);

        // Crash-only self-healing: the supervisor may re-admit a fresh instance of this
        // domain (fresh caps, reusing the freed slot) under a restart budget. Runs here —
        // after reap, before pick_next — so a restarted task is eligible immediately. No
        // live &mut SCHED is held across this call (all reads above were via raw ptr).
        crate::supervisor::on_domain_death(domain);

        // Choose the next runnable task and switch into it (idle is the floor).
        let now = now_ns();
        roll_deadlines(&mut *sched, now);
        let n = pick_next(&*sched);
        (*sched).current = n;
        set_kernel_stack((*sched).tasks[n].kstack_top);
        crate::capspace::set_current_domain((*sched).tasks[n].domain);
        let next = (*sched).tasks[n].rsp;
        jump_to_task(next)
    }
}

/// Current time in nanoseconds, derived from the single clock source (apic::TICKS).
#[inline]
fn now_ns() -> u64 {
    crate::apic::TICKS
        .load(core::sync::atomic::Ordering::Relaxed)
        .saturating_mul(crate::apic::tick_ns())
}

/// Release new periodic jobs whose absolute deadline has passed: advance the deadline
/// by whole periods and mark runnable. Bounded catch-up; saturating to avoid overflow.
fn roll_deadlines(s: &mut Scheduler, now: u64) {
    for i in 1..MAX_TASKS {
        let t = &mut s.tasks[i];
        if !t.present || t.period_ns == 0 {
            continue;
        }
        while t.abs_deadline_ns <= now {
            t.activations += 1;
            // Consecutive job deadlines are spaced by the PERIOD (job k: release0 + k*T,
            // deadline release_k + D), independent of D — so always advance by period_ns.
            // (Advancing by max(D,T) over-spaces deadlines when D > T.)
            t.abs_deadline_ns = t.abs_deadline_ns.saturating_add(t.period_ns);
            // Do NOT re-arm a task parked in a blocking syscall: its `runnable` must stay
            // gated by `blocked` until its IPC partner wakes it (else a periodic re-arm
            // would resurrect a mid-rendezvous task). Deadline/activations still advance.
            if !t.blocked {
                t.runnable = true;
            }
        }
    }
}

/// EDF selection: the present+runnable task with the smallest absolute deadline.
/// Tie-break: lowest index (strict `<` while scanning ascending). Task 0 (idle,
/// abs_deadline = u64::MAX, always runnable) is the floor — chosen only when no real
/// task is runnable. Bounded O(MAX_TASKS); no allocation, no lock.
fn pick_next(s: &Scheduler) -> usize {
    let mut best = 0usize; // idle fallback (task 0)
    let mut best_dl = u64::MAX;
    let mut found_real = false;
    for i in 1..MAX_TASKS {
        let t = &s.tasks[i];
        if !t.present || !t.runnable || t.blocked {
            continue;
        }
        // Any present+runnable real task beats idle — even one whose deadline has
        // saturated to u64::MAX (so it is never starved by the idle floor). Among real
        // tasks: earliest absolute deadline, lowest index on ties (strict `<`, ascending).
        if !found_real || t.abs_deadline_ns < best_dl {
            best = i;
            best_dl = t.abs_deadline_ns;
            found_real = true;
        }
    }
    best
}
