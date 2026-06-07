//! Cairnlog object store v0 (Phase 3, increments 4+5 — L3): an A/B double-buffered superblock
//! + content hashing over the virtio-blk L2 block layer, plus an **append-log `put`** that
//! commits content-addressed extents (INC5). Content-addressed Extent *caps* are minted in
//! `capspace::mint_extent` from a `put` result; "objects survive reboot" (re-mint the root
//! Extent on mount) is INC6.
//!
//! Crash-consistency: the superblock flip is the single linearization point. `put` writes the
//! data sectors THEN a record header (data-before-header, so a torn tail is never advertised),
//! `flush`es, then writes the OTHER superblock slot with `seq+1` + the new root and `flush`es
//! again — that flip is the commit. A failed write before the flip leaves the previous
//! superblock authoritative (the half-written log bytes sit past the old `log_head` and are
//! overwritten by the next `put`). Writing a superblock issues `virtio_blk::flush` before it
//! is considered committed (serialized completion is ordering, not durability, under QEMU's
//! writeback cache). On mount we read both slots, validate each by its content hash, and pick
//! the higher VALID `seq`; a torn superblock fails its hash and we fall back to the other slot.

use crate::virtio_blk;

const SB_MAGIC: u64 = 0x4361_6972_6E_4C67; // "Cairn Lg" — a recognizable superblock magic
const SB_VERSION: u32 = 1;
const LBA_SB_A: u64 = 0;
const LBA_SB_B: u64 = 1;
const LBA_LOG_START: u64 = 2; // first log sector (after the two superblock slots)

// Superblock byte layout within a 512B sector (little-endian):
//   0 magic:u64 | 8 version:u32 | 16 seq:u64 | 24 log_head_lba:u64 | 32 root_lba:u64
//   40 root_len:u32 | 48 root_hash:u64 | 56 sb_hash:u64 (FNV-1a of bytes [0..56])
const O_MAGIC: usize = 0;
const O_VERSION: usize = 8;
const O_SEQ: usize = 16;
const O_LOG_HEAD: usize = 24;
const O_ROOT_LBA: usize = 32;
const O_ROOT_LEN: usize = 40;
const O_ROOT_HASH: usize = 48;
const O_SB_HASH: usize = 56;

// Append-log RECORD HEADER (one 512B sector written AFTER its data sectors, so a torn
// tail is never advertised). Layout within the sector (little-endian):
//   0 rec_magic:u64 | 8 content_len:u32 | 16 content_hash:u64 | 24 prev_lba:u64
//   32 hdr_hash:u64 (FNV-1a of bytes [0..32])
// `prev_lba` chains to the previous root's data LBA (0 = none) for a future log walk;
// in v0 the superblock root pointer is authoritative and the header is informational.
const REC_MAGIC: u64 = 0x4361_6972_6E_5265; // "CairnRe" — a record-header magic
const RH_MAGIC: usize = 0;
const RH_LEN: usize = 8;
const RH_HASH: usize = 16;
const RH_PREV: usize = 24;
const RH_HDR_HASH: usize = 32;

/// In-RAM mounted superblock.
#[derive(Clone, Copy)]
pub struct Superblock {
    pub seq: u64,
    pub log_head_lba: u64,
    pub root_lba: u64,
    pub root_len: u32,
    pub root_hash: u64,
}

// The mounted superblock + flag. Written at mount, advanced by every committed `put`.
// SAFETY contract: single-CPU, accessed only with interrupts off (boot self-tests are
// pre-`sti`; mirrors the capspace side-table discipline).
static mut MOUNTED: Superblock = Superblock {
    seq: 0,
    log_head_lba: 0,
    root_lba: 0,
    root_len: 0,
    root_hash: 0,
};
static mut MOUNTED_OK: bool = false;
/// LBA of the superblock slot currently holding the live (mounted) superblock. The next
/// commit writes the OTHER slot and flips this — the A/B double-buffer linearization point.
static mut MOUNTED_SLOT: u64 = LBA_SB_A;

/// FNV-1a 64-bit basis offset (the empty-input hash).
const FNV1A_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// One FNV-1a streaming step: fold `data` into the running hash `h`. Lets a multi-sector
/// extent be hashed sector-by-sector (the on-disk read-back path) with the same result as
/// hashing the whole byte slice at once.
fn fnv1a_step(mut h: u64, data: &[u8]) -> u64 {
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// FNV-1a 64-bit content hash (v0; a 256-bit Merkle hash tying into attestation is deferred).
pub fn fnv1a(data: &[u8]) -> u64 {
    fnv1a_step(FNV1A_BASIS, data)
}

#[inline]
fn rd_u64(b: &[u8; 512], o: usize) -> u64 {
    let mut v = 0u64;
    let mut i = 0;
    while i < 8 {
        v |= (b[o + i] as u64) << (i * 8);
        i += 1;
    }
    v
}
#[inline]
fn rd_u32(b: &[u8; 512], o: usize) -> u32 {
    let mut v = 0u32;
    let mut i = 0;
    while i < 4 {
        v |= (b[o + i] as u32) << (i * 8);
        i += 1;
    }
    v
}
#[inline]
fn wr_u64(b: &mut [u8; 512], o: usize, v: u64) {
    let mut i = 0;
    while i < 8 {
        b[o + i] = (v >> (i * 8)) as u8;
        i += 1;
    }
}
#[inline]
fn wr_u32(b: &mut [u8; 512], o: usize, v: u32) {
    let mut i = 0;
    while i < 4 {
        b[o + i] = (v >> (i * 8)) as u8;
        i += 1;
    }
}

/// Write `sb` to superblock slot `slot_lba`, then FLUSH (durable before it counts as committed).
fn write_superblock(slot_lba: u64, sb: &Superblock) -> bool {
    let mut s = [0u8; 512];
    wr_u64(&mut s, O_MAGIC, SB_MAGIC);
    wr_u32(&mut s, O_VERSION, SB_VERSION);
    wr_u64(&mut s, O_SEQ, sb.seq);
    wr_u64(&mut s, O_LOG_HEAD, sb.log_head_lba);
    wr_u64(&mut s, O_ROOT_LBA, sb.root_lba);
    wr_u32(&mut s, O_ROOT_LEN, sb.root_len);
    wr_u64(&mut s, O_ROOT_HASH, sb.root_hash);
    let h = fnv1a(&s[0..O_SB_HASH]);
    wr_u64(&mut s, O_SB_HASH, h);
    virtio_blk::write_sector(slot_lba, &s) && virtio_blk::flush()
}

/// Outcome of reading one superblock slot. Crucially distinguishes a device-level read
/// FAILURE (`Io` — the slot's on-disk contents are UNKNOWN, so it may still hold the latest
/// committed superblock and must NEVER be reformatted over) from a slot that was successfully
/// read but is fresh/torn (`Invalid` — legitimately format-eligible). Conflating these two
/// (the original `Option`, where both collapsed to `None`) let a single transient read glitch
/// on the only good slot drive `mount()` into the format path, destroying all committed data.
#[derive(Clone, Copy)]
enum SlotRead {
    Io,               // read_sector failed at the device level (contents unknown)
    Invalid,          // read OK, but the magic or content-hash check failed (fresh/torn)
    Valid(Superblock),
}

/// Read + validate a superblock slot. `Io` if the block read failed; `Invalid` if magic
/// mismatches or the content hash fails (a torn/partial write or a fresh slot); else `Valid`.
fn read_superblock(slot_lba: u64) -> SlotRead {
    let mut s = [0u8; 512];
    if !virtio_blk::read_sector(slot_lba, &mut s) {
        return SlotRead::Io;
    }
    if rd_u64(&s, O_MAGIC) != SB_MAGIC {
        return SlotRead::Invalid;
    }
    if fnv1a(&s[0..O_SB_HASH]) != rd_u64(&s, O_SB_HASH) {
        return SlotRead::Invalid; // torn / corrupt
    }
    SlotRead::Valid(Superblock {
        seq: rd_u64(&s, O_SEQ),
        log_head_lba: rd_u64(&s, O_LOG_HEAD),
        root_lba: rd_u64(&s, O_ROOT_LBA),
        root_len: rd_u32(&s, O_ROOT_LEN),
        root_hash: rd_u64(&s, O_ROOT_HASH),
    })
}

/// Mount the store: read both superblock slots, pick the higher VALID seq. If NEITHER slot
/// is valid, format the disk (write superblock A, seq=1, empty root) — but ONLY when both
/// slots were successfully READ and found fresh/torn. If EITHER slot's read failed at the
/// device level (`Io`), that slot may still hold the latest committed superblock, so we
/// REFUSE to format (which would overwrite it) and leave the store unmounted. Prints the
/// outcome.
pub fn mount() {
    let a = read_superblock(LBA_SB_A);
    let b = read_superblock(LBA_SB_B);
    // Pick the higher VALID seq, carrying the slot LBA so `put`'s commit knows which to flip.
    let best: Option<(Superblock, u64)> = match (a, b) {
        (SlotRead::Valid(x), SlotRead::Valid(y)) => {
            Some(if x.seq >= y.seq { (x, LBA_SB_A) } else { (y, LBA_SB_B) })
        }
        (SlotRead::Valid(x), _) => Some((x, LBA_SB_A)),
        (_, SlotRead::Valid(y)) => Some((y, LBA_SB_B)),
        _ => None,
    };
    match best {
        Some((sb, slot)) => {
            // SAFETY: single-CPU, IRQs off; sole writer of MOUNTED at boot.
            unsafe {
                MOUNTED = sb;
                MOUNTED_OK = true;
                MOUNTED_SLOT = slot;
            }
            crate::serial_println!(
                "objstore: mounted superblock seq={} root_hash={:#x} log_head={} (slots A={} B={})",
                sb.seq, sb.root_hash, sb.log_head_lba,
                matches!(a, SlotRead::Valid(_)), matches!(b, SlotRead::Valid(_))
            );
        }
        // No valid superblock. Only FORMAT when both slots were definitively read and are
        // fresh/torn; an `Io` on either slot means its contents are unknown — do NOT clobber it.
        None if matches!(a, SlotRead::Invalid) && matches!(b, SlotRead::Invalid) => {
            let sb = Superblock {
                seq: 1,
                log_head_lba: LBA_LOG_START,
                root_lba: 0,
                root_len: 0,
                root_hash: 0,
            };
            if write_superblock(LBA_SB_A, &sb) {
                // SAFETY: as above.
                unsafe {
                    MOUNTED = sb;
                    MOUNTED_OK = true;
                    MOUNTED_SLOT = LBA_SB_A;
                }
                crate::serial_println!(
                    "objstore: formatted (no valid superblock) -> seq=1 log_head={}",
                    LBA_LOG_START
                );
            } else {
                crate::serial_println!("objstore: format FAILED (block write/flush error)");
            }
        }
        None => {
            // A read failed at the device level — refuse to format over a possibly-live slot.
            crate::serial_println!(
                "objstore: mount FAILED — superblock read error (A={} B={}); refusing to format (would risk overwriting a committed slot)",
                matches!(a, SlotRead::Io), matches!(b, SlotRead::Io)
            );
        }
    }
}

/// Append `bytes` to the log as a new content-addressed extent and commit it. Returns
/// `(data_lba, len, content_hash)` of the committed extent, or `None` on any block error
/// (in which case nothing is committed — the previous superblock stays authoritative).
///
/// Sequence (the durable-by-construction write path):
/// 1. write the data sectors (zero-padded tail) starting at the mounted `log_head_lba`;
/// 2. write the record header in the sector AFTER the data (data-before-header: a torn
///    tail can never be advertised as a record);
/// 3. `flush` (data + header durable before the commit);
/// 4. write the OTHER superblock slot with `seq+1`, the new root `{lba,len,hash}`, and an
///    advanced `log_head` — `write_superblock` flushes again. **This flip is the commit.**
///
/// Content addressing + CoW: identical `bytes` always hash to the same `content_hash` but
/// are appended at a fresh `log_head` each call (an immutable, never-overwritten extent).
pub fn put(bytes: &[u8]) -> Option<(u64, u32, u64)> {
    // SAFETY: single-CPU, IRQs off; snapshot the mounted state (sole accessor).
    let (cur, ok, slot) = unsafe { (MOUNTED, MOUNTED_OK, MOUNTED_SLOT) };
    if !ok {
        return None;
    }
    let len = bytes.len();
    if len > u32::MAX as usize {
        return None;
    }
    let ndata = len.div_ceil(512) as u64; // 0 for an empty object (degenerate but valid)
    let data_lba = cur.log_head_lba;
    let h = fnv1a(bytes);

    // 1. Data sectors (zero-padded final sector).
    let mut written = 0usize;
    let mut s = 0u64;
    while s < ndata {
        let mut sect = [0u8; 512];
        let remaining = len - written;
        let n = if remaining >= 512 { 512 } else { remaining };
        sect[..n].copy_from_slice(&bytes[written..written + n]);
        if !virtio_blk::write_sector(data_lba + s, &sect) {
            return None;
        }
        written += n;
        s += 1;
    }

    // 2. Record header, in the sector AFTER the data (data-before-header).
    let hdr_lba = data_lba + ndata;
    let mut hdr = [0u8; 512];
    wr_u64(&mut hdr, RH_MAGIC, REC_MAGIC);
    wr_u32(&mut hdr, RH_LEN, len as u32);
    wr_u64(&mut hdr, RH_HASH, h);
    wr_u64(&mut hdr, RH_PREV, cur.root_lba); // chain to the previous root (0 = none)
    let hh = fnv1a(&hdr[0..RH_HDR_HASH]);
    wr_u64(&mut hdr, RH_HDR_HASH, hh);
    if !virtio_blk::write_sector(hdr_lba, &hdr) {
        return None;
    }

    // 3. Flush data + header durable before the commit.
    if !virtio_blk::flush() {
        return None;
    }

    // 4. Commit: write the OTHER superblock slot (seq+1, new root, advanced log_head), flush.
    let new_sb = Superblock {
        seq: cur.seq + 1,
        log_head_lba: hdr_lba + 1,
        root_lba: data_lba,
        root_len: len as u32,
        root_hash: h,
    };
    let other = if slot == LBA_SB_A { LBA_SB_B } else { LBA_SB_A };
    if !write_superblock(other, &new_sb) {
        // Commit not acknowledged: either the sector write failed (previous superblock stays
        // authoritative) or only the FLUSH failed (new-slot durability is indeterminate under
        // writeback). Either way we do NOT advance MOUNTED; mount() deterministically picks
        // the highest VALID seq next boot, so no torn/partial extent is ever advertised.
        return None;
    }

    // Commit point passed — publish the new mounted state in RAM.
    // SAFETY: single-CPU, IRQs off; sole writer of MOUNTED.
    unsafe {
        MOUNTED = new_sb;
        MOUNTED_SLOT = other;
    }
    Some((data_lba, len as u32, h))
}

/// Re-read an extent's bytes from disk and return their FNV-1a content hash, or `None` on a
/// block error. Reads `ceil(len/512)` sectors from `lba` and hashes the first `len` bytes —
/// so the result equals the `content_hash` `put` committed iff the on-disk bytes are intact.
/// This is the read-back leg of the INC5 proof and the seed for INC6 recovery verification.
pub fn extent_content_hash(lba: u64, len: u32) -> Option<u64> {
    let len = len as usize;
    let nsect = len.div_ceil(512) as u64;
    let mut h = FNV1A_BASIS;
    let mut consumed = 0usize;
    let mut s = 0u64;
    while s < nsect {
        let mut sect = [0u8; 512];
        if !virtio_blk::read_sector(lba + s, &mut sect) {
            return None;
        }
        let remaining = len - consumed;
        let n = if remaining >= 512 { 512 } else { remaining };
        h = fnv1a_step(h, &sect[..n]);
        consumed += n;
        s += 1;
    }
    Some(h)
}

/// Recover the committed root extent from the mounted superblock (INC6 — objects survive
/// reboot). Returns the persisted root `{lba, len, hash}` iff the store is mounted, has a
/// non-empty committed root, AND that root's on-disk bytes still hash to the committed
/// `root_hash` (a content-integrity check on the persisted root — a torn root or a
/// superblock pointing at corrupt bytes yields `None`). `None` also for a fresh/empty store
/// (`root_len == 0`). The returned hash is the durable content NAME; the caller re-mints a
/// (ephemeral) Extent cap from it — caps are re-derived from on-disk content each boot, since
/// CPtrs are not persisted (sealed sparse tokens are CAP_ABI §7, deferred).
///
/// ORDERING: call this right after `mount`, BEFORE any `put` this boot — it reflects the root
/// committed by the PREVIOUS boot (a `put` advances `MOUNTED` to a new root).
pub fn recover() -> Option<(u64, u32, u64)> {
    // SAFETY: single-CPU, IRQs off; snapshot the mounted state (sole accessor).
    let (cur, ok) = unsafe { (MOUNTED, MOUNTED_OK) };
    if !ok || cur.root_len == 0 {
        return None;
    }
    match extent_content_hash(cur.root_lba, cur.root_len) {
        Some(h) if h == cur.root_hash => Some((cur.root_lba, cur.root_len, cur.root_hash)),
        _ => None, // root bytes unreadable or hash mismatch — do not advertise a corrupt root
    }
}

/// Load an extent's on-disk bytes into a fresh zeroed RAM frame and return the frame's RAW
/// guest-phys, for Extent **MAP** (INC7b) — extent data lives on disk, so it must be DMA'd into
/// RAM before it can be mapped into a domain. Reads `ceil(len/512)` sectors from `lba` into the
/// frame (the last sector's tail beyond `len` stays zero). v0 maps single-frame extents only
/// (`1..=4096` bytes); returns `None` for an out-of-range `len` or a block error.
///
/// MUST run pre-`sti` (it uses the kernel's queue 0; after `sti` a ring-3 driver may own it).
pub fn load_extent(lba: u64, len: u32) -> Option<u64> {
    let len = len as usize;
    if len == 0 || len > 4096 {
        return None; // v0: single-frame extents
    }
    let fnum = crate::memory::allocate_frame()?;
    let phys = fnum * 4096;
    let hhdm = crate::memory::hhdm_offset();
    let frame = (hhdm + phys) as *mut u8;
    // SAFETY: fresh frame, accessed via its HHDM alias; single-CPU, IRQs off.
    unsafe { core::ptr::write_bytes(frame, 0, 4096) };
    let nsect = len.div_ceil(512);
    let mut s = 0usize;
    while s < nsect {
        let mut sect = [0u8; 512];
        if !virtio_blk::read_sector(lba + s as u64, &mut sect) {
            crate::memory::deallocate_frame(fnum); // don't leak the frame on a block error
            return None;
        }
        let off = s * 512;
        let n = if len - off >= 512 { 512 } else { len - off };
        // SAFETY: copy this sector's bytes into the frame's HHDM alias at the right offset.
        unsafe { core::ptr::copy_nonoverlapping(sect.as_ptr(), frame.add(off), n) };
        s += 1;
    }
    Some(phys)
}
