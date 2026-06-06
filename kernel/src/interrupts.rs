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

        // #PF on its own IST stack so it can report a main-stack overflow (guard
        // page hit) instead of double-faulting while pushing the exception frame.
        unsafe {
            idt.page_fault
                .set_handler_fn(page_fault_handler)
                .set_stack_index(gdt::PAGE_FAULT_IST_INDEX);
        }
        idt.general_protection_fault.set_handler_fn(general_protection_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);

        // Hardware interrupts (APIC). The timer uses the scheduler's naked ISR
        // (raw address: it performs a context switch + iretq and must run on the
        // interrupted task's stack, so no x86-interrupt wrapper and no IST). The
        // spurious handler keeps a stray APIC spurious IRQ from becoming a #GP.
        unsafe {
            idt[crate::apic::TIMER_VECTOR].set_handler_addr(x86_64::VirtAddr::new(
                crate::sched::timer_isr as usize as u64,
            ));
        }
        idt[crate::apic::SPURIOUS_VECTOR].set_handler_fn(spurious_interrupt_handler);

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

    // Stack-overflow tripwire: a fault inside the unmapped guard page below the
    // kernel stack means we ran out of stack. Report loudly and halt — do NOT fall
    // through to the on-demand mapper (which would map the guard and mask the bug).
    let guard = crate::stack_guard_base();
    if (guard..guard + 0x1000).contains(&fault_addr) {
        crate::serial_println!("EXCEPTION: KERNEL STACK OVERFLOW");
        crate::serial_println!(
            "  hit guard page at {:#x} (kernel stack exhausted)",
            fault_addr
        );
        crate::serial_println!("{:#?}", stack_frame);
        loop {
            // SAFETY: controlled halt; running on the #PF IST stack.
            unsafe {
                core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
            }
        }
    }

    // Defensive backstop: on-demand-map a *not-present* kernel higher-half page.
    // Limine maps the whole kernel image (incl. .bss), so in normal operation this
    // never fires; it only covers a stray missing page. Retry after mapping.
    if !error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION)
        && (0xffff_ffff_8000_0000..0xffff_ffff_c000_0000).contains(&fault_addr)
        && crate::paging::manual_map(crate::memory::hhdm_offset(), fault_addr)
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

// ---------------- hardware interrupts (APIC) ----------------

// The periodic timer is handled by the scheduler's naked ISR (`sched::timer_isr`),
// which ticks, EOIs, and context-switches. See sched.rs.

/// APIC spurious interrupt. By spec it needs no EOI; just return.
extern "x86-interrupt" fn spurious_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::serial_println!("APIC: spurious interrupt");
}
