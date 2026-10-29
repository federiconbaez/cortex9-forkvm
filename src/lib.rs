//! `cortex9-forkvm`: fork copy-on-write de microVMs calientes sobre KVM.
//!
//! Una plantilla arranca sobre un memfd, corre hasta el punto de fork y se
//! congela (estado de vCPU capturado, memfd sellado). Cada rama es una VM
//! nueva que mapea el mismo memfd con MAP_PRIVATE y recibe una copia del
//! estado de la vCPU y de la UART.

pub mod boot;
pub mod guest;
pub mod layout;
pub mod mem;
pub mod state;
pub mod template;
pub mod uart;
pub mod vm;
