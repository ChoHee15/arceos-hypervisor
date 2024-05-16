pub mod device_emu;

extern crate alloc;
use crate::{
    nmi::NmiMessage, nmi::CPU_NMI_LIST, HyperCraftHal, PerCpuDevices, PerVmDevices,
    Result as HyperResult, VCpu, VmExitInfo, VmxExitReason,
};
use crate::{Error as HyperError, GuestPageTable, VmExitInfo as VmxExitInfo};
use alloc::string::String;
use alloc::{sync::Arc, vec, vec::Vec};
use axhal::{current_cpu_id, mem::phys_to_virt};
use bit_field::BitField;
use core::any::Any;
use core::marker::PhantomData;
use iced_x86::{Code, Instruction, OpKind, Register};
use page_table_entry::MappingFlags;
use pci::{AsAny, DummyPciHost, PciDevOps};
use spin::Mutex;

use device_emu::{ApicBaseMsrHandler, Bundle, VirtLocalApic};
use hypercraft::{GuestPageTableTrait, MmioOps, PioOps, VirtMsrOps};

use super::virtio::{
    DummyVirtioDevice, VirtioDevice, VirtioMsiIrqManager, VirtioPciDevice, VIRTIO_TYPE_BLOCK,
};

const VM_EXIT_INSTR_LEN_RDMSR: u8 = 2;
const VM_EXIT_INSTR_LEN_WRMSR: u8 = 2;
const VM_EXIT_INSTR_LEN_VMCALL: u8 = 3;

pub struct DeviceList<H: HyperCraftHal> {
    port_io_devices: Vec<Arc<Mutex<dyn PioOps>>>,
    memory_io_devices: Vec<Arc<Mutex<dyn MmioOps>>>,
    msr_devices: Vec<Arc<Mutex<dyn VirtMsrOps>>>,
    pci_devices: Option<Arc<Mutex<DummyPciHost>>>,
    marker: core::marker::PhantomData<H>,
}

impl<H: HyperCraftHal> DeviceList<H> {
    pub fn new() -> Self {
        Self {
            port_io_devices: vec![],
            memory_io_devices: vec![],
            msr_devices: vec![],
            pci_devices: None,
            marker: core::marker::PhantomData,
        }
    }

    fn init_pci_host(&mut self) {
        let pci_host = DummyPciHost::new(Some(Arc::new(super::virtio::VirtioMsiIrqManager {})));
        self.pci_devices = Some(Arc::new(Mutex::new(pci_host)));
    }

    // virtio pci devfn: 0x18 bus: 0x0.
    fn add_virtio_pci_device(
        &mut self,
        name: String,
        devfn: u8,
        device: Arc<Mutex<dyn VirtioDevice>>,
        multi_func: bool,
    ) -> HyperResult<()> {
        let mut pci_host = self.pci_devices.clone().unwrap();
        let pci_bus = pci_host.lock().root_bus.clone();
        let parent_bus = Arc::downgrade(&pci_bus);
        let mut pcidev = VirtioPciDevice::new(name, devfn, device, parent_bus, multi_func);
        pcidev.realize()
    }

    pub fn add_port_io_device(&mut self, device: Arc<Mutex<dyn PioOps>>) {
        self.port_io_devices.push(device)
    }

    pub fn add_port_io_devices(&mut self, devices: &mut Vec<Arc<Mutex<dyn PioOps>>>) {
        self.port_io_devices.append(devices)
    }

    pub fn find_port_io_device(&self, port: u16) -> Option<&Arc<Mutex<dyn PioOps>>> {
        self.port_io_devices
            .iter()
            .find(|dev| dev.lock().port_range().contains(&port))
    }

    pub fn add_memory_io_device(&mut self, device: Arc<Mutex<dyn MmioOps>>) {
        self.memory_io_devices.push(device)
    }

    pub fn add_memory_io_devices(&mut self, devices: &mut Vec<Arc<Mutex<dyn MmioOps>>>) {
        self.memory_io_devices.append(devices)
    }

    pub fn find_memory_io_device(&self, address: u64) -> Option<&Arc<Mutex<dyn MmioOps>>> {
        self.memory_io_devices
            .iter()
            .find(|dev| dev.lock().mmio_range().contains(&address))
    }

    pub fn add_msr_device(&mut self, device: Arc<Mutex<dyn VirtMsrOps>>) {
        self.msr_devices.push(device)
    }

    pub fn add_msr_devices(&mut self, devices: &mut Vec<Arc<Mutex<dyn VirtMsrOps>>>) {
        self.msr_devices.append(devices)
    }

    pub fn find_msr_device(&self, msr: u32) -> Option<&Arc<Mutex<dyn VirtMsrOps>>> {
        self.msr_devices
            .iter()
            .find(|dev| dev.lock().msr_range().contains(&msr))
    }

    fn handle_io_instruction_to_device(
        vcpu: &mut VCpu<H>,
        exit_info: &VmxExitInfo,
        device: &Arc<Mutex<dyn PioOps>>,
    ) -> HyperResult {
        let io_info = vcpu.io_exit_info().unwrap();
        trace!(
            "VM exit: I/O instruction @ {:#x}: {:#x?}",
            exit_info.guest_rip,
            io_info,
        );

        if io_info.is_string {
            error!("INS/OUTS instructions are not supported!");
            return Err(HyperError::NotSupported);
        }
        if io_info.is_repeat {
            error!("REP prefixed I/O instructions are not supported!");
            return Err(HyperError::NotSupported);
        }
        if io_info.is_in {
            let value = device.lock().read(io_info.port, io_info.access_size)?;
            let rax = &mut vcpu.regs_mut().rax;
            // SDM Vol. 1, Section 3.4.1.1:
            // * 32-bit operands generate a 32-bit result, zero-extended to a 64-bit result in the
            //   destination general-purpose register.
            // * 8-bit and 16-bit operands generate an 8-bit or 16-bit result. The upper 56 bits or
            //   48 bits (respectively) of the destination general-purpose register are not modified
            //   by the operation.
            match io_info.access_size {
                1 => *rax = (*rax & !0xff) | (value & 0xff) as u64,
                2 => *rax = (*rax & !0xffff) | (value & 0xffff) as u64,
                4 => *rax = value as u64,
                _ => unreachable!(),
            }
        } else {
            let rax = vcpu.regs().rax;
            let value = match io_info.access_size {
                1 => rax & 0xff,
                2 => rax & 0xffff,
                4 => rax,
                _ => unreachable!(),
            } as u32;
            device
                .lock()
                .write(io_info.port, io_info.access_size, value)?;
        }
        vcpu.advance_rip(exit_info.exit_instruction_length as _)?;
        Ok(())
    }

    pub fn handle_io_instruction(
        &mut self,
        vcpu: &mut VCpu<H>,
        exit_info: &VmxExitInfo,
    ) -> Option<HyperResult> {
        let io_info = vcpu.io_exit_info().unwrap();

        debug!("handle io {:#x?}", io_info);

        if let Some(dev) = self.find_port_io_device(io_info.port) {
            return Some(Self::handle_io_instruction_to_device(vcpu, exit_info, dev));
        } else {
            if self.pci_devices.is_some() {
                let pci_host = self.pci_devices.clone().unwrap();
                let mut root_bus = &pci_host.lock().root_bus;
                if let Some(bar) = root_bus.clone().lock().find_pio_bar(io_info.port) {
                    return Some(Self::handle_io_instruction_to_device(vcpu, exit_info, &bar));
                }
            }
            return None;
        }
    }

    fn handle_mmio_instruction_to_device(
        vcpu: &mut VCpu<H>,
        exit_info: &VmxExitInfo,
        device: &Arc<Mutex<dyn MmioOps>>,
    ) -> HyperResult {
        Ok(())
    }

    pub fn handle_mmio_instruction(
        &mut self,
        vcpu: &mut VCpu<H>,
        exit_info: &VmxExitInfo,
        instr: Option<Instruction>,
    ) -> Option<HyperResult> {
        if let Some(instr) = instr {
            if let ept_info = vcpu
                .nested_page_fault_info()
                .expect("Failed to get nested page fault info")
            {
                let is_write = ept_info.access_flags.contains(MappingFlags::WRITE);
                let fault_addr = ept_info.fault_guest_paddr;
                let access_size =
                    get_access_size(instr.clone()).expect("Failed to get access size");
                return None;
            } else {
                panic!(
                    "VM exit: EPT violation with unknown fault info @ {:#x}, vcpu: {:#x?}",
                    exit_info.guest_rip, vcpu
                );
            }
        } else {
            return None;
        }

        // match vcpu.nested_page_fault_info() {
        //     Ok(fault_info) =>  {
        //         debug!(
        //             "VM exit: EPT violation @ {:#x}, fault_paddr={:#x}, access_flags=({:?}), vcpu: {:#x?}",
        //             exit_info.guest_rip, fault_info.fault_guest_paddr, fault_info.access_flags, vcpu
        //         );
        //         // todo: get mmio info
        //         let mmio_info = vcpu.io_exit_info().unwrap();

        //         if let Some(dev) = self.find_memory_io_device() {
        //             return Some(Self::handle_mmio_instruction_to_device(vcpu, exit_info, dev));
        //         } else {
        //             if self.pci_devices.is_some() {
        //                 let pci_host = self.pci_devices.clone().unwrap();
        //                 let mut root_bus = pci_host.lock().root_bus.clone();
        //                 if let Some(bar) = root_bus.find_mmio_bar() {
        //                     return Some(Self::handle_mmio_instruction_to_device(vcpu, exit_info, dev));
        //                 }
        //             }
        //             return None;
        //         }
        //         Some(Ok(()))
        //     }
        //     Err(_err) => panic!(
        //         "VM exit: EPT violation with unknown fault info @ {:#x}, vcpu: {:#x?}",
        //         exit_info.guest_rip, vcpu
        //     ),
        // }
        None
    }

    pub fn handle_msr_read(&mut self, vcpu: &mut VCpu<H>) -> HyperResult {
        let msr = vcpu.regs().rcx as u32;

        if let Some(dev) = self.find_msr_device(msr) {
            match dev.lock().read(msr) {
                Ok(value) => {
                    trace!("VM exit: RDMSR({:#x}) -> {:#x}", msr, value);

                    vcpu.regs_mut().rax = value & 0xffff_ffff;
                    vcpu.regs_mut().rdx = value >> 32;

                    vcpu.advance_rip(VM_EXIT_INSTR_LEN_RDMSR)?;
                    Ok(())
                }
                Err(e) => {
                    panic!("Failed to handle RDMSR({:#x}): {:?}", msr, e);
                }
            }
        } else {
            panic!("Unsupported RDMSR {:#x}, vcpu: {:#x?}", msr, vcpu);
        }
    }

    pub fn handle_msr_write(&mut self, vcpu: &mut VCpu<H>) -> HyperResult {
        let msr = vcpu.regs().rcx as u32;
        let value = (vcpu.regs().rax & 0xffff_ffff) | (vcpu.regs().rdx << 32);

        if let Some(dev) = self.find_msr_device(msr) {
            match dev.lock().write(msr, value) {
                Ok(_) => {
                    trace!("VM exit: WRMSR({:#x}) <- {:#x}", msr, value);

                    vcpu.advance_rip(VM_EXIT_INSTR_LEN_WRMSR)?;
                    Ok(())
                }
                Err(e) => {
                    panic!("Failed to handle WRMSR({:#x}): {:?}", msr, e);
                }
            }
        } else {
            panic!("Unsupported WRMSR {:#x}, vcpu: {:#x?}", msr, vcpu);
        }
    }
}

pub struct X64VcpuDevices<H: HyperCraftHal> {
    pub(crate) apic_timer: Arc<Mutex<VirtLocalApic>>,
    pub(crate) bundle: Arc<Mutex<Bundle>>,
    pub(crate) devices: DeviceList<H>,
    // pub(crate) console: Arc<Mutex<device_emu::Uart16550<device_emu::MultiplexConsoleBackend>>>,
    pub(crate) pic: [Arc<Mutex<device_emu::I8259Pic>>; 2],
    last: Option<u64>,
    marker: PhantomData<H>,
}

impl<H: HyperCraftHal> PerCpuDevices<H> for X64VcpuDevices<H> {
    fn new(_vcpu: &VCpu<H>) -> HyperResult<Self> {
        let apic_timer = Arc::new(Mutex::new(VirtLocalApic::new()));
        let bundle = Arc::new(Mutex::new(Bundle::new()));
        let pic: [Arc<Mutex<device_emu::I8259Pic>>; 2] = [
            Arc::new(Mutex::new(device_emu::I8259Pic::new(0x20))),
            Arc::new(Mutex::new(device_emu::I8259Pic::new(0xA0))),
        ];

        let mut devices = DeviceList::new();

        let mut pmio_devices: Vec<Arc<Mutex<dyn PioOps>>> = vec![
            // These are all fully emulated consoles!!!
            // 0x3f8, 0x3f8 + 8
            Arc::new(Mutex::new(<device_emu::Uart16550>::new(0x3f8))), // COM1
            // 0x2f8, 0x2f8 + 8
            Arc::new(Mutex::new(<device_emu::Uart16550>::new(0x2f8))), // COM2
            // 0x3e8, 0x3e8 + 8
            Arc::new(Mutex::new(<device_emu::Uart16550>::new(0x3e8))), // COM3
            // 0x2e8, 0x2e8 + 8
            Arc::new(Mutex::new(<device_emu::Uart16550>::new(0x2e8))), // COM4
            // 0x20, 0x20 + 2
            pic[0].clone(), // PIC1
            // 0xa0, 0xa0 + 2
            pic[1].clone(), // PIC2
            // 0x80, 0x80 + 1
            Arc::new(Mutex::new(device_emu::DebugPort::new(0x80))), // Debug Port
            /*
               the complexity:
               - port 0x70 and 0x71 is for CMOS, but bit 7 of 0x70 is for NMI
               - port 0x40 ~ 0x43 is for PIT, but port 0x61 is also related
            */
            // 0x92, 0x92 + 1
            Arc::new(Mutex::new(Bundle::proxy_system_control_a(&bundle))),
            // 0x61, 0x61 + 1
            Arc::new(Mutex::new(Bundle::proxy_system_control_b(&bundle))),
            // 0x70, 0x70 + 2
            Arc::new(Mutex::new(Bundle::proxy_cmos(&bundle))),
            // 0x40, 0x40 + 4
            Arc::new(Mutex::new(Bundle::proxy_pit(&bundle))),
            // 0xf0, 0xf0 + 2
            Arc::new(Mutex::new(device_emu::Dummy::new(0xf0, 2))), // 0xf0 and 0xf1 are ports about fpu
            // 0x3d4, 0x3d4 + 2
            Arc::new(Mutex::new(device_emu::Dummy::new(0x3d4, 2))), // 0x3d4 and 0x3d5 are ports about vga
            // 0x87, 0x87 + 1
            Arc::new(Mutex::new(device_emu::Dummy::new(0x87, 1))), // 0x87 is a port about dma
            // 0x60, 0x60 + 1
            Arc::new(Mutex::new(device_emu::Dummy::new(0x60, 1))), // 0x60 and 0x64 are ports about ps/2 controller
            // 0x64, 0x64 + 1
            Arc::new(Mutex::new(device_emu::Dummy::new(0x64, 1))), //
                                                                   // Arc::new(Mutex::new(device_emu::PCIConfigurationSpace::new(0xcf8))),
                                                                   // Arc::new(Mutex::new(device_emu::PCIPassthrough::new(0xcf8))),
        ];
        devices.add_port_io_devices(&mut pmio_devices);

        devices.add_msr_device(Arc::new(Mutex::new(device_emu::ProxyLocalApic::new())));
        devices.add_msr_device(Arc::new(Mutex::new(ApicBaseMsrHandler {})));
        // linux read this amd-related msr on my intel cpu for some unknown reason... make it happy
        devices.add_msr_device(Arc::new(Mutex::new(device_emu::MsrDummy::new(0xc0011029))));
        const IA32_UMWAIT_CONTROL: u32 = 0xe1;
        devices.add_msr_device(Arc::new(Mutex::new(device_emu::MsrDummy::new(
            IA32_UMWAIT_CONTROL,
        ))));

        Ok(Self {
            apic_timer,
            bundle,
            devices,
            pic,
            last: None,
            marker: PhantomData,
        })
    }

    fn vmexit_handler(
        &mut self,
        vcpu: &mut VCpu<H>,
        exit_info: &VmExitInfo,
    ) -> Option<HyperResult> {
        match exit_info.exit_reason {
            VmxExitReason::IO_INSTRUCTION => self.devices.handle_io_instruction(vcpu, exit_info),
            VmxExitReason::MSR_READ => Some(self.devices.handle_msr_read(vcpu)),
            VmxExitReason::MSR_WRITE => Some(self.devices.handle_msr_write(vcpu)),
            _ => None,
        }
    }

    fn hypercall_handler(
        &mut self,
        vcpu: &mut VCpu<H>,
        id: u32,
        args: (usize, usize, usize),
    ) -> HyperResult<u32> {
        // debug!("hypercall #{id:#x?}, args: {args:#x?}");
        crate::hvc::handle_hvc(vcpu, id as usize, args)
    }

    fn nmi_handler(&mut self, vcpu: &mut VCpu<H>) -> HyperResult<u32> {
        let current_cpu_id = current_cpu_id();
        debug!("nmi handler on CPU {}!", current_cpu_id);
        // let mut cpu_nmi_list = CPU_NMI_LIST[].lock();
        let msg = CPU_NMI_LIST[current_cpu_id].lock().pop();
        match msg {
            Some(NmiMessage::BootVm(vm_id)) => {
                crate::vm::boot_vm(vm_id);
                Ok(0)
            }
            None => {
                warn!(
                    "Core [{}] unexpected NMI, something very bad happened",
                    current_cpu_id
                );
                warn!("VCPU ctx:\n{:#x?}", vcpu);
                Ok(0)
            }
        }
    }

    fn check_events(&mut self, vcpu: &mut VCpu<H>) -> HyperResult {
        if self.apic_timer.lock().inner.check_interrupt() {
            vcpu.queue_event(self.apic_timer.lock().inner.vector(), None);
        }

        // it's naive but it works.
        // inject 0x30(irq 0) every 1 ms after 5 seconds after booting.
        match self.last {
            Some(last) => {
                let now = axhal::time::current_time_nanos();
                if now > 1_000_000 + last {
                    // debug!(
                    //     "vcpu [{}] check events current {} last {} tick {} ns",
                    //     vcpu.vcpu_id(),
                    //     now,
                    //     last,
                    //     now - last,
                    // );
                    if !self.pic[0].lock().mask().get_bit(0) {
                        vcpu.queue_event(0x30, None);
                        let _mask = self.pic[0].lock().mask();
                        // debug!("0x30 queued, mask {_mask:#x}");
                    }
                    self.last = Some(now);
                }
            }
            None => {
                self.last = Some(axhal::time::current_time_nanos() + 5_000_000_000);
                debug!(
                    "vcpu [{}] check events last set to {} ns",
                    vcpu.vcpu_id(),
                    self.last.unwrap()
                );
            }
        }

        Ok(())
    }
}

pub struct X64VmDevices<H: HyperCraftHal> {
    devices: DeviceList<H>,
    marker: PhantomData<H>,
}

impl<H: HyperCraftHal> X64VmDevices<H> {
    fn handle_external_interrupt(vcpu: &VCpu<H>) -> HyperResult {
        let int_info = vcpu.interrupt_exit_info()?;
        debug!("VM-exit: external interrupt: {:#x?}", int_info);

        if int_info.vector != 0xf0 {
            panic!("VM-exit: external interrupt: {:#x?}", int_info);
        }

        assert!(int_info.valid);

        crate::irq::dispatch_host_irq(int_info.vector as usize)
    }
}

impl<H: HyperCraftHal> PerVmDevices<H> for X64VmDevices<H> {
    fn new() -> HyperResult<Self> {
        let mut devices = DeviceList::new();
        // init pci device
        devices.init_pci_host();
        devices.add_port_io_device(devices.pci_devices.clone().unwrap());
        // Create a virtio dummy device
        let virtio_device_dummy = DummyVirtioDevice::new(VIRTIO_TYPE_BLOCK, 1, 4);
        devices.add_virtio_pci_device(
            String::from("virtio_blk_dummy"),
            0x18,
            Arc::new(Mutex::new(virtio_device_dummy)),
            false,
        )?;

        Ok(Self {
            marker: PhantomData,
            devices,
        })
    }

    fn vmexit_handler(
        &mut self,
        vcpu: &mut VCpu<H>,
        exit_info: &VmExitInfo,
        instr: Option<Instruction>,
    ) -> Option<HyperResult> {
		debug!("PerVMDevice vmexit handler {:?}", exit_info.exit_reason);
        match exit_info.exit_reason {
            VmxExitReason::EXTERNAL_INTERRUPT => Some(Self::handle_external_interrupt(vcpu)),
            VmxExitReason::EPT_VIOLATION => {
                self.devices.handle_mmio_instruction(vcpu, exit_info, instr)
            }
            VmxExitReason::IO_INSTRUCTION => self.devices.handle_io_instruction(vcpu, exit_info),
            VmxExitReason::MSR_READ => Some(self.devices.handle_msr_read(vcpu)),
            VmxExitReason::MSR_WRITE => Some(self.devices.handle_msr_write(vcpu)),
            _ => None,
        }
    }
}

fn get_access_size(instruction: Instruction) -> HyperResult<u8> {
    match instruction.code() {
        Code::INVALID => Err(HyperError::DecodeError),
        _ => {
            let size = match (
                instruction.op0_kind(),
                instruction.op1_kind(),
                instruction.op2_kind(),
            ) {
                (OpKind::Register, _, _) | (_, OpKind::Register, _) | (_, _, OpKind::Register) => {
                    instruction.op_register(0).size()
                }
                (OpKind::Memory, _, _) | (_, OpKind::Memory, _) | (_, _, OpKind::Memory) => {
                    instruction.memory_size().size()
                }
                (OpKind::Immediate8, _, _)
                | (_, OpKind::Immediate8, _)
                | (_, _, OpKind::Immediate8) => 1,
                (OpKind::Immediate16, _, _)
                | (_, OpKind::Immediate16, _)
                | (_, _, OpKind::Immediate16) => 2,
                (OpKind::Immediate32, _, _)
                | (_, OpKind::Immediate32, _)
                | (_, _, OpKind::Immediate32) => 4,
                (OpKind::Immediate64, _, _)
                | (_, OpKind::Immediate64, _)
                | (_, _, OpKind::Immediate64) => 8,
                _ => 0,
            };
            Ok(size as u8)
        }
    }
}

fn get_write_data(instruction: Instruction) -> HyperResult<u64> {
    // TODO
    Ok(0)
}
