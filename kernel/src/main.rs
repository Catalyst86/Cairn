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

/// Request a generously-sized boot stack from Limine.
///
/// Limine's default boot stack (~64 KiB) is placed immediately above its own
/// page tables. A single deep call chain overflows it straight into those page
/// tables and zeroes them: in debug builds the by-value construction of the 8 KiB
/// `ObjectTable` (capspace `OBJECTS`) cascades several full-struct memcpys, ~90 KiB
/// of stack, which clobbered Limine's L1/L2/L3/L4 — the "map-then-unmap" page fault.
/// 1 MiB gives >10x head-room so the kernel stack can never reach the page tables.
/// (Confirmed via a GDB hardware watchpoint on the L1 PTE: a `compiler_builtins`
/// memcpy with dest inside the page-table region, RSP already below all four tables.)
const KERNEL_STACK_SIZE: u64 = 1024 * 1024; // 1 MiB
#[used]
#[unsafe(link_section = ".requests")]
static STACK_SIZE_REQUEST: StackSizeRequest = StackSizeRequest::new(KERNEL_STACK_SIZE);

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

/// The ELF entry point. Name must match ENTRY(kmain) in linker.ld.
/// Limine (and the crate) expect the symbol to be present and the bootloader
/// transfers control here after loading at the higher half.
#[unsafe(no_mangle)]
unsafe extern "C" fn kmain() -> ! {
    // Initialize COM1 16550 first so we can always report status.
    serial::init();

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

    // Confirm Limine gave us the large boot stack we asked for. Presence of the
    // response == compliance. If this is ever "NOT honored" we are still on the
    // tiny default stack and a deep call will corrupt the page tables (see the
    // STACK_SIZE_REQUEST doc comment).
    if STACK_SIZE_REQUEST.response().is_some() {
        serial_println!("Boot stack: {} KiB (Limine request honored)", KERNEL_STACK_SIZE / 1024);
    } else {
        serial_println!("Boot stack: WARNING request NOT honored — on default stack!");
    }

    // === CPU foundations + memory (order matters) ===
    gdt::init();
    interrupts::init_idt();

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
