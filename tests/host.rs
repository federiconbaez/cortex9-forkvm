//! Semántica de memoria en la que se apoya el fork, probada en el host sin
//! KVM: MAP_PRIVATE de un memfd es copy-on-write, el sello impide cambiar la
//! plantilla y smaps cuenta como anónimas solo las páginas copiadas.

use cortex9_forkvm::mem::{read_smaps, MapKind, Mapping, Memfd, PAGE, TEMPLATE_SEALS};
use std::os::fd::{AsFd, AsRawFd};

const PAGES: usize = 64;

fn filled() -> Memfd {
    let fd = Memfd::create(c"forkvm-test", PAGES * PAGE).unwrap();
    let mut m = fd.map(MapKind::Shared).unwrap();
    for (i, b) in m.as_mut_slice().chunks_exact_mut(PAGE).enumerate() {
        b.fill(i as u8);
    }
    drop(m);
    fd
}

fn anon_kib(m: &Mapping) -> u64 {
    read_smaps()
        .unwrap()
        .into_iter()
        .find(|e| e.start == m.host_addr())
        .expect("el mapeo aparece en smaps")
        .anonymous_kib
}

#[test]
fn private_mappings_are_copy_on_write() {
    let fd = filled();
    let mut a = fd.map(MapKind::Private).unwrap();
    let mut b = fd.map(MapKind::Private).unwrap();
    a.as_mut_slice()[0] = 0xaa;
    b.as_mut_slice()[0] = 0xbb;
    b.as_mut_slice()[PAGE] = 0xcc;
    assert_eq!(a.as_slice()[0], 0xaa);
    assert_eq!(b.as_slice()[0], 0xbb);
    assert_eq!(
        a.as_slice()[PAGE],
        1,
        "la página 1 de A no ve la escritura de B"
    );
    let ro = fd.map(MapKind::SharedReadOnly).unwrap();
    assert_eq!(ro.as_slice()[0], 0, "la plantilla no cambia");
    assert_eq!(ro.as_slice()[PAGE], 1);
}

#[test]
fn unsealed_template_leaks_into_untouched_private_pages() {
    // Por esto se sella: una página que la rama todavía no copió muestra
    // lo que se escriba después en el memfd.
    let fd = filled();
    let p = fd.map(MapKind::Private).unwrap();
    assert_eq!(p.as_slice()[3 * PAGE], 3);
    let mut s = fd.map(MapKind::Shared).unwrap();
    s.as_mut_slice()[3 * PAGE] = 0x77;
    assert_eq!(p.as_slice()[3 * PAGE], 0x77);
}

#[test]
fn seal_blocks_every_way_of_changing_the_template() {
    let fd = filled();
    let shared = fd.map(MapKind::Shared).unwrap();
    let err = fd.seal().unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EBUSY), "{err}");
    drop(shared);
    fd.seal().unwrap();
    assert_eq!(fd.seals().unwrap() & TEMPLATE_SEALS, TEMPLATE_SEALS);

    let err = fd.map(MapKind::Shared).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EPERM), "{err}");

    // Tampoco con write(2) ni ftruncate(2).
    let raw = fd.as_fd().as_raw_fd();
    // SAFETY: fd válido; buffer de 1 byte.
    let w = unsafe { libc::pwrite(raw, [1u8].as_ptr().cast(), 1, 0) };
    assert_eq!(w, -1);
    // SAFETY: fd válido.
    assert_eq!(unsafe { libc::ftruncate(raw, 0) }, -1);

    // Las ramas privadas siguen pudiendo escribir en su copia.
    let mut p = fd.map(MapKind::Private).unwrap();
    p.as_mut_slice()[0] = 9;
    assert_eq!(p.as_slice()[0], 9);
    let ro = fd.map(MapKind::SharedReadOnly).unwrap();
    assert_eq!(ro.as_slice()[0], 0);
}

#[test]
fn smaps_counts_only_copied_pages_as_anonymous() {
    let fd = filled();
    let mut p = fd.map(MapKind::Private).unwrap();
    // Leer todo no crea páginas anónimas.
    let sum: u64 = p.as_slice().iter().map(|&b| b as u64).sum();
    assert!(sum > 0);
    assert_eq!(anon_kib(&p), 0);
    // Escribir en 5 páginas crea exactamente 5 copias.
    for i in [0, 7, 8, 30, 63] {
        p.as_mut_slice()[i * PAGE + 17] = 1;
    }
    assert_eq!(anon_kib(&p), 5 * (PAGE as u64 / 1024));
    // Reescribir las mismas páginas no suma.
    p.as_mut_slice()[7 * PAGE] = 2;
    assert_eq!(anon_kib(&p), 5 * (PAGE as u64 / 1024));
}

#[test]
fn anonymous_copy_baseline_costs_the_whole_ram() {
    let fd = filled();
    let src = fd.map(MapKind::SharedReadOnly).unwrap();
    let mut dst = Mapping::anonymous(PAGES * PAGE).unwrap();
    dst.as_mut_slice().copy_from_slice(src.as_slice());
    assert_eq!(dst.as_slice()[5 * PAGE], 5);
    assert_eq!(anon_kib(&dst), (PAGES * PAGE / 1024) as u64);
}

#[test]
fn neighbouring_anonymous_copies_stay_separate_in_smaps() {
    // Dos copias anónimas seguidas: cada una tiene que verse con su propio
    // conteo (sin guardas, el kernel puede fusionar los VMAs).
    let mut a = Mapping::anonymous(8 * PAGE).unwrap();
    let mut b = Mapping::anonymous(8 * PAGE).unwrap();
    a.as_mut_slice().fill(1);
    b.as_mut_slice()[..3 * PAGE].fill(2);
    assert_eq!(anon_kib(&a), 8 * (PAGE as u64 / 1024));
    assert_eq!(anon_kib(&b), 3 * (PAGE as u64 / 1024));
}
