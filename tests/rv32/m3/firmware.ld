/* firmware.ld: links m3-firmware.elf at the RAM base (docs/m3-design.md §7, §11.2). The
 * whole image must fit below the trap frame at 0x80010000. */
OUTPUT_ARCH("riscv")
ENTRY(_start)

SECTIONS
{
    . = 0x80000000;
    .text : { *(.text.init) }
    ASSERT(. <= 0x80010000, "m3-firmware.elf overlaps the trap frame")
    /DISCARD/ : { *(.comment) *(.note .note.*) }
}
