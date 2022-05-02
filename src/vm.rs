// Copyright (c) 2021 by Rivos Inc.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

use page_collections::page_box::PageBox;
use page_collections::page_vec::PageVec;
use riscv_page_tables::PlatformPageTable;
use riscv_pages::{AlignedPageAddr4k, PageOwnerId, PageSize4k, PhysAddr, SequentialPages};
use riscv_regs::{hedeleg, hgatp, hideleg, hstatus, sie, sstatus, HgatpHelpers};
use riscv_regs::{
    Exception, GeneralPurposeRegisters, GprIndex, Interrupt, LocalRegisterCopy, Trap,
};
use sbi::Error as SbiError;
use sbi::{self, ResetFunction, SbiMessage, SbiReturn, TeeFunction};

use crate::data_measure::DataMeasure;
use crate::print_util::*;
use crate::vm_pages::{self, GuestRootBuilder, HostRootPages, VmPages};
use crate::GuestOwnedPage;
use crate::{print, println};

// Defined in guest.S
extern "C" {
    fn _run_guest(g: *mut VmCpuState);
}

#[derive(Default)]
#[repr(C)]
#[allow(dead_code)]
pub struct VmCsrs {
    sepc: u64,
    sie: u64,
    scause: u64,
    stvec: u64,
    hgatp: u64,
    hedeleg: u64,
    hideleg: u64,
    hstatus: u64,
    hcounteren: u64,
    sstatus: u64,
    stval: u64,
    htval: u64,
}

// With the exception of hgatp, any value of guest registers is safe for the host, the guest might
// malfunction but it can't affect host memory.
#[derive(Default)]
#[repr(C)]
#[allow(dead_code)]
struct VmCpuState {
    sp: u64,
    csrs: VmCsrs,
    gprs: GeneralPurposeRegisters,
}

struct Guests<T: PlatformPageTable, D: DataMeasure> {
    inner: PageVec<PageBox<GuestState<T, D>>>,
}

impl<T: PlatformPageTable, D: DataMeasure> Guests<T, D> {
    fn add(&mut self, guest_state: PageBox<GuestState<T, D>>) -> sbi::Result<()> {
        self.inner
            .try_reserve(1)
            .map_err(|_| SbiError::InvalidParam)?;
        self.inner.push(guest_state);
        Ok(())
    }

    // Returns the index in to the guest array of the given guest id. If the guest ID isn't valid or
    // isn't owned by this VM, then return an error.
    fn get_guest_index(&self, guest_id: u64) -> sbi::Result<usize> {
        let page_owner_id = PageOwnerId::new(guest_id).ok_or(SbiError::InvalidParam)?;
        self.inner
            .iter()
            .enumerate()
            .find(|(_i, g)| page_owner_id == g.page_owner_id())
            .map(|(guest_index, _guest)| guest_index)
            .ok_or(SbiError::InvalidParam)
    }

    fn remove(&mut self, guest_id: u64) -> sbi::Result<()> {
        let to_remove = PageOwnerId::new(guest_id).ok_or(SbiError::InvalidParam)?;
        self.inner.retain(|g| g.page_owner_id() != to_remove);
        Ok(())
    }

    // Returns the guest for the given ID if it exists, otherwise None.
    fn guest_mut(&mut self, guest_id: u64) -> sbi::Result<&mut PageBox<GuestState<T, D>>> {
        let guest_index = self.get_guest_index(guest_id)?;
        self.inner
            .get_mut(guest_index)
            .ok_or(SbiError::InvalidParam)
    }

    // returns the initializing guest if it's present and runnable, otherwise none
    fn initializing_guest_mut(
        &mut self,
        guest_id: u64,
    ) -> sbi::Result<&mut GuestRootBuilder<T, D>> {
        self.guest_mut(guest_id)
            .and_then(|g| g.init_mut().ok_or(SbiError::InvalidParam))
    }

    // Returns the runnable guest if it's present and runnable, otherwise None
    fn running_guest_mut(&mut self, guest_id: u64) -> sbi::Result<&mut Vm<T, D>> {
        self.guest_mut(guest_id)
            .and_then(|g| g.vm_mut().ok_or(SbiError::InvalidParam))
    }
}

enum GuestState<T: PlatformPageTable, D: DataMeasure> {
    Init(GuestRootBuilder<T, D>),
    Running(Vm<T, D>),
    Temp,
}

impl<T: PlatformPageTable, D: DataMeasure> GuestState<T, D> {
    fn page_owner_id(&self) -> PageOwnerId {
        match self {
            Self::Init(grb) => grb.page_owner_id(),
            Self::Running(v) => v.vm_pages.page_owner_id(),
            Self::Temp => unreachable!(),
        }
    }

    fn init_mut(&mut self) -> Option<&mut GuestRootBuilder<T, D>> {
        match self {
            Self::Init(ref mut grb) => Some(grb),
            Self::Running(_) => None,
            Self::Temp => unreachable!(),
        }
    }

    fn vm_mut(&mut self) -> Option<&mut Vm<T, D>> {
        match self {
            Self::Init(_) => None,
            Self::Running(ref mut v) => Some(v),
            Self::Temp => unreachable!(),
        }
    }
}

/// A Vm VM that is being run.
pub struct Vm<T: PlatformPageTable, D: DataMeasure> {
    // TODO, info should be per-hart.
    info: VmCpuState,
    vm_pages: VmPages<T, D>,
    guests: Option<Guests<T, D>>,
    has_run: bool, // TODO - different Vm type for different life cycle stages.
}

impl<T: PlatformPageTable, D: DataMeasure> Vm<T, D> {
    /// Create a new guest using the given initial page table and pool of initial pages.
    fn new(vm_pages: VmPages<T, D>) -> Self {
        let mut info = VmCpuState::default();

        // TODO: Several of these are not really per-VM registers and should be initialized
        // elsewhere. We're also not saving and restoring all the registers that we need to on
        // a VM context switch (and the ones we do don't necessarily all need to be done from asm).
        let mut sie = LocalRegisterCopy::<u64, sie::Register>::new(0);
        sie.modify(Interrupt::SupervisorSoft.to_sie_field().unwrap());
        sie.modify(Interrupt::SupervisorTimer.to_sie_field().unwrap());
        sie.modify(Interrupt::SupervisorExternal.to_sie_field().unwrap());
        info.csrs.sie = sie.get();

        let mut hgatp = LocalRegisterCopy::<u64, hgatp::Register>::new(0);
        hgatp.set_from(vm_pages.root(), 1);
        info.csrs.hgatp = hgatp.get();

        let mut hedeleg = LocalRegisterCopy::<u64, hedeleg::Register>::new(0);
        hedeleg.modify(Exception::InstructionMisaligned.to_hedeleg_field().unwrap());
        hedeleg.modify(Exception::Breakpoint.to_hedeleg_field().unwrap());
        hedeleg.modify(Exception::UserEnvCall.to_hedeleg_field().unwrap());
        hedeleg.modify(Exception::InstructionPageFault.to_hedeleg_field().unwrap());
        hedeleg.modify(Exception::LoadPageFault.to_hedeleg_field().unwrap());
        hedeleg.modify(Exception::StorePageFault.to_hedeleg_field().unwrap());
        info.csrs.hedeleg = hedeleg.get();

        let mut hideleg = LocalRegisterCopy::<u64, hideleg::Register>::new(0);
        hideleg.modify(Interrupt::VirtualSupervisorSoft.to_hideleg_field().unwrap());
        hideleg.modify(
            Interrupt::VirtualSupervisorTimer
                .to_hideleg_field()
                .unwrap(),
        );
        hideleg.modify(
            Interrupt::VirtualSupervisorExternal
                .to_hideleg_field()
                .unwrap(),
        );
        info.csrs.sie = hideleg.get();

        let mut hstatus = LocalRegisterCopy::<u64, hstatus::Register>::new(0);
        hstatus.modify(hstatus::spv.val(1));
        hstatus.modify(hstatus::spvp::Supervisor);
        info.csrs.hstatus = hstatus.get();

        info.csrs.hcounteren = 0xffff_ffff_ffff_ffff; // enable all

        let mut sstatus = LocalRegisterCopy::<u64, sstatus::Register>::new(0);
        sstatus.modify(sstatus::spp::Supervisor);
        sstatus.modify(sstatus::spie.val(1));
        info.csrs.sstatus = sstatus.get();

        // set the hart ID - TODO other hart IDs when multi-threaded
        info.gprs.set_reg(GprIndex::A0, 0);

        Vm {
            info,
            vm_pages,
            guests: None,
            has_run: false,
        }
    }

    fn set_entry_address(&mut self, entry_addr: u64) {
        self.info.csrs.sepc = entry_addr;
    }

    // TODO - also pass the DT here and copy it?
    fn add_device_tree(&mut self, dt_addr: u64) {
        // set the DT address to the one passed in.
        self.info.gprs.set_reg(GprIndex::A1, dt_addr);
    }

    /// `guests`: A vec for storing guest info if "nested" guests will be created. Must have
    /// length zero and capacity limits the number of nested guests.
    fn add_guest_tracking_pages(&mut self, pages: SequentialPages<PageSize4k>) {
        let guests = PageVec::from(pages);
        self.guests = Some(Guests { inner: guests });
    }

    /// Run this VM until the guest exits
    fn run_to_exit(&mut self, _hart_id: u64) -> Trap {
        unsafe {
            // Safe to run the guest as it only touches memory assigned to it by being owned
            // by its page table.
            _run_guest(&mut self.info as *mut VmCpuState);
        }
        Trap::from_scause(self.info.csrs.scause).unwrap()
    }

    /// Run this guest until it requests an exit or an interrupt is received for the host.
    fn run(&mut self, hart_id: u64) -> Trap {
        use Exception::*;
        self.has_run = true;
        loop {
            match self.run_to_exit(hart_id) {
                Trap::Exception(VirtualSupervisorEnvCall) => {
                    self.handle_ecall();
                    self.inc_sepc_ecall(); // must return to _after_ the ecall.
                }
                Trap::Exception(GuestInstructionPageFault) => {
                    if self.handle_guest_fault(/*Instruction*/).is_err() {
                        return Trap::Exception(GuestInstructionPageFault);
                    }
                }
                Trap::Exception(GuestLoadPageFault) => {
                    if self.handle_guest_fault(/*Load*/).is_err() {
                        return Trap::Exception(GuestLoadPageFault);
                    }
                }
                Trap::Exception(GuestStorePageFault) => {
                    if self.handle_guest_fault(/*Store*/).is_err() {
                        return Trap::Exception(GuestStorePageFault);
                    }
                }
                e => return e, // TODO
            }
        }
    }

    /// Gets the CSR values for this guest.
    fn csrs(&self) -> &VmCsrs {
        &self.info.csrs
    }

    /// Advances the sepc past the ecall instruction that caused the exit.
    fn inc_sepc_ecall(&mut self) {
        self.info.csrs.sepc += 4;
    }

    /// Handles ecalls from the guest.
    fn handle_ecall(&mut self) {
        // determine the call from a7, a6, and a2-5, put error code in a0 and return value in a1.
        // a0 and a1 aren't set by legacy extensions so the block below yields an `Option` that is
        // written when set to `Some(val)`.
        let result = SbiMessage::from_regs(&self.info.gprs).and_then(|msg| {
            match msg {
                SbiMessage::PutChar(c) => {
                    // put char - legacy command
                    print!("{}", c as u8 as char);
                    Ok(None)
                }
                SbiMessage::Reset(r) => {
                    match r {
                        ResetFunction::Reset {
                            reset_type: _,
                            reason: _,
                        } => {
                            // TODO do shutdown of VM or system if from primary host VM
                            println!("Vm shutdown/reboot request");
                            crate::poweroff();
                        }
                    }
                }
                SbiMessage::Base(_) => Err(SbiError::NotSupported), // TODO
                SbiMessage::HartState(_) => Err(SbiError::NotSupported), // TODO
                SbiMessage::Tee(tee_func) => Ok(Some(self.handle_tee_msg(tee_func))),
            }
        });

        match result {
            Ok(Some(sbi_ret)) => {
                self.info
                    .gprs
                    .set_reg(GprIndex::A0, sbi_ret.error_code as u64);
                self.info
                    .gprs
                    .set_reg(GprIndex::A1, sbi_ret.return_value as u64);
            }
            Ok(None) => {
                // for legacy, leave the a0 and a1 registers as-is.
            }
            Err(error_code) => {
                self.info
                    .gprs
                    .set_reg(GprIndex::A0, SbiReturn::from(error_code).error_code as u64);
            }
        }
    }

    fn handle_tee_msg(&mut self, tee_func: TeeFunction) -> SbiReturn {
        use TeeFunction::*;
        match tee_func {
            TvmCreate(state_page) => self.add_guest(state_page).into(),
            TvmDestroy { guest_id } => self.destroy_guest(guest_id).into(),
            AddPageTablePages {
                guest_id,
                page_addr,
                num_pages,
            } => self
                .guest_add_page_table_pages(guest_id, page_addr, num_pages)
                .into(),
            AddPages {
                guest_id,
                page_addr,
                page_type,
                num_pages,
                gpa,
                measure_preserve,
            } => self
                .guest_add_pages(
                    guest_id,
                    page_addr,
                    page_type,
                    num_pages,
                    gpa,
                    measure_preserve,
                )
                .into(),
            Finalize { guest_id } => self.guest_finalize(guest_id).into(),
            Run { guest_id } => self.guest_run(guest_id).into(),
            RemovePages {
                guest_id,
                gpa,
                remap_addr: _, // TODO - remove
                num_pages,
            } => self.guest_rm_pages(guest_id, gpa, num_pages).into(),
            GetGuestMeasurement {
                guest_id,
                measurement_version,
                measurement_type,
                page_addr,
            } => self
                .guest_get_measurement(guest_id, measurement_version, measurement_type, page_addr)
                .into(),
        }
    }

    // Handle access faults. For example, when a returned page needs to be demand-faulted back to
    // the page table.
    fn handle_guest_fault(&mut self) -> core::result::Result<(), vm_pages::Error> {
        let csrs = self.csrs();

        let fault_addr = csrs.htval << 2 | csrs.stval & 0x03;
        println!(
            "got fault {:x} {:x} {:x} {:x}",
            csrs.stval, csrs.htval, csrs.sepc, fault_addr
        );

        self.vm_pages.handle_page_fault(fault_addr)?;

        // Get instruction that caused the fault
        //   - disable ints
        //   - load hstatus with value from guest
        //   - set stvec to catch traps during access
        //   - read instruction using HLV.HU (or tow for 32 bit).
        //   - reset stvec
        //   - reset hstatus
        //   - re-enable ints

        // Determine width of faulting access
        // determine destination/source register
        // Check how to service access (device emulation for example) and run.
        // if load, set destination register

        Ok(())
    }

    fn add_guest(&mut self, donor_pages_addr: u64) -> sbi::Result<u64> {
        println!("Add guest {:x}", donor_pages_addr);
        if self.guests.is_none() {
            return Err(SbiError::InvalidParam); // TODO different error
        }

        let from_page_addr = AlignedPageAddr4k::new(PhysAddr::new(donor_pages_addr))
            .ok_or(SbiError::InvalidAddress)?;

        let (guest_builder, state_page) = self
            .vm_pages
            .create_guest_root_builder(from_page_addr)
            .map_err(|_| SbiError::InvalidParam)?;
        // unwrap can't fail because a valid guest must have a valid guest id.
        let id = guest_builder.page_owner_id();

        // create a boxpage for builder state and add it to the list of vms.
        let guest_state: PageBox<GuestState<T, D>> =
            PageBox::new_with(GuestState::Init(guest_builder), state_page);
        self.guests
            .as_mut()
            .ok_or(SbiError::InvalidParam)
            .and_then(|g| g.add(guest_state))?;

        Ok(id.raw())
    }

    fn destroy_guest(&mut self, guest_id: u64) -> sbi::Result<u64> {
        self.guests
            .as_mut()
            .ok_or(SbiError::InvalidParam)
            .and_then(|guests| guests.remove(guest_id))?;
        Ok(0)
    }

    // converts the given guest from init to running
    fn guest_finalize(&mut self, guest_id: u64) -> sbi::Result<u64> {
        let guests = self.guests.as_mut().ok_or(SbiError::InvalidParam)?;
        guests.guest_mut(guest_id).map(|g| {
            let mut temp = GuestState::Temp;
            core::mem::swap(&mut **g, &mut temp);
            let mut running = match temp {
                GuestState::Init(gbr) => GuestState::Running(Vm::new(gbr.create_pages())),
                t => t,
            };
            core::mem::swap(&mut **g, &mut running);
        })?;
        Ok(0)
    }

    fn guest_run(&mut self, guest_id: u64) -> sbi::Result<u64> {
        self.guests
            .as_mut()
            .ok_or(SbiError::InvalidParam)
            .and_then(|guests| guests.running_guest_mut(guest_id))
            .map(|v| v.run(0))?; // TODO take hart id
        Ok(0)
    }

    fn guest_add_page_table_pages(
        &mut self,
        guest_id: u64,
        from_addr: u64,
        num_pages: u64,
    ) -> sbi::Result<u64> {
        let from_page_addr =
            AlignedPageAddr4k::new(PhysAddr::new(from_addr)).ok_or(SbiError::InvalidAddress)?;

        self.guests
            .as_mut()
            .ok_or(SbiError::InvalidParam)
            .and_then(|guests| guests.initializing_guest_mut(guest_id))
            .and_then(|grb| {
                self.vm_pages
                    .add_pte_pages_builder(from_page_addr, num_pages, grb)
                    .map_err(|e| {
                        println!("Salus - pte_pages_builder error {e:?}");
                        SbiError::InvalidAddress
                    })
            })?;

        Ok(0)
    }

    fn guest_rm_pages(&mut self, guest_id: u64, gpa: u64, num_pages: u64) -> sbi::Result<u64> {
        println!("Salus - Rm pages {guest_id:x} gpa:{gpa:x} num_pages:{num_pages}",);
        let from_page_addr =
            AlignedPageAddr4k::new(PhysAddr::new(gpa)).ok_or(SbiError::InvalidAddress)?;

        self.guests
            .as_mut()
            .ok_or(SbiError::InvalidParam)
            .and_then(|guests| guests.running_guest_mut(guest_id))
            .and_then(|g| {
                g.vm_pages
                    .remove_4k_pages(from_page_addr, num_pages)
                    .map_err(|e| {
                        println!("Salus - remove_4k_pages error {e:?}");
                        SbiError::InvalidAddress
                    })
            })
    }

    /// page_type: 0 => 4K, 1=> 2M, 2=> 1G, 3=512G
    fn guest_add_pages(
        &mut self,
        guest_id: u64,
        from_addr: u64,
        page_type: u64,
        num_pages: u64,
        to_addr: u64,
        measure_preserve: bool,
    ) -> sbi::Result<u64> {
        println!(
            "Add pages {from_addr:x} page_type:{page_type} num_pages:{num_pages} to_addr:{to_addr:x}",
        );
        if page_type > 3 {
            // TODO - need to break up mappings if given address that's part of a huge page.
            return Err(SbiError::InvalidParam);
        }

        let from_page_addr =
            AlignedPageAddr4k::new(PhysAddr::new(from_addr)).ok_or(SbiError::InvalidAddress)?;
        let to_page_addr =
            AlignedPageAddr4k::new(PhysAddr::new(to_addr)).ok_or(SbiError::InvalidAddress)?;

        self.guests
            .as_mut()
            .ok_or(SbiError::InvalidParam)
            .and_then(|guests| guests.initializing_guest_mut(guest_id))
            .and_then(|grb| {
                self.vm_pages
                    .add_4k_pages_builder(
                        from_page_addr,
                        num_pages,
                        grb,
                        to_page_addr,
                        measure_preserve,
                    )
                    .map_err(|_| SbiError::InvalidParam)
            })?;

        Ok(num_pages)
    }

    // TODO: Add code to return actual measurements
    fn guest_get_measurement(
        &mut self,
        guest_id: u64,
        measurement_version: u64,
        measurement_type: u64,
        page_addr: u64,
    ) -> sbi::Result<u64> {
        if (measurement_version != 1)
            || (measurement_type != 1)
            || AlignedPageAddr4k::new(PhysAddr::new(page_addr)).is_none()
        {
            return Err(SbiError::InvalidParam);
        }

        let guests = self.guests.as_mut().ok_or(SbiError::InvalidParam)?;
        let _ = guests.get_guest_index(guest_id)?;
        self.execute_with_guest_owned_page(page_addr, |spa| {
            // TODO: Replace this with actual measurement and handle potential failure
            let measurement = 0x55AA_55AAu32.to_le_bytes();
            let _ = spa.write(0, &measurement);
        })
    }

    fn execute_with_guest_owned_page<F>(&mut self, gpa: u64, callback: F) -> sbi::Result<u64>
    where
        F: FnOnce(&mut GuestOwnedPage),
    {
        self.vm_pages
            .execute_with_guest_owned_page(gpa, callback)
            .map_err(|_| SbiError::InvalidAddress)?;
        Ok(0)
    }
}
/// Represents the special VM that serves as the host for the system.
pub struct Host<T: PlatformPageTable, D: DataMeasure> {
    inner: Vm<T, D>,
}

impl<T: PlatformPageTable, D: DataMeasure> Host<T, D> {
    /* TODO
    /// Creates from the system memory pool
    pub fn from_mem_pool(HypMemMap?) -> Self{}
    */

    /// Creates a new `Host` using the given initial page table root.
    /// `guests`: A vec for storing guest info if "nested" guests will be created. Must have
    /// length zero and capacity limits the number of nested guests.
    pub fn new(page_root: HostRootPages<T, D>, pages: SequentialPages<PageSize4k>) -> Self {
        let mut inner = Vm::new(page_root.into_inner());
        inner.add_guest_tracking_pages(pages);
        Self { inner }
    }

    // TODO - also pass the DT here and copy it?
    pub fn add_device_tree(&mut self, dt_addr: u64) {
        self.inner.add_device_tree(dt_addr)
    }

    /// Set the address we should 'sret' to upon entering the VM.
    pub fn set_entry_address(&mut self, entry_addr: u64) {
        self.inner.set_entry_address(entry_addr);
    }

    /// Run the host. Only returns for system shutdown
    //TODO - return value need to be host specific
    pub fn run(&mut self, hart_id: u64) -> Trap {
        self.inner.run(hart_id)
    }
}
