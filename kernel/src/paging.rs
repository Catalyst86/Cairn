//! Minimal kernel paging helpers over Limine's page tables.
//!
//! Limine maps the entire kernel ELF — including the NOBITS `.bss` tail — and a
//! higher-half direct map (HHDM). We confirmed this empirically (a normal-context
//! page-table walk shows every `.bss` page present before we touch it), so the
//! kernel does not need to pre-map `.bss`.
//!
//! What remains here:
//! - `dump_walk`: a normal-context page-table walker for diagnostics.
//! - `manual_map`: install a single L1 entry into the active hierarchy. Retained
//!   as a defensive backstop in the `#PF` handler (maps a kernel higher-half page
//!   on demand should one ever be missing), and as a building block for the
//!   demand-paged heap in a later phase.
//! - `map_one_page`: the same idea via the `x86_64` `Mapper`, kept for later use.
//!
//! NOTE: a previous, incorrect theory held that Limine left part of `.bss`
//! unmapped and that on-demand mapping was load-bearing. The real defect was a
//! kernel-stack overflow into Limine's page tables (fixed by requesting a 1 MiB
//! boot stack in `main.rs`); see that commit / `RESUME.md` for the full trace.

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

/// Unmap the 4 KiB page containing `va` by clearing its L1 entry in the active
/// (Limine) hierarchy, then flushing the TLB. Used to install the kernel stack
/// guard page: after this, any access to `va` takes a not-present page fault.
///
/// Deliberately does NOT free a backing frame — the guard page is part of a static
/// (`.bss`), never came from the frame allocator, and we only want faults on touch.
/// Returns false (nothing cleared) if an intermediate level is missing or huge.
pub fn unmap_page(hhdm: u64, va: u64) -> bool {
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

    let l1 = (hhdm + table_phys) as *mut u64;
    let i1 = ((va >> 12) & 0x1ff) as usize;
    unsafe {
        core::ptr::write_volatile(l1.add(i1), 0);
        x86_64::instructions::tlb::flush(VirtAddr::new(va));
    }
    true
}

/// Map a single device-MMIO page: `virt` -> `phys`, present + writable + no-cache
/// + no-execute, creating any missing intermediate tables from the frame allocator.
/// Used to map the Local APIC register window. Returns false if the page is already
/// mapped or a frame could not be allocated.
///
/// (NO_CACHE is correct for MMIO on real hardware. Under QEMU TCG it is moot —
/// MMIO is dispatched by address — but we set it anyway for correctness.)
pub fn map_mmio(hhdm_offset: u64, virt: u64, phys: u64) -> bool {
    let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(virt & !0xfff));
    let frame: PhysFrame<Size4KiB> = PhysFrame::containing_address(PhysAddr::new(phys & !0xfff));
    let mut mapper = unsafe { active_mapper(hhdm_offset) };
    let mut frames = KernelFrames;
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_CACHE
        | PageTableFlags::NO_EXECUTE;
    // SAFETY: mapping a device-MMIO page to a fixed physical address; map_to flushes
    // the new mapping and allocates any intermediate tables from our frame allocator.
    unsafe {
        match mapper.map_to(page, frame, flags, &mut frames) {
            Ok(flush) => {
                flush.flush();
                true
            }
            Err(_) => false,
        }
    }
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
