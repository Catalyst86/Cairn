//! Cairnlog object store v0 (Phase 3, increment 4 — L3): an A/B double-buffered superblock
//! + content hashing over the virtio-blk L2 block layer. The append log + content-addressed
//! Extent caps arrive in INC5; "objects survive reboot" is INC6.
//!
//! Crash-consistency: the superblock flip is the single linearization point. Writing a
//! superblock issues `virtio_blk::flush` before it is considered committed (serialized
//! completion is ordering, not durability, under QEMU's writeback cache). On mount we read
//! both slots, validate each by its content hash, and pick the higher VALID `seq`; a torn
//! superblock fails its hash and we fall back to the other slot.

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

/// In-RAM mounted superblock.
#[derive(Clone, Copy)]
pub struct Superblock {
    pub seq: u64,
    pub log_head_lba: u64,
    pub root_lba: u64,
    pub root_len: u32,
    pub root_hash: u64,
}

// The mounted superblock + flag. Written at mount; READ by INC5's put/read (append log +
// Extent caps), so allow dead-code until then.
#[allow(dead_code)]
static mut MOUNTED: Superblock = Superblock {
    seq: 0,
    log_head_lba: 0,
    root_lba: 0,
    root_len: 0,
    root_hash: 0,
};
#[allow(dead_code)]
static mut MOUNTED_OK: bool = false;

/// FNV-1a 64-bit content hash (v0; a 256-bit Merkle hash tying into attestation is deferred).
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
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

/// Read + validate a superblock slot. None if magic mismatches or the content hash fails
/// (a torn/partial write).
fn read_superblock(slot_lba: u64) -> Option<Superblock> {
    let mut s = [0u8; 512];
    if !virtio_blk::read_sector(slot_lba, &mut s) {
        return None;
    }
    if rd_u64(&s, O_MAGIC) != SB_MAGIC {
        return None;
    }
    if fnv1a(&s[0..O_SB_HASH]) != rd_u64(&s, O_SB_HASH) {
        return None; // torn / corrupt
    }
    Some(Superblock {
        seq: rd_u64(&s, O_SEQ),
        log_head_lba: rd_u64(&s, O_LOG_HEAD),
        root_lba: rd_u64(&s, O_ROOT_LBA),
        root_len: rd_u32(&s, O_ROOT_LEN),
        root_hash: rd_u64(&s, O_ROOT_HASH),
    })
}

/// Mount the store: read both superblock slots, pick the higher VALID seq; if neither is
/// valid, format the disk (write superblock A, seq=1, empty root). Prints the outcome.
pub fn mount() {
    let a = read_superblock(LBA_SB_A);
    let b = read_superblock(LBA_SB_B);
    let best = match (a, b) {
        (Some(x), Some(y)) => Some(if x.seq >= y.seq { x } else { y }),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    };
    match best {
        Some(sb) => {
            // SAFETY: single-CPU, IRQs off; sole writer of MOUNTED at boot.
            unsafe {
                MOUNTED = sb;
                MOUNTED_OK = true;
            }
            crate::serial_println!(
                "objstore: mounted superblock seq={} root_hash={:#x} log_head={} (slots A={} B={})",
                sb.seq, sb.root_hash, sb.log_head_lba, a.is_some(), b.is_some()
            );
        }
        None => {
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
                }
                crate::serial_println!(
                    "objstore: formatted (no valid superblock) -> seq=1 log_head={}",
                    LBA_LOG_START
                );
            } else {
                crate::serial_println!("objstore: format FAILED (block write/flush error)");
            }
        }
    }
}
