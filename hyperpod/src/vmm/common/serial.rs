use std::io::{stdout, Stdout, Write};
use std::sync::Mutex;

const SERIAL_PORT_BASE: u16 = 0x3f8;
const SERIAL_PORT_END: u16 = 0x3ff;
const REG_THR: u16 = 0;
const REG_LSR: u16 = 5;
const LSR_THRE: u8 = 0x20;
const LSR_TEMT: u8 = 0x40;

pub struct Serial {
    out: Mutex<Stdout>,
}

impl Serial {
    pub fn new() -> Self {
        Self {
            out: Mutex::new(stdout()),
        }
    }

    pub fn handles(port: u16) -> bool {
        (SERIAL_PORT_BASE..=SERIAL_PORT_END).contains(&port)
    }

    pub fn write(&self, port: u16, data: &[u8]) {
        let reg = port - SERIAL_PORT_BASE;
        if reg == REG_THR && !data.is_empty() {
            if let Ok(mut out) = self.out.lock() {
                let _ = out.write_all(&data[..1]);
                let _ = out.flush();
            }
        }
    }

    pub fn read(&self, port: u16, data: &mut [u8]) {
        if data.is_empty() {
            return;
        }
        let reg = port - SERIAL_PORT_BASE;
        data[0] = match reg {
            REG_LSR => LSR_THRE | LSR_TEMT,
            _ => 0,
        };
    }
}

impl Default for Serial {
    fn default() -> Self {
        Self::new()
    }
}
