//! Bitmap physical frame allocator, const-generic and heap-free.
//!
//! This is a pure data structure intended for formal verification with Kani
//! (see `verification.rs` under the `kani` feature). It models lowest-first
//! allocation over a compile-time bounded set of frames (0..FRAMES).
//!
//! The kernel's runtime frame allocator (in `kernel/src/memory.rs`) uses a
//! similar lowest-first strategy over the Limine-provided usable ranges; the
//! const-generic bitmap here is the "model" we can prove properties about.
//! We deliberately do *not* use a single fixed-size bitmap in the kernel
//! because usable memory regions are discontiguous and total RAM size is
//! not a compile-time constant.
//!
//! All operations are O(FRAMES/64) worst-case scan (fine for verification
//! bounds and for early-boot kernel use with small N or few frees).

#![no_std]
#![forbid(unsafe_code)]

/// A fixed-capacity frame allocator backed by a bitmap.
///
/// Bit semantics: 1 = free/available, 0 = used/reserved.
/// `new()` starts with every frame marked used.
/// Frame indices are 0..FRAMES.
pub struct BitmapFrameAllocator<const FRAMES: usize> {
    /// Bit words; high bits in the final word (beyond FRAMES) are always kept 0.
    words: [u64; (FRAMES + 63) / 64],
}

impl<const FRAMES: usize> BitmapFrameAllocator<FRAMES> {
    /// Construct with all frames initially USED (reserved). Caller must call
    /// `mark_free` for each actually-available frame from the memory map.
    pub const fn new() -> Self {
        Self {
            words: [0u64; (FRAMES + 63) / 64],
        }
    }

    /// Mark `frame` as free (available to allocate). Out-of-range calls are
    /// ignored (bounds-checked, no panic).
    pub fn mark_free(&mut self, frame: usize) {
        if frame >= FRAMES {
            return;
        }
        let w = frame / 64;
        let b = frame % 64;
        self.words[w] |= 1u64 << b;
    }

    /// Mark `frame` as used (unavailable). Out-of-range calls are ignored.
    pub fn mark_used(&mut self, frame: usize) {
        if frame >= FRAMES {
            return;
        }
        let w = frame / 64;
        let b = frame % 64;
        self.words[w] &= !(1u64 << b);
    }

    /// Allocate the lowest free frame and mark it used. Returns `None` if the
    /// allocator is exhausted.
    pub fn alloc(&mut self) -> Option<usize> {
        for (wi, word) in self.words.iter_mut().enumerate() {
            if *word != 0 {
                let bi = word.trailing_zeros() as usize;
                let frame = wi * 64 + bi;
                if frame < FRAMES {
                    *word &= !(1u64 << bi);
                    return Some(frame);
                } else {
                    // Should never happen (we never set high bits), but keep mask clean.
                    *word &= !(1u64 << bi);
                }
            }
        }
        None
    }

    /// Free a frame previously returned by `alloc` (or equivalent). This is
    /// simply `mark_free`; out-of-range is ignored.
    pub fn free(&mut self, frame: usize) {
        self.mark_free(frame);
    }

    /// Query whether a given frame is currently free. Out-of-range => false.
    pub fn is_free(&self, frame: usize) -> bool {
        if frame >= FRAMES {
            return false;
        }
        let w = frame / 64;
        let b = frame % 64;
        (self.words[w] & (1u64 << b)) != 0
    }

    /// Return the number of frames currently marked free.
    pub fn free_count(&self) -> usize {
        let mut cnt = 0usize;
        for &w in &self.words {
            cnt += w.count_ones() as usize;
        }
        cnt
    }
}

#[cfg(feature = "kani")]
pub mod verification;

#[cfg(test)]
mod tests;
