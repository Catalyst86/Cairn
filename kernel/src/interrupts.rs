//! Early IDT with CPU exception handlers only (no PIC/APIC, no hardware IRQs yet).
//!
//! Handlers for: #BP, #DF (on IST), #PF (reports CR2 + error code), #GP, #UD.
//! All handlers print diagnostic info via serial_println!.
//!
//! Double-fault, page-fault, GP, and invalid-opcode are fatal at this stage
//! (we hlt; a future phase will integrate proper fault handling + recovery).

use spin::Once;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::gdt;

/// Static IDT, initialized once.
static IDT: Once<InterruptDescriptorTable> = Once::new();

/// Build the IDT and load it. Must be called after gdt::init() (for IST).
pub fn init_idt() {
    let idt = IDT.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();

        idt.breakpoint.set_handler_fn(breakpoint_handler);

        // #DF must use a known-good stack (the IST entry set up in the TSS).
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }

        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.general_protection_fault.set_handler_fn(general_protection_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);

        idt
    });

    idt.load();
}

// ---------------- handlers ----------------

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    crate::serial_println!("EXCEPTION: BREAKPOINT");
    crate::serial_println!("{:#?}", stack_frame);
    // Return to the int3 site; caller in main will print "recovered from #BP".
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    crate::serial_println!("EXCEPTION: DOUBLE FAULT");
    crate::serial_println!("{:#?}", stack_frame);
    // Cannot recover. Use a local hlt loop (we must not unwind or call code that
    // might fault again).
    // SAFETY: classic single-CPU early-boot halt; interrupts are disabled in #DF context.
    loop {
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    // Read CR2 (faulting virtual address) via the x86_64 crate.
    let fault_addr = x86_64::registers::control::Cr2::read()
        .map(|a| a.as_u64())
        .unwrap_or(0);

    // On-demand map kernel higher-half pages that Limine left unmapped (the
    // NOBITS .bss tail). Only for *not-present* faults inside the kernel image
    // range; map the page and retry the faulting instruction.
    if !error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION)
        && (0xffff_ffff_8000_0000..0xffff_ffff_c000_0000).contains(&fault_addr)
        && crate::paging::map_one_page(crate::memory::hhdm_offset(), fault_addr)
    {
        return;
    }

    crate::serial_println!("EXCEPTION: PAGE FAULT");
    crate::serial_println!("  accessed address: {:#x}", fault_addr);
    crate::serial_println!("  error code: {:?}", error_code);
    crate::serial_println!("{:#?}", stack_frame);

    // Fatal for v0 (no paging / page-fault recovery yet).
    // SAFETY: see double_fault_handler.
    loop {
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

extern "x86-interrupt" fn general_protection_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    crate::serial_println!("EXCEPTION: GENERAL PROTECTION FAULT (error={:#x})", error_code);
    crate::serial_println!("{:#?}", stack_frame);

    loop {
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    crate::serial_println!("EXCEPTION: INVALID OPCODE");
    crate::serial_println!("{:#?}", stack_frame);

    loop {
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
