//! Imagen del guest, sus puertos y las funciones que el host necesita para
//! comprobar lo que calcula cada rama.
//!
//! `pattern`, `branch_pattern` y `checksum` son copias exactas de lo que hace
//! `guest/guest.S`; si cambian allá, tienen que cambiar acá.

use crate::layout::{MAILBOX_ADDR, TEMPLATE_INFO_ADDR, WARM_BASE};

include!(concat!(env!("OUT_DIR"), "/guest_syms.rs"));

/// Imagen plana del guest, enlazada para `GUEST_LOAD_ADDR`.
pub static GUEST_IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.bin"));

/// Puerto de salida: el guest termina con el código escrito.
pub const DEBUG_EXIT_PORT: u16 = 0xf4;
/// Puerto de salida: la plantilla llegó al punto de fork.
pub const FORK_PORT: u16 = 0xf5;
/// Puerto de entrada: cola de parámetros (u32) que entrega el VMM.
pub const PARAM_PORT: u16 = 0xf6;

/// Primera línea que imprime la plantilla.
pub const BANNER: &str = "cortex9-forkvm guest: plantilla x86-64";
/// Línea justo antes del punto de fork.
pub const READY_LINE: &str = "plantilla lista para fork";

pub const K_ADDR: u64 = 0x9e37_79b9_7f4a_7c15;
pub const K_MUL: u64 = 0xbf58_476d_1ce4_e5b9;
pub const K_BRANCH: u64 = 0x94d0_49bb_1331_11eb;
pub const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
pub const MARK_GPR: u64 = 0x15f0_f0f0_c0ff_ee15;
pub const MARK_XMM: u64 = 0x0123_4567_89ab_cdef;
pub const MARK_MSR: u64 = 0xffff_8000_1234_5000;
pub const MAGIC_TINFO: u64 = 0x4f46_4e49_504d_4554;
pub const MAGIC_MBOX: u64 = 0x2158_4f42_4c49_414d;
/// Todos los marcadores sobrevivieron: r15, xmm0, KERNEL_GS_BASE y TSC.
pub const FLAGS_ALL: u64 = 0b1111;
pub const PAGE: u64 = 4096;

/// Valor que la plantilla escribe en `addr`.
pub const fn pattern(addr: u64) -> u64 {
    (addr ^ K_ADDR).wrapping_mul(K_MUL)
}

/// Valor que la rama `id` escribe en `addr`.
pub const fn branch_pattern(addr: u64, id: u64) -> u64 {
    pattern(addr) ^ id.wrapping_add(1).wrapping_mul(K_BRANCH)
}

/// Un paso del checksum.
#[inline(always)]
pub const fn mix(h: u64, v: u64) -> u64 {
    (h.rotate_left(7) ^ v).wrapping_mul(FNV_PRIME)
}

/// Checksum de un bloque de memoria (qwords little-endian).
pub fn checksum(bytes: &[u8]) -> u64 {
    assert!(bytes.len().is_multiple_of(8), "longitud no múltiplo de 8");
    bytes
        .chunks_exact(8)
        .fold(0, |h, c| mix(h, u64::from_le_bytes(c.try_into().unwrap())))
}

/// Checksum que tiene que dar el conjunto caliente de `warm_bytes` cuando la
/// rama `branch` reescribió sus primeras `dirty_pages` páginas (`None`: la
/// plantilla, sin cambios). Se calcula sin tocar memoria del guest.
pub fn expected_warm_checksum(warm_bytes: u64, dirty_pages: u64, branch: Option<u64>) -> u64 {
    let dirty_end = WARM_BASE + dirty_pages.min(warm_bytes / PAGE) * PAGE;
    let mut h = 0;
    let mut a = WARM_BASE;
    while a < WARM_BASE + warm_bytes {
        let v = match branch {
            Some(id) if a < dirty_end => branch_pattern(a, id),
            _ => pattern(a),
        };
        h = mix(h, v);
        a += 8;
    }
    h
}

/// Checksum de la zona que escribió la rama.
pub fn expected_dirty_checksum(warm_bytes: u64, dirty_pages: u64, branch: u64) -> u64 {
    let end = WARM_BASE + dirty_pages.min(warm_bytes / PAGE) * PAGE;
    let mut h = 0;
    let mut a = WARM_BASE;
    while a < end {
        h = mix(h, branch_pattern(a, branch));
        a += 8;
    }
    h
}

fn qword(mem: &[u8], addr: u64) -> u64 {
    let s = addr as usize;
    u64::from_le_bytes(mem[s..s + 8].try_into().unwrap())
}

/// Lo que deja la plantilla en `TEMPLATE_INFO_ADDR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemplateInfo {
    pub magic: u64,
    pub warm_bytes: u64,
    pub warm_checksum: u64,
    pub tsc_at_fork: u64,
}

impl TemplateInfo {
    pub fn read(mem: &[u8]) -> Self {
        let b = TEMPLATE_INFO_ADDR;
        Self {
            magic: qword(mem, b),
            warm_bytes: qword(mem, b + 8),
            warm_checksum: qword(mem, b + 16),
            tsc_at_fork: qword(mem, b + 24),
        }
    }
}

/// Resultados de una rama en `MAILBOX_ADDR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mailbox {
    pub magic: u64,
    pub branch_id: u64,
    pub warm_checksum: u64,
    pub dirty_checksum: u64,
    pub mismatches: u64,
    pub passes: u64,
    pub flags: u64,
    pub tsc_start: u64,
    pub tsc_end: u64,
}

impl Mailbox {
    pub fn read(mem: &[u8]) -> Self {
        let b = MAILBOX_ADDR;
        Self {
            magic: qword(mem, b),
            branch_id: qword(mem, b + 8),
            warm_checksum: qword(mem, b + 16),
            dirty_checksum: qword(mem, b + 24),
            mismatches: qword(mem, b + 32),
            passes: qword(mem, b + 40),
            flags: qword(mem, b + 48),
            tsc_start: qword(mem, b + 56),
            tsc_end: qword(mem, b + 64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{setup_memory, GUEST_LOAD_ADDR, MIN_GUEST_MEM};

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn image_is_flat_and_small() {
        assert!(!GUEST_IMAGE.is_empty());
        assert!(GUEST_IMAGE.len() < 4096, "{} bytes", GUEST_IMAGE.len());
        assert_eq!(GUEST_IMAGE[0], 0xfc, "la entrada debe ser `cld`");
        assert_ne!(&GUEST_IMAGE[..4], b"\x7fELF");
        for m in [BANNER, READY_LINE] {
            assert!(contains(GUEST_IMAGE, m.as_bytes()), "falta {m:?}");
        }
    }

    #[test]
    fn branch_entry_follows_fork_out() {
        // `out 0xf5, al` (e6 f5) termina exactamente en branch_entry.
        let off = (BRANCH_ENTRY - GUEST_LOAD_ADDR) as usize;
        assert!(off >= 2 && off < GUEST_IMAGE.len());
        assert_eq!(&GUEST_IMAGE[off - 2..off], &[0xe6, FORK_PORT as u8]);
        // y lo primero que hace la rama es `mov dx, 0xf6` + `in eax, dx`.
        assert_eq!(&GUEST_IMAGE[off..off + 5], &[0x66, 0xba, 0xf6, 0x00, 0xed]);
    }

    #[test]
    fn image_embeds_the_constants() {
        for k in [K_ADDR, K_MUL, K_BRANCH, FNV_PRIME, MARK_GPR, MARK_MSR] {
            assert!(contains(GUEST_IMAGE, &k.to_le_bytes()), "falta {k:#x}");
        }
    }

    #[test]
    fn image_loads_below_warm_base() {
        let mut mem = vec![0u8; MIN_GUEST_MEM];
        setup_memory(&mut mem, GUEST_IMAGE).unwrap();
        assert!(GUEST_LOAD_ADDR + (GUEST_IMAGE.len() as u64) < WARM_BASE);
    }

    #[test]
    fn checksum_matches_streaming_definition() {
        let warm = 3 * PAGE;
        let mut mem = Vec::new();
        let mut a = WARM_BASE;
        while a < WARM_BASE + warm {
            mem.extend_from_slice(&pattern(a).to_le_bytes());
            a += 8;
        }
        assert_eq!(checksum(&mem), expected_warm_checksum(warm, 0, None));
        assert_eq!(checksum(&mem), expected_warm_checksum(warm, 2, None));
        // Reescribir la primera página como la rama 5.
        for (i, c) in mem[..PAGE as usize].chunks_exact_mut(8).enumerate() {
            c.copy_from_slice(&branch_pattern(WARM_BASE + 8 * i as u64, 5).to_le_bytes());
        }
        assert_eq!(checksum(&mem), expected_warm_checksum(warm, 1, Some(5)));
        assert_eq!(
            checksum(&mem[..PAGE as usize]),
            expected_dirty_checksum(warm, 1, 5)
        );
    }

    #[test]
    fn branches_get_distinct_checksums() {
        let warm = 4 * PAGE;
        let mut seen = std::collections::HashSet::new();
        assert!(seen.insert(expected_warm_checksum(warm, 1, None)));
        for id in 0..64 {
            assert!(
                seen.insert(expected_warm_checksum(warm, 1, Some(id))),
                "{id}"
            );
        }
    }

    #[test]
    fn dirty_pages_are_clamped_to_warm_set() {
        let warm = 2 * PAGE;
        assert_eq!(
            expected_warm_checksum(warm, 99, Some(1)),
            expected_warm_checksum(warm, 2, Some(1))
        );
        assert_eq!(
            expected_dirty_checksum(warm, 99, 1),
            expected_dirty_checksum(warm, 2, 1)
        );
    }

    #[test]
    fn checksum_known_vector() {
        assert_eq!(checksum(&[]), 0);
        assert_eq!(checksum(&1u64.to_le_bytes()), FNV_PRIME);
        let h1 = FNV_PRIME;
        assert_eq!(
            checksum(&[1u64.to_le_bytes(), 2u64.to_le_bytes()].concat()),
            ((h1.rotate_left(7)) ^ 2).wrapping_mul(FNV_PRIME)
        );
    }
}
