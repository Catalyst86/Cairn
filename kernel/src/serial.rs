//! 16550 UART driver for COM1 (0x3F8) using x86 port I/O via inline asm.
//! Provides a spin-locked global writer and the serial_print / serial_println macros.
//!
//! This is the only I/O the keystone v0 kernel performs on boot.

use core::fmt::{self, Write};
use spin::Mutex;

/// COM1 base port (standard PC serial).
const COM1: u16 = 0x3F8;

/// Raw port I/O. These are the only `unsafe` blocks in the kernel for v0.
#[inline]
unsafe fn outb(port: u16, val: u8) {
    // SAFETY: port I/O to a well-known 16550 COM1 during early boot.
    // Only called from controlled init/write paths.
    core::arch::asm!(
        "out dx, al",
        in("dx") port,
        in("al") val,
        options(nomem, nostack, preserves_flags)
    );
}

#[inline]
unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    // SAFETY: read from known serial status/data port.
    core::arch::asm!(
        "in al, dx",
        out("al") val,
        in("dx") port,
        options(nomem, nostack, preserves_flags)
    );
    val
}

/// Initialize COM1 for 38400 8N1 with FIFO.
pub fn init() {
    // SAFETY: all port writes are to the standard COM1 registers during boot.
    unsafe {
        outb(COM1 + 1, 0x00); // disable all interrupts
        outb(COM1 + 3, 0x80); // enable DLAB (set baud rate divisor)
        outb(COM1 + 0, 0x03); // divisor = 3 (38400 baud)
        outb(COM1 + 1, 0x00); // high byte of divisor
        outb(COM1 + 3, 0x03); // 8 bits, no parity, one stop bit (DLAB off)
        outb(COM1 + 2, 0xC7); // enable FIFO, clear them, 14-byte threshold
        outb(COM1 + 4, 0x0B); // IRQs enabled, RTS/DSR set
    }
}

/// Wait until the transmitter holding register is empty, then write a byte.
#[inline]
fn write_byte(b: u8) {
    // Wait for THRE (bit 5 of LSR).
    // SAFETY: status read + data write to COM1.
    unsafe {
        while (inb(COM1 + 5) & 0x20) == 0 {
            // spin (very short)
        }
        outb(COM1, b);
    }
}

/// A fmt::Write implementation over the UART.
pub struct Serial;

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                write_byte(b'\r');
            }
            write_byte(byte);
        }
        Ok(())
    }
}

/// Global locked serial port. Using spin::Mutex because we are pre-emptible
/// only by interrupts we control (none yet) and single-core boot path.
pub static SERIAL: Mutex<Serial> = Mutex::new(Serial);

/// Print to serial without trailing newline.
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut guard = $crate::serial::SERIAL.lock();
        let _ = write!(guard, $($arg)*);
    }};
}

/// Print a line to serial (with \r\n handling inside the writer).
#[macro_export]
macro_rules! serial_println {
    () => {
        $crate::serial_print!("\n")
    };
    ($($arg:tt)*) => {
        $crate::serial_print!("{}\n", format_args!($($arg)*))
    };
}
