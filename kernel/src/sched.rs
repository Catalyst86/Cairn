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

const MAX_TASKS: usize = 4;
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
unsafe fn build_task_frame(i: usize, entry: extern "C" fn() -> !) -> u64 {
    let (code_sel, data_sel) = crate::gdt::kernel_selectors();
    let base = core::ptr::addr_of_mut!(TASK_STACKS[i]) as usize as u64;
    let top = (base + TASK_STACK_SIZE as u64) & !0xf;
    let frame_ptr = top - 20 * 8;
    let f = frame_ptr as *mut u64;
    for k in 0..15 {
        f.add(k).write(0);
    }
    f.add(15).write(entry as usize as u64); // RIP
    f.add(16).write(code_sel as u64); // CS
    f.add(17).write(0x202); // RFLAGS: IF=1 (preemptible) + reserved bit
    // RSP = top-8 so the entry fn begins with RSP%16==8, as the SysV ABI requires
    // (iretq pushes no return address, unlike a `call`, so we must pre-offset by 8).
    f.add(18).write(top - 8);
    f.add(19).write(data_sel as u64); // SS
    frame_ptr
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
        s.tasks[n].rsp
    }
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
            t.runnable = true;
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
        if !t.present || !t.runnable {
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
