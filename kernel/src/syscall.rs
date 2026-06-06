//! `syscall`/`sysret` fast system-call path + the ring-3 → kernel cap_invoke ABI.
//!
//! On `syscall` from ring 3 the CPU loads kernel CS/SS from STAR, puts the user
//! RIP in RCX and user RFLAGS in R11, masks RFLAGS by SFMASK (so **IF=0** — the
//! timer cannot preempt us in the entry window), but leaves RSP pointing at the
//! *user* stack and GS at the *user* base. The naked [`syscall_entry`] stub
//! `swapgs`es to the per-CPU block, switches to *this task's* kernel stack
//! (`PER_CPU.kernel_rsp0`, kept current by the scheduler), marshals the CAP_ABI
//! registers into the SysV ABI, calls [`syscall_dispatch`], packs the result back
//! into the CAP_ABI output registers, zeroes any kernel-derived scratch (anti-leak),
//! and `sysretq`s to ring 3.
//!
//! v0 keeps the whole syscall non-preemptible (IF stays 0): `cap_invoke` is short
//! and non-blocking, so there is no re-entrancy on the per-task kernel stack and no
//! lock the timer ISR could contend. cap-core is used unchanged (no Kani re-run).

use core::arch::naked_asm;
use core::sync::atomic::{AtomicU64, Ordering};
use cap_core::table::Status;

/// Per-CPU block reached via `gs:` after `swapgs`. Field offsets are load-bearing
/// for the asm stub: `kernel_rsp0` @ 0, `user_rsp_scratch` @ 8, `reply_cptr` @ 16.
#[repr(C)]
pub struct PerCpu {
    pub kernel_rsp0: u64,
    pub user_rsp_scratch: u64,
    /// IPC reply CPtr (a transferred capability's destination slot, or `CPTR_NULL` if
    /// none), written by `syscall_dispatch` and read by the entry stub's epilogue into rdx.
    pub reply_cptr: u64,
}

/// Single-CPU: one per-CPU block. `kernel_rsp0` is kept equal to the current
/// task's kernel-stack top by the scheduler (mirrors TSS.rsp0).
pub static mut PER_CPU: PerCpu = PerCpu {
    kernel_rsp0: 0,
    user_rsp_scratch: 0,
    reply_cptr: 0,
};

/// The only syscall number in v0 (no ambient authority: everything is cap_invoke).
pub const SYS_CAP_INVOKE: u64 = 1;
/// Sentinel "no capability" transfer slot.
pub const CPTR_NULL: u64 = 0xffff;

/// SysV returns a 2-eightbyte integer struct in RAX:RDX, so `status`→RAX, `reply`→RDX.
#[repr(C)]
pub struct SyscallRet {
    pub status: u64,
    pub reply: u64,
}

/// Arm the syscall/sysret machinery. Call AFTER `gdt::init()` (STAR is validated
/// against the GDT layout) and BEFORE enabling interrupts.
pub fn init() {
    use x86_64::registers::model_specific::{Efer, EferFlags, GsBase, KernelGsBase, LStar, SFMask, Star};
    use x86_64::registers::rflags::RFlags;
    use x86_64::structures::gdt::SegmentSelector;
    use x86_64::VirtAddr;

    let (ucs, uss) = crate::gdt::user_selectors(); // (0x23, 0x1b)
    let (kcs, kss) = crate::gdt::kernel_selectors(); // (0x08, 0x10)

    // STAR: user (sysret) + kernel (syscall) selector bases. Panics loudly if the
    // GDT order is wrong (cs_sysret-16 == ss_sysret-8, ss_syscall == cs_syscall+8).
    Star::write(
        SegmentSelector(ucs),
        SegmentSelector(uss),
        SegmentSelector(kcs),
        SegmentSelector(kss),
    )
    .expect("STAR: GDT segment layout invalid for syscall/sysret");

    // LSTAR: the entry RIP. SFMASK: clear IF (atomic entry — most important bit),
    // DF (SysV string ABI), TF (no single-step into kernel), AC (defeat a user
    // AC=1 SMAP-bypass), NT (defensive).
    LStar::write(VirtAddr::new(syscall_entry as *const () as u64));
    SFMask::write(
        RFlags::INTERRUPT_FLAG
            | RFlags::DIRECTION_FLAG
            | RFlags::TRAP_FLAG
            | RFlags::ALIGNMENT_CHECK
            | RFlags::NESTED_TASK,
    );

    // GS bases: KERNEL_GS_BASE = &PER_CPU (loaded by swapgs on entry); user GS = 0.
    KernelGsBase::write(VirtAddr::new(core::ptr::addr_of!(PER_CPU) as u64));
    GsBase::write(VirtAddr::new(0));

    // Enable SCE (syscall/sysret) + NXE (honor the NX bit on user pages). OR-in only
    // so we do not clobber LME/LMA that Limine set.
    // SAFETY: enabling architectural features; we preserve all other EFER bits.
    unsafe {
        Efer::update(|f| {
            f.insert(EferFlags::SYSTEM_CALL_EXTENSIONS | EferFlags::NO_EXECUTE_ENABLE);
        });
    }

    crate::serial_println!(
        "syscall: armed (EFER.SCE+NXE); STAR uCS={:#x} uSS={:#x} kCS={:#x} kSS={:#x}; LSTAR={:#x}",
        ucs,
        uss,
        kcs,
        kss,
        syscall_entry as *const () as u64
    );
}

/// Naked syscall entry. See module docs for the full register protocol.
///
/// SAFETY: installed in LSTAR; entered only by the CPU on `syscall` from ring 3.
#[unsafe(naked)]
pub unsafe extern "C" fn syscall_entry() {
    naked_asm!(
        // Switch GS to the kernel per-CPU block; stash user RSP; load this task's
        // kernel stack. (IF is already 0 via SFMASK, so this is atomic vs the timer.)
        "swapgs",
        "mov qword ptr gs:[8], rsp", // PerCpu.user_rsp_scratch = user RSP
        "mov rsp, qword ptr gs:[0]", // RSP = PerCpu.kernel_rsp0 (16-aligned)
        // Save the bits sysret needs + the user RSP onto the kernel stack.
        "push qword ptr gs:[8]", // user RSP
        "push r11",              // user RFLAGS
        "push rcx",              // user RIP   (frees rcx for marshalling)
        "push r9",               // 7th SysV arg (transfer cptr) on the stack
        // Marshal CAP_ABI (rax=nr, rdi=cptr, rsi=method, rdx=arg0, r10=arg1, r8=arg2)
        // into SysV args (rdi,rsi,rdx,rcx,r8,r9). Order avoids clobber-before-use.
        "mov r9, r8",   // arg2  -> 6th
        "mov r8, r10",  // arg1  -> 5th
        "mov rcx, rdx", // arg0  -> 4th
        "mov rdx, rsi", // method-> 3rd
        "mov rsi, rdi", // cptr  -> 2nd
        "mov rdi, rax", // nr    -> 1st
        "call {dispatch}", // -> SyscallRet { status: rax, reply: rdx }
        "add rsp, 8",      // pop the 7th-arg slot
        // Pack outputs (CAP_ABI: rax=status, rsi=reply, rdx=reply_cptr).
        "mov rsi, rdx",               // reply -> rsi
        "mov rdx, qword ptr gs:[16]", // reply_cptr (PerCpu.reply_cptr) -> rdx; kernel GS still active
        // Restore user context for sysret.
        "pop rcx",  // user RIP    -> RCX
        "pop r11",  // user RFLAGS -> R11 (restores IF=1)
        "pop rdi",  // user RSP    -> RDI (temp)
        // Anti-leak: zero caller-saved scratch that held kernel-derived values.
        // Returns kept: rax/rsi/rdx; rcx/r11 consumed by sysret. Callee-saved
        // (rbx,rbp,r12-r15) still hold the USER's values (dispatch preserved them).
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",
        "mov rsp, rdi", // restore user RSP
        "xor edi, edi", // zero last kernel-derived temp
        "swapgs",       // GS back to user
        "sysretq",      // -> ring 3 (CS=0x23, SS=0x1b, RIP=RCX, RFLAGS=R11)
        dispatch = sym syscall_dispatch,
    )
}

/// Throttle for the demo proof prints.
static SYSCALL_REPORTS: AtomicU64 = AtomicU64::new(0);

/// The Rust side of the syscall: the ONLY place untrusted ring-3 inputs are seen.
/// All arguments are scalars passed by value — the kernel never dereferences a user
/// pointer — and the CPtr/method are range-checked before truncation, then handed to
/// the verified cap-core wiring (which re-checks epoch/type/rights and bounds the
/// slot). cap-core is unchanged.
pub extern "C" fn syscall_dispatch(
    nr: u64,
    cptr: u64,
    method: u64,
    arg0: u64,
    _arg1: u64,
    _arg2: u64,
    xfer: u64,
) -> SyscallRet {
    // Anti-leak default: no transferred cap (CPTR_NULL sentinel, not slot 0). The IPC
    // path overwrites this just before returning (staged per-task across a block, never
    // relying on PER_CPU across yields).
    // SAFETY: single-CPU, IRQs off; direct static write (not gs-relative).
    unsafe {
        core::ptr::addr_of_mut!(PER_CPU.reply_cptr).write(CPTR_NULL);
    }

    if nr != SYS_CAP_INVOKE {
        return SyscallRet { status: Status::ErrMethod as u64, reply: 0 };
    }
    if cptr > u16::MAX as u64 || method > u16::MAX as u64 || xfer > u16::MAX as u64 {
        return SyscallRet { status: Status::ErrBadCPtr as u64, reply: 0 };
    }

    // The xfer slot is honored only for Endpoint targets (capspace enforces this and
    // rejects a stray xfer on any other object). Capability transfer + blocking
    // rendezvous live in cap_invoke_ipc, which returns (status, reply, reply_cptr).
    let (st, reply, reply_cptr) =
        crate::capspace::cap_invoke_ipc(cptr as u16, method as u16, arg0, xfer as u16);

    // SAFETY: single-CPU, IRQs off; direct static write read back by the epilogue.
    unsafe {
        core::ptr::addr_of_mut!(PER_CPU.reply_cptr).write(reply_cptr as u64);
    }

    // Proof print (throttled): a ring-3 task reached the cap path via syscall.
    let n = SYSCALL_REPORTS.fetch_add(1, Ordering::Relaxed);
    if n < 12 {
        crate::serial_println!(
            "ring3 syscall #{}: cap_invoke(cptr={}, method={}) => status={} reply={:#x} rcptr={}",
            n + 1,
            cptr,
            method,
            st,
            reply,
            reply_cptr
        );
    }

    SyscallRet { status: st, reply }
}
