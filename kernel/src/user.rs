//! Ring-3 user-mode demo blobs + page setup for the portal-IPC (4b) smoke test.
//!
//! Two tiny position-independent ring-3 programs exercise the Endpoint IPC ABI across
//! two protection domains:
//!   - `client_main` (domain 1): `E_SEND` a message word AND grant (transfer) a Memory
//!     capability over the endpoint, then try to use the granted-away cap (now gone).
//!   - `server_main` (domain 2): `E_RECV` the message + the moved cap, then `M_ALLOC`
//!     via the received cap to prove it works in its domain; loops back to `E_RECV`
//!     (which then blocks forever — no more senders).
//!
//! Both blobs are PIC (immediates + `syscall` + relative `jmp` only) so they can be
//! copied to fixed user VAs. CPtr slots are by *delegation order* in `main.rs`:
//! each domain's endpoint cap is delegated first (slot 0, passed in RDI), and the
//! client's grantable Memory cap second (slot 1).

use crate::paging::map_user_page;

// The two ring-3 blobs. Compiled position-independent; copied (not executed) from the
// kernel image into user-mapped executable pages. See module docs for the cptr layout.
core::arch::global_asm!(
    // ---- client (domain 1): send msg + grant a Memory cap, then touch the moved slot ----
    ".global client_main",
    ".p2align 4",
    "client_main:",
    "    mov rbx, rdi",        // rbx = endpoint cptr (slot 0, passed in RDI)
    "    mov rax, 1",          // SYS_CAP_INVOKE
    "    mov rdi, rbx",        // cptr = endpoint
    "    mov rsi, 1",          // method = E_SEND
    "    mov rdx, 0xCA11",     // arg0 = message word
    "    xor r10d, r10d",
    "    xor r8d, r8d",
    "    mov r9, 1",           // transfer cptr = grantable Memory cap (slot 1)
    "    syscall",
    "    mov rax, 1",          // now prove MOVE: the granted-away slot 1 no longer resolves
    "    mov rdi, 1",
    "    mov rsi, 1",          // method = M_ALLOC (expect ErrBadCPtr)
    "    xor edx, edx",
    "    xor r10d, r10d",
    "    xor r8d, r8d",
    "    mov r9, 0xffff",      // transfer = CPTR_NULL
    "    syscall",
    "1:",
    "    jmp 1b",
    ".global client_main_end",
    "client_main_end:",
    // ---- server (domain 2): recv msg + moved cap, alloc via it, then recv again ----
    ".global server_main",
    ".p2align 4",
    "server_main:",
    "    mov rbx, rdi",        // rbx = endpoint cptr (slot 0)
    "2:",
    "    mov rax, 1",          // SYS_CAP_INVOKE
    "    mov rdi, rbx",        // cptr = endpoint
    "    mov rsi, 2",          // method = E_RECV (blocks until a sender arrives)
    "    xor edx, edx",
    "    xor r10d, r10d",
    "    xor r8d, r8d",
    "    mov r9, 0xffff",      // transfer = CPTR_NULL
    "    syscall",
    "    mov r12, rdx",        // r12 = received cptr (the moved Memory cap, or 0)
    "    mov rax, 1",
    "    mov rdi, r12",        // cptr = received cap
    "    mov rsi, 1",          // method = M_ALLOC (prove the moved cap works in domain 2)
    "    xor edx, edx",
    "    xor r10d, r10d",
    "    xor r8d, r8d",
    "    mov r9, 0xffff",
    "    syscall",
    "    jmp 2b",              // loop: next E_RECV blocks forever (no more senders)
    ".global server_main_end",
    "server_main_end:",
);

extern "C" {
    pub fn client_main();
    pub fn client_main_end();
    pub fn server_main();
    pub fn server_main_end();
}

/// Map a read-only executable user code page at `code_va` and a writable NX user stack
/// page at `stack_va`, copy the PIC `blob` (whose end is `blob_end`) into the code page
/// via its HHDM alias, and return `(entry_va, stack_top)` where `stack_top = stack_va +
/// 4096`. The caller places these into the ring-3 task's initial context (RIP/RSP) with
/// the endpoint cptr in RDI.
///
/// SAFETY: single-CPU, interrupts off, early boot; `code_va`/`stack_va` are fresh user
/// VAs not already mapped, and the blob fits in one page.
pub fn setup_user_task(
    hhdm: u64,
    code_va: u64,
    stack_va: u64,
    blob: unsafe extern "C" fn(),
    blob_end: unsafe extern "C" fn(),
) -> Option<(u64, u64)> {
    // Code page: USER | PRESENT | (no WRITABLE) | (EXEC allowed) — W^X.
    let code_fnum = map_user_page(hhdm, code_va, /*writable=*/ false, /*exec=*/ true)?;
    // Stack page: USER | PRESENT | WRITABLE | NO_EXECUTE.
    let _stack_fnum = map_user_page(hhdm, stack_va, /*writable=*/ true, /*exec=*/ false)?;

    let len = (blob_end as *const () as usize) - (blob as *const () as usize);

    // SAFETY: copy the PIC blob into the freshly allocated code frame via the kernel HHDM
    // alias (the user VA is mapped read-only). `len` is exact from the end label; the
    // source is a static from global_asm in our own image; single-CPU, no concurrent
    // accessors, no user TLB entry for this VA yet.
    unsafe {
        core::ptr::copy_nonoverlapping(
            blob as *const () as *const u8,
            (hhdm + code_fnum * 4096) as *mut u8,
            len,
        );
    }

    crate::serial_println!(
        "user: mapped code @{:#x} (U,X,RO) stack @{:#x} (U,W,NX), blob {} bytes",
        code_va, stack_va, len
    );

    Some((code_va, stack_va + 0x1000))
}
