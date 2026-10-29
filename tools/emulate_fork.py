#!/usr/bin/env python3
"""Ejecuta el guest de forkvm en Unicorn (emulación, sin KVM).

Comprueba la lógica del guest, no el VMM ni el copy-on-write: corre la
plantilla hasta el OUT del punto de fork, guarda contexto y memoria, y
después corre varias ramas, cada una desde una copia de esa memoria y con su
propio id, contra checksums calculados acá de forma independiente.

Uso:
    pip install unicorn
    python3 tools/emulate_fork.py IMAGEN BRANCH_ENTRY_HEX [--ramas 4]
    python3 tools/emulate_fork.py IMAGEN BRANCH_ENTRY_HEX --control

Sale con 0 solo si la plantilla y todas las ramas dan lo esperado. Con
--control se altera un qword del conjunto caliente (fuera de la zona que
escribe la rama) en la copia de la rama 0, y sale con 0 solo si eso se
detecta.
"""
import struct
import sys

from unicorn import UC_ARCH_X86, UC_HOOK_INSN, UC_MODE_64, Uc
from unicorn.x86_const import (UC_X86_INS_IN, UC_X86_INS_OUT, UC_X86_REG_CR0,
                               UC_X86_REG_CR3, UC_X86_REG_CR4, UC_X86_REG_RIP,
                               UC_X86_REG_RSP)

LOAD, PML4, PDPT, PD, STACK = 0x100000, 0x9000, 0xA000, 0xB000, 0x80000
TINFO, MBOX, WARM_BASE = 0x6000, 0x7000, 0x400000
MEM = 8 << 20
WARM = 1 << 20
DIRTY = 16
M64 = (1 << 64) - 1
K_ADDR, K_MUL = 0x9E3779B97F4A7C15, 0xBF58476D1CE4E5B9
K_BRANCH, FNV = 0x94D049BB133111EB, 0x100000001B3
MAGIC_TINFO, MAGIC_MBOX = 0x4F464E49504D4554, 0x21584F424C49414D


def pattern(a):
    return ((a ^ K_ADDR) * K_MUL) & M64


def branch_pattern(a, i):
    return pattern(a) ^ (((i + 1) * K_BRANCH) & M64)


def mix(h, v):
    h = ((h << 7) | (h >> 57)) & M64
    return ((h ^ v) * FNV) & M64


def expected(branch):
    end = WARM_BASE + DIRTY * 4096
    h = d = 0
    for a in range(WARM_BASE, WARM_BASE + WARM, 8):
        if branch is not None and a < end:
            v = branch_pattern(a, branch)
            d = mix(d, v)
        else:
            v = pattern(a)
        h = mix(h, v)
    return h, d


def q(mu, addr, n):
    return struct.unpack("<%dQ" % n, mu.mem_read(addr, 8 * n))


def main():
    image = open(sys.argv[1], "rb").read()
    branch_entry = int(sys.argv[2], 16)
    ramas = int(sys.argv[sys.argv.index("--ramas") + 1]) if "--ramas" in sys.argv else 4
    mu = Uc(UC_ARCH_X86, UC_MODE_64)
    mu.mem_map(0, MEM)
    mu.mem_write(LOAD, image)
    mu.mem_write(PML4, struct.pack("<Q", PDPT | 3))
    mu.mem_write(PDPT, struct.pack("<Q", PD | 3))
    for i in range(512):
        mu.mem_write(PD + 8 * i, struct.pack("<Q", (i << 21) | 0x83))
    mu.reg_write(UC_X86_REG_CR3, PML4)
    mu.reg_write(UC_X86_REG_RSP, STACK)
    mu.reg_write(UC_X86_REG_CR4, mu.reg_read(UC_X86_REG_CR4) | 0x620)
    mu.reg_write(UC_X86_REG_CR0, 0x80010033)

    st = {"params": [WARM], "event": None, "lcr": 0, "out": bytearray()}

    def on_in(uc, port, size, _):
        if port == 0x3FD:
            return 0x60
        if port == 0xF6:
            return st["params"].pop(0)
        return 0

    def on_out(uc, port, size, value, _):
        if port == 0x3FB:
            st["lcr"] = value
        elif port == 0x3F8 and not st["lcr"] & 0x80:
            st["out"].append(value & 0xFF)
        elif port in (0xF4, 0xF5):
            st["event"] = (port, value & 0xFF)
            uc.emu_stop()

    mu.hook_add(UC_HOOK_INSN, on_in, None, 1, 0, UC_X86_INS_IN)
    mu.hook_add(UC_HOOK_INSN, on_out, None, 1, 0, UC_X86_INS_OUT)
    mu.emu_start(LOAD, LOAD + len(image), count=200_000_000)

    ok = True
    sys.stdout.write(st["out"].decode())
    if st["event"] != (0xF5, 1):
        print(f"[emu] la plantilla no llegó al fork: {st['event']}")
        return 1
    magic, warm, csum, _tsc = q(mu, TINFO, 4)
    want_t, _ = expected(None)
    print(f"[emu] plantilla: warm={warm} checksum={csum:#x} esperado={want_t:#x}")
    ok &= magic == MAGIC_TINFO and warm == WARM and csum == want_t

    ctx = mu.context_save()
    snap = bytes(mu.mem_read(0, MEM))
    seen = set()
    control = "--control" in sys.argv
    for rid in range(ramas):
        mu.mem_write(0, snap)
        if control and rid == 0:
            mu.mem_write(WARM_BASE + WARM - 8, b"\x01" * 8)
        mu.context_restore(ctx)
        mu.reg_write(UC_X86_REG_RIP, branch_entry)
        st.update(params=[rid, DIRTY, 0, 0, 2], event=None)
        mu.emu_start(branch_entry, LOAD + len(image), count=200_000_000)
        mb = q(mu, MBOX, 9)
        want_w, want_d = expected(rid)
        good = (st["event"] == (0xF4, 0) and mb[0] == MAGIC_MBOX and mb[1] == rid
                and mb[2] == want_w and mb[3] == want_d and mb[4] == 0
                and mb[5] == 2 and mb[6] & 0b111 == 0b111)
        print(f"[emu] rama {rid}: salida={st['event']} checksum={mb[2]:#x} "
              f"esperado={want_w:#x} pasadas={mb[5]} pisados={mb[4]} "
              f"flags={mb[6]:#06b} -> {'ok' if good else 'FALLO'}")
        ok &= good
        seen.add(mb[2])
    ok &= len(seen) == ramas
    print(f"[emu] {len(seen)} checksums distintos en {ramas} ramas: {'ok' if ok else 'FALLO'}")
    if control:
        print(f"[emu] control: alteración {'detectada' if not ok else 'NO detectada'}")
        return 0 if not ok else 1
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
