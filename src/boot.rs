//! Estado inicial de la vCPU para entrar directo en modo largo.
//!
//! Funciones puras sobre `kvm_sregs`/`kvm_regs`: se prueban sin KVM.

use crate::layout::{
    decode_segment, Segment, CODE_SELECTOR, DATA_SELECTOR, GDT, GDT_ADDR, PML4_ADDR, STACK_TOP,
};
use kvm_bindings::{kvm_regs, kvm_segment, kvm_sregs};

pub const CR0_PE: u64 = 1 << 0;
pub const CR0_MP: u64 = 1 << 1;
pub const CR0_ET: u64 = 1 << 4;
pub const CR0_NE: u64 = 1 << 5;
pub const CR0_WP: u64 = 1 << 16;
pub const CR0_PG: u64 = 1 << 31;
pub const CR4_PAE: u64 = 1 << 5;
pub const CR4_OSFXSR: u64 = 1 << 9;
pub const CR4_OSXMMEXCPT: u64 = 1 << 10;
pub const EFER_SCE: u64 = 1 << 0;
pub const EFER_LME: u64 = 1 << 8;
pub const EFER_LMA: u64 = 1 << 10;
/// Bit 1 de RFLAGS está reservado y siempre en 1.
pub const RFLAGS_RESERVED: u64 = 1 << 1;

/// Convierte un descriptor decodificado al formato de KVM.
pub fn to_kvm_segment(s: &Segment) -> kvm_segment {
    kvm_segment {
        base: s.base,
        limit: s.limit,
        selector: s.selector,
        type_: s.type_,
        present: s.present,
        dpl: s.dpl,
        db: s.db,
        s: s.s,
        l: s.l,
        g: s.g,
        avl: s.avl,
        unusable: 0,
        padding: 0,
    }
}

/// Ajusta `sregs` (tal como los devuelve KVM tras el reset) para modo largo:
/// GDT, segmentos planos, CR3 a la PML4, PAE, paginación y EFER.LME|LMA.
pub fn long_mode_sregs(mut sregs: kvm_sregs) -> kvm_sregs {
    sregs.gdt.base = GDT_ADDR;
    sregs.gdt.limit = (std::mem::size_of_val(&GDT) - 1) as u16;
    sregs.idt.base = 0;
    sregs.idt.limit = 0;

    let code = to_kvm_segment(&decode_segment(GDT[1], CODE_SELECTOR));
    let data = to_kvm_segment(&decode_segment(GDT[2], DATA_SELECTOR));
    sregs.cs = code;
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;

    sregs.cr3 = PML4_ADDR;
    sregs.cr4 |= CR4_PAE | CR4_OSFXSR | CR4_OSXMMEXCPT;
    sregs.cr0 |= CR0_PE | CR0_MP | CR0_ET | CR0_NE | CR0_WP | CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA | EFER_SCE;
    sregs
}

/// Registros generales iniciales: RIP en la entrada, pila y RFLAGS.
pub fn initial_regs(entry: u64) -> kvm_regs {
    kvm_regs {
        rip: entry,
        rsp: STACK_TOP,
        rbp: STACK_TOP,
        rflags: RFLAGS_RESERVED,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::GUEST_LOAD_ADDR;

    #[test]
    fn control_registers_enable_long_mode() {
        let s = long_mode_sregs(kvm_sregs::default());
        assert_eq!(s.cr0 & (CR0_PE | CR0_PG), CR0_PE | CR0_PG);
        assert_eq!(s.cr0, 0x8001_0033);
        assert_ne!(s.cr4 & CR4_PAE, 0);
        assert_eq!(s.efer & (EFER_LME | EFER_LMA), EFER_LME | EFER_LMA);
        assert_eq!(s.cr3, PML4_ADDR);
    }

    #[test]
    fn preserves_reset_bits() {
        // CD|NW|ET como tras el reset
        let base = kvm_sregs {
            cr0: 0x6000_0010,
            ..Default::default()
        };
        let s = long_mode_sregs(base);
        assert_eq!(s.cr0 & 0x6000_0010, 0x6000_0010);
    }

    #[test]
    fn segments_are_flat_64bit() {
        let s = long_mode_sregs(kvm_sregs::default());
        assert_eq!(s.cs.selector, CODE_SELECTOR);
        assert_eq!((s.cs.l, s.cs.db, s.cs.present, s.cs.s), (1, 0, 1, 1));
        assert_eq!(s.cs.type_, 0xb);
        for seg in [s.ds, s.es, s.fs, s.gs, s.ss] {
            assert_eq!(seg.selector, DATA_SELECTOR);
            assert_eq!((seg.type_, seg.present, seg.base), (0x3, 1, 0));
        }
        assert_eq!(s.gdt.base, GDT_ADDR);
        assert_eq!(s.gdt.limit, 23);
    }

    #[test]
    fn regs_point_to_entry() {
        let r = initial_regs(GUEST_LOAD_ADDR);
        assert_eq!(r.rip, GUEST_LOAD_ADDR);
        assert_eq!(r.rsp, STACK_TOP);
        assert_eq!(r.rflags, 2);
        assert_eq!(r.rax, 0);
    }
}
