//! 64-bit GDT: kernel + user code/data segments and a TSS (IST stacks + rsp0).
//!
//! Entry order is dictated by `syscall`/`sysret` (the STAR MSR): kernel CS=0x08,
//! kernel SS=0x10, **user DS=0x18, user CS=0x20** (user data BEFORE user code,
//! because `sysretq` loads user CS = STAR.user_base+16 and user SS = base+8), then
//! the TSS=0x28. Ring-3 selectors are therefore 0x23 (CS|3) and 0x1b (SS|3).
//!
//! The TSS is a fixed-address mutable static so `rsp0`
//! (`privilege_stack_table[0]`) can be rewritten in place on every context switch
//! ([`set_rsp0`]) — the kernel stack a syscall / privilege-changing interrupt from
//! ring 3 switches to. The LTR'd descriptor caches only the TSS base, and the CPU
//! re-reads `rsp0` from TSS memory on each ring3→ring0 transition, so no reload of
//! the TSS descriptor is ever needed.

use spin::Once;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// IST index for the double-fault handler stack (index 0 is the first IST entry).
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// IST index for the page-fault handler stack (catches kernel-stack overflow loudly).
pub const PAGE_FAULT_IST_INDEX: u16 = 1;

/// Fixed-address mutable TSS so `rsp0` can be updated per context switch in place.
/// `TaskStateSegment::new()` is a const fn, so this initializes at compile time.
static mut TSS: TaskStateSegment = TaskStateSegment::new();

struct GdtSelectors {
    code_selector: SegmentSelector,
    data_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
    tss_selector: SegmentSelector,
}

static GDT: Once<(GlobalDescriptorTable, GdtSelectors)> = Once::new();

/// Kernel (CS, SS) selector values — unchanged 0x08 / 0x10.
pub fn kernel_selectors() -> (u16, u16) {
    let (_, s) = GDT.get().expect("GDT not initialized");
    (s.code_selector.0, s.data_selector.0)
}

/// User (CS, SS) selector values, RPL 3 — (0x23, 0x1b) — for ring-3 task frames.
pub fn user_selectors() -> (u16, u16) {
    let (_, s) = GDT.get().expect("GDT not initialized");
    (s.user_code_selector.0 | 3, s.user_data_selector.0 | 3)
}

/// Rewrite TSS.rsp0 (the kernel stack used when entering ring 0 from ring 3 via a
/// syscall or a privilege-changing interrupt). Call on every context switch with
/// the incoming task's kernel-stack top so two tasks never share a kernel stack.
///
/// SAFETY: single-CPU, interrupts off (called from schedule_tick / before `sti`).
pub unsafe fn set_rsp0(top: u64) {
    (*core::ptr::addr_of_mut!(TSS)).privilege_stack_table[0] = VirtAddr::new(top);
}

/// Initialize and load the GDT + TSS. Must run before interrupts::init_idt (so the
/// IST is valid when the #DF/#PF handlers are registered) and before syscall::init
/// (which validates STAR against this layout).
pub fn init() {
    // Configure the TSS: IST stacks for #DF/#PF + an initial rsp0 (the boot kernel
    // stack; schedule_tick keeps rsp0 current per task once tasks run).
    // SAFETY: single-CPU, interrupts off; sole initializer of the static TSS.
    unsafe {
        let tss = &mut *core::ptr::addr_of_mut!(TSS);

        const STACK_SIZE: usize = 4096 * 5;
        static mut DF_STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
        static mut PF_STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
        let df_top = VirtAddr::from_ptr(core::ptr::addr_of!(DF_STACK)) + STACK_SIZE as u64;
        let pf_top = VirtAddr::from_ptr(core::ptr::addr_of!(PF_STACK)) + STACK_SIZE as u64;
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = df_top;
        tss.interrupt_stack_table[PAGE_FAULT_IST_INDEX as usize] = pf_top;
        tss.privilege_stack_table[0] = VirtAddr::new(crate::stack_top());
    }

    let (gdt, selectors) = GDT.call_once(|| {
        let mut gdt = GlobalDescriptorTable::new();
        let code_selector = gdt.append(Descriptor::kernel_code_segment()); // 0x08
        let data_selector = gdt.append(Descriptor::kernel_data_segment()); // 0x10
        // User DATA before user CODE — required by the sysret selector layout.
        let user_data_selector = gdt.append(Descriptor::user_data_segment()); // 0x18
        let user_code_selector = gdt.append(Descriptor::user_code_segment()); // 0x20
        // TSS last (16-byte system descriptor → 0x28, occupies two slots).
        // SAFETY: the static TSS has a stable 'static address; the GDT outlives the kernel.
        let tss_selector =
            gdt.append(Descriptor::tss_segment(unsafe { &*core::ptr::addr_of!(TSS) }));
        (
            gdt,
            GdtSelectors {
                code_selector,
                data_selector,
                user_code_selector,
                user_data_selector,
                tss_selector,
            },
        )
    });

    gdt.load();

    // Reload CS and segment regs to OUR GDT, and load the TSS.
    // SAFETY: the GDT is loaded and these selectors are valid entries we just installed.
    unsafe {
        CS::set_reg(selectors.code_selector);
        load_tss(selectors.tss_selector);
        // Limine left SS/DS/ES pointing at its own GDT; an iretq restoring stale
        // selectors would #GP. Point them at our kernel data segment.
        SS::set_reg(selectors.data_selector);
        DS::set_reg(selectors.data_selector);
        ES::set_reg(selectors.data_selector);
    }
}
