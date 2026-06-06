//! Ring-3 user-mode demo blob and page setup for the early cap-invoke smoke test.
//!
//! Provides a tiny position-independent ring-3 program (via global_asm) that
//! exercises the cap_invoke ABI a bounded number of times, plus the helper that
//! maps its code and stack pages with appropriate USER|W^X flags and copies the
//! blob in.
//!
//! Claude wires the returned (entry, stack_top) into the initial task context
//! and performs the first cap_invoke to grant the Memory cap; this module only
//! prepares the address space.

use crate::paging::map_user_page;

/// The ring-3 demo blob. Compiled position-independent; copied (not executed)
/// from the kernel image into a user-mapped executable page.
core::arch::global_asm!(
    ".global user_main",
    ".p2align 4",
    "user_main:",
    "    mov rbx, rdi",          // save Memory cptr (callee-saved, survives syscalls)
    "    mov r12, 8",            // bounded number of ALLOC calls
    "1:",
    "    test r12, r12",
    "    jz 2f",
    "    dec r12",
    "    mov rax, 1",            // SYS_CAP_INVOKE
    "    mov rdi, rbx",          // cptr = Memory cap
    "    mov rsi, 1",            // method = M_ALLOC
    "    xor edx, edx",          // arg0 = 0
    "    xor r10d, r10d",        // arg1
    "    xor r8d, r8d",          // arg2
    "    mov r9, 0xffff",        // transfer = CPTR_NULL
    "    syscall",
    "    jmp 1b",
    "2:",
    "    jmp 2b",
    ".global user_main_end",
    "user_main_end:",
);

extern "C" {
    /// The ring-3 blob (defined in global_asm). Its ADDRESS in the kernel image is the
    /// source we copy from; it is NOT called directly.
    pub fn user_main();
    /// End label used only to compute the exact byte length of the blob for the copy.
    pub fn user_main_end();
}

/// Map a read-only executable user code page at 0x400000 and a writable NX user
/// stack page at 0x7ff000, copy the demo blob into the code page via its HHDM
/// alias, and return the entry VA and initial user stack top.
///
/// The caller (main) will place the returned values into the initial ring-3
/// context (RIP=entry, RSP=stack_top) along with a Memory capability in RDI.
///
/// SAFETY: single-CPU, interrupts disabled, early boot; the VAs are known-fresh
/// and not present in the Limine tables. We map via HHDM for the zero and copy
/// because the user VAs may have W or X restrictions from the kernel side.
pub fn setup_user_demo(hhdm: u64) -> Option<(u64, u64)> {
    // Code page: USER | PRESENT | (no WRITABLE) | (EXEC allowed, i.e. no NO_EXECUTE)
    let code_fnum = map_user_page(hhdm, 0x40_0000, /*writable=*/ false, /*exec=*/ true)?;

    // Stack page: USER | PRESENT | WRITABLE | NO_EXECUTE
    let _stack_fnum = map_user_page(hhdm, 0x7f_f000, /*writable=*/ true, /*exec=*/ false)?;

    let len = (user_main_end as usize) - (user_main as usize);

    // SAFETY: we are writing into the freshly allocated frame that backs the
    // user code page. We use the kernel HHDM alias (writable from our CPL0 view)
    // rather than the user VA 0x400000 (which we mapped read-only for W^X).
    // The source is a static from global_asm in our own image; len is exact
    // from the end label. Single-CPU, no concurrent accessors, TLB not yet
    // holding any user translation for this VA.
    unsafe {
        core::ptr::copy_nonoverlapping(
            user_main as *const u8,
            (hhdm + code_fnum * 4096) as *mut u8,
            len,
        );
    }

    crate::serial_println!(
        "user: mapped code @0x400000 (U,X,RO) stack @0x800000 (U,W,NX), blob {} bytes",
        len
    );

    Some((0x40_0000, 0x80_0000))
}
