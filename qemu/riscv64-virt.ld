OUTPUT_ARCH(riscv)
ENTRY(_start)

MEMORY
{
  RAM : ORIGIN = 0x80000000, LENGTH = 128M
}

SECTIONS
{
  .text : {
    *(.text.entry)
    *(.text .text.*)
  } > RAM

  .rodata : {
    *(.rodata .rodata.*)
  } > RAM

  .data : {
    *(.data .data.*)
  } > RAM

  .bss : {
    __bss_start = .;
    *(.bss .bss.*)
    *(COMMON)
    __bss_end = .;
  } > RAM

  __stack_top = 0x88000000;

  /DISCARD/ : {
    *(.eh_frame*)
  }
}
