//! Keystone v0 — the Cairn bare-metal microkernel.
//!
//! Boots via Limine, prints a banner over COM1 serial, then halts.
//! This is intentionally tiny: no_std + no_main, only the absolute minimum
//! to prove the boot + serial path and the cap-core linkage.

#![no_std]
#![no_main]

use core::arch::asm;

mod serial;

use limine::BaseRevision;
use limine::request::{MemoryMapRequest, RequestsEndMarker, RequestsStartMarker};

/// Limine base revision marker (must be present).
#[used]
#[unsafe(link_section = ".requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

/// Memory map request so we can report entry count on boot (exactly as spec asks).
#[used]
#[unsafe(link_section = ".requests")]
static MEMORY_MAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();

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
    // All limine requests must be *referenced* from a reachable function
    // (the assert does that) otherwise the linker may garbage-collect them.
    assert!(BASE_REVISION.is_supported(), "Limine base revision not supported");


    // Initialize COM1 16550 for reliable output (works in QEMU -serial and on bare metal).
    serial::init();

    // Exact banner required by the task description.
    serial_println!("Cairn keystone v0 — the core is alive");

    // Report memory map entry count (the "if easy" part of the spec).
    if let Some(memmap) = MEMORY_MAP_REQUEST.get_response() {
        serial_println!("Memory map: {} entries detected", memmap.entries().len());
    } else {
        serial_println!("Memory map: request not answered by bootloader");
    }

    // (Phase 1+) here we will bring up GDT/IDT, the cap tables from cap-core, etc.

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
