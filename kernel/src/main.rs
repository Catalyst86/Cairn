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

mod gdt;
mod interrupts;
mod memory;
mod paging;
mod capspace;
mod serial;

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

    if let Some(mm) = memmap_resp {
        memory::init(hhdm_offset, mm);
    } else {
        memory::init_hhdm(hhdm_offset);
        serial_println!("frame allocator: 0 free 4KiB frames (no memory map from Limine)");
    }

    memory::init_heap();

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
            let rs = capspace::demo_revoke_then_invoke(cptr);
            serial_println!("demo_revoke_then_invoke => {:?}", rs);
        }
        None => serial_println!("init_root failed"),
    }

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
