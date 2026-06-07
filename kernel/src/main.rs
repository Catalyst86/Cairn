//! Keystone v0 — the Cairn bare-metal microkernel.
//!
//! Boots via Limine, brings up GDT + IDT (exceptions only), a physical frame
//! allocator from the memory map, and a static kernel heap. Then runs a few
//! self-tests (breakpoint exception + heap allocation) and halts.
//!
//! This is still tiny: no_std + no_main. All unsafe is documented and limited
//! to the hardware-required operations (segment loads, IDT load, static heap
//! hand-off, port I/O in serial, hlt).

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use core::arch::asm;

mod apic;
mod gdt;
mod interrupts;
mod memory;
mod objstore;
mod paging;
mod pci;
mod capspace;
mod sched;
mod serial;
mod supervisor;
mod syscall;
mod user;
mod virtio_blk;

use cap_core::capability::Rights;
use cap_core::table::Status;
use limine::request::{HhdmRequest, MemmapRequest, StackSizeRequest};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

/// Ask Limine for a generously-sized boot stack for the *pre-switch prologue*.
///
/// Limine's default boot stack (~64 KiB) sits immediately above its own page
/// tables, so a deep call chain overflows straight into them. That was the root
/// cause of the original "map-then-unmap" page fault: in debug builds the by-value
/// construction of the 8 KiB `ObjectTable` (capspace `OBJECTS`) cascaded ~90 KiB of
/// memcpys and zeroed Limine's L1/L2/L3/L4 — proven with a GDB hardware watchpoint
/// on the L1 PTE (a `compiler_builtins` memcpy writing into the page-table region,
/// RSP already below all four tables). `kmain` switches to our own guard-paged
/// `KERNEL_STACK` almost immediately; this request just guarantees head-room for the
/// tiny code that runs before that switch.
const LIMINE_STACK_SIZE: u64 = 1024 * 1024; // 1 MiB
#[used]
#[unsafe(link_section = ".requests")]
static STACK_SIZE_REQUEST: StackSizeRequest = StackSizeRequest::new(LIMINE_STACK_SIZE);

/// Dedicated kernel stack with a guard page at its low (bottom) end.
///
/// `kmain` switches RSP here at the very start of boot, and we **unmap the guard
/// page** (`paging::unmap_page`) so a stack overflow takes an immediate page fault —
/// reported as "KERNEL STACK OVERFLOW" by the #PF handler (which runs on its own IST
/// stack so it can report even when this stack is exhausted) — instead of silently
/// scribbling over adjacent memory the way the Limine boot stack scribbled the page
/// tables. Lives in `.bss` (physically far from any page table), so even a missed
/// guard could never repeat the page-table corruption.
const KSTACK_USABLE: usize = 512 * 1024; // 512 KiB usable
#[repr(C, align(4096))]
struct KernelStack {
    /// Unmapped at boot; touching it faults (the overflow tripwire). Lowest address.
    _guard: [u8; 4096],
    /// The usable stack; grows DOWN from its top toward `_guard`.
    usable: [u8; KSTACK_USABLE],
}
static mut KERNEL_STACK: KernelStack = KernelStack {
    _guard: [0; 4096],
    usable: [0; KSTACK_USABLE],
};

/// Base (low) address of the stack guard page — the #PF handler uses this to
/// recognize an overflow.
pub fn stack_guard_base() -> u64 {
    // SAFETY: addr_of! only takes the address of the static's field — it neither
    // reads nor writes it — and the address is stable for the kernel's lifetime.
    unsafe { core::ptr::addr_of!(KERNEL_STACK._guard) as u64 }
}

/// Top (16-aligned) of the boot thread's guarded kernel stack — task 0's rsp0 and
/// the rsp0 used for a ring-3→ring-0 entry while the boot thread is current.
pub fn stack_top() -> u64 {
    // SAFETY: addr_of! only takes the field address; stable for the kernel's life.
    unsafe { (core::ptr::addr_of!(KERNEL_STACK.usable) as u64 + KSTACK_USABLE as u64) & !0xf }
}

/// Limine base revision marker (must be present).
#[used]
#[unsafe(link_section = ".requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

/// Memory map request so we can report entry count on boot (exactly as spec asks).
#[used]
#[unsafe(link_section = ".requests")]
static MEMORY_MAP_REQUEST: MemmapRequest = MemmapRequest::new();

/// HHDM request so we can obtain the higher-half direct-map offset for phys<->virt.
#[used]
#[unsafe(link_section = ".requests")]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

/// Required by the limine 0.3 crate to delimit the request array for the bootloader.
#[used]
#[unsafe(link_section = ".requests_start_marker")]
static _START_MARKER: RequestsStartMarker = RequestsStartMarker::new();
#[used]
#[unsafe(link_section = ".requests_end_marker")]
static _END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

/// The ELF entry point. Name must match ENTRY(kmain) in linker.ld. Limine
/// transfers control here, on its own boot stack, after loading the higher half.
///
/// This is a tiny trampoline: it switches RSP to our dedicated, guard-paged
/// `KERNEL_STACK` and calls `kmain_main`. Everything substantial then runs on the
/// guarded stack, so an overflow trips the guard page instead of corrupting memory.
/// Keep this minimal — it is the only code that runs on Limine's boot stack.
#[unsafe(no_mangle)]
unsafe extern "C" fn kmain() -> ! {
    // Initialize COM1 16550 first so we can always report status.
    serial::init();

    // Switch to the dedicated kernel stack (grows down from its top; 16-byte
    // aligned per the SysV ABI), then enter the real kernel. The Limine boot
    // stack is abandoned here.
    let stack_top = (core::ptr::addr_of!(KERNEL_STACK.usable) as u64 + KSTACK_USABLE as u64) & !0xf;
    asm!(
        "mov rsp, {top}",
        "xor rbp, rbp", // terminate the frame-pointer chain for clean backtraces
        "call {main}",
        top = in(reg) stack_top,
        main = sym kmain_main,
        options(noreturn),
    );
}

/// The real kernel entry, running on the guard-paged `KERNEL_STACK`.
#[inline(never)]
unsafe extern "C" fn kmain_main() -> ! {
    // Exact banner.
    serial_println!("Cairn keystone v0 — the core is alive");

    // Report Limine base-revision negotiation (non-fatal). Referencing
    // BASE_REVISION here also keeps the linker from garbage-collecting it.
    if BASE_REVISION.is_supported() {
        serial_println!("Limine base revision: supported");
    } else {
        serial_println!(
            "Limine base revision: negotiated (actual = {:?})",
            BASE_REVISION.actual_revision()
        );
    }

    // Report memory map entry count early (keep the exact line/behavior from v0).
    let memmap_resp = MEMORY_MAP_REQUEST.response();
    if let Some(memmap) = memmap_resp {
        serial_println!("Memory map: {} entries detected", memmap.entries().len());
    } else {
        serial_println!("Memory map: request not answered by bootloader");
    }

    // Also fetch HHDM (we need the offset for memory::init even if we don't use
    // virtual addresses in v0).
    let hhdm_resp = HHDM_REQUEST.response();
    let hhdm_offset = hhdm_resp.map(|r| r.offset).unwrap_or(0);
    serial_println!("HHDM offset: {:#x}", hhdm_offset);

    // We are already running on our own KERNEL_STACK (kmain switched RSP). Report
    // the Limine prologue-stack compliance for visibility.
    let limine_stack = if STACK_SIZE_REQUEST.response().is_some() {
        "honored"
    } else {
        "NOT honored"
    };
    serial_println!(
        "Boot stack: on {} KiB guarded kernel stack (Limine {} KiB prologue {})",
        KSTACK_USABLE / 1024,
        LIMINE_STACK_SIZE / 1024,
        limine_stack
    );

    // === CPU foundations + memory (order matters) ===
    gdt::init();
    interrupts::init_idt();

    // Arm the stack guard: unmap the guard page below KERNEL_STACK so an overflow
    // faults immediately (the #PF handler, now on its own IST stack, reports it).
    // Done after init_idt so the IST is live before any fault can occur.
    let guard = stack_guard_base();
    if paging::unmap_page(hhdm_offset, guard) {
        serial_println!("stack guard armed: guard page {:#x} unmapped", guard);
    } else {
        serial_println!("stack guard: WARNING could not unmap guard page {:#x}", guard);
    }

    // Arm syscall/sysret + ring-3 entry (needs the GDT's user segments; before sti).
    syscall::init();

    if let Some(mm) = memmap_resp {
        memory::init(hhdm_offset, mm);
    } else {
        memory::init_hhdm(hhdm_offset);
        serial_println!("frame allocator: 0 free 4KiB frames (no memory map from Limine)");
    }

    memory::init_heap();

    // --- Phase 3 (L0): enumerate PCI bus 0 (IRQs still off — BAR-size probe is atomic) ---
    // Discovers the virtio-blk device + its MMIO BARs (the config window the driver maps next).
    pci::scan();

    // --- Phase 3 (L1): bring up the virtio-blk driver + L2 read/write round-trip ---
    virtio_blk::smoke_test(hhdm_offset);

    // --- Phase 3 (L3): mount the Cairnlog object store (format on first boot) ---
    objstore::mount();

    // --- Phase 3 (L4): INC6 objects survive reboot (T2 milestone) ---
    // The store persists: each boot's committed `put` becomes the NEXT boot's mounted root.
    // recover() re-derives that committed root from the superblock and re-verifies its on-disk
    // content hash; we then RE-MINT a live Extent cap from it (CPtrs are ephemeral — re-derived
    // each boot from the durable content hash) and cap_invoke(X_READ) to confirm the cap names
    // the same hash. Runs BEFORE this boot's INC5 put, so it reflects the PRIOR boot's root. On
    // a fresh store there is no committed root yet (root_len=0) → skipped. Across two runs on the
    // persisted disk, run2 recovers run1's committed object WITHOUT a new put producing it —
    // that is "objects survive reboot".
    match objstore::recover() {
        Some((rlba, rlen, rhash)) => match capspace::mint_extent(rlba, rlen, rhash) {
            Some(root_ext) => {
                let (rst, rh) = capspace::cap_invoke(root_ext, capspace::X_READ, 0);
                serial_println!(
                    "objstore: recovered root Extent cptr={} lba={} len={} hash={:#x} (on-disk re-hash matched); X_READ=>{:?} reply_hash={:#x}; objects-survive-reboot={}",
                    root_ext, rlba, rlen, rhash, rst, rh, rst == Status::Ok && rh == rhash
                );
            }
            None => serial_println!("objstore: recover mint_extent failed"),
        },
        None => serial_println!("objstore: no committed root to recover (fresh store)"),
    }

    // --- Phase 3 (L4): INC5 append-log put + content-addressed Extent caps ---
    // `put` writes the data sectors + a record header, flushes, then flips the OTHER
    // superblock slot (seq+1, new root) — that flip is the commit. We then mint an Extent
    // cap naming the committed content and prove the whole chain:
    //   (a) X_READ (via cap_invoke) is gated by READ and returns the content hash;
    //   (b) extent_metadata reads the on-disk {lba,len,hash} *through* the cap;
    //   (c) re-reading those sectors and re-hashing yields the SAME content hash — content
    //       addressing holds end-to-end (the durable name == the bytes' hash);
    //   (d) a READ-masked delegated copy is refused X_READ (ErrRights), and that same copy
    //       (it holds WRITE) gets ErrMethod on X_WRITE (its bulk-data path lands in INC7).
    // Runs in the root domain (0); the negative test uses the transient domain 3 so it
    // never perturbs the domain-1/2 IPC slot proofs (recv_cptr / grant slot 1).
    {
        const EXT_NEG_DOMAIN: usize = 3;
        let msg = b"CAIRN-EXTENT-INC5: content-addressed put + Extent cap proof";
        match objstore::put(msg) {
            Some((lba, len, hash)) => {
                match capspace::mint_extent(lba, len, hash) {
                Some(ext) => {
                    let (rst, rhash) = capspace::cap_invoke(ext, capspace::X_READ, 0);
                    let meta = capspace::extent_metadata(0, ext);
                    let disk = objstore::extent_content_hash(lba, len);
                    let content_ok = rst == Status::Ok
                        && rhash == hash
                        && meta == Some((lba, len, hash))
                        && disk == Some(hash);
                    serial_println!(
                        "extent: put lba={} len={} hash={:#x}; X_READ=>{:?} reply_hash={:#x}; meta={:?}; on-disk re-read={:?}; content-addressed match={}",
                        lba, len, hash, rst, rhash, meta, disk, content_ok
                    );
                    if let Some(nc) = capspace::delegate_from_root(
                        EXT_NEG_DOMAIN, ext, Rights::INVOKE | Rights::WRITE, 0,
                    ) {
                        let (no_read, _) =
                            capspace::cap_invoke_in(EXT_NEG_DOMAIN, nc, capspace::X_READ, 0);
                        let (wr, _) =
                            capspace::cap_invoke_in(EXT_NEG_DOMAIN, nc, capspace::X_WRITE, 0);
                        serial_println!(
                            "extent: READ-masked cap X_READ=>{:?} (expect ErrRights); X_WRITE=>{:?} (expect ErrMethod, data path=INC7)",
                            no_read, wr
                        );
                    }
                }
                None => serial_println!("extent: mint_extent failed"),
                }

                // --- Phase 3 (L4): INC7b Extent MAP — bytes reach a domain via MAP ---
                // Map the just-committed extent's PERSISTED bytes read-only into a domain (the
                // Extent object's "bytes via MAP, never a register" promise — CAP_ABI §5). load_extent
                // DMAs the sectors into a RAM frame (pre-`sti`; the kernel owns queue 0); X_MAP
                // (exercises Rights::MAP) maps that frame RO at EXTENT_MAP_BASE; we read the mapped
                // bytes and confirm fnv1a == the committed content hash. Negative: a MAP-masked copy
                // (INVOKE|READ, no MAP) is refused X_MAP (ErrRights).
                if let Some(frame_phys) = objstore::load_extent(lba, len) {
                    if let Some(mext) = capspace::mint_extent_mapped(lba, len, hash, frame_phys) {
                        let (mst, va) = capspace::cap_invoke(mext, capspace::X_MAP, 0);
                        let mapped_hash = if mst == Status::Ok {
                            // SAFETY: X_MAP just mapped `len` bytes RO at `va`; single-CPU, IRQs off.
                            let bytes =
                                unsafe { core::slice::from_raw_parts(va as *const u8, len as usize) };
                            Some(objstore::fnv1a(bytes))
                        } else {
                            None
                        };
                        let nomap = capspace::delegate_from_root(
                            EXT_NEG_DOMAIN, mext, Rights::INVOKE | Rights::READ, 0,
                        )
                        .map(|c| capspace::cap_invoke_in(EXT_NEG_DOMAIN, c, capspace::X_MAP, 0).0);
                        serial_println!(
                            "extent: X_MAP=>{:?} va={:#x}; mapped-bytes hash={:?} committed={:#x} match={}; MAP-masked X_MAP=>{:?} (expect ErrRights)",
                            mst, va, mapped_hash, hash, mapped_hash == Some(hash), nomap
                        );
                    } else {
                        serial_println!("extent: mint_extent_mapped failed");
                    }
                } else {
                    serial_println!("extent: load_extent failed (X_MAP proof skipped)");
                }
            },
            None => serial_println!("extent: objstore::put failed"),
        }
    }

    // --- self-tests (as specified) ---

    // Trigger a breakpoint exception and prove we recovered (IDT + handler work).
    // SAFETY: software interrupt; IDT is loaded and #BP handler returns normally.
    x86_64::instructions::interrupts::int3();
    serial_println!("recovered from #BP");

    // Prove the heap works (alloc + Vec).
    {
        use alloc::vec::Vec;
        let v: Vec<u64> = (0..5).collect();
        serial_println!("heap self-test: allocated Vec<u64> len={}", v.len());
        // Drop happens automatically; proves allocator didn't explode.
    }

    // Report free frames from our map-derived allocator (after the heap init line).
    serial_println!("free frames: {}", memory::free_frame_count());

    // --- capability self-test: live cap_invoke demo (verified cap-core wired in) ---
    // Creates a Memory object, mints a fully-powered cap, invokes ALLOC twice
    // (distinct frames), then revokes (epoch bump) and proves the stale cap now
    // fails with ErrRevoked — the verified I2 invariant, live.
    match capspace::init_root() {
        Some(cptr) => {
            serial_println!("init_root => cptr={}", cptr);
            let (s1, f1) = capspace::cap_invoke(cptr, capspace::M_ALLOC, 0);
            serial_println!("cap_invoke(ALLOC,0) => {:?} frame={:#x}", s1, f1 * 4096);
            let (s2, f2) = capspace::cap_invoke(cptr, capspace::M_ALLOC, 0);
            serial_println!("cap_invoke(ALLOC,0) => {:?} frame={:#x}", s2, f2 * 4096);
            // Security gate: an untrusted M_FREE of an arbitrary frame must be refused
            // (no per-frame ownership tracking in v0), so a ring-3 caller cannot return
            // a kernel-owned frame into the allocator and re-alloc it. Expect ErrMethod.
            let (fs, _) = capspace::cap_invoke(cptr, capspace::M_FREE, 0x100 /* kernel frame */);
            serial_println!("cap_invoke(M_FREE, 0x100) => {:?} (refused: no ownership tracking)", fs);
            let rs = capspace::demo_revoke_then_invoke(cptr);
            serial_println!("demo_revoke_then_invoke => {:?}", rs);
        }
        None => serial_println!("init_root failed"),
    }

    // --- per-domain CapTables + Notification (async IPC) self-test (Phase 2) ---
    // Stands up the per-domain authority model live: mint in root (domain 0), delegate
    // a rights-SUBSET copy into a second domain, and prove (a) verified delegate never
    // amplifies (I3), (b) a CPtr is domain-relative (root's CPtr is meaningless in the
    // other domain — isolation), and (c) a Notification carries data across a domain
    // boundary via signal/poll with per-method rights enforced.
    const DEMO_DOMAIN: usize = 2;
    if let Some(root_mem) = capspace::init_root() {
        // Delegate Memory with INVOKE only (root had READ|WRITE|INVOKE|MAP|DELEGATE).
        if let Some(d2_mem) = capspace::delegate_from_root(DEMO_DOMAIN, root_mem, Rights::INVOKE, 0) {
            let (d2_st, _) = capspace::cap_invoke_in(DEMO_DOMAIN, d2_mem, capspace::M_ALLOC, 0);
            // The same integer that names a cap in ROOT does not resolve in domain 2.
            let (iso_st, _) = capspace::cap_invoke_in(DEMO_DOMAIN, root_mem, capspace::M_ALLOC, 0);
            serial_println!(
                "perdomain: domain{} ALLOC via delegated(INVOKE-only) cap => {:?}; root CPtr {} in domain{} => {:?} (isolated)",
                DEMO_DOMAIN, d2_st, root_mem, DEMO_DOMAIN, iso_st
            );
        } else {
            serial_println!("perdomain: delegate_from_root failed");
        }
    }
    if let Some(notif_root) = capspace::create_notification() {
        // Delegate a SIGNAL-ONLY cap (INVOKE|WRITE, no READ) into domain 2.
        if let Some(notif_d2) =
            capspace::delegate_from_root(DEMO_DOMAIN, notif_root, Rights::INVOKE | Rights::WRITE, 0)
        {
            let (sig, _) = capspace::cap_invoke_in(DEMO_DOMAIN, notif_d2, capspace::N_SIGNAL, 0b101);
            // domain 2's cap has no READ → POLL must be refused (ErrRights).
            let (deny, _) = capspace::cap_invoke_in(DEMO_DOMAIN, notif_d2, capspace::N_POLL, 0);
            // root holds READ → observes the bits domain 2 raised; a second poll clears.
            let (p1_st, p1) = capspace::cap_invoke_in(0, notif_root, capspace::N_POLL, 0);
            let (_, p2) = capspace::cap_invoke_in(0, notif_root, capspace::N_POLL, 0);
            serial_println!(
                "notify: domain{} SIGNAL=>{:?} POLL(no READ)=>{:?}; root POLL=>{:?} bits={:#b} then {:#b}",
                DEMO_DOMAIN, sig, deny, p1_st, p1, p2
            );
        }
    }

    // --- Phase 2 step 4b: blocking client/server Endpoint IPC across two domains ---
    // Two ring-3 tasks share one endpoint: the server (domain 2) E_RECVs and BLOCKS;
    // the client (domain 1) E_SENDs a message AND grants (transfers) a Memory cap; the
    // rendezvous wakes the server, which receives the message + moved cap and uses it.
    // Both are gated by a TimeSlice cap ("time is a capability"). cap-core unchanged.
    if apic::init_timer(hhdm_offset) {
        sched::init();

        // --- crash-only supervision demo: a faulter domain that #UDs, then self-heals ---
        // Admitted FIRST (lowest task index ⇒ runs first), so it M_ALLOCs then faults
        // EARLY. Registered with the supervisor for RESTART: it crashes, is re-admitted
        // (budget 2), crashes again, and is finally reaped — the kernel and the 4b
        // endpoint demo survive throughout, proving fault isolation + self-healing.
        let fault_domain = supervisor::FAULT_DOMAIN as usize;
        let fault_mem = capspace::init_root()
            .and_then(|m| capspace::delegate_from_root(fault_domain, m, Rights::INVOKE, 0));
        let fault_blob = user::setup_user_task(
            hhdm_offset, 0x42_0000, 0x7d_f000, user::faulter_main, user::faulter_main_end,
        );
        match (fault_mem, fault_blob, capspace::mint_timeslice()) {
            (Some(fm), Some((fe, fs)), Some(ft)) => {
                // Register the (persistent) entry/stack for restart before first admission.
                supervisor::register_faulter(fe, fs, 2);
                match sched::admit_user(
                    fe, fs, fm, ft, supervisor::FAULT_DOMAIN, 5_000_000, 5_000_000, 1_000_000,
                ) {
                    Some(i) => serial_println!(
                        "crash-only: admitted faulter (domain{}) as task {}; mem_cptr={} (M_ALLOC then #UD; restart budget=2)",
                        fault_domain, i, fm
                    ),
                    None => serial_println!("crash-only: faulter admit failed"),
                }
            }
            _ => serial_println!("crash-only: faulter setup incomplete"),
        }

        const CLIENT_DOMAIN: usize = 1;
        const SERVER_DOMAIN: usize = 2;

        // Shared endpoint. Delegate the endpoint cap FIRST into each domain (→ slot 0,
        // passed in RDI): client gets INVOKE|GRANT_CAP (may transfer caps), server INVOKE.
        let ep = capspace::create_endpoint();
        let ep_client =
            ep.and_then(|e| capspace::delegate_from_root(CLIENT_DOMAIN, e, Rights::INVOKE | Rights::GRANT_CAP, 0));
        let ep_server =
            ep.and_then(|e| capspace::delegate_from_root(SERVER_DOMAIN, e, Rights::INVOKE, 0));
        // The Memory cap the client will GRANT to the server (needs DELEGATE to be MOVEd).
        // Delegated SECOND into the client domain → slot 1 (the client blob grants slot 1).
        let client_mem = capspace::init_root()
            .and_then(|m| capspace::delegate_from_root(CLIENT_DOMAIN, m, Rights::INVOKE | Rights::DELEGATE, 0));

        let server_blob = user::setup_user_task(
            hhdm_offset, 0x41_0000, 0x7e_f000, user::server_main, user::server_main_end,
        );
        let client_blob = user::setup_user_task(
            hhdm_offset, 0x40_0000, 0x7f_f000, user::client_main, user::client_main_end,
        );

        // Admit SERVER first (lower task index → runs first → parks on E_RECV), then
        // CLIENT. Each needs its own TimeSlice cap. The endpoint cptr goes in RDI.
        match (ep_server, ep_client, client_mem, server_blob, client_blob) {
            (Some(eps), Some(epc), Some(cm), Some((se, ss)), Some((ce, cs))) => {
                match (capspace::mint_timeslice(), capspace::mint_timeslice()) {
                    (Some(t1), Some(t2)) => {
                        let s = sched::admit_user(
                            se, ss, eps, t1, SERVER_DOMAIN as u16, 5_000_000, 5_000_000, 1_000_000,
                        );
                        let c = sched::admit_user(
                            ce, cs, epc, t2, CLIENT_DOMAIN as u16, 5_000_000, 5_000_000, 1_000_000,
                        );
                        serial_println!(
                            "EDF: admitted server(domain{})={:?} ep_cptr={}; client(domain{})={:?} ep_cptr={} grant_mem_cptr={}",
                            SERVER_DOMAIN, s, eps, CLIENT_DOMAIN, c, epc, cm
                        );
                    }
                    _ => serial_println!("ipc demo: TimeSlice mint failed"),
                }
            }
            _ => serial_println!(
                "ipc demo: setup incomplete (ep_server={} ep_client={} client_mem={} server_blob={} client_blob={})",
                ep_server.is_some(), ep_client.is_some(), client_mem.is_some(),
                server_blob.is_some(), client_blob.is_some()
            ),
        }

        // Negative proof (synchronous, returns before any park): an endpoint cap WITHOUT
        // GRANT_CAP cannot transfer a cap — E_SEND with a non-null xfer => ErrRights (4).
        if let Some(ep2) = capspace::create_endpoint() {
            if let Some(ep2_d3) = capspace::delegate_from_root(3, ep2, Rights::INVOKE, 0) {
                capspace::set_current_domain(3);
                let (st, _, _) = capspace::cap_invoke_ipc(ep2_d3, capspace::E_SEND, 0xBEEF, 5);
                capspace::set_current_domain(0);
                serial_println!(
                    "ep: GRANT_CAP gate — E_SEND w/ xfer but no GRANT_CAP => status={} (expect 4=ErrRights)",
                    st
                );
            }
        }

        // Capability gate + live O(1) revocation (verified I2): a revoked TimeSlice
        // cap is denied admission.
        if let Some(c) = capspace::mint_timeslice() {
            let _ = capspace::revoke_timeslice(c);
            serial_println!(
                "EDF: admission with a REVOKED TimeSlice cap => {:?}",
                capspace::admit_check(c)
            );
        }

        // --- Phase 3 INC7: zero-kernel DeviceQueue I/O (T1 milestone, the namesake) ---
        // A trusted ring-3 driver (domain 5) does a full virtio-blk READ — descriptor chain,
        // doorbell, used-ring poll — with ZERO syscalls, over a granted DeviceQueue cap whose
        // DQ_MAP mapped the rings+buffer+doorbell into the (shared) address space. The kernel
        // seeds a known magic at a scratch LBA, clears the shared buffer to a sentinel (so the
        // read must genuinely come from disk), bakes the I/O params into the buffer tail, DQ_MAPs
        // the rings via cap_invoke (the FIRST live use of Rights::MAP), then admits the driver
        // with the earliest deadline so it runs first and exits (#UD). Trust boundary: no IOMMU +
        // one shared address space => the mapped ring is write-anywhere DMA, gated ONLY by who
        // holds the cap (see docs/PHASE3.md). cap-core byte-unchanged.
        const DRIVER_DOMAIN: usize = 5;
        const DQ_MAGIC: u64 = 0xCA17_07D0_DEAD_0007;
        const DQ_SCRATCH_LBA: u64 = 32_700; // clear of the objstore log and the L2 smoke LBA (32760)
        {
            // 1. Seed the known magic at the scratch LBA (after smoke_test/objstore; pre-sti).
            let mut seed = [0u8; 512];
            seed[0..8].copy_from_slice(&DQ_MAGIC.to_le_bytes());
            let seeded = virtio_blk::write_sector(DQ_SCRATCH_LBA, &seed);
            let dqd = virtio_blk::device_queue_desc();
            // 2. Build the DeviceQueue cap (snapshots queue-0 phys layout + the proof state).
            let dq_root = capspace::create_device_queue(DQ_MAGIC, DQ_SCRATCH_LBA);
            if let (true, Some(dqd), Some(dq_root)) = (seeded, dqd, dq_root) {
                // 3. Clear the shared data buffer to a sentinel, then bake the I/O params into
                //    the buffer tail (buf_phys+2048) — both via the kernel HHDM alias. SAFETY:
                //    single-CPU, IRQs off; the buffer frame is live device RAM we own pre-sti.
                unsafe {
                    let buf = (hhdm_offset + dqd.buf_phys) as *mut u8;
                    core::ptr::write_bytes(buf.add(512), 0, 8); // sentinel (!= magic)
                    let p = buf.add(2048) as *mut u64;
                    p.add(0).write_unaligned(dqd.buf_phys);
                    p.add(1).write_unaligned(DQ_SCRATCH_LBA);
                    p.add(2).write_unaligned(dqd.qsize as u64);
                    p.add(3).write_unaligned(dqd.doorbell_phys & 0xfff); // doorbell sub-page offset
                }
                // 4. Delegate the cap into the driver domain (slot 0 -> RDI) WITH MAP, and a
                //    MAP-masked copy into the transient domain 3 for the negative test.
                let dq_drv = capspace::delegate_from_root(
                    DRIVER_DOMAIN, dq_root,
                    Rights::INVOKE | Rights::READ | Rights::WRITE | Rights::MAP, 0,
                );
                let dq_nomap = capspace::delegate_from_root(
                    3, dq_root, Rights::INVOKE | Rights::READ | Rights::WRITE, 0,
                );
                if let Some(dqc) = dq_drv {
                    // 5. DQ_MAP via the verified invoke (FIRST live Rights::MAP); negative test:
                    //    a MAP-masked copy is refused DQ_MAP (ErrRights).
                    let (mst, base) = capspace::cap_invoke_in(DRIVER_DOMAIN, dqc, capspace::DQ_MAP, 0);
                    let deny = dq_nomap.map(|c| capspace::cap_invoke_in(3, c, capspace::DQ_MAP, 0).0);
                    serial_println!(
                        "devqueue: DQ_MAP (first live Rights::MAP) => {:?} base={:#x}; MAP-masked copy DQ_MAP => {:?} (expect ErrRights)",
                        mst, base, deny
                    );
                    // 6. Admit the ring-3 driver in domain 5 with the EARLIEST deadline so it
                    //    runs first; its ONLY syscall is the final DQ_REPORT, then it exits (#UD).
                    let driver_blob = user::setup_user_task(
                        hhdm_offset, 0x43_0000, 0x7c_f000, user::driver_main, user::driver_main_end,
                    );
                    match (driver_blob, capspace::mint_timeslice()) {
                        (Some((de, ds)), Some(dt)) => {
                            let d = sched::admit_user(
                                de, ds, dqc, dt, DRIVER_DOMAIN as u16, 2_000_000, 2_000_000, 1_000_000,
                            );
                            serial_println!(
                                "devqueue: admitted ring3 driver (domain{})={:?} dq_cptr={} (zero-syscall virtio READ of LBA {})",
                                DRIVER_DOMAIN, d, dqc, DQ_SCRATCH_LBA
                            );
                        }
                        _ => serial_println!("devqueue: driver task setup incomplete"),
                    }
                } else {
                    serial_println!("devqueue: delegate DeviceQueue into driver domain failed");
                }
            } else {
                serial_println!("devqueue: INC7 setup incomplete (seeded={})", seeded);
            }
        }

        serial_println!("scheduler: enabling EDF preemption (client+server endpoint IPC + INC7 driver)");
        x86_64::instructions::interrupts::enable(); // sti
    } else {
        serial_println!("timer unavailable — no preemption");
    }

    // Boot thread (task 0) is the idle task; EDF runs the two ring-3 tasks, which
    // rendezvous over the endpoint (proof prints from capspace + syscall_dispatch).
    hcf();
}

#[panic_handler]
fn rust_panic(info: &core::panic::PanicInfo) -> ! {
    // Best effort: re-init serial (idempotent) and report.
    serial::init();
    serial_println!("");
    serial_println!("!!! KERNEL PANIC !!!");
    serial_println!("{}", info);
    hcf();
}

fn hcf() -> ! {
    loop {
        // SAFETY: classic halt loop. We are in a controlled early-boot single-CPU
        // environment with no interrupts enabled yet.
        unsafe {
            #[cfg(target_arch = "x86_64")]
            asm!("hlt", options(nomem, nostack, preserves_flags));
            #[cfg(not(target_arch = "x86_64"))]
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
