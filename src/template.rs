//! Plantilla congelada en el punto de fork y creación de ramas.

use crate::guest::{expected_warm_checksum, TemplateInfo, GUEST_IMAGE, MAGIC_TINFO};
use crate::layout::{setup_memory, MAX_GUEST_MEM, MIN_GUEST_MEM, WARM_BASE};
use crate::mem::{MapKind, Mapping, Memfd, PAGE};
use crate::state::{host_msr_list, VcpuState};
use crate::uart::Serial16550;
use crate::vm::{host_cpuid, kvm, Event, Machine, Shell, VmmError};
use kvm_bindings::CpuId;
use kvm_ioctls::Kvm;
use std::time::Instant;

/// Parámetros de la plantilla.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemplateConfig {
    /// RAM del guest en bytes.
    pub mem_size: usize,
    /// Bytes del conjunto caliente que llena el guest antes del fork.
    pub warm_bytes: u64,
    /// Sellar el memfd después de capturar (desactivarlo solo para el
    /// control negativo con mapeo compartido).
    pub seal: bool,
}

impl TemplateConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.mem_size < MIN_GUEST_MEM + PAGE || self.mem_size > MAX_GUEST_MEM {
            return Err(format!(
                "mem_size {} fuera de [{}, {}]",
                self.mem_size,
                MIN_GUEST_MEM + PAGE,
                MAX_GUEST_MEM
            ));
        }
        if !self.mem_size.is_multiple_of(PAGE) {
            return Err("mem_size no es múltiplo de 4 KiB".into());
        }
        if self.warm_bytes == 0 || !self.warm_bytes.is_multiple_of(PAGE as u64) {
            return Err("warm_bytes tiene que ser > 0 y múltiplo de 4 KiB".into());
        }
        if WARM_BASE + self.warm_bytes > self.mem_size as u64 || self.warm_bytes > u32::MAX as u64 {
            return Err("el conjunto caliente no entra en la RAM del guest".into());
        }
        Ok(())
    }
}

/// Cómo se crea la memoria de una rama.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkMode {
    /// mmap MAP_PRIVATE del memfd: copy-on-write.
    Cow,
    /// Copia completa de la RAM a memoria anónima (línea de base).
    Copy,
    /// mmap MAP_SHARED del memfd: SIN aislamiento. Solo para el control
    /// negativo; falla si la plantilla está sellada.
    SharedUnsafe,
}

impl std::str::FromStr for ForkMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "cow" => Ok(Self::Cow),
            "copy" => Ok(Self::Copy),
            "shared" => Ok(Self::SharedUnsafe),
            _ => Err(format!("modo desconocido {s:?} (cow|copy|shared)")),
        }
    }
}

impl std::fmt::Display for ForkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cow => "cow",
            Self::Copy => "copy",
            Self::SharedUnsafe => "shared",
        })
    }
}

/// Parámetros de una rama, en el orden en que los lee el guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchParams {
    pub id: u32,
    pub dirty_pages: u32,
    pub tsc_budget: u64,
    pub max_passes: u32,
}

impl BranchParams {
    pub fn queue(&self) -> [u32; 5] {
        [
            self.id,
            self.dirty_pages,
            self.tsc_budget as u32,
            (self.tsc_budget >> 32) as u32,
            self.max_passes,
        ]
    }
}

/// Tiempos de cada paso del fork, en nanosegundos.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForkTiming {
    pub map_ns: u64,
    pub shell_ns: u64,
    pub memslot_ns: u64,
    pub restore_ns: u64,
}

/// Frecuencia del TSC medida en el host contra el reloj monotónico (solo si
/// KVM_GET_TSC_KHZ no está disponible).
pub fn calibrate_tsc_khz() -> u32 {
    // SAFETY: rdtsc no tiene precondiciones en x86-64.
    let r = || unsafe { core::arch::x86_64::_rdtsc() };
    let t0 = Instant::now();
    let c0 = r();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let c1 = r();
    let us = t0.elapsed().as_micros().max(1) as u64;
    ((c1 - c0) * 1000 / us) as u32
}

/// Plantilla: memfd con la RAM + estado de vCPU y dispositivos.
pub struct Template {
    memfd: Memfd,
    pub cfg: TemplateConfig,
    pub state: VcpuState,
    pub uart: Serial16550,
    pub cpuid: CpuId,
    pub info: TemplateInfo,
    /// Lo que imprimió la plantilla por la consola serie.
    pub serial: Vec<u8>,
    pub vm_exits: u64,
    /// Arranque hasta el punto de fork.
    pub boot_ns: u64,
    /// Completar el OUT + leer estado + soltar el mapeo + sellar.
    pub capture_ns: u64,
    /// Frecuencia del TSC del guest según KVM_GET_TSC_KHZ.
    pub tsc_khz: u32,
}

impl Template {
    /// Arranca el guest sobre un memfd, lo corre hasta el punto de fork y
    /// lo congela.
    pub fn boot(kvm_fd: &Kvm, cfg: TemplateConfig) -> Result<Self, VmmError> {
        cfg.validate().map_err(VmmError::Guest)?;
        let t0 = Instant::now();
        let memfd = Memfd::create(c"cortex9-forkvm", cfg.mem_size)
            .map_err(|e| VmmError::Io("memfd_create", e))?;
        let mut mem = memfd
            .map(MapKind::Shared)
            .map_err(|e| VmmError::Io("mmap compartido", e))?;
        let entry = setup_memory(mem.as_mut_slice(), GUEST_IMAGE)
            .map_err(|e| VmmError::Guest(e.to_string()))?;

        let cpuid = host_cpuid(kvm_fd)?;
        let msr_list = kvm("get_msr_index_list", host_msr_list(kvm_fd))?;
        let shell = Shell::new(kvm_fd, &cpuid)?;
        let sregs = crate::boot::long_mode_sregs(kvm("get_sregs", shell.vcpu.get_sregs())?);
        kvm("set_sregs", shell.vcpu.set_sregs(&sregs))?;
        kvm(
            "set_regs",
            shell.vcpu.set_regs(&crate::boot::initial_regs(entry)),
        )?;
        let tsc_khz = shell
            .vcpu
            .get_tsc_khz()
            .ok()
            .filter(|&k| k > 0)
            .unwrap_or_else(calibrate_tsc_khz);
        let mut m = Machine::attach(shell, mem)?;
        m.params.push_back(cfg.warm_bytes as u32);
        match m.run(10_000_000)? {
            Event::ForkPoint => {}
            other => {
                let serial = String::from_utf8_lossy(m.uart.output()).into_owned();
                return Err(VmmError::Guest(format!(
                    "la plantilla terminó con {other:?} antes del fork; serie: {serial:?}"
                )));
            }
        }
        let boot_ns = t0.elapsed().as_nanos() as u64;

        let t1 = Instant::now();
        m.complete_pending_io()?;
        let state = VcpuState::capture(&m.vcpu, &msr_list)?;
        let info = TemplateInfo::read(m.mem.as_slice());
        let mut uart = m.uart.clone();
        let serial = uart.take_output();
        let vm_exits = m.vm_exits;
        drop(m);
        if cfg.seal {
            memfd.seal().map_err(|e| VmmError::Io("sellar memfd", e))?;
        }
        let capture_ns = t1.elapsed().as_nanos() as u64;

        if info.magic != MAGIC_TINFO || info.warm_bytes != cfg.warm_bytes {
            return Err(VmmError::Guest(format!("TEMPLATE_INFO inválido: {info:?}")));
        }
        Ok(Self {
            memfd,
            cfg,
            state,
            uart,
            cpuid,
            info,
            serial,
            vm_exits,
            boot_ns,
            capture_ns,
            tsc_khz,
        })
    }

    pub fn memfd(&self) -> &Memfd {
        &self.memfd
    }

    /// Crea una rama lista para correr. `shell` permite usar una VM creada de
    /// antemano (pool); si es `None` se crea acá y su costo entra en el fork.
    pub fn fork(
        &self,
        kvm_fd: &Kvm,
        shell: Option<Shell>,
        mode: ForkMode,
        params: BranchParams,
    ) -> Result<(Machine, ForkTiming), VmmError> {
        let mut t = ForkTiming::default();
        let c = Instant::now();
        let mem = match mode {
            ForkMode::Cow => self
                .memfd
                .map(MapKind::Private)
                .map_err(|e| VmmError::Io("mmap privado", e))?,
            ForkMode::SharedUnsafe => self
                .memfd
                .map(MapKind::Shared)
                .map_err(|e| VmmError::Io("mmap compartido", e))?,
            ForkMode::Copy => {
                let src = self
                    .memfd
                    .map(MapKind::SharedReadOnly)
                    .map_err(|e| VmmError::Io("mmap de lectura", e))?;
                let mut dst = Mapping::anonymous(self.cfg.mem_size)
                    .map_err(|e| VmmError::Io("mmap anónimo", e))?;
                dst.as_mut_slice().copy_from_slice(src.as_slice());
                dst
            }
        };
        t.map_ns = c.elapsed().as_nanos() as u64;

        let c = Instant::now();
        let shell = match shell {
            Some(s) => s,
            None => Shell::new(kvm_fd, &self.cpuid)?,
        };
        t.shell_ns = c.elapsed().as_nanos() as u64;

        let c = Instant::now();
        let mut m = Machine::attach(shell, mem)?;
        t.memslot_ns = c.elapsed().as_nanos() as u64;

        let c = Instant::now();
        self.state.restore(&m.vcpu)?;
        let mut uart = self.uart.clone();
        uart.take_output();
        m.uart = uart;
        m.params.extend(params.queue());
        t.restore_ns = c.elapsed().as_nanos() as u64;
        Ok((m, t))
    }

    /// Checksum actual del conjunto caliente leído directo del memfd.
    pub fn current_warm_checksum(&self) -> Result<u64, VmmError> {
        let ro = self
            .memfd
            .map(MapKind::SharedReadOnly)
            .map_err(|e| VmmError::Io("mmap de lectura", e))?;
        let s = WARM_BASE as usize;
        Ok(crate::guest::checksum(
            &ro.as_slice()[s..s + self.cfg.warm_bytes as usize],
        ))
    }

    /// ¿La plantilla sigue idéntica a lo que calculó el guest y a la
    /// referencia independiente del host?
    pub fn unchanged(&self) -> Result<bool, VmmError> {
        let now = self.current_warm_checksum()?;
        Ok(now == self.info.warm_checksum
            && now == expected_warm_checksum(self.cfg.warm_bytes, 0, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mem_mib: usize, warm: u64) -> TemplateConfig {
        TemplateConfig {
            mem_size: mem_mib << 20,
            warm_bytes: warm,
            seal: true,
        }
    }

    #[test]
    fn config_validation() {
        assert!(cfg(256, 64 << 20).validate().is_ok());
        assert!(
            cfg(256, 252 << 20).validate().is_ok(),
            "llena hasta el final"
        );
        assert!(cfg(256, 253 << 20).validate().is_err(), "no entra");
        assert!(
            cfg(4, PAGE as u64).validate().is_err(),
            "sin lugar para el conjunto"
        );
        assert!(cfg(2048, PAGE as u64).validate().is_err(), "más de 1 GiB");
        assert!(cfg(64, 100).validate().is_err(), "no múltiplo de página");
        assert!(cfg(64, 0).validate().is_err());
        let mut c = cfg(64, PAGE as u64);
        c.mem_size += 1;
        assert!(c.validate().is_err());
    }

    #[test]
    fn params_queue_order_matches_guest() {
        let p = BranchParams {
            id: 7,
            dirty_pages: 256,
            tsc_budget: 0x1_2345_6789,
            max_passes: 3,
        };
        assert_eq!(p.queue(), [7, 256, 0x2345_6789, 1, 3]);
    }

    #[test]
    fn fork_mode_roundtrip() {
        for m in [ForkMode::Cow, ForkMode::Copy, ForkMode::SharedUnsafe] {
            assert_eq!(m.to_string().parse::<ForkMode>(), Ok(m));
        }
        assert!("x".parse::<ForkMode>().is_err());
    }

    #[test]
    fn tsc_calibration_is_plausible() {
        let khz = calibrate_tsc_khz();
        assert!((100_000..20_000_000).contains(&khz), "{khz} kHz");
    }
}
