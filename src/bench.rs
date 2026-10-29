//! Fork de N ramas en paralelo desde una plantilla, con medición de
//! latencia, memoria y aislamiento.
//!
//! Cada ronda:
//! 1. (pool) cada hilo crea su VM vacía antes de largar;
//! 2. todos los hilos largan juntos; cada uno hace `fork` y corre su rama
//!    hasta la primera lectura de parámetro: eso es "rama lista" y define la
//!    latencia de fork de esa rama;
//! 3. cuando todas están listas, cada rama hace su trabajo (escribir y
//!    verificar sus páginas durante `work_ms`, luego checksums) y sale;
//! 4. con todas las ramas todavía vivas se leen smaps y meminfo;
//! 5. se verifica cada rama contra la referencia del host y la plantilla
//!    contra su checksum original.

use crate::guest::{
    checksum, expected_dirty_checksum, expected_warm_checksum, Mailbox, FLAGS_ALL, MAGIC_MBOX,
};
use crate::layout::WARM_BASE;
use crate::mem::{read_meminfo, read_smaps, MemInfo};
use crate::stats::{json_str, summarize, Summary};
use crate::template::{BranchParams, ForkMode, ForkTiming, Template};
use crate::vm::{Event, Shell, VmmError};
use kvm_ioctls::Kvm;
use std::collections::HashMap;
use std::sync::{Barrier, Mutex};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct RoundConfig {
    pub branches: usize,
    pub mode: ForkMode,
    pub pool: bool,
    pub dirty_pages: u32,
    pub work_ms: u64,
}

/// Resultado de una rama.
#[derive(Debug, Clone, Default)]
pub struct BranchResult {
    pub id: u32,
    /// Desde que el hilo empieza el fork hasta que la rama ejecutó su primera
    /// instrucción de E/S (lectura del id).
    pub ready_ns: u64,
    pub timing: ForkTiming,
    pub first_run_ns: u64,
    pub mailbox: Mailbox,
    pub host_warm_checksum: u64,
    pub anonymous_kib: u64,
    pub rss_kib: u64,
    pub problems: Vec<String>,
}

impl BranchResult {
    pub fn isolated(&self) -> bool {
        self.problems.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct RoundResult {
    pub cfg: RoundConfig,
    pub branches: Vec<BranchResult>,
    /// Desde la largada hasta que la última rama quedó lista.
    pub all_ready_ns: u64,
    pub before: MemInfo,
    pub after: MemInfo,
    pub template_unchanged: bool,
}

impl RoundResult {
    pub fn isolation_ok(&self) -> bool {
        self.template_unchanged && self.branches.iter().all(BranchResult::isolated)
    }

    /// Caída de MemAvailable por rama, en KiB (incluye memoria del kernel:
    /// estructuras de KVM, EPT, tablas de páginas).
    pub fn mem_available_drop_per_branch_kib(&self) -> i64 {
        (self.before.mem_available_kib as i64 - self.after.mem_available_kib as i64)
            / self.cfg.branches as i64
    }
}

/// Referencias del host por id de rama (se calculan una vez).
pub struct Expected {
    warm: HashMap<u32, u64>,
    dirty: HashMap<u32, u64>,
}

impl Expected {
    pub fn compute(warm_bytes: u64, dirty_pages: u32, max_id: u32) -> Self {
        let ids: Vec<u32> = (0..max_id).collect();
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8);
        let chunk = ids.len().div_ceil(workers).max(1);
        let pairs: Vec<(u32, u64, u64)> = std::thread::scope(|s| {
            let hs: Vec<_> = ids
                .chunks(chunk)
                .map(|c| {
                    s.spawn(move || {
                        c.iter()
                            .map(|&id| {
                                (
                                    id,
                                    expected_warm_checksum(
                                        warm_bytes,
                                        dirty_pages as u64,
                                        Some(id as u64),
                                    ),
                                    expected_dirty_checksum(
                                        warm_bytes,
                                        dirty_pages as u64,
                                        id as u64,
                                    ),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
        });
        Self {
            warm: pairs.iter().map(|&(i, w, _)| (i, w)).collect(),
            dirty: pairs.iter().map(|&(i, _, d)| (i, d)).collect(),
        }
    }
}

fn verify(
    r: &mut BranchResult,
    exit: Option<Event>,
    tpl: &Template,
    exp: &Expected,
    host_mem: Option<&[u8]>,
) {
    let mb = r.mailbox;
    let id = r.id;
    if exit != Some(Event::DebugExit(0)) {
        r.problems.push(format!("terminó con {exit:?}"));
    }
    if mb.magic != MAGIC_MBOX {
        r.problems
            .push(format!("mailbox sin magic ({:#x})", mb.magic));
    }
    if mb.branch_id != id as u64 {
        r.problems
            .push(format!("id leído {} en vez de {id}", mb.branch_id));
    }
    if mb.flags != FLAGS_ALL {
        r.problems.push(format!(
            "marcadores perdidos: flags={:#06b} (r15|xmm0|KGS_BASE|TSC)",
            mb.flags
        ));
    }
    if mb.mismatches != 0 {
        r.problems.push(format!(
            "{} qwords propios pisados durante el trabajo",
            mb.mismatches
        ));
    }
    if mb.passes == 0 {
        r.problems.push("cero pasadas de trabajo".into());
    }
    let want_warm = exp.warm.get(&id).copied();
    if Some(mb.warm_checksum) != want_warm {
        r.problems.push(format!(
            "checksum del guest {:#x} != esperado {:?}",
            mb.warm_checksum, want_warm
        ));
    }
    if Some(mb.dirty_checksum) != exp.dirty.get(&id).copied() {
        r.problems
            .push("checksum de la zona escrita no coincide".to_string());
    }
    if let Some(mem) = host_mem {
        let s = WARM_BASE as usize;
        r.host_warm_checksum = checksum(&mem[s..s + tpl.cfg.warm_bytes as usize]);
        if Some(r.host_warm_checksum) != want_warm {
            r.problems.push(format!(
                "checksum leído por el host {:#x} != esperado",
                r.host_warm_checksum
            ));
        }
    }
}

/// Corre una ronda de `cfg.branches` ramas en paralelo.
pub fn run_round(
    kvm_fd: &Kvm,
    tpl: &Template,
    exp: &Expected,
    cfg: &RoundConfig,
) -> Result<RoundResult, VmmError> {
    let n = cfg.branches;
    let tsc_budget = cfg.work_ms * tpl.tsc_khz as u64;
    let start = Barrier::new(n + 1);
    let ready = Barrier::new(n + 1);
    let done = Barrier::new(n + 1);
    let measured = Barrier::new(n + 1);
    let smaps_slot: Mutex<Vec<crate::mem::SmapsEntry>> = Mutex::new(Vec::new());

    let before = read_meminfo().map_err(|e| VmmError::Io("/proc/meminfo", e))?;
    let (results, all_ready_ns, after) = std::thread::scope(|s| {
        let handles: Vec<_> = (0..n as u32)
            .map(|id| {
                let (start, ready, done, measured) = (&start, &ready, &done, &measured);
                let smaps_slot = &smaps_slot;
                s.spawn(move || {
                    let mut r = BranchResult {
                        id,
                        ..Default::default()
                    };
                    let params = BranchParams {
                        id,
                        dirty_pages: cfg.dirty_pages,
                        tsc_budget,
                        max_passes: 0,
                    };
                    let shell = if cfg.pool {
                        match Shell::new(kvm_fd, &tpl.cpuid) {
                            Ok(s) => Some(s),
                            Err(e) => {
                                r.problems.push(format!("pool: {e}"));
                                None
                            }
                        }
                    } else {
                        None
                    };
                    start.wait();
                    let t0 = Instant::now();
                    let mut machine = None;
                    if r.problems.is_empty() {
                        match tpl.fork(kvm_fd, shell, cfg.mode, params) {
                            Ok((mut m, timing)) => {
                                r.timing = timing;
                                m.stop_on_param = true;
                                let c = Instant::now();
                                match m.run(1_000) {
                                    Ok(Event::ParamRead) => {
                                        r.first_run_ns = c.elapsed().as_nanos() as u64;
                                        r.ready_ns = t0.elapsed().as_nanos() as u64;
                                        machine = Some(m);
                                    }
                                    Ok(ev) => r.problems.push(format!("primer evento {ev:?}")),
                                    Err(e) => r.problems.push(format!("primer run: {e}")),
                                }
                            }
                            Err(e) => r.problems.push(format!("fork: {e}")),
                        }
                    }
                    ready.wait();
                    let mut exit = None;
                    if let Some(m) = machine.as_mut() {
                        match m.run(10_000) {
                            Ok(ev) => exit = Some(ev),
                            Err(e) => r.problems.push(format!("run: {e}")),
                        }
                    }
                    done.wait();
                    measured.wait();
                    if let Some(m) = machine.as_ref() {
                        r.mailbox = Mailbox::read(m.mem.as_slice());
                        if let Some(e) = smaps_slot
                            .lock()
                            .unwrap()
                            .iter()
                            .find(|e| e.start == m.mem.host_addr())
                        {
                            r.anonymous_kib = e.anonymous_kib;
                            r.rss_kib = e.rss_kib;
                        }
                        verify(&mut r, exit, tpl, exp, Some(m.mem.as_slice()));
                    }
                    drop(machine);
                    r
                })
            })
            .collect();

        start.wait();
        let t_start = Instant::now();
        ready.wait();
        let all_ready_ns = t_start.elapsed().as_nanos() as u64;
        done.wait();
        let after = read_meminfo().unwrap_or_default();
        *smaps_slot.lock().unwrap() = read_smaps().unwrap_or_default();
        measured.wait();
        let results: Vec<BranchResult> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        (results, all_ready_ns, after)
    });

    let template_unchanged = tpl.unchanged()?;
    Ok(RoundResult {
        cfg: cfg.clone(),
        branches: results,
        all_ready_ns,
        before,
        after,
        template_unchanged,
    })
}

/// Agregado de varias rondas con la misma configuración.
#[derive(Debug, Clone)]
pub struct Aggregate {
    pub cfg: RoundConfig,
    pub rounds: usize,
    pub ready: Summary,
    pub shell: Summary,
    pub map: Summary,
    pub restore: Summary,
    pub first_run: Summary,
    pub all_ready: Summary,
    pub anonymous_kib: Summary,
    pub mem_drop_per_branch_kib: i64,
    pub sec_page_tables_per_branch_kib: Option<i64>,
    pub branches_ok: usize,
    pub branches_total: usize,
    pub templates_unchanged: usize,
    pub distinct_checksums: usize,
    pub problems: Vec<String>,
}

pub fn aggregate(rounds: &[RoundResult]) -> Aggregate {
    let all: Vec<&BranchResult> = rounds.iter().flat_map(|r| r.branches.iter()).collect();
    let pick = |f: &dyn Fn(&BranchResult) -> u64| -> Vec<u64> {
        all.iter()
            .filter(|b| b.ready_ns > 0)
            .map(|b| f(b))
            .collect()
    };
    let mut problems: Vec<String> = Vec::new();
    for r in rounds {
        if !r.template_unchanged {
            problems.push("la plantilla cambió".into());
        }
        for b in &r.branches {
            for p in &b.problems {
                if problems.len() < 20 {
                    problems.push(format!("rama {}: {p}", b.id));
                }
            }
        }
    }
    let last = rounds.last().expect("al menos una ronda");
    let distinct: std::collections::HashSet<u64> = last
        .branches
        .iter()
        .map(|b| b.mailbox.warm_checksum)
        .collect();
    let n = last.cfg.branches as i64;
    Aggregate {
        cfg: last.cfg.clone(),
        rounds: rounds.len(),
        ready: summarize(&pick(&|b| b.ready_ns)),
        shell: summarize(&pick(&|b| b.timing.shell_ns)),
        map: summarize(&pick(&|b| b.timing.map_ns)),
        restore: summarize(&pick(&|b| b.timing.memslot_ns + b.timing.restore_ns)),
        first_run: summarize(&pick(&|b| b.first_run_ns)),
        all_ready: summarize(&rounds.iter().map(|r| r.all_ready_ns).collect::<Vec<_>>()),
        anonymous_kib: summarize(&all.iter().map(|b| b.anonymous_kib).collect::<Vec<_>>()),
        mem_drop_per_branch_kib: last.mem_available_drop_per_branch_kib(),
        sec_page_tables_per_branch_kib: match (
            last.before.sec_page_tables_kib,
            last.after.sec_page_tables_kib,
        ) {
            (Some(a), Some(b)) => Some((b as i64 - a as i64) / n),
            _ => None,
        },
        branches_ok: all.iter().filter(|b| b.isolated()).count(),
        branches_total: all.len(),
        templates_unchanged: rounds.iter().filter(|r| r.template_unchanged).count(),
        distinct_checksums: distinct.len(),
        problems,
    }
}

fn summary_json(s: &Summary) -> String {
    format!(
        "{{\"n\":{},\"p50\":{},\"p99\":{},\"max\":{}}}",
        s.n, s.p50, s.p99, s.max
    )
}

impl Aggregate {
    pub fn isolation_ok(&self) -> bool {
        self.problems.is_empty()
            && self.branches_ok == self.branches_total
            && self.templates_unchanged == self.rounds
    }

    pub fn to_json(&self) -> String {
        format!(
            concat!(
                "{{\"branches\":{},\"mode\":{},\"pool\":{},\"rounds\":{},",
                "\"dirty_pages\":{},\"work_ms\":{},",
                "\"ready_ns\":{},\"shell_ns\":{},\"map_ns\":{},\"memslot_restore_ns\":{},",
                "\"first_run_ns\":{},\"all_ready_ns\":{},\"anonymous_kib\":{},",
                "\"mem_available_drop_per_branch_kib\":{},\"sec_page_tables_per_branch_kib\":{},",
                "\"branches_ok\":{},\"branches_total\":{},\"templates_unchanged\":{},",
                "\"distinct_checksums_last_round\":{},\"problems\":[{}]}}"
            ),
            self.cfg.branches,
            json_str(&self.cfg.mode.to_string()),
            self.cfg.pool,
            self.rounds,
            self.cfg.dirty_pages,
            self.cfg.work_ms,
            summary_json(&self.ready),
            summary_json(&self.shell),
            summary_json(&self.map),
            summary_json(&self.restore),
            summary_json(&self.first_run),
            summary_json(&self.all_ready),
            summary_json(&self.anonymous_kib),
            self.mem_drop_per_branch_kib,
            self.sec_page_tables_per_branch_kib
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".into()),
            self.branches_ok,
            self.branches_total,
            self.templates_unchanged,
            self.distinct_checksums,
            self.problems
                .iter()
                .map(|p| json_str(p))
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}
