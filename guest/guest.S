/*
 * Guest x86-64 de cortex9-forkvm.
 *
 * El VMM lo carga en 1 MiB y entra con la CPU ya en modo largo. Tiene dos
 * fases separadas por el punto de fork:
 *
 * Plantilla (una sola vez):
 *   1. inicializa la UART de COM1, imprime un banner y verifica el modo largo;
 *   2. lee WARM_BYTES del puerto de parámetros (0xf6) y llena
 *      [WARM_BASE, WARM_BASE + WARM_BYTES) con pattern(addr);
 *   3. guarda el checksum del conjunto, deja marcadores en r15, xmm0 y el
 *      MSR KERNEL_GS_BASE y el TSC actual en TEMPLATE_INFO;
 *   4. escribe 1 en el puerto de fork (0xf5). El VMM congela la VM ahí.
 *
 * Rama (cada hijo arranca en branch_entry, justo después del OUT):
 *   5. lee del puerto 0xf6: id de rama, páginas a ensuciar, presupuesto de
 *      TSC (dos mitades) y máximo de pasadas (si no es 0, se hacen
 *      exactamente esas pasadas e ignora el presupuesto);
 *   6. comprueba que r15, xmm0, el MSR y el TSC sobrevivieron al fork;
 *   7. en bucle (al menos una vez): reescribe las primeras páginas
 *      del conjunto con branch_pattern(addr, id) y verifica que sigan ahí;
 *   8. calcula el checksum de todo el conjunto y de la zona escrita, deja
 *      todo en MAILBOX y sale por el puerto 0xf4 con código 0.
 *
 * Las funciones pattern y checksum están duplicadas en src/guest.rs.
 */
    .intel_syntax noprefix
    .code64

    .set COM1,            0x3f8
    .set DEBUG_EXIT,      0xf4
    .set FORK_PORT,       0xf5
    .set PARAM_PORT,      0xf6
    .set MSR_EFER,        0xc0000080
    .set MSR_KGS_BASE,    0xc0000102
    .set WARM_BASE,       0x400000
    .set TINFO,           0x6000
    .set MBOX,            0x7000
    .set PAGE,            4096

    .set K_ADDR,          0x9e3779b97f4a7c15
    .set K_MUL,           0xbf58476d1ce4e5b9
    .set K_BRANCH,        0x94d049bb133111eb
    .set FNV_PRIME,       0x100000001b3
    .set MARK_GPR,        0x15f0f0f0c0ffee15
    .set MARK_XMM,        0x0123456789abcdef
    .set MARK_MSR,        0xffff800012345000
    .set MAGIC_TINFO,     0x4f464e49504d4554
    .set MAGIC_MBOX,      0x21584f424c49414d

    .section .text.entry, "ax"
    .globl _start
_start:
    cld
    /* UART: DLAB=1, divisor 1 (115200), 8N1, FIFO, DTR|RTS. */
    mov dx, COM1 + 3
    mov al, 0x80
    out dx, al
    mov dx, COM1 + 0
    mov al, 0x01
    out dx, al
    mov dx, COM1 + 1
    xor eax, eax
    out dx, al
    mov dx, COM1 + 3
    mov al, 0x03
    out dx, al
    mov dx, COM1 + 2
    mov al, 0xc7
    out dx, al
    mov dx, COM1 + 4
    mov al, 0x03
    out dx, al

    lea rsi, [rip + msg_banner]
    call puts

    mov ecx, MSR_EFER
    rdmsr
    bt eax, 10
    jnc fail
    mov rax, cr0
    bt rax, 31
    jnc fail
    mov rax, cr4
    bt rax, 5
    jnc fail

    /* WARM_BYTES (múltiplo de 8). */
    mov dx, PARAM_PORT
    in eax, dx
    mov r14, rax

    /* Llenado del conjunto caliente. */
    mov rdi, WARM_BASE
    lea rcx, [rdi + r14]
    movabs r9, K_ADDR
    movabs r10, K_MUL
1:
    cmp rdi, rcx
    jae 2f
    mov rax, rdi
    xor rax, r9
    imul rax, r10
    mov [rdi], rax
    add rdi, 8
    jmp 1b
2:
    mov rsi, WARM_BASE
    mov rcx, r14
    call checksum
    mov [TINFO + 16], rax
    mov [TINFO + 8], r14
    movabs rax, MAGIC_TINFO
    mov [TINFO], rax

    /* Marcadores que tienen que sobrevivir al fork. */
    movabs r15, MARK_GPR
    movabs rax, MARK_XMM
    movq xmm0, rax
    mov ecx, MSR_KGS_BASE
    movabs rax, MARK_MSR
    mov rdx, rax
    shr rdx, 32
    wrmsr
    rdtsc
    shl rdx, 32
    or rax, rdx
    mov [TINFO + 24], rax

    lea rsi, [rip + msg_ready]
    call puts

    mov al, 1
    out FORK_PORT, al

    .globl branch_entry
branch_entry:
    mov dx, PARAM_PORT
    in eax, dx
    mov rbx, rax                /* id de rama */
    in eax, dx
    mov r12, rax                /* páginas a ensuciar */
    in eax, dx
    mov r13, rax
    in eax, dx
    shl rax, 32
    or r13, rax                 /* presupuesto en ciclos de TSC */
    in eax, dx
    mov r11, rax                /* máximo de pasadas (0 = sin límite) */

    /* No ensuciar más que el conjunto caliente. */
    mov rax, r14
    shr rax, 12
    cmp r12, rax
    jbe 3f
    mov r12, rax
3:
    xor ebp, ebp
    movabs rax, MARK_GPR
    cmp r15, rax
    jne 4f
    or ebp, 1
4:
    movq rax, xmm0
    movabs rcx, MARK_XMM
    cmp rax, rcx
    jne 5f
    or ebp, 2
5:
    mov ecx, MSR_KGS_BASE
    rdmsr
    shl rdx, 32
    or rax, rdx
    movabs rcx, MARK_MSR
    cmp rax, rcx
    jne 6f
    or ebp, 4
6:
    rdtsc
    shl rdx, 32
    or rax, rdx
    mov [MBOX + 56], rax
    mov rdi, rax                /* TSC al empezar */
    cmp rax, [TINFO + 24]
    jb 7f
    or ebp, 8
7:
    mov [MBOX + 48], rbp
    mov qword ptr [MBOX + 32], 0

    lea rax, [rbx + 1]
    movabs rcx, K_BRANCH
    imul rax, rcx
    mov r8, rax                 /* clave de la rama */
    movabs r9, K_ADDR
    movabs r10, K_MUL
    xor ebp, ebp                /* pasadas */

work:
    /* Escritura: primeras r12 páginas con branch_pattern. */
    mov rsi, WARM_BASE
    mov rcx, r12
    shl rcx, 12
    add rcx, rsi
8:
    cmp rsi, rcx
    jae 9f
    mov rax, rsi
    xor rax, r9
    imul rax, r10
    xor rax, r8
    mov [rsi], rax
    add rsi, 8
    jmp 8b
9:
    /* Verificación: cada qword tiene que seguir siendo de esta rama. */
    mov rsi, WARM_BASE
10:
    cmp rsi, rcx
    jae 12f
    mov rax, rsi
    xor rax, r9
    imul rax, r10
    xor rax, r8
    cmp rax, [rsi]
    je 11f
    inc qword ptr [MBOX + 32]
11:
    add rsi, 8
    jmp 10b
12:
    inc rbp
    test r11, r11
    jz 13f
    cmp rbp, r11                /* con máximo: exactamente r11 pasadas */
    jb work
    jmp 14f
13:
    rdtsc
    shl rdx, 32
    or rax, rdx
    sub rax, rdi
    cmp rax, r13
    jb work
14:
    mov [MBOX + 40], rbp
    mov rsi, WARM_BASE
    mov rcx, r14
    call checksum
    mov [MBOX + 16], rax
    mov rsi, WARM_BASE
    mov rcx, r12
    shl rcx, 12
    call checksum
    mov [MBOX + 24], rax
    rdtsc
    shl rdx, 32
    or rax, rdx
    mov [MBOX + 64], rax
    mov [MBOX + 8], rbx
    movabs rax, MAGIC_MBOX
    mov [MBOX], rax
    xor eax, eax
    jmp exit

fail:
    lea rsi, [rip + msg_fail]
    call puts
    mov al, 1
exit:
    out DEBUG_EXIT, al
15:
    hlt
    jmp 15b

/*
 * checksum(rsi = inicio, rcx = bytes, múltiplo de 8) -> rax.
 * h = 0; por cada qword v: h = rotl(h, 7) ^ v; h = h * FNV_PRIME.
 * Usa rax, rcx, rdx, rsi.
 */
checksum:
    xor eax, eax
    movabs rdx, FNV_PRIME
    add rcx, rsi
16:
    cmp rsi, rcx
    jae 17f
    rol rax, 7
    xor rax, [rsi]
    imul rax, rdx
    add rsi, 8
    jmp 16b
17:
    ret

/* putc(al): espera THRE en LSR y transmite. Preserva todo salvo rdx. */
putc:
    push rax
    mov dx, COM1 + 5
18:
    in al, dx
    test al, 0x20
    jz 18b
    pop rax
    mov dx, COM1
    out dx, al
    ret

/* puts(rsi): cadena terminada en NUL. */
puts:
    push rsi
19:
    lodsb
    test al, al
    jz 20f
    call putc
    jmp 19b
20:
    pop rsi
    ret

    .section .rodata
msg_banner: .asciz "cortex9-forkvm guest: plantilla x86-64\n"
msg_ready:  .asciz "plantilla lista para fork\n"
msg_fail:   .asciz "guest: FALLO comprobando el modo largo\n"
