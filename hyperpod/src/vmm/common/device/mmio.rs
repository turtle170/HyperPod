// VirtIO-MMIO transport + virtqueue walker. Structurally complete and
// covered by unit tests, but not yet wired into either run loop because the
// guest driver requires interrupt injection (next phase). The dead_code
// allowance is on the module rather than individual items so the surface
// stays visible for the upcoming device integration.
#![allow(dead_code)]

use std::sync::Mutex;

use anyhow::{bail, Result};

use crate::vmm::common::ram::GuestRam;

// VirtIO MMIO register offsets (transitional/virtio-mmio spec v2).
pub const MAGIC_VALUE: u64 = 0x000; // "virt" = 0x74726976
pub const VERSION: u64 = 0x004;
pub const DEVICE_ID: u64 = 0x008;
pub const VENDOR_ID: u64 = 0x00c;
pub const DEVICE_FEATURES: u64 = 0x010;
pub const DEVICE_FEATURES_SEL: u64 = 0x014;
pub const DRIVER_FEATURES: u64 = 0x020;
pub const DRIVER_FEATURES_SEL: u64 = 0x024;
pub const QUEUE_SEL: u64 = 0x030;
pub const QUEUE_NUM_MAX: u64 = 0x034;
pub const QUEUE_NUM: u64 = 0x038;
pub const QUEUE_READY: u64 = 0x044;
pub const QUEUE_NOTIFY: u64 = 0x050;
pub const INTERRUPT_STATUS: u64 = 0x060;
pub const INTERRUPT_ACK: u64 = 0x064;
pub const STATUS: u64 = 0x070;
pub const QUEUE_DESC_LOW: u64 = 0x080;
pub const QUEUE_DESC_HIGH: u64 = 0x084;
pub const QUEUE_AVAIL_LOW: u64 = 0x090;
pub const QUEUE_AVAIL_HIGH: u64 = 0x094;
pub const QUEUE_USED_LOW: u64 = 0x0a0;
pub const QUEUE_USED_HIGH: u64 = 0x0a4;
pub const CONFIG_GENERATION: u64 = 0x0fc;
pub const CONFIG_SPACE: u64 = 0x100;

pub const MAGIC: u32 = 0x7472_6976;
pub const VERSION_2: u32 = 2;

/// MMIO transport state for a single VirtIO device.
pub struct VirtioMmio {
    pub device_id: u32,
    pub vendor_id: u32,
    pub device_features: u64,
    pub config: Vec<u8>,
    state: Mutex<MmioState>,
    queues: Vec<Mutex<VirtQueue>>,
}

#[derive(Default)]
struct MmioState {
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    status: u32,
    interrupt_status: u32,
    config_generation: u32,
}

#[derive(Debug, Default)]
pub struct VirtQueue {
    pub size_max: u32,
    pub size: u32,
    pub ready: bool,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
    pub last_avail_idx: u16,
}

impl VirtioMmio {
    pub fn new(device_id: u32, vendor_id: u32, device_features: u64, queue_sizes: &[u32]) -> Self {
        let queues = queue_sizes
            .iter()
            .map(|&n| Mutex::new(VirtQueue { size_max: n, ..Default::default() }))
            .collect();
        Self {
            device_id,
            vendor_id,
            device_features,
            config: Vec::new(),
            state: Mutex::new(MmioState::default()),
            queues,
        }
    }

    pub fn status(&self) -> u32 {
        self.state.lock().unwrap().status
    }

    /// Handle a 32-bit MMIO read. Returns the value to give the guest.
    pub fn read32(&self, offset: u64) -> u32 {
        match offset {
            MAGIC_VALUE => MAGIC,
            VERSION => VERSION_2,
            DEVICE_ID => self.device_id,
            VENDOR_ID => self.vendor_id,
            DEVICE_FEATURES => {
                let s = self.state.lock().unwrap();
                if s.device_features_sel == 0 {
                    self.device_features as u32
                } else if s.device_features_sel == 1 {
                    (self.device_features >> 32) as u32
                } else {
                    0
                }
            }
            QUEUE_NUM_MAX => self
                .selected_queue(|q| q.size_max)
                .unwrap_or(0),
            QUEUE_READY => self
                .selected_queue(|q| if q.ready { 1 } else { 0 })
                .unwrap_or(0),
            INTERRUPT_STATUS => self.state.lock().unwrap().interrupt_status,
            STATUS => self.state.lock().unwrap().status,
            CONFIG_GENERATION => self.state.lock().unwrap().config_generation,
            o if o >= CONFIG_SPACE => self.read_config(o - CONFIG_SPACE),
            _ => 0,
        }
    }

    /// Handle a 32-bit MMIO write. Returns `Some(queue_index)` if the write
    /// was a `QueueNotify` (i.e. the guest is asking the device to process the
    /// virtqueue).
    pub fn write32(&self, offset: u64, val: u32) -> Option<u32> {
        let mut notify_queue = None;
        match offset {
            DEVICE_FEATURES_SEL => self.state.lock().unwrap().device_features_sel = val,
            DRIVER_FEATURES_SEL => self.state.lock().unwrap().driver_features_sel = val,
            DRIVER_FEATURES => {
                let mut s = self.state.lock().unwrap();
                let shift = if s.driver_features_sel == 0 { 0 } else { 32 };
                let mask = 0xffff_ffffu64 << shift;
                s.driver_features = (s.driver_features & !mask) | ((val as u64) << shift);
            }
            QUEUE_SEL => self.state.lock().unwrap().queue_sel = val,
            QUEUE_NUM => self.mutate_selected_queue(|q| q.size = val),
            QUEUE_READY => self.mutate_selected_queue(|q| q.ready = val != 0),
            QUEUE_NOTIFY => notify_queue = Some(val),
            INTERRUPT_ACK => {
                let mut s = self.state.lock().unwrap();
                s.interrupt_status &= !val;
            }
            STATUS => self.state.lock().unwrap().status = val,
            QUEUE_DESC_LOW => self.mutate_selected_queue(|q| q.desc_addr = (q.desc_addr & !0xffff_ffff) | val as u64),
            QUEUE_DESC_HIGH => self.mutate_selected_queue(|q| q.desc_addr = (q.desc_addr & 0xffff_ffff) | ((val as u64) << 32)),
            QUEUE_AVAIL_LOW => self.mutate_selected_queue(|q| q.avail_addr = (q.avail_addr & !0xffff_ffff) | val as u64),
            QUEUE_AVAIL_HIGH => self.mutate_selected_queue(|q| q.avail_addr = (q.avail_addr & 0xffff_ffff) | ((val as u64) << 32)),
            QUEUE_USED_LOW => self.mutate_selected_queue(|q| q.used_addr = (q.used_addr & !0xffff_ffff) | val as u64),
            QUEUE_USED_HIGH => self.mutate_selected_queue(|q| q.used_addr = (q.used_addr & 0xffff_ffff) | ((val as u64) << 32)),
            _ => {}
        }
        notify_queue
    }

    fn selected_queue<R>(&self, f: impl FnOnce(&VirtQueue) -> R) -> Option<R> {
        let sel = self.state.lock().unwrap().queue_sel as usize;
        self.queues.get(sel).map(|q| f(&q.lock().unwrap()))
    }

    fn mutate_selected_queue(&self, f: impl FnOnce(&mut VirtQueue)) {
        let sel = self.state.lock().unwrap().queue_sel as usize;
        if let Some(q) = self.queues.get(sel) {
            f(&mut q.lock().unwrap());
        }
    }

    fn read_config(&self, off: u64) -> u32 {
        let off = off as usize;
        if off + 4 > self.config.len() {
            return 0;
        }
        let bytes: [u8; 4] = self.config[off..off + 4].try_into().unwrap();
        u32::from_le_bytes(bytes)
    }
}

/// A walked descriptor chain from the available ring.
#[derive(Debug)]
pub struct DescriptorChain {
    pub head_index: u16,
    pub descriptors: Vec<Descriptor>,
}

#[derive(Debug, Clone, Copy)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;
const DESC_SIZE: u64 = 16;
const AVAIL_RING_HDR: u64 = 4;

impl DescriptorChain {
    /// Walks the next descriptor chain from the available ring, advancing the
    /// queue's `last_avail_idx`. Returns `Ok(None)` if no new requests are
    /// pending.
    pub fn pop(queue: &mut VirtQueue, ram: &GuestRam) -> Result<Option<DescriptorChain>> {
        if !queue.ready || queue.size == 0 {
            return Ok(None);
        }
        let avail_idx: u16 = ram.read_obj(queue.avail_addr + 2)?;
        if avail_idx == queue.last_avail_idx {
            return Ok(None);
        }
        let slot = (queue.last_avail_idx as u64) % (queue.size as u64);
        let head_index: u16 = ram.read_obj(queue.avail_addr + AVAIL_RING_HDR + slot * 2)?;
        queue.last_avail_idx = queue.last_avail_idx.wrapping_add(1);

        let mut descriptors = Vec::new();
        let mut idx = head_index;
        for _ in 0..queue.size {
            if (idx as u32) >= queue.size {
                bail!("descriptor index {idx} out of range {}", queue.size);
            }
            let base = queue.desc_addr + (idx as u64) * DESC_SIZE;
            let d = Descriptor {
                addr: ram.read_obj(base)?,
                len: ram.read_obj(base + 8)?,
                flags: ram.read_obj(base + 12)?,
                next: ram.read_obj(base + 14)?,
            };
            descriptors.push(d);
            if d.flags & VRING_DESC_F_NEXT == 0 {
                return Ok(Some(DescriptorChain { head_index, descriptors }));
            }
            idx = d.next;
        }
        bail!("descriptor chain longer than queue size {}", queue.size);
    }

    pub fn is_write_only(&self) -> bool {
        self.descriptors
            .iter()
            .all(|d| d.flags & VRING_DESC_F_WRITE != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_and_version_are_constant() {
        let m = VirtioMmio::new(2, 0x1af4, 0, &[256]);
        assert_eq!(m.read32(MAGIC_VALUE), MAGIC);
        assert_eq!(m.read32(VERSION), VERSION_2);
        assert_eq!(m.read32(DEVICE_ID), 2);
    }

    #[test]
    fn queue_addr_split_writes_roundtrip() {
        let m = VirtioMmio::new(2, 0x1af4, 0, &[256]);
        m.write32(QUEUE_SEL, 0);
        m.write32(QUEUE_NUM, 64);
        m.write32(QUEUE_DESC_LOW, 0x1000);
        m.write32(QUEUE_DESC_HIGH, 0x0000_0001);
        m.write32(QUEUE_READY, 1);
        assert_eq!(m.read32(QUEUE_READY), 1);
        let q = m.queues[0].lock().unwrap();
        assert_eq!(q.size, 64);
        assert_eq!(q.desc_addr, 0x0000_0001_0000_1000);
        assert!(q.ready);
    }

    #[test]
    fn queue_notify_returns_index() {
        let m = VirtioMmio::new(2, 0x1af4, 0, &[256]);
        assert_eq!(m.write32(QUEUE_NOTIFY, 0), Some(0));
        assert_eq!(m.write32(QUEUE_NOTIFY, 7), Some(7));
        assert_eq!(m.write32(QUEUE_SEL, 0), None);
    }

    #[test]
    fn descriptor_chain_pops_from_avail_ring() {
        let ram = GuestRam::new(1).unwrap();
        let mut queue = VirtQueue {
            size_max: 8,
            size: 8,
            ready: true,
            desc_addr: 0x1000,
            avail_addr: 0x2000,
            used_addr: 0x3000,
            last_avail_idx: 0,
        };
        // Single descriptor at index 0: addr=0x4000, len=512, no next.
        ram.write_obj::<u64>(queue.desc_addr, 0x4000).unwrap();
        ram.write_obj::<u32>(queue.desc_addr + 8, 512).unwrap();
        ram.write_obj::<u16>(queue.desc_addr + 12, 0).unwrap();
        ram.write_obj::<u16>(queue.desc_addr + 14, 0).unwrap();
        // Available ring: idx=1, ring[0]=0 (head descriptor).
        ram.write_obj::<u16>(queue.avail_addr + 2, 1).unwrap();
        ram.write_obj::<u16>(queue.avail_addr + AVAIL_RING_HDR, 0).unwrap();

        let chain = DescriptorChain::pop(&mut queue, &ram).unwrap().unwrap();
        assert_eq!(chain.head_index, 0);
        assert_eq!(chain.descriptors.len(), 1);
        assert_eq!(chain.descriptors[0].addr, 0x4000);
        assert_eq!(chain.descriptors[0].len, 512);
        assert_eq!(queue.last_avail_idx, 1);

        // No further requests.
        assert!(DescriptorChain::pop(&mut queue, &ram).unwrap().is_none());
    }
}
