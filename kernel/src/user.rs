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
    // ---- faulter (own domain): do one real syscall, then deliberately fault (#UD) ----
    // Proves crash-only supervision: the kernel terminates JUST this domain and lives on.
    ".global faulter_main",
    ".p2align 4",
    "faulter_main:",
    "    mov rbx, rdi",        // rbx = its Memory cptr (slot 0 in its domain)
    "    mov rax, 1",          // SYS_CAP_INVOKE
    "    mov rdi, rbx",        // cptr = Memory cap
    "    mov rsi, 1",          // method = M_ALLOC (proves it ran in ring 3)
    "    xor edx, edx",
    "    xor r10d, r10d",
    "    xor r8d, r8d",
    "    mov r9, 0xffff",      // transfer = CPTR_NULL
    "    syscall",
    "    ud2",                 // deliberate invalid opcode => #UD => crash-only terminate
    ".global faulter_main_end",
    "faulter_main_end:",
    // ---- virtio driver (domain 5, INC7): ZERO-syscall block I/O over a granted DeviceQueue --
    // The kernel has already DQ_MAP'd the virtqueue rings + buffer + doorbell at DQ_BASE
    // (0x100_0000) and baked params into the buffer tail (buf_va+2048): [0]=buf_phys,
    // [8]=scratch_lba, [16]=qsize, [24]=doorbell_off. This blob replicates virtio_blk::submit
    // for a single-sector READ of scratch_lba — building the descriptor chain, publishing
    // avail, ringing the doorbell, polling used — entirely via the mapped USER pages with NO
    // syscalls, then makes ONE cap_invoke(DQ_REPORT) so the kernel can validate + log the proof.
    // DUAL ADDRESSING: descriptor addr fields are RAW guest-phys (buf_phys from params); every
    // pointer dereference uses the USER VA (rbp-relative). rbp = DQ_BASE throughout (PIC).
    ".global driver_main",
    ".p2align 4",
    "driver_main:",
    "    mov rbx, rdi",                       // rbx = DeviceQueue cptr (slot 0); survives syscall
    "    mov rbp, 0x1000000",                 // rbp = DQ_BASE (keep in sync with capspace::DQ_BASE)
    "    mov r12, [rbp+0x3800]",              // r12 = buf_phys  (params @ buf_va+2048)
    "    mov r13, [rbp+0x3808]",              // r13 = scratch_lba
    "    mov r14, [rbp+0x3810]",              // r14 = qsize (low 16 used)
    "    mov r15, [rbp+0x3818]",              // r15 = doorbell sub-page offset
    // desc0 (hdr): addr=buf_phys, len=16, flags=NEXT(1), next=1
    "    mov [rbp+0x00], r12",
    "    mov dword ptr [rbp+0x08], 16",
    "    mov word ptr [rbp+0x0c], 1",
    "    mov word ptr [rbp+0x0e], 1",
    // desc1 (data): addr=buf_phys+512, len=512, flags=NEXT|WRITE(3) (device writes — it's a read), next=2
    "    lea rax, [r12+512]",
    "    mov [rbp+0x10], rax",
    "    mov dword ptr [rbp+0x18], 512",
    "    mov word ptr [rbp+0x1c], 3",
    "    mov word ptr [rbp+0x1e], 2",
    // desc2 (status): addr=buf_phys+1024, len=1, flags=WRITE(2), next=0
    "    lea rax, [r12+1024]",
    "    mov [rbp+0x20], rax",
    "    mov dword ptr [rbp+0x28], 1",
    "    mov word ptr [rbp+0x2c], 2",
    "    mov word ptr [rbp+0x2e], 0",
    // request header @ buf_va (rbp+0x3000): type=VIRTIO_BLK_T_IN(0), reserved=0, sector=scratch_lba
    "    mov dword ptr [rbp+0x3000], 0",
    "    mov dword ptr [rbp+0x3004], 0",
    "    mov [rbp+0x3008], r13",
    "    mov byte ptr [rbp+0x3400], 0xff",    // status (buf+1024) = 0xFF (device overwrites with 0)
    // publish avail (rbp+0x1000): ring[avail_idx % qsize] = head desc 0; idx = avail_idx + 1
    "    movzx eax, word ptr [rbp+0x1002]",   // eax = avail_idx
    "    mov ecx, eax",                        // ecx = avail_idx (saved)
    "    xor edx, edx",                        // dividend high = 0
    "    div r14w",                            // dx = avail_idx % qsize (slot); ax = quotient
    "    movzx r9d, dx",                       // r9 = slot
    "    mov word ptr [rbp+r9*2+0x1004], 0",   // avail.ring[slot] = 0
    "    lea eax, [ecx+1]",                    // target = avail_idx + 1
    "    mov word ptr [rbp+0x1002], ax",       // avail.idx = target
    "    mov ecx, eax",                        // ecx = target (poll compare)
    // ring doorbell (rbp+0x4000 + doorbell_off): 16-bit store of queue index 0 (NO_CACHE page)
    "    lea r10, [rbp+r15+0x4000]",
    "    mov word ptr [r10], 0",
    // poll used.idx (rbp+0x2002) until == target, BOUNDED (a mis-mapped doorbell must not spin
    // forever — on timeout we still report, so the proof line always prints, just match=false)
    "    mov r11, 100000000",                  // poll bound (caller-saved; only used pre-syscall)
    "3:",
    "    movzx eax, word ptr [rbp+0x2002]",
    "    cmp ax, cx",
    "    je 4f",
    "    pause",
    "    dec r11",
    "    jnz 3b",
    "4:",
    // read back the first 8 bytes the device DMA'd into data (buf+512 = rbp+0x3200)
    "    mov rdx, [rbp+0x3200]",               // arg0 = magic read from disk (or sentinel on timeout)
    // ONE syscall: cap_invoke(dq, DQ_REPORT=3, arg0=magic) — the kernel validates + logs
    "    mov rax, 1",                          // SYS_CAP_INVOKE
    "    mov rdi, rbx",                        // cptr = DeviceQueue
    "    mov rsi, 3",                          // method = DQ_REPORT
    "    xor r10d, r10d",
    "    xor r8d, r8d",
    "    mov r9, 0xffff",                      // xfer = CPTR_NULL
    "    syscall",
    "    ud2",                                 // done — v0 has no voluntary-exit syscall (crash-only reap)
    ".global driver_main_end",
    "driver_main_end:",
);

extern "C" {
    pub fn client_main();
    pub fn client_main_end();
    pub fn server_main();
    pub fn server_main_end();
    pub fn faulter_main();
    pub fn faulter_main_end();
    pub fn driver_main();
    pub fn driver_main_end();
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
