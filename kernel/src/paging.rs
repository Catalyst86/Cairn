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

/// Diagnostic: manually walk the active page tables for `va` and print each
/// level's entry (present / huge / physical address). Reads tables via the HHDM.
pub fn dump_walk(hhdm: u64, va: u64) {
    let (l4f, _) = Cr3::read();
    let mut table = (hhdm + l4f.start_address().as_u64()) as *const u64;
    crate::serial_println!("walk {:#x} (cr3 phys {:#x}):", va, l4f.start_address().as_u64());
    for (lvl, shift) in [(4u32, 39u32), (3, 30), (2, 21), (1, 12)] {
        let idx = ((va >> shift) & 0x1ff) as usize;
        // SAFETY: reading a page-table entry through the HHDM.
        let e = unsafe { core::ptr::read_volatile(table.add(idx)) };
        let present = e & 1;
        let huge = (e >> 7) & 1;
        let phys = e & 0x000f_ffff_ffff_f000;
        crate::serial_println!(
            "  L{} idx={} entry={:#x} present={} huge={} phys={:#x}",
            lvl, idx, e, present, huge, phys
        );
        if present == 0 {
            crate::serial_println!("  -> NOT PRESENT at L{}", lvl);
            return;
        }
        if huge == 1 {
            crate::serial_println!("  -> HUGE at L{}", lvl);
            return;
        }
        table = (hhdm + phys) as *const u64;
    }
    crate::serial_println!("  -> 4KiB mapped");
}

/// Map `va`'s page by adding an L1 entry to the EXISTING page-table hierarchy
/// (the one Limine built). We walk L4/L3/L2 — which must already be present 4 KiB
/// tables — then install a single present+writable L1 entry pointing at a fresh
/// frame, and zero the page via its now-mapped kernel virtual address.
///
/// This deliberately avoids the x86_64 crate's `map_to`, which creates new
/// intermediate tables and accesses those fresh frames through the HHDM — the
/// operation that was faulting. Here we only ever write into a table the walk
/// already proved reachable, and only touch the new frame via `va` (not the HHDM).
/// Returns false (caller treats as fatal) if an intermediate level is missing or
/// a huge page is in the way.
pub fn manual_map(hhdm: u64, va: u64) -> bool {
    let va = va & !0xfff;
    let (l4f, _) = Cr3::read();
    let mut table_phys = l4f.start_address().as_u64();

    // Walk L4 -> L3 -> L2; each must be a present, non-huge table.
    for shift in [39u32, 30, 21] {
        let t = (hhdm + table_phys) as *const u64;
        let idx = ((va >> shift) & 0x1ff) as usize;
        let e = unsafe { core::ptr::read_volatile(t.add(idx)) };
        if e & 1 == 0 || (e >> 7) & 1 == 1 {
            return false;
        }
        table_phys = e & 0x000f_ffff_ffff_f000;
    }

    // table_phys now points at the L1 table.
    let l1 = (hhdm + table_phys) as *mut u64;
    let i1 = ((va >> 12) & 0x1ff) as usize;
    let existing = unsafe { core::ptr::read_volatile(l1.add(i1)) };
    if existing & 1 == 1 {
        return true; // already mapped
    }

    let frame = match crate::memory::allocate_frame() {
        Some(f) => f * 4096,
        None => return false,
    };
    unsafe {
        // present + writable
        core::ptr::write_volatile(l1.add(i1), frame | 0x3);
        x86_64::instructions::tlb::flush(VirtAddr::new(va));
        core::ptr::write_bytes(va as *mut u8, 0, 4096);
    }
    true
}

/// Pre-map the entire kernel `.bss` from NORMAL (non-interrupt) context, where
/// HHDM page-table reads are reliable — unlike the `#PF` handler, where reading
/// the active L4 through the HHDM returned 0. Must run after the frame allocator
/// is up and before any large bss static is first written.
pub fn premap_bss(hhdm: u64) {
    extern "C" {
        static __bss_start: u8;
        static __bss_end: u8;
    }
    let start = unsafe { core::ptr::addr_of!(__bss_start) as u64 };
    let end = unsafe { core::ptr::addr_of!(__bss_end) as u64 };
    let mut addr = start & !0xfff;
    let mut ok = true;
    while addr < end {
        if !manual_map(hhdm, addr) {
            ok = false;
        }
        addr += 0x1000;
    }
    crate::serial_println!("premap_bss [{:#x}..{:#x}) all_ok={}", start, end, ok);
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
