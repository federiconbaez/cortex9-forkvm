//! Estado de la vCPU que se copia de la plantilla a cada rama.
//!
//! Se captura una sola vez, con la vCPU detenida después de completar el OUT
//! del punto de fork, y se escribe tal cual en la vCPU nueva de cada rama.

use kvm_bindings::{
    kvm_debugregs, kvm_fpu, kvm_msr_entry, kvm_regs, kvm_sregs, kvm_vcpu_events, kvm_xcrs,
    kvm_xsave, Msrs,
};
use kvm_ioctls::{Kvm, VcpuFd};

/// MSRs que se copian, si KVM los informa en `KVM_GET_MSR_INDEX_LIST`.
/// El TSC va al final para que la rama siga contando desde la plantilla.
pub const SNAPSHOT_MSRS: &[u32] = &[
    0x174,       // IA32_SYSENTER_CS
    0x175,       // IA32_SYSENTER_ESP
    0x176,       // IA32_SYSENTER_EIP
    0x277,       // IA32_PAT
    0xc000_0081, // STAR
    0xc000_0082, // LSTAR
    0xc000_0083, // CSTAR
    0xc000_0084, // SFMASK
    0xc000_0102, // KERNEL_GS_BASE
    0x10,        // IA32_TSC
];

/// Filtra `SNAPSHOT_MSRS` contra la lista que soporta KVM.
pub fn msrs_to_snapshot(supported: &[u32]) -> Vec<u32> {
    SNAPSHOT_MSRS
        .iter()
        .copied()
        .filter(|m| supported.contains(m))
        .collect()
}

/// Lista de MSRs que se van a copiar en este host.
pub fn host_msr_list(kvm: &Kvm) -> Result<Vec<u32>, kvm_ioctls::Error> {
    Ok(msrs_to_snapshot(kvm.get_msr_index_list()?.as_slice()))
}

/// Estado completo de una vCPU sin irqchip en el kernel.
#[derive(Clone)]
pub struct VcpuState {
    pub regs: kvm_regs,
    pub sregs: kvm_sregs,
    pub fpu: kvm_fpu,
    pub xsave: Option<[u32; 1024]>,
    pub xcrs: Option<kvm_xcrs>,
    pub debugregs: kvm_debugregs,
    pub events: kvm_vcpu_events,
    pub msrs: Vec<kvm_msr_entry>,
}

impl std::fmt::Debug for VcpuState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VcpuState")
            .field("rip", &format_args!("{:#x}", self.regs.rip))
            .field("rsp", &format_args!("{:#x}", self.regs.rsp))
            .field("cr3", &format_args!("{:#x}", self.sregs.cr3))
            .field("efer", &format_args!("{:#x}", self.sregs.efer))
            .field("xsave", &self.xsave.is_some())
            .field("xcrs", &self.xcrs.is_some())
            .field("msrs", &self.msrs.len())
            .finish()
    }
}

#[derive(Debug)]
pub struct StateError(pub &'static str, pub kvm_ioctls::Error);

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.0, self.1)
    }
}

impl std::error::Error for StateError {}

fn k<T>(what: &'static str, r: Result<T, kvm_ioctls::Error>) -> Result<T, StateError> {
    r.map_err(|e| StateError(what, e))
}

fn msr_entries(list: &[u32]) -> Vec<kvm_msr_entry> {
    list.iter()
        .map(|&index| kvm_msr_entry {
            index,
            ..Default::default()
        })
        .collect()
}

impl VcpuState {
    /// Lee el estado. `xsave`/`xcrs` son opcionales (dependen del host).
    pub fn capture(vcpu: &VcpuFd, msr_list: &[u32]) -> Result<Self, StateError> {
        let mut msrs = k(
            "Msrs::from_entries",
            Msrs::from_entries(&msr_entries(msr_list))
                .map_err(|_| kvm_ioctls::Error::new(libc::E2BIG)),
        )?;
        let n = k("get_msrs", vcpu.get_msrs(&mut msrs))?;
        if n != msr_list.len() {
            return Err(StateError(
                "get_msrs devolvió menos MSRs que los pedidos",
                kvm_ioctls::Error::new(libc::EIO),
            ));
        }
        Ok(Self {
            regs: k("get_regs", vcpu.get_regs())?,
            sregs: k("get_sregs", vcpu.get_sregs())?,
            fpu: k("get_fpu", vcpu.get_fpu())?,
            xsave: vcpu.get_xsave().ok().map(|x| x.region),
            xcrs: vcpu.get_xcrs().ok(),
            debugregs: k("get_debug_regs", vcpu.get_debug_regs())?,
            events: k("get_vcpu_events", vcpu.get_vcpu_events())?,
            msrs: msrs.as_slice().to_vec(),
        })
    }

    /// Escribe el estado en una vCPU nueva (con CPUID ya configurado).
    pub fn restore(&self, vcpu: &VcpuFd) -> Result<(), StateError> {
        k("set_sregs", vcpu.set_sregs(&self.sregs))?;
        k("set_regs", vcpu.set_regs(&self.regs))?;
        k("set_fpu", vcpu.set_fpu(&self.fpu))?;
        if let Some(region) = &self.xsave {
            let xsave = kvm_xsave {
                region: *region,
                extra: Default::default(),
            };
            // SAFETY: `kvm_xsave` de 4 KiB sin área extendida: es el formato
            // que devolvió KVM_GET_XSAVE en este mismo host.
            k("set_xsave", unsafe { vcpu.set_xsave(&xsave) })?;
        }
        if let Some(x) = &self.xcrs {
            k("set_xcrs", vcpu.set_xcrs(x))?;
        }
        k("set_debug_regs", vcpu.set_debug_regs(&self.debugregs))?;
        k("set_vcpu_events", vcpu.set_vcpu_events(&self.events))?;
        let msrs = k(
            "Msrs::from_entries",
            Msrs::from_entries(&self.msrs).map_err(|_| kvm_ioctls::Error::new(libc::E2BIG)),
        )?;
        let n = k("set_msrs", vcpu.set_msrs(&msrs))?;
        if n != self.msrs.len() {
            return Err(StateError(
                "set_msrs escribió menos MSRs que los pedidos",
                kvm_ioctls::Error::new(libc::EIO),
            ));
        }
        Ok(())
    }

    /// Valor capturado de un MSR.
    pub fn msr(&self, index: u32) -> Option<u64> {
        self.msrs.iter().find(|e| e.index == index).map(|e| e.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msr_filter_keeps_order_and_drops_unsupported() {
        let supported = [0x10, 0xc000_0102, 0x174, 0xdead];
        assert_eq!(msrs_to_snapshot(&supported), vec![0x174, 0xc000_0102, 0x10]);
        assert!(msrs_to_snapshot(&[]).is_empty());
    }

    #[test]
    fn tsc_is_restored_last() {
        assert_eq!(*SNAPSHOT_MSRS.last().unwrap(), 0x10);
    }

    #[test]
    fn msr_entries_carry_indices() {
        let e = msr_entries(&[0x10, 0x174]);
        assert_eq!(e.len(), 2);
        assert_eq!((e[0].index, e[0].data, e[1].index), (0x10, 0, 0x174));
    }
}
