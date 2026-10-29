//! Mapa de memoria del guest, GDT y tablas de páginas de 4 niveles.
//!
//! Todo lo de este módulo es puro: escribe en un `&mut [u8]` que representa
//! la memoria física del guest, así que se prueba sin `/dev/kvm`.

/// Dirección física de la GDT.
pub const GDT_ADDR: u64 = 0x500;
/// PML4 (nivel 4).
pub const PML4_ADDR: u64 = 0x9000;
/// PDPT (nivel 3).
pub const PDPT_ADDR: u64 = 0xA000;
/// Page directory (nivel 2) con páginas de 2 MiB.
pub const PD_ADDR: u64 = 0xB000;
/// Tope inicial de la pila del guest (crece hacia abajo).
pub const STACK_TOP: u64 = 0x8_0000;
/// Dirección de carga y entrada del guest (1 MiB).
pub const GUEST_LOAD_ADDR: u64 = 0x10_0000;
/// Memoria mínima del guest (las tablas, la pila y la imagen entran acá).
pub const MIN_GUEST_MEM: usize = 4 << 20;
/// Página con datos de la plantilla que escribe el guest antes del fork.
pub const TEMPLATE_INFO_ADDR: u64 = 0x6000;
/// Página donde cada rama deja sus resultados.
pub const MAILBOX_ADDR: u64 = 0x7000;
/// Inicio del conjunto de trabajo "caliente" que llena el guest antes del fork.
pub const WARM_BASE: u64 = 0x40_0000;
/// Memoria máxima soportada (lo que cubre el mapa de identidad).
pub const MAX_GUEST_MEM: usize = 1 << 30;
/// Bytes mapeados con identidad: 512 entradas de 2 MiB = 1 GiB.
pub const IDENTITY_MAP_BYTES: u64 = 512 * (2 << 20);

/// Selector del segmento de código de 64 bits (entrada 1).
pub const CODE_SELECTOR: u16 = 0x08;
/// Selector del segmento de datos (entrada 2).
pub const DATA_SELECTOR: u16 = 0x10;

// Bits de entradas de tablas de páginas.
pub const PTE_PRESENT: u64 = 1 << 0;
pub const PTE_WRITABLE: u64 = 1 << 1;
pub const PTE_HUGE: u64 = 1 << 7;

// Las regiones no se solapan y las tablas están alineadas a página:
// se verifica en compilación.
const _: () = {
    assert!(GDT_ADDR + 8 * GDT.len() as u64 <= TEMPLATE_INFO_ADDR);
    assert!(TEMPLATE_INFO_ADDR + 0x1000 <= MAILBOX_ADDR);
    assert!(MAILBOX_ADDR + 0x1000 <= PML4_ADDR);
    assert!(WARM_BASE as usize == MIN_GUEST_MEM);
    assert!(PML4_ADDR + 0x1000 <= PDPT_ADDR);
    assert!(PDPT_ADDR + 0x1000 <= PD_ADDR);
    assert!(PD_ADDR + 0x1000 <= STACK_TOP - 0x1000);
    assert!(STACK_TOP <= GUEST_LOAD_ADDR);
    assert!(
        PML4_ADDR.is_multiple_of(0x1000)
            && PDPT_ADDR.is_multiple_of(0x1000)
            && PD_ADDR.is_multiple_of(0x1000)
    );
    assert!((GUEST_LOAD_ADDR as usize) < MIN_GUEST_MEM);
    assert!(MAX_GUEST_MEM as u64 <= IDENTITY_MAP_BYTES);
};

/// Errores al preparar la memoria del guest.
#[derive(Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// Una escritura cae fuera de la memoria del guest.
    OutOfBounds { addr: u64, len: usize },
    /// La imagen del guest pisaría memoria que no le corresponde.
    ImageTooLarge { len: usize, max: usize },
    /// Imagen vacía.
    EmptyImage,
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfBounds { addr, len } => {
                write!(f, "escritura fuera de rango: {len} bytes en {addr:#x}")
            }
            Self::ImageTooLarge { len, max } => {
                write!(f, "imagen de {len} bytes supera el máximo de {max}")
            }
            Self::EmptyImage => write!(f, "imagen del guest vacía"),
        }
    }
}

impl std::error::Error for LayoutError {}

/// Codifica un descriptor de segmento de la GDT.
///
/// `flags` son los 12 bits de acceso + flags tal como en la forma clásica
/// `0xGFAA` (G/DB/L/AVL en el nibble alto, byte de acceso abajo).
pub const fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    let base = base as u64;
    let limit = limit as u64;
    let flags = flags as u64;
    ((base & 0xff00_0000) << 32)
        | ((base & 0x00ff_ffff) << 16)
        | (limit & 0x0000_ffff)
        | ((limit & 0x000f_0000) << 32)
        | ((flags & 0xf0ff) << 40)
}

/// GDT mínima: nula, código de 64 bits (L=1) y datos.
pub const GDT: [u64; 3] = [
    0,
    gdt_entry(0xa09b, 0, 0xfffff), // G=1 L=1, P=1 DPL=0 S=1 código exec/read accedido
    gdt_entry(0xc093, 0, 0xfffff), // G=1 DB=1, P=1 DPL=0 S=1 datos read/write accedido
];

/// Campos decodificados de un descriptor (lo que KVM espera en `kvm_segment`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub type_: u8,
    pub present: u8,
    pub dpl: u8,
    pub db: u8,
    pub s: u8,
    pub l: u8,
    pub g: u8,
    pub avl: u8,
}

/// Decodifica un descriptor de la GDT, expandiendo el límite si G=1.
pub const fn decode_segment(entry: u64, selector: u16) -> Segment {
    let base = ((entry >> 16) & 0x00ff_ffff) | ((entry >> 32) & 0xff00_0000);
    let raw_limit = ((entry & 0xffff) | ((entry >> 32) & 0x000f_0000)) as u32;
    let g = ((entry >> 55) & 1) as u8;
    let limit = if g == 1 {
        (raw_limit << 12) | 0xfff
    } else {
        raw_limit
    };
    Segment {
        base,
        limit,
        selector,
        type_: ((entry >> 40) & 0xf) as u8,
        s: ((entry >> 44) & 1) as u8,
        dpl: ((entry >> 45) & 3) as u8,
        present: ((entry >> 47) & 1) as u8,
        avl: ((entry >> 52) & 1) as u8,
        l: ((entry >> 53) & 1) as u8,
        db: ((entry >> 54) & 1) as u8,
        g,
    }
}

fn write(mem: &mut [u8], addr: u64, bytes: &[u8]) -> Result<(), LayoutError> {
    let oob = || LayoutError::OutOfBounds {
        addr,
        len: bytes.len(),
    };
    let start = usize::try_from(addr).map_err(|_| oob())?;
    let end = start.checked_add(bytes.len()).ok_or_else(oob)?;
    let dst = mem.get_mut(start..end).ok_or_else(oob)?;
    dst.copy_from_slice(bytes);
    Ok(())
}

/// Lee un `u64` little-endian de la memoria del guest.
pub fn read_u64(mem: &[u8], addr: u64) -> Option<u64> {
    let start = usize::try_from(addr).ok()?;
    let bytes = mem.get(start..start.checked_add(8)?)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

/// Escribe la GDT en `GDT_ADDR`.
pub fn write_gdt(mem: &mut [u8]) -> Result<(), LayoutError> {
    for (i, e) in GDT.iter().enumerate() {
        write(mem, GDT_ADDR + 8 * i as u64, &e.to_le_bytes())?;
    }
    Ok(())
}

/// Construye tablas de 4 niveles (PML4 → PDPT → PD con páginas de 2 MiB)
/// que mapean con identidad el primer GiB.
pub fn write_page_tables(mem: &mut [u8]) -> Result<(), LayoutError> {
    write(
        mem,
        PML4_ADDR,
        &(PDPT_ADDR | PTE_PRESENT | PTE_WRITABLE).to_le_bytes(),
    )?;
    write(
        mem,
        PDPT_ADDR,
        &(PD_ADDR | PTE_PRESENT | PTE_WRITABLE).to_le_bytes(),
    )?;
    for i in 0..512u64 {
        let entry = (i << 21) | PTE_PRESENT | PTE_WRITABLE | PTE_HUGE;
        write(mem, PD_ADDR + 8 * i, &entry.to_le_bytes())?;
    }
    Ok(())
}

/// Traduce una dirección virtual recorriendo las tablas como lo haría la MMU
/// (solo soporta el caso que construimos: páginas de 2 MiB en el PD).
pub fn translate(mem: &[u8], cr3: u64, vaddr: u64) -> Option<u64> {
    let mask = 0x000f_ffff_ffff_f000u64;
    let pml4e = read_u64(mem, (cr3 & mask) + 8 * ((vaddr >> 39) & 0x1ff))?;
    if pml4e & PTE_PRESENT == 0 {
        return None;
    }
    let pdpte = read_u64(mem, (pml4e & mask) + 8 * ((vaddr >> 30) & 0x1ff))?;
    if pdpte & PTE_PRESENT == 0 || pdpte & PTE_HUGE != 0 {
        return None;
    }
    let pde = read_u64(mem, (pdpte & mask) + 8 * ((vaddr >> 21) & 0x1ff))?;
    if pde & PTE_PRESENT == 0 || pde & PTE_HUGE == 0 {
        return None;
    }
    Some((pde & 0x000f_ffff_ffe0_0000) | (vaddr & 0x1f_ffff))
}

/// Copia la imagen del guest a `GUEST_LOAD_ADDR`, verificando que entra.
pub fn load_image(mem: &mut [u8], image: &[u8]) -> Result<u64, LayoutError> {
    if image.is_empty() {
        return Err(LayoutError::EmptyImage);
    }
    let max = mem.len().saturating_sub(GUEST_LOAD_ADDR as usize);
    if image.len() > max {
        return Err(LayoutError::ImageTooLarge {
            len: image.len(),
            max,
        });
    }
    write(mem, GUEST_LOAD_ADDR, image)?;
    Ok(GUEST_LOAD_ADDR)
}

/// Prepara toda la memoria: GDT, tablas de páginas e imagen. Devuelve la
/// dirección de entrada.
pub fn setup_memory(mem: &mut [u8], image: &[u8]) -> Result<u64, LayoutError> {
    write_gdt(mem)?;
    write_page_tables(mem)?;
    load_image(mem, image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdt_entries_match_known_encodings() {
        // Valores de referencia usados por Linux/Firecracker para el mismo layout.
        assert_eq!(GDT[0], 0);
        assert_eq!(GDT[1], 0x00af_9b00_0000_ffff);
        assert_eq!(GDT[2], 0x00cf_9300_0000_ffff);
    }

    #[test]
    fn decode_code_segment_is_64bit() {
        let s = decode_segment(GDT[1], CODE_SELECTOR);
        assert_eq!(s.base, 0);
        assert_eq!(s.limit, 0xffff_ffff);
        assert_eq!((s.type_, s.present, s.dpl, s.s), (0xb, 1, 0, 1));
        assert_eq!((s.l, s.db, s.g), (1, 0, 1));
    }

    #[test]
    fn decode_data_segment() {
        let s = decode_segment(GDT[2], DATA_SELECTOR);
        assert_eq!((s.type_, s.l, s.db, s.g, s.present), (0x3, 0, 1, 1, 1));
        assert_eq!(s.limit, 0xffff_ffff);
    }

    #[test]
    fn decode_roundtrips_base_and_limit() {
        let e = gdt_entry(0x4093, 0x1234_5678, 0xabcde);
        let s = decode_segment(e, 0);
        assert_eq!(s.base, 0x1234_5678);
        assert_eq!(s.limit, 0xabcde); // G=0: sin escalar
        assert_eq!(s.g, 0);
        assert_eq!(s.db, 1);
    }

    #[test]
    fn page_tables_identity_map_first_gib() {
        let mut mem = vec![0u8; MIN_GUEST_MEM];
        write_page_tables(&mut mem).unwrap();
        for vaddr in [
            0u64,
            0x1000,
            GUEST_LOAD_ADDR,
            GUEST_LOAD_ADDR + 0x1234,
            STACK_TOP - 8,
            0x3fff_ffff,
            0x2345_6789,
        ] {
            assert_eq!(translate(&mem, PML4_ADDR, vaddr), Some(vaddr), "{vaddr:#x}");
        }
        assert_eq!(translate(&mem, PML4_ADDR, IDENTITY_MAP_BYTES), None);
    }

    #[test]
    fn page_table_entries_have_expected_bits() {
        let mut mem = vec![0u8; MIN_GUEST_MEM];
        write_page_tables(&mut mem).unwrap();
        assert_eq!(read_u64(&mem, PML4_ADDR), Some(PDPT_ADDR | 0b11));
        assert_eq!(read_u64(&mem, PDPT_ADDR), Some(PD_ADDR | 0b11));
        assert_eq!(read_u64(&mem, PD_ADDR), Some(0x83));
        assert_eq!(read_u64(&mem, PD_ADDR + 8 * 511), Some((511 << 21) | 0x83));
        // Solo una entrada en PML4 y PDPT.
        assert_eq!(read_u64(&mem, PML4_ADDR + 8), Some(0));
        assert_eq!(read_u64(&mem, PDPT_ADDR + 8), Some(0));
    }

    #[test]
    fn load_image_bounds() {
        let mut mem = vec![0u8; MIN_GUEST_MEM];
        assert_eq!(load_image(&mut mem, &[]), Err(LayoutError::EmptyImage));
        let max = MIN_GUEST_MEM - GUEST_LOAD_ADDR as usize;
        let big = vec![0x90u8; max + 1];
        assert_eq!(
            load_image(&mut mem, &big),
            Err(LayoutError::ImageTooLarge { len: max + 1, max })
        );
        let ok = vec![0xf4u8; max];
        assert_eq!(load_image(&mut mem, &ok), Ok(GUEST_LOAD_ADDR));
        assert_eq!(mem[GUEST_LOAD_ADDR as usize], 0xf4);
        assert_eq!(*mem.last().unwrap(), 0xf4);
    }

    #[test]
    fn write_rejects_out_of_bounds() {
        let mut mem = vec![0u8; 16];
        assert!(write(&mut mem, 12, &[0; 4]).is_ok());
        assert!(write(&mut mem, 13, &[0; 4]).is_err());
        assert!(write(&mut mem, u64::MAX, &[0; 1]).is_err());
        assert!(write_page_tables(&mut mem).is_err());
    }

    #[test]
    fn setup_memory_places_everything() {
        let mut mem = vec![0u8; MIN_GUEST_MEM];
        let entry = setup_memory(&mut mem, &[0xf4]).unwrap();
        assert_eq!(entry, GUEST_LOAD_ADDR);
        assert_eq!(read_u64(&mem, GDT_ADDR + 8), Some(GDT[1]));
        assert_eq!(translate(&mem, PML4_ADDR, entry), Some(entry));
        assert_eq!(mem[GUEST_LOAD_ADDR as usize], 0xf4);
    }
}
