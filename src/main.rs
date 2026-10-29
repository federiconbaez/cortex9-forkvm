//! CLI `forkvm`: arranca la plantilla y mide forks de N ramas.

use cortex9_forkvm::bench::{aggregate, run_round, Aggregate, Expected, RoundConfig};
use cortex9_forkvm::stats::json_str;
use cortex9_forkvm::template::{ForkMode, Template, TemplateConfig};
use cortex9_forkvm::vm::open_kvm;
use std::process::ExitCode;

const USAGE: &str = "\
uso:
  forkvm boot  [--mem-mib N] [--warm-mib N]
  forkvm bench [--mem-mib N] [--warm-mib N] [--branches 1,8,64] [--rounds 20,5,2]
               [--dirty-pages N] [--work-ms N] [--mode cow|copy|shared] [--pool]
               [--json RUTA] [--expect-violation]

bench sale con 0 si todas las ramas quedaron aisladas (o, con
--expect-violation, si detectó al menos una violación), 1 si no, 2 por error.";

struct Opts {
    mem_mib: usize,
    warm_mib: u64,
    branches: Vec<usize>,
    rounds: Vec<usize>,
    dirty_pages: u32,
    work_ms: u64,
    mode: ForkMode,
    pool: bool,
    json: Option<String>,
    expect_violation: bool,
}

fn list(v: &str) -> Result<Vec<usize>, String> {
    v.split(',')
        .map(|x| {
            x.trim()
                .parse()
                .map_err(|_| format!("número inválido {x:?}"))
        })
        .collect()
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        mem_mib: 256,
        warm_mib: 64,
        branches: vec![1, 8, 64],
        rounds: vec![20, 5, 2],
        dirty_pages: 256,
        work_ms: 1000,
        mode: ForkMode::Cow,
        pool: false,
        json: None,
        expect_violation: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} necesita un valor"));
        match a.as_str() {
            "--mem-mib" => o.mem_mib = val()?.parse().map_err(|_| "--mem-mib")?,
            "--warm-mib" => o.warm_mib = val()?.parse().map_err(|_| "--warm-mib")?,
            "--branches" => o.branches = list(&val()?)?,
            "--rounds" => o.rounds = list(&val()?)?,
            "--dirty-pages" => o.dirty_pages = val()?.parse().map_err(|_| "--dirty-pages")?,
            "--work-ms" => o.work_ms = val()?.parse().map_err(|_| "--work-ms")?,
            "--mode" => o.mode = val()?.parse()?,
            "--pool" => o.pool = true,
            "--json" => o.json = Some(val()?),
            "--expect-violation" => o.expect_violation = true,
            other => return Err(format!("argumento desconocido {other:?}")),
        }
    }
    if o.rounds.len() == 1 {
        o.rounds = vec![o.rounds[0]; o.branches.len()];
    }
    if o.rounds.len() != o.branches.len() {
        return Err("--rounds necesita un valor o uno por cada --branches".into());
    }
    if o.branches.contains(&0) || o.rounds.contains(&0) {
        return Err("--branches y --rounds tienen que ser > 0".into());
    }
    Ok(o)
}

fn ms(ns: u64) -> String {
    format!("{:.3}", ns as f64 / 1e6)
}

fn print_aggregate(a: &Aggregate, mem_size: usize) {
    let pct = a.anonymous_kib.p50 as f64 * 1024.0 * 100.0 / mem_size as f64;
    println!(
        "N={:<3} modo={} pool={} rondas={} muestras={}",
        a.cfg.branches, a.cfg.mode, a.cfg.pool, a.rounds, a.ready.n
    );
    println!(
        "  fork->lista ms: p50={} p99={} max={} | cáscara p50={} mmap p50={} memslot+estado p50={} primer run p50={}",
        ms(a.ready.p50),
        ms(a.ready.p99),
        ms(a.ready.max),
        ms(a.shell.p50),
        ms(a.map.p50),
        ms(a.restore.p50),
        ms(a.first_run.p50)
    );
    println!(
        "  todas listas ms: p50={} max={}",
        ms(a.all_ready.p50),
        ms(a.all_ready.max)
    );
    println!(
        "  memoria por rama: anónima p50={} KiB ({pct:.2}% de la RAM) max={} KiB | caída de MemAvailable={} KiB | SecPageTables={}",
        a.anonymous_kib.p50,
        a.anonymous_kib.max,
        a.mem_drop_per_branch_kib,
        a.sec_page_tables_per_branch_kib
            .map(|v| format!("{v} KiB"))
            .unwrap_or_else(|| "n/d".into())
    );
    println!(
        "  aislamiento: {}/{} ramas ok, plantilla intacta en {}/{} rondas, {} checksums distintos en la última ronda",
        a.branches_ok, a.branches_total, a.templates_unchanged, a.rounds, a.distinct_checksums
    );
    for p in &a.problems {
        println!("  PROBLEMA: {p}");
    }
}

fn cmd_boot(o: &Opts) -> Result<ExitCode, String> {
    let kvm = open_kvm().map_err(|e| e.to_string())?;
    let cfg = TemplateConfig {
        mem_size: o.mem_mib << 20,
        warm_bytes: o.warm_mib << 20,
        seal: true,
    };
    let t = Template::boot(&kvm, cfg).map_err(|e| e.to_string())?;
    print!("{}", String::from_utf8_lossy(&t.serial));
    println!(
        "[forkvm] arranque->fork {} ms, captura {} ms, salidas={}, rip={:#x}, tsc_khz={}, sellos={:#x}, checksum={:#x}, intacta={}",
        ms(t.boot_ns),
        ms(t.capture_ns),
        t.vm_exits,
        t.state.regs.rip,
        t.tsc_khz,
        t.memfd().seals().unwrap_or(-1),
        t.info.warm_checksum,
        t.unchanged().map_err(|e| e.to_string())?
    );
    println!("[forkvm] estado: {:?}", t.state);
    Ok(ExitCode::SUCCESS)
}

fn cmd_bench(o: &Opts) -> Result<ExitCode, String> {
    let kvm = open_kvm().map_err(|e| e.to_string())?;
    let mem_size = o.mem_mib << 20;
    let cfg = TemplateConfig {
        mem_size,
        warm_bytes: o.warm_mib << 20,
        seal: o.mode != ForkMode::SharedUnsafe,
    };
    if o.mode == ForkMode::Copy {
        let need = o.branches.iter().max().copied().unwrap_or(1) * mem_size;
        let avail = cortex9_forkvm::mem::read_meminfo()
            .map(|m| m.mem_available_kib as usize * 1024)
            .unwrap_or(usize::MAX);
        if need > avail / 10 * 8 {
            return Err(format!(
                "modo copy: {} MiB para la ronda más grande no entran en el 80% de MemAvailable",
                need >> 20
            ));
        }
    }
    let t = Template::boot(&kvm, cfg).map_err(|e| e.to_string())?;
    println!(
        "plantilla: RAM {} MiB, conjunto caliente {} MiB, arranque->fork {} ms, captura {} ms, tsc_khz {}, sellada={}",
        o.mem_mib,
        o.warm_mib,
        ms(t.boot_ns),
        ms(t.capture_ns),
        t.tsc_khz,
        cfg.seal
    );
    let max_n = *o.branches.iter().max().unwrap() as u32;
    let exp = Expected::compute(cfg.warm_bytes, o.dirty_pages, max_n);
    let mut aggs = Vec::new();
    for (&n, &rounds) in o.branches.iter().zip(&o.rounds) {
        let rc = RoundConfig {
            branches: n,
            mode: o.mode,
            pool: o.pool,
            dirty_pages: o.dirty_pages,
            work_ms: o.work_ms,
        };
        let mut rs = Vec::new();
        for _ in 0..rounds {
            rs.push(run_round(&kvm, &t, &exp, &rc).map_err(|e| e.to_string())?);
            if o.mode == ForkMode::SharedUnsafe {
                break; // la plantilla ya quedó pisada; más rondas no suman
            }
        }
        let a = aggregate(&rs);
        print_aggregate(&a, mem_size);
        aggs.push(a);
    }

    let isolated = aggs.iter().all(Aggregate::isolation_ok);
    if o.mode == ForkMode::Cow {
        let lat = aggs.iter().all(|a| a.ready.p50 < 10_000_000);
        let rss = aggs
            .iter()
            .all(|a| (a.anonymous_kib.max as f64 * 1024.0) < 0.05 * mem_size as f64);
        println!(
            "criterio: p50 fork<10 ms en todas las N: {} | memoria anónima por rama <5% de la RAM: {} | aislamiento: {}",
            if lat { "CUMPLE" } else { "NO CUMPLE" },
            if rss { "CUMPLE" } else { "NO CUMPLE" },
            if isolated { "CUMPLE" } else { "NO CUMPLE" }
        );
    }
    if let Some(path) = &o.json {
        let body = format!(
            "{{\"mem_mib\":{},\"warm_mib\":{},\"boot_ns\":{},\"capture_ns\":{},\"tsc_khz\":{},\"sealed\":{},\"kernel\":{},\"results\":[{}]}}\n",
            o.mem_mib,
            o.warm_mib,
            t.boot_ns,
            t.capture_ns,
            t.tsc_khz,
            cfg.seal,
            json_str(
                std::fs::read_to_string("/proc/sys/kernel/osrelease")
                    .unwrap_or_default()
                    .trim()
            ),
            aggs.iter().map(Aggregate::to_json).collect::<Vec<_>>().join(",")
        );
        std::fs::write(path, body).map_err(|e| format!("{path}: {e}"))?;
    }
    let ok = if o.expect_violation {
        if isolated {
            println!("control: NO se detectó la violación de aislamiento esperada");
        } else {
            println!("control: violación de aislamiento detectada, como se esperaba");
        }
        !isolated
    } else {
        isolated
    };
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    if matches!(cmd, "-h" | "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let o = match parse(rest) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let r = match cmd {
        "boot" => cmd_boot(&o),
        "bench" => cmd_bench(&o),
        other => Err(format!("comando desconocido {other:?}")),
    };
    match r {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[forkvm] error: {e}");
            ExitCode::from(2)
        }
    }
}
