//! Pruebas reales sobre KVM. Necesitan `/dev/kvm` con lectura y escritura y
//! FALLAN (no se saltean) si no está: en CI corren en el job `kvm`, que
//! además verifica `/dev/kvm` antes de empezar.
//!
//! `cargo test --release --test kvm -- --test-threads=1`

use cortex9_forkvm::bench::{aggregate, run_round, Expected, RoundConfig};
use cortex9_forkvm::guest::{
    expected_warm_checksum, BANNER, BRANCH_ENTRY, MARK_MSR, PAGE, READY_LINE,
};
use cortex9_forkvm::mem::TEMPLATE_SEALS;
use cortex9_forkvm::template::{BranchParams, ForkMode, Template, TemplateConfig};
use cortex9_forkvm::vm::{open_kvm, Event};
use kvm_ioctls::Kvm;

const MEM: usize = 64 << 20;
const WARM: u64 = 16 << 20;
const DIRTY: u32 = 64;

fn kvm() -> Kvm {
    open_kvm().unwrap_or_else(|e| {
        panic!("esta prueba necesita /dev/kvm con lectura/escritura y no se puede abrir: {e}")
    })
}

fn template(kvm: &Kvm, seal: bool) -> Template {
    Template::boot(
        kvm,
        TemplateConfig {
            mem_size: MEM,
            warm_bytes: WARM,
            seal,
        },
    )
    .expect("arranque de la plantilla")
}

fn round(n: usize, mode: ForkMode, pool: bool) -> RoundConfig {
    RoundConfig {
        branches: n,
        mode,
        pool,
        dirty_pages: DIRTY,
        work_ms: 50,
    }
}

#[test]
fn template_reaches_fork_point_and_is_sealed() {
    let kvm = kvm();
    let t = template(&kvm, true);
    assert_eq!(
        String::from_utf8_lossy(&t.serial),
        format!("{BANNER}\n{READY_LINE}\n")
    );
    assert_eq!(t.info.warm_bytes, WARM);
    assert_eq!(
        t.info.warm_checksum,
        expected_warm_checksum(WARM, 0, None),
        "el checksum del guest coincide con la referencia del host"
    );
    // immediate_exit completó el OUT: RIP ya está en branch_entry.
    assert_eq!(t.state.regs.rip, BRANCH_ENTRY, "{:?}", t.state);
    assert_eq!(t.state.msr(0xc000_0102), Some(MARK_MSR));
    assert!(t.state.msr(0x10).is_some(), "TSC capturado");
    assert_eq!(t.memfd().seals().unwrap() & TEMPLATE_SEALS, TEMPLATE_SEALS);
    assert!(t.unchanged().unwrap());
    eprintln!(
        "arranque->fork {:.3} ms, captura {:.3} ms, xsave={} xcrs={} msrs={}",
        t.boot_ns as f64 / 1e6,
        t.capture_ns as f64 / 1e6,
        t.state.xsave.is_some(),
        t.state.xcrs.is_some(),
        t.state.msrs.len()
    );
}

#[test]
fn one_branch_runs_with_state_intact() {
    let kvm = kvm();
    let t = template(&kvm, true);
    let params = BranchParams {
        id: 3,
        dirty_pages: DIRTY,
        tsc_budget: 0,
        max_passes: 4,
    };
    let (mut m, _) = t.fork(&kvm, None, ForkMode::Cow, params).unwrap();
    m.stop_on_param = true;
    assert_eq!(m.run(100).unwrap(), Event::ParamRead);
    assert_eq!(m.run(100).unwrap(), Event::DebugExit(0));
    let mb = cortex9_forkvm::guest::Mailbox::read(m.mem.as_slice());
    assert_eq!(mb.branch_id, 3);
    assert_eq!(mb.flags, 0b1111, "r15, xmm0, KERNEL_GS_BASE y TSC");
    assert_eq!(mb.passes, 4);
    assert_eq!(mb.mismatches, 0);
    assert_eq!(
        mb.warm_checksum,
        expected_warm_checksum(WARM, DIRTY as u64, Some(3))
    );
    assert!(mb.tsc_start >= t.info.tsc_at_fork);
    assert!(t.unchanged().unwrap());
}

#[test]
fn eight_cow_branches_are_isolated() {
    let kvm = kvm();
    let t = template(&kvm, true);
    let exp = Expected::compute(WARM, DIRTY, 8);
    let r = run_round(&kvm, &t, &exp, &round(8, ForkMode::Cow, false)).unwrap();
    for b in &r.branches {
        assert!(b.isolated(), "rama {}: {:?}", b.id, b.problems);
        let dirty_kib = DIRTY as u64 * PAGE / 1024;
        // Copias = páginas escritas + unas pocas de pila/mailbox/tablas.
        assert!(
            b.anonymous_kib >= dirty_kib && b.anonymous_kib <= dirty_kib + 256,
            "rama {}: {} KiB anónimos para {} KiB escritos",
            b.id,
            b.anonymous_kib,
            dirty_kib
        );
    }
    assert!(r.template_unchanged);
    let a = aggregate(std::slice::from_ref(&r));
    assert!(a.isolation_ok(), "{:?}", a.problems);
    assert_eq!(a.distinct_checksums, 8);
}

#[test]
fn pooled_shells_fork_too() {
    let kvm = kvm();
    let t = template(&kvm, true);
    let exp = Expected::compute(WARM, DIRTY, 4);
    let r = run_round(&kvm, &t, &exp, &round(4, ForkMode::Cow, true)).unwrap();
    assert!(
        r.isolation_ok(),
        "{:?}",
        aggregate(std::slice::from_ref(&r)).problems
    );
    assert!(r.branches.iter().all(|b| b.timing.shell_ns < 1_000_000));
}

#[test]
fn repeated_rounds_keep_the_template_intact() {
    let kvm = kvm();
    let t = template(&kvm, true);
    let exp = Expected::compute(WARM, DIRTY, 2);
    for _ in 0..3 {
        let r = run_round(&kvm, &t, &exp, &round(2, ForkMode::Cow, false)).unwrap();
        assert!(r.isolation_ok());
    }
    assert!(t.unchanged().unwrap());
}

#[test]
fn copy_baseline_is_isolated_but_costs_the_whole_ram() {
    let kvm = kvm();
    let t = template(&kvm, true);
    let exp = Expected::compute(WARM, DIRTY, 2);
    let r = run_round(&kvm, &t, &exp, &round(2, ForkMode::Copy, false)).unwrap();
    assert!(
        r.isolation_ok(),
        "{:?}",
        aggregate(std::slice::from_ref(&r)).problems
    );
    for b in &r.branches {
        assert!(
            b.anonymous_kib * 1024 >= MEM as u64 * 9 / 10,
            "copia completa: {} KiB",
            b.anonymous_kib
        );
    }
}

#[test]
fn sealed_template_refuses_shared_fork() {
    let kvm = kvm();
    let t = template(&kvm, true);
    let p = BranchParams {
        id: 0,
        dirty_pages: 1,
        tsc_budget: 0,
        max_passes: 1,
    };
    let err = t
        .fork(&kvm, None, ForkMode::SharedUnsafe, p)
        .err()
        .expect("debe fallar");
    assert!(err.to_string().contains("mmap compartido"), "{err}");
}

#[test]
fn shared_mapping_control_is_detected() {
    // Control negativo: sin MAP_PRIVATE las ramas se pisan entre sí y la
    // plantilla cambia; la verificación lo tiene que ver.
    let kvm = kvm();
    let t = template(&kvm, false);
    let exp = Expected::compute(WARM, DIRTY, 4);
    let r = run_round(&kvm, &t, &exp, &round(4, ForkMode::SharedUnsafe, false)).unwrap();
    assert!(!r.template_unchanged, "la plantilla tenía que cambiar");
    let bad = r.branches.iter().filter(|b| !b.isolated()).count();
    assert!(bad >= 3, "solo {bad} de 4 ramas detectadas");
}
