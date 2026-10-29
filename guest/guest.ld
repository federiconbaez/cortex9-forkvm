/* Imagen plana: .text.entry primero para que la entrada quede en el offset 0. */
ENTRY(_start)
SECTIONS
{
    . = 0x100000;
    .text : { *(.text.entry) *(.text*) }
    .rodata : { *(.rodata*) }
    /DISCARD/ : { *(.data*) *(.bss*) *(.note*) *(.comment) *(.eh_frame*) }
}
