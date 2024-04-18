#[cfg(feature = "type1_5")]
use hypercraft::LinuxContext;
/// Temporar module to boot Linux as a guest VM.
///
/// To be removed...
// use hypercraft::GuestPageTableTrait;
use hypercraft::{PerCpu, VCpu, VmCpus, VM};

use super::arch::new_vcpu;
#[cfg(target_arch = "x86_64")]
use super::device::{self, X64VcpuDevices, X64VmDevices};
use crate::{phys_to_virt, PhysAddr};
use axhal::hv::HyperCraftHalImpl;
use axhal::mem::PAGE_SIZE_4K;

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
// use super::type1_5::cell;
static INIT_GPM_OK: AtomicU32 = AtomicU32::new(0);
static INITED_CPUS: AtomicUsize = AtomicUsize::new(0);

static VM_ID_ALLOCATOR: AtomicU32 = AtomicUsize::new(1);

pub fn generate_vm_id() {
    VM_ID_ALLOCATOR.fetch_add(1, Ordering::SeqCst);
}

pub fn config_boot_linux(hart_id: usize, linux_context: &LinuxContext) {
    crate::arch::cpu_hv_hardware_enable(hart_id, linux_context);

    if hart_id == 0 {
        super::config::init_root_gpm();
        INIT_GPM_OK.store(1, Ordering::Release);
    } else {
        while INIT_GPM_OK.load(Ordering::Acquire) < 1 {
            core::hint::spin_loop();
        }
    }
    info!("CPU{} after init_gpm", hart_id);

    debug!(
        "CPU{} type 1.5 gpm: {:#x?}",
        hart_id,
        super::config::root_gpm()
    );

    let ept_root = super::config::root_gpm().nest_page_table_root();

    let vcpu = new_vcpu(
        hart_id,
        crate::arch::cpu_vmcs_revision_id(),
        ept_root,
        &linux_context,
    )
    .unwrap();
    let mut vcpus = VmCpus::<HyperCraftHalImpl, X64VcpuDevices<HyperCraftHalImpl>>::new();
    info!("CPU{} add vcpu to vm...", hart_id);
    vcpus.add_vcpu(vcpu).expect("add vcpu failed");
    let mut vm = VM::<
        HyperCraftHalImpl,
        X64VcpuDevices<HyperCraftHalImpl>,
        X64VmDevices<HyperCraftHalImpl>,
    >::new(vcpus);
    // The bind_vcpu method should be decoupled with vm struct.
    vm.bind_vcpu(hart_id).expect("bind vcpu failed");

    INITED_CPUS.fetch_add(1, Ordering::SeqCst);
    while INITED_CPUS.load(Ordering::Acquire) < axconfig::SMP {
        core::hint::spin_loop();
    }

    debug!("CPU{} before run vcpu", hart_id);
    info!("{:?}", vm.run_type15_vcpu(hart_id, &linux_context));

    // disable hardware virtualization todo
}

pub fn boot_vm(vm_type: usize, entry: usize, phy_addr: usize) {
    info!("boot_vm");
    let size = unsafe {
        core::slice::from_raw_parts(
            phys_to_virt(PhysAddr::from(phy_addr)).as_ptr() as *const u64,
            3,
        )
    };
    info!("size: {:x?}: ", size);
    let code = unsafe {
        core::slice::from_raw_parts(
            phys_to_virt(PhysAddr::from(phy_addr)).as_ptr(),
            size[0] as usize,
        )
    };
    // info!("content: {:x?}: ", code);

    if vm_type == 1 {
        info!("start nimbos vm");
        let bios_paddr = phy_addr + PAGE_SIZE_4K;
        let guest_image_paddr = phy_addr + PAGE_SIZE_4K + size[1] as usize;
        let gpm = super::config::setup_nimbos_gpm(
            bios_paddr,
            size[1] as usize,
            guest_image_paddr,
            size[2] as usize,
        )
        .unwrap();
        let npt = gpm.nest_page_table_root();
        info!("{:#x?}", gpm);

        // Main scheduling item, managed by `axtask`
        let vcpu = VCpu::new_nimbos(0, crate::arch::cpu_vmcs_revision_id(), entry, npt).unwrap();
        info!("vcpu...");
        let mut vcpus = VmCpus::<HyperCraftHalImpl, X64VcpuDevices<HyperCraftHalImpl>>::new();
        info!("vcpus...");
        vcpus.add_vcpu(vcpu).expect("add vcpu failed");
        info!("add vcpus...");
        let mut vm = VM::<
            HyperCraftHalImpl,
            X64VcpuDevices<HyperCraftHalImpl>,
            X64VmDevices<HyperCraftHalImpl>,
        >::new(vcpus);
        info!("vm...");
        // The bind_vcpu method should be decoupled with vm struct.
        vm.bind_vcpu(0).expect("bind vcpu failed");

        info!("Running guest...");
        info!("{:?}", vm.run_vcpu(0));
    }
}
