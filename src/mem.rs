//! Memoria del guest respaldada por un memfd y las tres formas de mapearla.
//!
//! - `Shared`: la plantilla corre sobre el memfd (MAP_SHARED), así que lo que
//!   escribe queda en el memfd sin copiar nada al capturar.
//! - `Private`: cada rama mapea el mismo memfd con MAP_PRIVATE. Las lecturas
//!   comparten las páginas del page cache; la primera escritura a una página
//!   la copia a memoria anónima de esa rama (copy-on-write del kernel).
//! - `Anonymous`: memoria anónima donde se copia la plantilla entera (la línea
//!   de base "copiar toda la RAM").
//!
//! Después de capturar, el memfd se sella (F_SEAL_WRITE y compañía): el kernel
//! rechaza cualquier mapeo compartido con escritura, así que ninguna rama ni
//! el propio VMM puede cambiar la plantilla.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

pub const PAGE: usize = 4096;

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Archivo anónimo en memoria con el contenido de la plantilla.
#[derive(Debug)]
pub struct Memfd {
    fd: OwnedFd,
    size: usize,
}

impl AsFd for Memfd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Cómo mapear un memfd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapKind {
    /// MAP_SHARED lectura/escritura.
    Shared,
    /// MAP_SHARED solo lectura (para inspeccionar la plantilla sellada).
    SharedReadOnly,
    /// MAP_PRIVATE lectura/escritura: copy-on-write.
    Private,
}

/// Sellos que se aplican a la plantilla.
pub const TEMPLATE_SEALS: libc::c_int =
    libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;

impl Memfd {
    /// Crea un memfd de `size` bytes (múltiplo de página) que admite sellos.
    pub fn create(name: &CStr, size: usize) -> io::Result<Self> {
        if size == 0 || !size.is_multiple_of(PAGE) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tamaño cero o no múltiplo de 4 KiB",
            ));
        }
        // SAFETY: nombre terminado en NUL; flags válidos.
        let fd = cvt(unsafe {
            libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
                as libc::c_int
        })?;
        // SAFETY: fd recién creado y propio.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: fd válido.
        cvt(unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) })?;
        Ok(Self { fd, size })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Aplica `TEMPLATE_SEALS`. Falla con EBUSY si queda algún mapeo
    /// compartido con escritura.
    pub fn seal(&self) -> io::Result<()> {
        // SAFETY: fd válido.
        cvt(unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_ADD_SEALS, TEMPLATE_SEALS) })?;
        Ok(())
    }

    /// Sellos actuales.
    pub fn seals(&self) -> io::Result<libc::c_int> {
        // SAFETY: fd válido.
        cvt(unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GET_SEALS) })
    }

    pub fn map(&self, kind: MapKind) -> io::Result<Mapping> {
        let (prot, flags) = match kind {
            MapKind::Shared => (libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED),
            MapKind::SharedReadOnly => (libc::PROT_READ, libc::MAP_SHARED),
            MapKind::Private => (libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE),
        };
        // SAFETY: mapeo nuevo de un fd propio; el resultado se valida.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                self.size,
                prot,
                flags | libc::MAP_NORESERVE,
                self.fd.as_raw_fd(),
                0,
            )
        };
        Mapping::from_mmap(p, self.size, kind == MapKind::SharedReadOnly)
    }
}

/// Región mapeada; se desmapea al soltarla.
#[derive(Debug)]
pub struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
    read_only: bool,
}

// SAFETY: la región es propia; el acceso concurrente lo regulan `&`/`&mut`.
unsafe impl Send for Mapping {}
// SAFETY: a través de `&Mapping` solo se lee.
unsafe impl Sync for Mapping {}

impl Mapping {
    fn from_mmap(p: *mut libc::c_void, len: usize, read_only: bool) -> io::Result<Self> {
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: NonNull::new(p.cast()).expect("mmap devolvió NULL"),
            len,
            read_only,
        })
    }

    /// Memoria anónima privada de `len` bytes, en cero.
    pub fn anonymous(len: usize) -> io::Result<Self> {
        // SAFETY: mapeo anónimo nuevo.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        Self::from_mmap(p, len, false)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Dirección anfitriona (para `KVM_SET_USER_MEMORY_REGION` y smaps).
    pub fn host_addr(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr válido por len bytes mientras viva el mapeo.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// Escritura desde el host. Entra en pánico si el mapeo es de solo lectura.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        assert!(!self.read_only, "mapeo de solo lectura");
        // SAFETY: igual que as_slice, con acceso exclusivo por &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: ptr/len vienen de mmap.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// Campos de `/proc/self/smaps` de un mapeo, en KiB.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SmapsEntry {
    pub start: u64,
    pub rss_kib: u64,
    pub anonymous_kib: u64,
    pub private_dirty_kib: u64,
    pub shared_clean_kib: u64,
}

/// Parsea el texto de smaps y devuelve una entrada por mapeo.
pub fn parse_smaps(text: &str) -> Vec<SmapsEntry> {
    let mut out: Vec<SmapsEntry> = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(first) = it.next() else { continue };
        if let Some((a, _b)) = first.split_once('-') {
            if !first.ends_with(':') {
                if let Ok(start) = u64::from_str_radix(a, 16) {
                    out.push(SmapsEntry {
                        start,
                        ..Default::default()
                    });
                    continue;
                }
            }
        }
        let Some(cur) = out.last_mut() else { continue };
        let val = it.next().and_then(|v| v.parse::<u64>().ok());
        let Some(v) = val else { continue };
        match first {
            "Rss:" => cur.rss_kib = v,
            "Anonymous:" => cur.anonymous_kib = v,
            "Private_Dirty:" => cur.private_dirty_kib = v,
            "Shared_Clean:" => cur.shared_clean_kib = v,
            _ => {}
        }
    }
    out
}

/// Lee `/proc/self/smaps`.
pub fn read_smaps() -> io::Result<Vec<SmapsEntry>> {
    Ok(parse_smaps(&std::fs::read_to_string("/proc/self/smaps")?))
}

/// Campos de `/proc/meminfo` que importan para medir el costo de una rama.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemInfo {
    pub mem_available_kib: u64,
    pub page_tables_kib: u64,
    /// Tablas secundarias (EPT/NPT de KVM), desde Linux 6.0.
    pub sec_page_tables_kib: Option<u64>,
    pub shmem_kib: u64,
}

pub fn parse_meminfo(text: &str) -> MemInfo {
    let mut m = MemInfo::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next().and_then(|v| v.parse::<u64>().ok())) else {
            continue;
        };
        match k {
            "MemAvailable:" => m.mem_available_kib = v,
            "PageTables:" => m.page_tables_kib = v,
            "SecPageTables:" => m.sec_page_tables_kib = Some(v),
            "Shmem:" => m.shmem_kib = v,
            _ => {}
        }
    }
    m
}

pub fn read_meminfo() -> io::Result<MemInfo> {
    Ok(parse_meminfo(&std::fs::read_to_string("/proc/meminfo")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMAPS: &str = "\
7f0000000000-7f0000400000 rw-p 00000000 00:01 12   /memfd:forkvm (deleted)
Size:               4096 kB
Rss:                 120 kB
Shared_Clean:         80 kB
Private_Dirty:        40 kB
Anonymous:            40 kB
VmFlags: rd wr mr mw me nr
7f0000400000-7f0000401000 r--p 00000000 00:00 0
Rss:                   4 kB
Anonymous:             0 kB
";

    #[test]
    fn smaps_entries() {
        let e = parse_smaps(SMAPS);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].start, 0x7f00_0000_0000);
        assert_eq!(
            (
                e[0].rss_kib,
                e[0].anonymous_kib,
                e[0].private_dirty_kib,
                e[0].shared_clean_kib
            ),
            (120, 40, 40, 80)
        );
        assert_eq!(e[1].start, 0x7f00_0040_0000);
        assert_eq!(e[1].rss_kib, 4);
    }

    #[test]
    fn meminfo_fields() {
        let m = parse_meminfo(
            "MemTotal: 100 kB\nMemAvailable:   50 kB\nShmem: 7 kB\nPageTables: 3 kB\nSecPageTables: 2 kB\n",
        );
        assert_eq!(m.mem_available_kib, 50);
        assert_eq!(m.page_tables_kib, 3);
        assert_eq!(m.sec_page_tables_kib, Some(2));
        assert_eq!(m.shmem_kib, 7);
        assert_eq!(
            parse_meminfo("PageTables: 1 kB\n").sec_page_tables_kib,
            None
        );
    }

    #[test]
    fn rejects_bad_sizes() {
        assert!(Memfd::create(c"x", 0).is_err());
        assert!(Memfd::create(c"x", 4097).is_err());
    }
}
