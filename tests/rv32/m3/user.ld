/* user.ld: links the M3 reference programs that have only code and constants (ping,
 * pong, fault, badptr) at USER_BASE (docs/m3-design.md §11.2, §12.2). user-data.ld is
 * the same with the data and bss segments added, for hello.
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
}

SECTIONS
{
    . = 0x10000;
    .text : { *(.text.init) *(.text .text.*) } :text
    . = ALIGN(0x1000);
    .rodata : { *(.rodata .rodata.*) } :rodata
    .data : { *(.data .data.*) *(.bss .bss.*) } :rodata
    ASSERT(SIZEOF(.data) == 0, "a program with data or bss must be linked with user-data.ld")
    /DISCARD/ : { *(.comment) *(.note .note.*) }
}
