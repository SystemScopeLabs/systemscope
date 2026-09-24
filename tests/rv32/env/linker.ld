/* SystemScope layout for riscv-tests (docs/m1-design.md §10.2). See
 * LICENSE.riscv-test-env: derived from env/p/link.ld of riscv-test-env.
 *
 * The image starts at the base of the m1-reference RAM (16 MiB at 0x8000_0000); the
 * fixture build checks that it fits. Sections are placed in a fixed order at fixed
 * alignments, as in the upstream link.ld, and no section has a separate load address
 * (no AT>), so every PT_LOAD has p_paddr == p_vaddr as the SystemScope ELF loader
 * requires (§8). The entry is _start, the first word of .text.init at the RAM base.
 *
 * There is deliberately no MEMORY region: with `> RAM`, GNU ld places each section at
 * the region's next free address and ignores the `. = ALIGN(...)` between sections.
 */

OUTPUT_ARCH("riscv")
ENTRY(_start)

SECTIONS
{
  . = 0x80000000;
  .text.init : { *(.text.init) }
  . = ALIGN(0x1000);
  .tohost : { *(.tohost) }
  . = ALIGN(0x1000);
  .text : { *(.text) *(.text.*) }
  .rodata : { *(.rodata) *(.rodata.*) *(.srodata) *(.srodata.*) }
  . = ALIGN(0x1000);
  .data : { *(.data) *(.data.*) *(.sdata) *(.sdata.*) }
  .bss : { *(.sbss) *(.sbss.*) *(.bss) *(.bss.*) *(COMMON) }
  _end = .;
}
