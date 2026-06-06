//! Local APIC (xAPIC, MMIO) bring-up + periodic timer — the Phase 2 foundation.
//!
//! We use classic **xAPIC via MMIO**: QEMU's TCG (software) backend, which we run
//! under (no KVM in WSL1), does not emulate x2APIC. The LAPIC MMIO window lives at
//! the physical base in `IA32_APIC_BASE` (default `0xFEE00000`) and we reach it
//! through Limine's HHDM. (Under TCG, MMIO is dispatched by address regardless of
//! cache attribute, so a write-back HHDM mapping is fine here; on real hardware
//! this page must be mapped uncached — a Phase 4 follow-up.)
//!
//! The timer fires `TIMER_VECTOR` periodically; the ISR (`interrupts.rs`) bumps
//! `TICKS` and signals EOI. This tick is the clock the EDF scheduler will drive
//! preemption + deadlines from. v0 is uncalibrated (raw reload count) — real time
//! units come with the scheduler.

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::registers::model_specific::Msr;

const IA32_APIC_BASE: u32 = 0x1B;
const APIC_BASE_GLOBAL_ENABLE: u64 = 1 << 11; // xAPIC global enable (bit 10 = x2APIC, left 0)
const APIC_BASE_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000; // physical base bits [12:51]

// xAPIC register byte offsets from the MMIO base (each is a 32-bit register).
const REG_ID: usize = 0x020;
const REG_EOI: usize = 0x0B0;
const REG_SVR: usize = 0x0F0;
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INIT: usize = 0x380;
const REG_TIMER_CURR: usize = 0x390;   // APIC current-count register (read-only)
const REG_TIMER_DIV: usize = 0x3E0;

const PIT_HZ: u64 = 1_193_182;         // fixed i8254 input clock
const QUANTUM_NS: u64 = 1_000_000;     // 1 ms scheduler tick

const SVR_APIC_SOFTWARE_ENABLE: u32 = 1 << 8;
const LVT_TIMER_PERIODIC: u32 = 1 << 17;
const TIMER_DIVIDE_BY_16: u32 = 0b0011; // DCR encoding for divide-by-16 (SDM Fig 10-10)

/// Interrupt vector for the periodic APIC timer (>= 32, outside the exceptions).
pub const TIMER_VECTOR: u8 = 0x20;
/// Spurious-interrupt vector handed to the SVR.
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// Monotonic tick counter, incremented by the timer ISR.
pub static TICKS: AtomicU64 = AtomicU64::new(0);

/// Nanoseconds advanced per timer interrupt. Published by init_timer after calibration.
pub static TICK_NS: AtomicU64 = AtomicU64::new(QUANTUM_NS);

/// LAPIC MMIO virtual base (HHDM + physical base); set once during init.
static LAPIC_VIRT: AtomicU64 = AtomicU64::new(0);

#[inline]
fn rdmsr(reg: u32) -> u64 {
    // SAFETY: reading an architectural MSR has no side effects.
    unsafe { Msr::new(reg).read() }
}

#[inline]
fn wrmsr(reg: u32, val: u64) {
    // SAFETY: caller writes only valid APIC base configuration (see init_timer).
    let mut m = Msr::new(reg);
    unsafe { m.write(val) }
}

#[inline]
fn reg_read(off: usize) -> u32 {
    let base = LAPIC_VIRT.load(Ordering::Relaxed) as usize;
    // SAFETY: base is the HHDM-mapped LAPIC window; off is a valid 32-bit register.
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

#[inline]
fn reg_write(off: usize, val: u32) {
    let base = LAPIC_VIRT.load(Ordering::Relaxed) as usize;
    // SAFETY: as reg_read; volatile MMIO write to a valid LAPIC register.
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, val) }
}

#[inline]
pub fn tick_ns() -> u64 {
    TICK_NS.load(Ordering::Relaxed)
}

#[inline]
fn timer_curr() -> u32 {
    reg_read(REG_TIMER_CURR)
}

/// Calibrate the APIC timer against PIT channel 2: returns APIC ticks (div-16) per 1 ms.
/// IRQs MUST be off and both PICs masked when called (PIT ch2 raises no IRQ; we poll
/// port 0x61 bit 5 = OUT2). Clamps to a sane fallback if the measurement is implausible.
fn calibrate_apic_per_ms() -> u32 {
    use x86_64::instructions::port::Port;
    // SAFETY: standard i8254 (0x40-0x43) / 0x61 ports; PICs masked so ch2 raises no IRQ;
    // single CPU with interrupts off; anti-hang guard bounds the poll.
    unsafe {
        // Free-run the APIC down-counter, masked (counts but raises no IRQ), div-16.
        reg_write(REG_TIMER_DIV, TIMER_DIVIDE_BY_16);
        reg_write(REG_LVT_TIMER, 1 << 16); // bit16 = masked
        reg_write(REG_TIMER_INIT, 0xFFFF_FFFF);

        let mut p61: Port<u8> = Port::new(0x61);
        let mut cmd: Port<u8> = Port::new(0x43);
        let mut ch2: Port<u8> = Port::new(0x42);

        let gate = (p61.read() & 0xFC) | 0x01; // bit0 = gate on, bit1 = speaker off
        p61.write(gate & !0x01);               // gate low: arm/reset
        cmd.write(0xB0);                        // ch2, lobyte/hibyte, mode 0, binary
        let count: u16 = (PIT_HZ / 100) as u16; // ~11_931 == 10 ms window
        ch2.write((count & 0xFF) as u8);
        ch2.write((count >> 8) as u8);

        let start = timer_curr();
        p61.write(gate | 0x01); // gate high: start counting
        let mut guard: u32 = 0;
        let mut fired = false;
        loop {
            if p61.read() & 0x20 != 0 {
                fired = true; // OUT2 high => the 10 ms gate genuinely elapsed
                break;
            }
            guard = guard.wrapping_add(1);
            if guard > 50_000_000 {
                break; // anti-hang: OUT2 never observed
            }
        }
        let end = timer_curr();
        reg_write(REG_LVT_TIMER, 1 << 16); // keep masked until init arms it

        // Fallback ~ this target's measured APIC rate (QEMU TCG ~62k ticks/ms);
        // calibration normally supersedes it. It is a ticks-PER-MILLISECOND value, so it
        // stays consistent with TICK_NS = 1 ms even on the failure path.
        const FALLBACK_PER_MS: u32 = 62_500;
        if !fired {
            return FALLBACK_PER_MS; // gate timed out -> measurement invalid, do not trust it
        }
        let elapsed = start.wrapping_sub(end) as u64; // APIC is a down-counter
        let apic_hz = elapsed.saturating_mul(100); // 10 ms window -> ticks/sec
        let per_ms = (apic_hz / 1000) as u32;
        if per_ms < 100 {
            FALLBACK_PER_MS // implausible measurement -> fallback
        } else {
            per_ms
        }
    }
}

/// Mask both legacy 8259 PICs so they cannot deliver IRQs while we run in APIC
/// mode. We poll the UART, so no legacy IRQ source is needed.
fn mask_legacy_pic() {
    use x86_64::instructions::port::Port;
    // SAFETY: standard PIC data ports; writing 0xFF masks all lines.
    unsafe {
        let mut pic1_data: Port<u8> = Port::new(0x21);
        let mut pic2_data: Port<u8> = Port::new(0xA1);
        pic1_data.write(0xFFu8);
        pic2_data.write(0xFFu8);
    }
}

/// Enable the xAPIC and start the periodic timer on [`TIMER_VECTOR`].
///
/// `hhdm` is Limine's higher-half direct-map offset. The timer is calibrated
/// against PIT channel 2 for a ~1 ms tick. Must be called with the IDT already
/// holding handlers for `TIMER_VECTOR` + `SPURIOUS_VECTOR` and before enabling
/// interrupts. Returns true once the timer is armed.
pub fn init_timer(hhdm: u64) -> bool {
    mask_legacy_pic();

    // Resolve the LAPIC MMIO base from the MSR and ensure the APIC is globally
    // enabled in xAPIC mode (do not set bit 10 — that would request x2APIC).
    let base_msr = rdmsr(IA32_APIC_BASE);
    let phys_base = base_msr & APIC_BASE_ADDR_MASK;
    wrmsr(IA32_APIC_BASE, base_msr | APIC_BASE_GLOBAL_ENABLE);

    // Limine's HHDM does not map the LAPIC MMIO hole, so map it explicitly (one
    // page covers all registers, max offset 0x3E0). We map it at its HHDM address
    // so reg_read/reg_write can use hhdm + phys directly.
    let virt_base = hhdm + phys_base;
    if !crate::paging::map_mmio(hhdm, virt_base, phys_base) {
        crate::serial_println!("APIC: failed to map LAPIC MMIO page at phys {:#x}", phys_base);
        return false;
    }
    LAPIC_VIRT.store(virt_base, Ordering::Relaxed);

    // Software-enable the APIC and set the spurious vector.
    reg_write(REG_SVR, SVR_APIC_SOFTWARE_ENABLE | SPURIOUS_VECTOR as u32);

    // Calibrate against PIT ch2 (IRQs off, PICs masked), then arm periodic timer.
    let per_ms = calibrate_apic_per_ms();
    TICK_NS.store(QUANTUM_NS, Ordering::Relaxed);
    reg_write(REG_TIMER_DIV, TIMER_DIVIDE_BY_16);
    reg_write(REG_LVT_TIMER, LVT_TIMER_PERIODIC | TIMER_VECTOR as u32);
    reg_write(REG_TIMER_INIT, per_ms); // one interrupt == 1 ms

    let id = reg_read(REG_ID) >> 24;
    crate::serial_println!(
        "APIC calibrated: {} ticks/ms (PIT 10ms gate); tick={}ns",
        per_ms, QUANTUM_NS
    );
    crate::serial_println!(
        "APIC: xAPIC id={} @ phys {:#x} (virt {:#x}); periodic timer vec {:#x}",
        id,
        phys_base,
        virt_base,
        TIMER_VECTOR
    );
    true
}

/// Signal end-of-interrupt to the LAPIC. Call once at the end of every APIC ISR
/// (the timer ISR); spurious interrupts do not require an EOI.
#[inline]
pub fn eoi() {
    reg_write(REG_EOI, 0);
}
