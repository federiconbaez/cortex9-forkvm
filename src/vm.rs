//! VM de una vCPU sobre KVM: cáscaras (VM + vCPU sin memoria), máquinas con
//! memoria y el bucle de salidas con los dispositivos del guest.

use crate::guest::{DEBUG_EXIT_PORT, FORK_PORT, PARAM_PORT};
use crate::mem::Mapping;
use crate::state::StateError;
use crate::uart::Serial16550;
use kvm_bindings::{kvm_userspace_memory_region, CpuId, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use std::collections::VecDeque;

/// Cómo terminó (o se detuvo) el guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// La plantilla escribió en `FORK_PORT`.
    ForkPoint,
    /// El guest leyó un parámetro y `stop_on_param` estaba activo.
    ParamRead,
    /// El guest escribió `code` en `DEBUG_EXIT_PORT`.
    DebugExit(u8),
    Hlt,
    Shutdown,
    /// Se alcanzó el límite de salidas.
    ExitLimit,
}

#[derive(Debug)]
pub enum VmmError {
    Kvm(&'static str, kvm_ioctls::Error),
    State(StateError),
    Io(&'static str, std::io::Error),
    Guest(String),
}

impl std::fmt::Display for VmmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Kvm(what, e) => write!(f, "KVM {what}: {e}"),
            Self::State(e) => write!(f, "estado de vCPU: {e}"),
            Self::Io(what, e) => write!(f, "{what}: {e}"),
            Self::Guest(s) => write!(f, "guest: {s}"),
        }
    }
}

impl std::error::Error for VmmError {}

impl From<StateError> for VmmError {
    fn from(e: StateError) -> Self {
        Self::State(e)
    }
}

pub fn kvm<T>(what: &'static str, r: Result<T, kvm_ioctls::Error>) -> Result<T, VmmError> {
    r.map_err(|e| VmmError::Kvm(what, e))
}

/// Abre `/dev/kvm` con un mensaje claro si no se puede.
pub fn open_kvm() -> Result<Kvm, VmmError> {
    kvm("open /dev/kvm", Kvm::new())
}

/// CPUID que se le da a todas las vCPU (el que soporta el host).
pub fn host_cpuid(kvm_fd: &Kvm) -> Result<CpuId, VmmError> {
    kvm(
        "get_supported_cpuid",
        kvm_fd.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES),
    )
}

/// Dirección de las 3 páginas de TSS que KVM reserva en Intel (fuera de la
/// RAM del guest, como hacen otros VMMs).
pub const TSS_ADDR: usize = 0xfffb_d000;

/// VM con una vCPU y CPUID configurado, todavía sin memoria.
pub struct Shell {
    pub vcpu: VcpuFd,
    pub vm: VmFd,
}

impl Shell {
    pub fn new(kvm_fd: &Kvm, cpuid: &CpuId) -> Result<Self, VmmError> {
        let vm = kvm("create_vm", kvm_fd.create_vm())?;
        // Intel sin "unrestricted guest" lo exige; en AMD no hace nada.
        kvm("set_tss_address", vm.set_tss_address(TSS_ADDR))?;
        let vcpu = kvm("create_vcpu", vm.create_vcpu(0))?;
        kvm("set_cpuid2", vcpu.set_cpuid2(cpuid))?;
        Ok(Self { vcpu, vm })
    }
}

/// VM lista para correr: cáscara + memoria + dispositivos.
///
/// El orden de los campos importa: se destruyen vCPU y VM antes que la memoria.
pub struct Machine {
    pub vcpu: VcpuFd,
    pub vm: VmFd,
    pub mem: Mapping,
    pub uart: Serial16550,
    /// Valores que devuelve `PARAM_PORT`, en orden.
    pub params: VecDeque<u32>,
    /// Si está activo, `run` vuelve después de la próxima lectura de parámetro.
    pub stop_on_param: bool,
    pub vm_exits: u64,
}

impl Machine {
    /// Conecta `mem` como slot 0 desde la dirección física 0.
    pub fn attach(shell: Shell, mem: Mapping) -> Result<Self, VmmError> {
        let region = kvm_userspace_memory_region {
            slot: 0,
            flags: 0,
            guest_phys_addr: 0,
            memory_size: mem.len() as u64,
            userspace_addr: mem.host_addr(),
        };
        // SAFETY: la región vive en `mem`, que se destruye después de la VM.
        kvm("set_user_memory_region", unsafe {
            shell.vm.set_user_memory_region(region)
        })?;
        Ok(Self {
            vcpu: shell.vcpu,
            vm: shell.vm,
            mem,
            uart: Serial16550::default(),
            params: VecDeque::new(),
            stop_on_param: false,
            vm_exits: 0,
        })
    }

    /// Corre hasta un evento o hasta `max_exits` salidas más.
    pub fn run(&mut self, max_exits: u64) -> Result<Event, VmmError> {
        let limit = self.vm_exits.saturating_add(max_exits);
        loop {
            if self.vm_exits >= limit {
                return Ok(Event::ExitLimit);
            }
            let exit = match self.vcpu.run() {
                Ok(e) => e,
                Err(e) if e.errno() == libc::EINTR => continue,
                Err(e) => return Err(VmmError::Kvm("run", e)),
            };
            self.vm_exits += 1;
            match exit {
                VcpuExit::IoOut(port, data) if self.uart.handles(port) => {
                    for (i, b) in data.iter().enumerate() {
                        self.uart.write(port.wrapping_add(i as u16), *b);
                    }
                }
                VcpuExit::IoOut(DEBUG_EXIT_PORT, data) => {
                    return Ok(Event::DebugExit(data.first().copied().unwrap_or(0)));
                }
                VcpuExit::IoOut(FORK_PORT, _) => return Ok(Event::ForkPoint),
                VcpuExit::IoOut(_, _) => {}
                VcpuExit::IoIn(PARAM_PORT, data) => {
                    let v = self.params.pop_front().ok_or_else(|| {
                        VmmError::Guest("leyó un parámetro con la cola vacía".into())
                    })?;
                    let bytes = v.to_le_bytes();
                    let n = data.len().min(4);
                    data[..n].copy_from_slice(&bytes[..n]);
                    if self.stop_on_param {
                        self.stop_on_param = false;
                        return Ok(Event::ParamRead);
                    }
                }
                VcpuExit::IoIn(port, data) => {
                    for (i, b) in data.iter_mut().enumerate() {
                        let p = port.wrapping_add(i as u16);
                        *b = if self.uart.handles(p) {
                            self.uart.read(p)
                        } else {
                            0xff
                        };
                    }
                }
                VcpuExit::Hlt => return Ok(Event::Hlt),
                VcpuExit::Shutdown => return Ok(Event::Shutdown),
                other => {
                    return Err(VmmError::Guest(format!(
                        "salida de vCPU no manejada: {other:?}"
                    )))
                }
            }
        }
    }

    /// Completa la E/S pendiente (el OUT del punto de fork) sin ejecutar más
    /// instrucciones del guest: KVM_RUN con `immediate_exit = 1` termina el
    /// OUT, avanza RIP y vuelve con EINTR.
    pub fn complete_pending_io(&mut self) -> Result<(), VmmError> {
        self.vcpu.set_kvm_immediate_exit(1);
        let r = self.vcpu.run().map(|_| ());
        self.vcpu.set_kvm_immediate_exit(0);
        match r {
            Err(e) if e.errno() == libc::EINTR => Ok(()),
            Err(e) => Err(VmmError::Kvm("run(immediate_exit)", e)),
            Ok(()) => Err(VmmError::Guest(
                "KVM_RUN con immediate_exit no devolvió EINTR".into(),
            )),
        }
    }
}
