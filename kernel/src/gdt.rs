//! 64-bit GDT with kernel code/data segments and a TSS (IST for double-fault).
//!
//! Loaded once at boot. We use a single IST stack for #DF so that a double fault
//! (e.g. stack overflow in an exception handler) has a known-good stack to run on.
//!
//! Only the minimal unsafe required for GDT/TSS loads and the static stack.

use spin::Once;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// IST index for the double-fault handler stack (index 0 is the first IST entry).
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// Static TSS. Initialized once via spin::Once so we can hand out a &'static ref
/// to the GDT descriptor constructor.
static TSS: Once<TaskStateSegment> = Once::new();

/// The GDT and the selectors we need later (code + TSS).
struct GdtSelectors {
    code_selector: SegmentSelector,
    tss_selector: SegmentSelector,
}

static GDT: Once<(GlobalDescriptorTable, GdtSelectors)> = Once::new();

/// Initialize and load the GDT + TSS. Must be called before interrupts::init_idt
/// (so that the IST is valid when we register the #DF handler).
pub fn init() {
    let tss = TSS.call_once(|| {
        let mut tss = TaskStateSegment::new();

        // Allocate a small stack for double-faults (20 KiB). Must be valid for the
        // lifetime of the kernel; we use a static mut buffer.
        const STACK_SIZE: usize = 4096 * 5;
        static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

        // SAFETY: We never move or deallocate this stack. It is used only by the
        // #DF handler on IST0. The address is stable for the life of the kernel.
        // We write the *top* of the stack (grows down).
        let stack_start = VirtAddr::from_ptr(unsafe { core::ptr::addr_of!(STACK) });
        let stack_end = stack_start + STACK_SIZE as u64;
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = stack_end;

        tss
    });

    let (gdt, selectors) = GDT.call_once(|| {
        let mut gdt = GlobalDescriptorTable::new();

        // Kernel code and data segments (data is largely unused in long mode but
        // included per the conventional 64-bit GDT layout).
        let code_selector = gdt.add_entry(Descriptor::kernel_code_segment());
        let _data_selector = gdt.add_entry(Descriptor::kernel_data_segment());

        // TSS descriptor (holds a reference to the TSS we just built).
        let tss_selector = gdt.add_entry(Descriptor::tss_segment(tss));

        (
            gdt,
            GdtSelectors {
                code_selector,
                tss_selector,
            },
        )
    });

    // Load the GDT.
    gdt.load();

    // Reload CS (required after changing GDT) and load the TSS.
    // SAFETY: The GDT has been loaded and the selectors are valid entries that
    // we just installed. This is the canonical way to enter the new GDT.
    unsafe {
        x86_64::instructions::segmentation::CS::set_reg(selectors.code_selector);
        x86_64::instructions::tables::load_tss(selectors.tss_selector);
    }
}
