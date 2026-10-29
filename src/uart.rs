//! Emulación mínima de una UART 16550 en puertos PIO.
//!
//! Cubre lo que usa un driver de consola por polling: DLAB y divisor,
//! IER, FCR/IIR, LCR, MCR, LSR (siempre lista para transmitir), MSR y
//! scratch. No hay recepción ni interrupciones.

/// Puerto base de COM1.
pub const COM1_BASE: u16 = 0x3f8;

const THR_RBR_DLL: u16 = 0;
const IER_DLM: u16 = 1;
const IIR_FCR: u16 = 2;
const LCR: u16 = 3;
const MCR: u16 = 4;
const LSR: u16 = 5;
const MSR: u16 = 6;
const SCR: u16 = 7;

const LCR_DLAB: u8 = 0x80;
/// THRE | TEMT: transmisor vacío, siempre listo.
pub const LSR_TX_IDLE: u8 = 0x60;
const IIR_NO_INT: u8 = 0x01;
const IIR_FIFO_ENABLED: u8 = 0xc0;
const MCR_LOOP: u8 = 0x10;

/// Estado de la UART.
#[derive(Debug, Clone)]
pub struct Serial16550 {
    base: u16,
    dll: u8,
    dlm: u8,
    ier: u8,
    fcr: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    out: Vec<u8>,
}

impl Default for Serial16550 {
    fn default() -> Self {
        Self::new(COM1_BASE)
    }
}

impl Serial16550 {
    pub fn new(base: u16) -> Self {
        Self {
            base,
            dll: 0x0c, // 9600 baudios, valor de reset típico de la BIOS
            dlm: 0,
            ier: 0,
            fcr: 0,
            lcr: 0x03,
            mcr: 0x08,
            scr: 0,
            out: Vec::new(),
        }
    }

    /// ¿El puerto pertenece a esta UART?
    pub fn handles(&self, port: u16) -> bool {
        port.wrapping_sub(self.base) < 8
    }

    fn dlab(&self) -> bool {
        self.lcr & LCR_DLAB != 0
    }

    /// Divisor de baudios programado.
    pub fn divisor(&self) -> u16 {
        u16::from_le_bytes([self.dll, self.dlm])
    }

    /// Bytes transmitidos por el guest.
    pub fn output(&self) -> &[u8] {
        &self.out
    }

    /// Consume la salida acumulada.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// Escritura de un byte en `port`.
    pub fn write(&mut self, port: u16, value: u8) {
        match port.wrapping_sub(self.base) {
            THR_RBR_DLL if self.dlab() => self.dll = value,
            THR_RBR_DLL => {
                // En modo loopback el byte no sale por la línea.
                if self.mcr & MCR_LOOP == 0 {
                    self.out.push(value);
                }
            }
            IER_DLM if self.dlab() => self.dlm = value,
            IER_DLM => self.ier = value & 0x0f,
            IIR_FCR => self.fcr = value,
            LCR => self.lcr = value,
            MCR => self.mcr = value & 0x1f,
            SCR => self.scr = value,
            _ => {} // LSR/MSR son de solo lectura
        }
    }

    /// Lectura de un byte de `port`.
    pub fn read(&self, port: u16) -> u8 {
        match port.wrapping_sub(self.base) {
            THR_RBR_DLL if self.dlab() => self.dll,
            THR_RBR_DLL => 0, // sin datos recibidos
            IER_DLM if self.dlab() => self.dlm,
            IER_DLM => self.ier,
            IIR_FCR => {
                let fifo = if self.fcr & 1 != 0 {
                    IIR_FIFO_ENABLED
                } else {
                    0
                };
                fifo | IIR_NO_INT
            }
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => LSR_TX_IDLE,
            MSR => {
                // DCD|RI|DSR|CTS reflejan OUT2|OUT1|RTS|DTR en loopback.
                if self.mcr & MCR_LOOP != 0 {
                    let m = self.mcr;
                    ((m & 0x08) << 4) | ((m & 0x04) << 4) | ((m & 0x01) << 5) | ((m & 0x02) << 3)
                } else {
                    0xb0 // DCD|DSR|CTS
                }
            }
            SCR => self.scr,
            _ => 0xff,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: u16 = COM1_BASE;

    #[test]
    fn thr_write_goes_to_output() {
        let mut u = Serial16550::default();
        for b in b"hola\n" {
            u.write(B, *b);
        }
        assert_eq!(u.output(), b"hola\n");
        assert_eq!(u.take_output(), b"hola\n".to_vec());
        assert!(u.output().is_empty());
    }

    #[test]
    fn dlab_redirects_to_divisor() {
        let mut u = Serial16550::default();
        u.write(B + 3, 0x80);
        u.write(B, 0x01);
        u.write(B + 1, 0x00);
        assert_eq!(u.read(B), 0x01);
        assert_eq!(u.read(B + 1), 0x00);
        u.write(B + 3, 0x03);
        assert_eq!(u.divisor(), 1);
        assert!(u.output().is_empty(), "con DLAB no se transmite nada");
        u.write(B + 1, 0x0f);
        assert_eq!(u.read(B + 1), 0x0f, "sin DLAB el puerto+1 es IER");
        assert_eq!(u.divisor(), 1);
    }

    #[test]
    fn lsr_reports_transmitter_ready() {
        let u = Serial16550::default();
        assert_eq!(u.read(B + 5) & 0x20, 0x20);
        assert_eq!(u.read(B + 5) & 0x40, 0x40);
        assert_eq!(u.read(B + 5) & 0x01, 0, "sin datos recibidos");
    }

    #[test]
    fn iir_reflects_fifo_enable() {
        let mut u = Serial16550::default();
        assert_eq!(u.read(B + 2), 0x01);
        u.write(B + 2, 0xc7);
        assert_eq!(u.read(B + 2), 0xc1);
    }

    #[test]
    fn scratch_and_lcr_roundtrip() {
        let mut u = Serial16550::default();
        u.write(B + 7, 0x5a);
        assert_eq!(u.read(B + 7), 0x5a);
        u.write(B + 3, 0x1b);
        assert_eq!(u.read(B + 3), 0x1b);
    }

    #[test]
    fn loopback_suppresses_output_and_mirrors_msr() {
        let mut u = Serial16550::default();
        u.write(B + 4, 0x1e); // LOOP|OUT2|OUT1|RTS
        u.write(B, b'x');
        assert!(u.output().is_empty());
        assert_eq!(u.read(B + 6), 0xd0); // DCD|RI|CTS
        u.write(B + 4, 0x0f);
        u.write(B, b'y');
        assert_eq!(u.output(), b"y");
    }

    #[test]
    fn port_range() {
        let u = Serial16550::default();
        assert!(u.handles(0x3f8) && u.handles(0x3ff));
        assert!(!u.handles(0x3f7) && !u.handles(0x400) && !u.handles(0x2f8));
    }

    #[test]
    fn read_only_registers_ignore_writes() {
        let mut u = Serial16550::default();
        u.write(B + 5, 0x00);
        u.write(B + 6, 0x00);
        assert_eq!(u.read(B + 5), LSR_TX_IDLE);
        assert_eq!(u.read(B + 6), 0xb0);
    }
}
