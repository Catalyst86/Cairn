//! Minimal kernel paging: on-demand mapping of the kernel's higher-half pages.
//!
//! Limine maps the kernel ELF, but on this setup it leaves the first part of the
//! NOBITS `.bss` tail unmapped — writing a static there faults (page-not-present).
//! Rather than guess the range up front, we map the exact faulting page on demand
//! from the `#PF` handler, using the active (Limine) page tables reached through
//! the HHDM plus our physical frame allocator. This is the kernel taking
//! ownership of its own virtual memory, one page at a time.

use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

/// Adapter exposing the kernel physical frame allocator to the x86_64 `Mapper`.
struct KernelFrames;

unsafe impl FrameAllocator<Size4KiB> for KernelFrames {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        crate::memory::allocate_frame().map(|f| PhysFrame::containing_address(PhysAddr::new(f * 4096)))
    }
}

/// Build an `OffsetPageTable` over the active (Limine-provided) page tables.
///
/// SAFETY: `hhdm_offset` must be Limine's higher-half direct-map base and CR3
/// must hold the active L4 table (both true after Limine hands off).
unsafe fn active_mapper(hhdm_offset: u64) -> OffsetPageTable<'static> {
    let (l4_frame, _) = Cr3::read();
    let l4_virt = hhdm_offset + l4_frame.start_address().as_u64();
    let l4: &'static mut PageTable = &mut *(l4_virt as *mut PageTable);
    OffsetPageTable::new(l4, VirtAddr::new(hhdm_offset))
}

/// Ensure the 4 KiB page containing `addr` is mapped present+writable. If it is
/// already mapped, this is a no-op; otherwise a fresh frame is allocated, mapped,
/// and zeroed (bss must read as zero). Returns true if the page is mapped on exit.
pub fn map_one_page(hhdm_offset: u64, addr: u64) -> bool {
    let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(addr & !0xfff));
    let mut mapper = unsafe { active_mapper(hhdm_offset) };

    if mapper.translate_page(page).is_ok() {
        return true;
    }

    let mut frames = KernelFrames;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    if let Some(frame) = frames.allocate_frame() {
        // SAFETY: mapping a previously-unmapped kernel page to a fresh frame;
        // we flush the TLB and zero it before the faulting instruction retries.
        unsafe {
            if let Ok(flush) = mapper.map_to(page, frame, flags, &mut frames) {
                flush.flush();
                core::ptr::write_bytes(page.start_address().as_u64() as *mut u8, 0, 4096);
                return true;
            }
        }
    }
    false
}
