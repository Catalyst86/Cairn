/* Limine higher-half linker script for keystone (x86-64).
 * Matches the layout expected by the limine 0.3 crate (requests in .requests
 * section, kept inside the data segment, plus start/end markers).
 *
 * Kernel virtual base mandated by Limine spec: 0xffffffff80000000
 */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kmain)

PHDRS
{
    text    PT_LOAD;
    rodata  PT_LOAD;
    data    PT_LOAD;
}

SECTIONS
{
    . = 0xffffffff80000000;

    .text : {
        *(.text .text.*)
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .data : {
        *(.data .data.*)

        /* The GOT must live in a file-backed section BEFORE .bss. Otherwise the
         * NOBITS .bss is sandwiched between .data and .got, lld makes the whole
         * segment file-backed (FileSiz == MemSiz), and Limine fails to map .bss
         * -> write page-fault on the first .bss access (e.g. a large static). */
        *(.got .got.*)
        *(.got.plt)

        /* Limine request markers (required by the 0.3+ crate) */
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))
    } :data

    /* Page-align .bss so it begins in its OWN page rather than sharing the last
     * file-backed page of .data. When .bss starts mid-page (e.g. 0x300c0 inside
     * the 0x30000 .data page), the bootloader leaves that in-page .bss region
     * unmapped -> write page-fault on the first large static. Its own page makes
     * .bss a clean NOBITS tail that Limine maps as zeroed writable pages. */
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .bss : {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(CONSTANT(MAXPAGESIZE));
        __bss_end = .;
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
    }
}
