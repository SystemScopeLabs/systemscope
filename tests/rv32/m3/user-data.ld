/* user-data.ld: links hello, the M3 reference program with all four segment kinds, at
 * USER_BASE (docs/m3-design.md §11.2, §12.2). user.ld is the same without data and bss.
 *
 * Each output section gets its own PT_LOAD with its own permissions and starts on a
 * fresh page, because the kernel maps permissions per page and rejects two segments that
 * share one (§8.3).
 */
OUTPUT_ARCH("riscv")
ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);   /* R X */
    rodata PT_LOAD FLAGS(4); /* R */
    data PT_LOAD FLAGS(6);   /* R W */
    bss PT_LOAD FLAGS(6);    /* R W */
}

SECTIONS
{
    . = 0x10000;
    .text : { *(.text.init) *(.text .text.*) } :text
    . = ALIGN(0x1000);
    .rodata : { *(.rodata .rodata.*) } :rodata
    . = ALIGN(0x1000);
    .data : { *(.data .data.*) } :data
    . = ALIGN(0x1000);
    .bss : { *(.bss .bss.*) } :bss
    /DISCARD/ : { *(.comment) *(.note .note.*) }
}
