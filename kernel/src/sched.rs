//! Minimal preemptive round-robin scheduler for ring-0 kernel tasks.
//!
//! This is the foundation the EDF policy (next, Grok's lane) will plug into: it
//! owns the task table, per-task stacks, the context switch, and the timer ISR
//! that drives preemption. The *policy* here is trivial round-robin; swapping in
//! deadline-ordered selection only changes `pick_next`.
//!
//! ## How a switch works
//! The APIC timer fires the naked [`timer_isr`], which saves the interrupted
//! task's full register set, calls [`schedule_tick`] with the saved-context
//! pointer, sets RSP to the next task's saved-context pointer, restores its
//! registers, and `iretq`s into it. A task's "saved context" is just a region of
//! its own stack holding 15 pushed GP registers followed by the CPU's `iretq`
//! frame (RIP/CS/RFLAGS/RSP/SS) — identical in layout for a preempted task and a
//! freshly [`spawn`]ed one, so the same restore path starts both.
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
    /// Saved-context pointer (points at the lowest saved GP register). For task 0
    /// (the boot thread) this is filled on its first preemption.
    rsp: u64,
    present: bool,
}

impl Task {
    const EMPTY: Task = Task { rsp: 0, present: false };
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
        s.tasks[0] = Task { rsp: 0, present: true };
        s.num = 1;
        s.current = 0;
    }
}

/// Spawn a kernel task that runs `entry` (which must never return). Returns false
/// if the task table is full. Call before enabling interrupts.
pub fn spawn(entry: extern "C" fn() -> !) -> bool {
    // SAFETY: single-CPU, interrupts disabled during setup; builds a fresh frame
    // in an otherwise-unused per-task stack and publishes it once complete.
    unsafe {
        let s = &mut *core::ptr::addr_of_mut!(SCHED);
        let mut slot = None;
        for i in 1..MAX_TASKS {
            if !s.tasks[i].present {
                slot = Some(i);
                break;
            }
        }
        let i = match slot {
            Some(i) => i,
            None => return false,
        };

        let (code_sel, data_sel) = crate::gdt::kernel_selectors();
        let base = core::ptr::addr_of_mut!(TASK_STACKS[i]) as usize as u64;
        let top = (base + TASK_STACK_SIZE as u64) & !0xf;

        // Build the initial saved context at the top of the stack: 15 zeroed GP
        // registers (r15..rax, the order timer_isr pops them) followed by the
        // iretq frame. frame_ptr is what timer_isr will `mov rsp,` and pop from.
        let frame_ptr = top - 20 * 8;
        let f = frame_ptr as *mut u64;
        for k in 0..15 {
            f.add(k).write(0);
        }
        f.add(15).write(entry as usize as u64); // RIP
        f.add(16).write(code_sel as u64); // CS
        f.add(17).write(0x202); // RFLAGS: IF=1 (preemptible) + reserved bit
        f.add(18).write(top); // RSP (task runs on its own stack)
        f.add(19).write(data_sel as u64); // SS

        s.tasks[i] = Task {
            rsp: frame_ptr,
            present: true,
        };
        s.num += 1;
        true
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
        s.tasks[s.current].rsp = current_rsp;

        let mut n = s.current;
        loop {
            n = (n + 1) % MAX_TASKS;
            if s.tasks[n].present {
                break;
            }
        }
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
        // rdi = saved-context pointer; rax = next task's saved-context pointer.
        "mov rdi, rsp",
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
