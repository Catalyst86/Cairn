//! Memory management for keystone v0: HHDM offset, physical frame allocator,
//! and a static kernel heap (no paging involved yet).
//!
//! Frame allocator strategy:
//! - We consume the Limine memory map at init and build a list of usable
//!   4 KiB frame ranges (start_frame inclusive .. end_frame exclusive).
//! - Allocation always returns the *lowest* available frame number (mirrors the
//!   strategy proved in `crates/frame-alloc`).
//! - We keep a small fixed array of regions so we can support free() and
//!   coalescing without any heap allocation. MAX_REGIONS is deliberately larger
//!   than any realistic Limine map entry count (~20-30) to tolerate moderate
//!   fragmentation from early frees.
//! - The verified `BitmapFrameAllocator` is the *model* (const-bounded, Kani-
//!   provable). The kernel uses a runtime range-list because usable regions
//!   are discontiguous and total RAM is unknown at compile time. A future
//!   revision can adopt a (runtime-sized) bitmap or hand the verified type a
//!   bounded "cap-controlled" sub-range.
//!
//! The heap is a 1 MiB static region handed to linked_list_allocator. It lets
//! us use Box/Vec/alloc immediately for kernel data structures *without*
//! having set up paging or a frame-backed heap yet. (Real demand-paged heap
//! comes in a later phase.)

use core::sync::atomic::{AtomicU64, Ordering};
use limine::memmap::MEMMAP_USABLE;
use limine::request::MemmapResponse;
use spin::Mutex;

/// Higher-half direct map offset reported by Limine (phys -> virt).
static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Public accessor (written once during memory::init, read after).
pub fn hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Relaxed)
}

/// Convert a physical frame number to its HHDM virtual address (if you need to
/// touch the frame directly before a full page table is up).
#[inline]
pub fn phys_frame_to_virt(frame: u64) -> u64 {
    (frame * 4096) + hhdm_offset()
}

// ---------------- frame allocator (range list) ----------------

const MAX_REGIONS: usize = 64;

#[derive(Clone, Copy, Debug)]
struct FreeRegion {
    start: u64, // inclusive
    end: u64,   // exclusive
}

impl FreeRegion {
    #[inline]
    fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

struct FrameAllocator {
    regions: [Option<FreeRegion>; MAX_REGIONS],
}

impl FrameAllocator {
    const fn empty() -> Self {
        Self {
            regions: [None; MAX_REGIONS],
        }
    }

    fn clear(&mut self) {
        for slot in &mut self.regions {
            *slot = None;
        }
    }

    /// Insert a region (used at init for contiguous usable blocks). We do not
    /// sort here; `coalesce` will be called once after the whole map is added.
    fn add_region(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        for slot in &mut self.regions {
            if slot.is_none() {
                *slot = Some(FreeRegion { start, end });
                return;
            }
        }
        // If we run out of slots the map was pathologically large; drop the tail.
        // (MAX_REGIONS is generous.)
    }

    /// Allocate the lowest free frame number. O(MAX_REGIONS) scan is acceptable.
    fn alloc(&mut self) -> Option<u64> {
        let mut best_idx: Option<usize> = None;
        let mut best_start = u64::MAX;

        for (i, slot) in self.regions.iter().enumerate() {
            if let Some(r) = slot {
                if r.start < r.end && r.start < best_start {
                    best_start = r.start;
                    best_idx = Some(i);
                }
            }
        }

        if let Some(i) = best_idx {
            let r = self.regions[i].as_mut().unwrap();
            let f = r.start;
            r.start += 1;
            if r.start >= r.end {
                self.regions[i] = None;
            }
            Some(f)
        } else {
            None
        }
    }

    /// Return a frame to the pool. We attempt cheap adjacent merges; on any
    /// change we run a full coalesce to keep the list clean.
    fn free(&mut self, frame: u64) {
        // Try to extend an existing region that is adjacent.
        let mut extended = false;
        for slot in &mut self.regions {
            if let Some(r) = slot {
                if r.end == frame {
                    r.end += 1;
                    extended = true;
                    break;
                } else if r.start == frame + 1 {
                    r.start = frame;
                    extended = true;
                    break;
                }
            }
        }

        if !extended {
            // Find a free slot and insert a singleton region.
            for slot in &mut self.regions {
                if slot.is_none() {
                    *slot = Some(FreeRegion {
                        start: frame,
                        end: frame + 1,
                    });
                    break;
                }
            }
            // If no slot, the free is silently dropped (very rare with MAX=64).
        }

        self.coalesce();
    }

    fn free_count(&self) -> u64 {
        let mut sum: u64 = 0;
        for slot in &self.regions {
            if let Some(r) = slot {
                sum += r.len();
            }
        }
        sum
    }

    /// Rebuild the region list sorted and merged. Called after frees and once
    /// after init. Small N => simple sort + linear merge is fine.
    fn coalesce(&mut self) {
        // Collect live regions.
        let mut live: [FreeRegion; MAX_REGIONS] = [FreeRegion { start: 0, end: 0 }; MAX_REGIONS];
        let mut n = 0usize;
        for slot in &self.regions {
            if let Some(r) = slot {
                if r.start < r.end {
                    live[n] = *r;
                    n += 1;
                }
            }
        }

        if n == 0 {
            self.clear();
            return;
        }

        // Insertion sort by start (tiny N).
        for i in 1..n {
            let mut j = i;
            while j > 0 && live[j - 1].start > live[j].start {
                live.swap(j - 1, j);
                j -= 1;
            }
        }

        // Merge adjacent/overlapping runs.
        let mut out: [Option<FreeRegion>; MAX_REGIONS] = [None; MAX_REGIONS];
        let mut oi = 0usize;
        let mut cur = live[0];
        for k in 1..n {
            if cur.end >= live[k].start {
                // adjacent (==) or overlapping (>)
                if live[k].end > cur.end {
                    cur.end = live[k].end;
                }
            } else {
                out[oi] = Some(cur);
                oi += 1;
                cur = live[k];
            }
        }
        out[oi] = Some(cur);
        oi += 1;

        // Write back.
        self.clear();
        for i in 0..oi {
            self.regions[i] = out[i];
        }
    }
}

static FRAME_ALLOCATOR: Mutex<FrameAllocator> = Mutex::new(FrameAllocator::empty());

/// Initialize the frame allocator from Limine's usable memory map entries.
/// Also records the HHDM offset.
pub fn init(hhdm_offset: u64, memmap: &MemmapResponse) {
    {
        let mut fa = FRAME_ALLOCATOR.lock();
        fa.clear();

        for entry in memmap.entries() {
            if entry.type_ != MEMMAP_USABLE {
                continue;
            }
            // Convert byte range to frame range (4 KiB). Align outward for safety.
            let first = (entry.base + 0xfff) / 0x1000;
            let last = (entry.base + entry.length) / 0x1000;
            if first < last {
                fa.add_region(first, last);
            }
        }
        fa.coalesce();
    }

    HHDM_OFFSET.store(hhdm_offset, Ordering::Relaxed);

    let free = free_frame_count();
    crate::serial_println!("frame allocator: {} free 4KiB frames", free);
}

/// Record only the HHDM offset. Used in the (theoretically impossible) path
/// where Limine did not answer the memory map request but did answer HHDM.
pub fn init_hhdm(hhdm_offset: u64) {
    HHDM_OFFSET.store(hhdm_offset, Ordering::Relaxed);
}

/// Allocate one 4 KiB physical frame. Returns the *frame number* (paddr/4096).
pub fn allocate_frame() -> Option<u64> {
    FRAME_ALLOCATOR.lock().alloc()
}

/// Return a frame previously obtained from `allocate_frame`.
pub fn deallocate_frame(frame: u64) {
    FRAME_ALLOCATOR.lock().free(frame);
}

/// Current number of free physical frames.
pub fn free_frame_count() -> u64 {
    FRAME_ALLOCATOR.lock().free_count()
}

// ---------------- kernel heap (static 1 MiB, pre-paging) ----------------

use linked_list_allocator::LockedHeap;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// 1 MiB static heap. Enough for early kernel structures, Vecs for tests, etc.
/// A real frame-backed / page-table-backed heap arrives later.
const HEAP_SIZE: usize = 1 << 20; // 1 MiB
static mut HEAP_SPACE: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

/// Initialize the global allocator. Must be called after frame init (order in
/// main is memory::init then memory::init_heap). The heap lives in a static
/// buffer; we do not consume any physical frames for it yet.
///
/// SAFETY: called exactly once, very early, single-CPU, before any other
/// allocator use. The buffer is exclusively ours and never freed.
pub fn init_heap() {
    // Obtain a raw pointer to the static buffer without creating intermediate refs
    // that could be invalidated by mutation.
    let heap_start = core::ptr::addr_of_mut!(HEAP_SPACE) as *mut u8;

    // SAFETY (documented per task requirements):
    // - The static buffer has static lifetime and is never used for anything else.
    // - linked_list_allocator::LockedHeap::init is the documented way to hand it
    //   an exclusive region.
    // - We are still single-CPU with interrupts off; no concurrent allocators exist.
    // - After this call, the global allocator is live and Box/Vec work.
    unsafe {
        ALLOCATOR.lock().init(heap_start, HEAP_SIZE);
    }

    crate::serial_println!("kernel heap initialized ({} bytes static)", HEAP_SIZE);
}
