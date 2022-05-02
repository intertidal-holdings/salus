// Copyright (c) 2021 by Rivos Inc.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

//! Rust SBI message parsing.
//! `SbiMessage` is an enum of all the SBI extensions.
//! For each extension, a function enum is defined to contain the SBI function data.
#![no_std]

use riscv_regs::{GeneralPurposeRegisters, GprIndex};

const EXT_PUT_CHAR: u64 = 0x01;
const EXT_BASE: u64 = 0x10;
const EXT_HART_STATE: u64 = 0x48534D;
const EXT_RESET: u64 = 0x53525354;
const EXT_TEE: u64 = 0x544545;

/// Error constants from the sbi [spec](https://github.com/riscv-non-isa/riscv-sbi-doc/releases)
pub const SBI_SUCCESS: i64 = 0;
pub const SBI_ERR_FAILED: i64 = -1;
pub const SBI_ERR_NOT_SUPPORTED: i64 = -2;
pub const SBI_ERR_INVALID_PARAM: i64 = -3;
pub const SBI_ERR_DENIED: i64 = -4;
pub const SBI_ERR_INVALID_ADDRESS: i64 = -5;
pub const SBI_ERR_ALREADY_AVAILABLE: i64 = -6;
pub const SBI_ERR_ALREADY_STARTED: i64 = -7;
pub const SBI_ERR_ALREADY_STOPPED: i64 = -8;

/// Errors passed over the SBI protocol
#[derive(Debug)]
pub enum Error {
    InvalidAddress,
    InvalidParam,
    Failed,
    NotSupported,
    UnknownSbiExtension,
}

impl Error {
    /// Parse the given error code to an `Error` enum.
    pub fn from_code(e: i64) -> Self {
        use Error::*;
        match e {
            SBI_ERR_INVALID_ADDRESS => InvalidAddress,
            SBI_ERR_INVALID_PARAM => InvalidParam,
            SBI_ERR_NOT_SUPPORTED => NotSupported,
            _ => Failed,
        }
    }

    /// Convert `Self` to a 64bit error code to be returned over SBI.
    pub fn to_code(&self) -> i64 {
        use Error::*;
        match self {
            InvalidAddress => SBI_ERR_INVALID_ADDRESS,
            InvalidParam => SBI_ERR_INVALID_PARAM,
            Failed => SBI_ERR_FAILED,
            NotSupported => SBI_ERR_NOT_SUPPORTED,
            UnknownSbiExtension => SBI_ERR_INVALID_PARAM,
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

/// Functions defined for the Base extension
pub enum BaseFunction {
    GetSpecificationVersion,
    GetImplementationID,
    GetImplementationVersion,
    GetMachineVendorID,
    GetMachineArchitectureID,
    GetMachineImplementationID,
}

impl BaseFunction {
    fn from_func_id(a6: u64) -> Result<Self> {
        use BaseFunction::*;

        Ok(match a6 {
            0 => GetSpecificationVersion,
            1 => GetImplementationID,
            2 => GetImplementationVersion,
            3 => GetMachineVendorID,
            4 => GetMachineArchitectureID,
            5 => GetMachineImplementationID,
            _ => return Err(Error::InvalidParam),
        })
    }
}

/// Functions defined for the State extension
pub enum StateFunction {
    HartStart,
    HartStop,
    HartStatus,
    HartSuspend,
}

impl StateFunction {
    fn from_func_id(a6: u64) -> Result<Self> {
        use StateFunction::*;

        Ok(match a6 {
            0 => HartStart,
            1 => HartStop,
            2 => HartStatus,
            3 => HartSuspend,
            _ => return Err(Error::InvalidParam),
        })
    }
}

/// Funcions for the Reset extension
#[derive(Copy, Clone)]
pub enum ResetFunction {
    Reset {
        reset_type: ResetType,
        reason: ResetReason,
    },
}

#[derive(Copy, Clone)]
pub enum ResetType {
    Shutdown,
    ColdReset,
    WarmReset,
}

impl ResetType {
    fn from_reg(a0: u64) -> Result<Self> {
        use ResetType::*;
        Ok(match a0 {
            0 => Shutdown,
            1 => ColdReset,
            2 => WarmReset,
            _ => return Err(Error::InvalidParam),
        })
    }
}

#[derive(Copy, Clone)]
pub enum ResetReason {
    NoReason,
    SystemFailure,
}

impl ResetReason {
    fn from_reg(a1: u64) -> Result<Self> {
        use ResetReason::*;
        Ok(match a1 {
            0 => NoReason,
            2 => SystemFailure,
            _ => return Err(Error::InvalidParam),
        })
    }
}

impl ResetFunction {
    pub fn shutdown() -> Self {
        ResetFunction::Reset {
            reset_type: ResetType::Shutdown,
            reason: ResetReason::NoReason,
        }
    }

    fn from_regs(a6: u64, a0: u64, a1: u64) -> Result<Self> {
        use ResetFunction::*;

        Ok(match a6 {
            0 => Reset {
                reset_type: ResetType::from_reg(a0)?,
                reason: ResetReason::from_reg(a1)?,
            },
            _ => return Err(Error::InvalidParam),
        })
    }

    fn get_a0(&self) -> u64 {
        match self {
            ResetFunction::Reset {
                reset_type: _,
                reason,
            } => *reason as u64,
        }
    }

    fn get_a1(&self) -> u64 {
        match self {
            ResetFunction::Reset {
                reset_type,
                reason: _,
            } => *reset_type as u64,
        }
    }
}

#[derive(Copy, Clone)]
pub enum TeeFunction {
    /// Message to create a TVM, contains a u64 address 5 coniguous, 16k-aligned 4k pages.
    /// The first four pages will be used for the top level page table, the fifth for TEE state
    /// tracking.
    /// a6 = 0, a0 = address of pages to use for tracking.
    TvmCreate(u64),
    /// Message to destroy a TVM created with `TvmCreate`.
    /// a6 = 1, a0 = guest id returned from `TvmCreate`.
    TvmDestroy { guest_id: u64 },
    /// Message from the host to add page tables pages to a TVM it created with `TvmCreate`. Pages
    /// must be added to the page table before mappings for more memory can be made. These must be
    /// 4k Pages.
    /// a6 = 2, a0 = guest_id, a1 = address of the first page, and a2 = number of pages
    AddPageTablePages {
        guest_id: u64,
        page_addr: u64,
        num_pages: u64,
    },
    /// Message from the host to add page(s) to a TVM it created with `TvmCreate`.
    /// a6 = 3,
    /// a0 = guest_id,
    /// a1 = address of the first page,
    /// a2 = page_type: 0 => 4k, 1=> 2M, 2=> 1G, 3=512G, Others: reserved
    /// a3 = number of pages
    /// a4 = Guest Address
    /// a4 = if non-zero don't zero pages before passing to the guest(only allowed before starting
    /// the guest, pages will be added to measurement of the guest.)
    AddPages {
        guest_id: u64,
        page_addr: u64,
        page_type: u64,
        num_pages: u64,
        gpa: u64,
        measure_preserve: bool,
    },
    /// Moves a VM from the initializing state to the Runnable state
    /// a6 = 4
    /// a0 = guest id
    Finalize { guest_id: u64 },
    /// Runs the given TVM.
    /// a6 = 5
    /// a0 = guest id
    Run { guest_id: u64 },
    /// Removes pages that were previously added with `AddPages`.
    /// a6 = 6
    /// a0 = guest id,
    /// a1 = guest address to unmap
    /// a2 = address to remap the pages to in the requestor
    /// a3 = number of pages
    RemovePages {
        guest_id: u64,
        gpa: u64,
        remap_addr: u64, // TODO should we track this locally?
        num_pages: u64,
    },
    /// Gets the measurement for the guest and copies it
    ///  the previously configured data transfer page
    /// a6 = 7
    /// a0 = guest id
    /// a1 = measurement version
    /// a2 = measurement type
    /// a3 = page_addr
    GetGuestMeasurement {
        guest_id: u64,
        measurement_version: u64,
        measurement_type: u64,
        page_addr: u64
    },
}

impl TeeFunction {
    // Takes registers a0-6 as the input.
    pub fn from_regs(args: &[u64]) -> Result<Self> {
        use TeeFunction::*;
        match args[6] {
            0 => Ok(TvmCreate(args[0])),
            1 => Ok(TvmDestroy { guest_id: args[0] }),
            2 => Ok(AddPageTablePages {
                guest_id: args[0],
                page_addr: args[1],
                num_pages: args[2],
            }),
            3 => Ok(AddPages {
                guest_id: args[0],
                page_addr: args[1],
                page_type: args[2],
                num_pages: args[3],
                gpa: args[4],
                measure_preserve: args[5] == 0,
            }),
            4 => Ok(Finalize { guest_id: args[0] }),
            5 => Ok(Run { guest_id: args[0] }),
            6 => Ok(RemovePages {
                guest_id: args[0],
                gpa: args[1],
                remap_addr: args[2],
                num_pages: args[3],
            }),
            7 => Ok(GetGuestMeasurement {
                guest_id: args[0],
                measurement_version: args[1],
                measurement_type: args[2],
                page_addr: args[3]
            }),
            _ => Err(Error::InvalidParam),
        }
    }

    pub fn a6(&self) -> u64 {
        use TeeFunction::*;
        match self {
            TvmCreate(_) => 0,
            TvmDestroy { guest_id: _ } => 1,
            AddPageTablePages {
                guest_id: _,
                page_addr: _,
                num_pages: _,
            } => 2,
            AddPages {
                guest_id: _,
                page_addr: _,
                page_type: _,
                num_pages: _,
                gpa: _,
                measure_preserve: _,
            } => 3,
            Finalize { guest_id: _ } => 4,
            Run { guest_id: _ } => 5,
            RemovePages {
                guest_id: _,
                gpa: _,
                remap_addr: _,
                num_pages: _,
            } => 6,
            GetGuestMeasurement {
                guest_id: _,
                measurement_type: _,
                measurement_version: _,
                page_addr: _,
            } => 7,
        }
    }

    pub fn a0(&self) -> u64 {
        use TeeFunction::*;
        match self {
            TvmCreate(page_addr) => *page_addr,
            TvmDestroy { guest_id } => *guest_id,
            AddPageTablePages {
                guest_id,
                page_addr: _,
                num_pages: _,
            } => *guest_id,
            AddPages {
                guest_id,
                page_addr: _,
                page_type: _,
                num_pages: _,
                gpa: _,
                measure_preserve: _,
            } => *guest_id,
            Finalize { guest_id } => *guest_id,
            Run { guest_id } => *guest_id,
            RemovePages {
                guest_id,
                gpa: _,
                remap_addr: _,
                num_pages: _,
            } => *guest_id,
            GetGuestMeasurement {
                guest_id,
                measurement_version: _,
                measurement_type: _,
                page_addr: _,
            } => *guest_id,
        }
    }

    pub fn a1(&self) -> u64 {
        use TeeFunction::*;
        match self {
            AddPageTablePages {
                guest_id: _,
                page_addr,
                num_pages: _,
            } => *page_addr,
            AddPages {
                guest_id: _,
                page_addr,
                page_type: _,
                num_pages: _,
                gpa: _,
                measure_preserve: _,
            } => *page_addr,
            RemovePages {
                guest_id: _,
                gpa,
                remap_addr: _,
                num_pages: _,
            } => *gpa,
            GetGuestMeasurement {
                guest_id: _,
                measurement_version,
                measurement_type: _,
                page_addr:_,
            } => *measurement_version,
            _ => 0,
        }
    }

    pub fn a2(&self) -> u64 {
        use TeeFunction::*;
        match self {
            AddPageTablePages {
                guest_id: _,
                page_addr: _,
                num_pages,
            } => *num_pages,
            AddPages {
                guest_id: _,
                page_addr: _,
                page_type,
                num_pages: _,
                gpa: _,
                measure_preserve: _,
            } => *page_type,
            RemovePages {
                guest_id: _,
                gpa: _,
                remap_addr,
                num_pages: _,
            } => *remap_addr,
            GetGuestMeasurement {
                guest_id: _,
                measurement_version: _,
                measurement_type,
                page_addr:_,
            } => *measurement_type,
            _ => 0,
        }
    }

    pub fn a3(&self) -> u64 {
        use TeeFunction::*;
        match self {
            AddPages {
                guest_id: _,
                page_addr: _,
                page_type: _,
                num_pages,
                gpa: _,
                measure_preserve: _,
            } => *num_pages,
            RemovePages {
                guest_id: _,
                gpa: _,
                remap_addr: _,
                num_pages,
            } => *num_pages,
            GetGuestMeasurement {
                guest_id: _,
                measurement_version: _,
                measurement_type: _,
                page_addr,
            } => *page_addr,
            _ => 0,
        }
    }

    pub fn a4(&self) -> u64 {
        use TeeFunction::*;
        match self {
            AddPages {
                guest_id: _,
                page_addr: _,
                page_type: _,
                num_pages: _,
                gpa,
                measure_preserve: _,
            } => *gpa,
            _ => 0,
        }
    }

    pub fn a5(&self) -> u64 {
        use TeeFunction::*;
        match self {
            AddPages {
                guest_id: _,
                page_addr: _,
                page_type: _,
                num_pages: _,
                gpa: _,
                measure_preserve,
            } => {
                if *measure_preserve {
                    1
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    pub fn result(&self, a0: u64, a1: u64) -> Result<u64> {
        // TODO - Does it need function-specific returns?
        match a0 {
            0 => Ok(a1),
            e => Err(Error::from_code(e as i64)),
        }
    }
}

pub struct SbiReturn {
    pub error_code: i64,
    pub return_value: u64,
}

impl SbiReturn {
    pub fn success(return_value: u64) -> Self {
        Self {
            error_code: SBI_SUCCESS,
            return_value,
        }
    }
}

impl From<Result<u64>> for SbiReturn {
    fn from(result: Result<u64>) -> SbiReturn {
        match result {
            Ok(rv) => Self::success(rv),
            Err(e) => Self::from(e),
        }
    }
}

impl From<Error> for SbiReturn {
    fn from(error: Error) -> SbiReturn {
        SbiReturn {
            error_code: error.to_code(),
            return_value: 0,
        }
    }
}

/// SBI Message used to invoke the specified SBI extension in the firmware.
pub enum SbiMessage {
    Base(BaseFunction),
    PutChar(u64),
    HartState(StateFunction),
    Reset(ResetFunction),
    Tee(TeeFunction),
}

impl SbiMessage {
    /// Creates an SbiMessage struct from the given GPRs. Intended for use from the ECALL handler
    /// and passed the saved register state from the calling OS. A7 must contain a valid SBI
    /// extension and the other A* registers will be interpreted based on the extension A7 selects.
    pub fn from_regs(gprs: &GeneralPurposeRegisters) -> Result<Self> {
        use GprIndex::*;
        match gprs.reg(A7) {
            EXT_PUT_CHAR => Ok(SbiMessage::PutChar(gprs.reg(A0))),
            EXT_BASE => BaseFunction::from_func_id(gprs.reg(A6)).map(SbiMessage::Base),
            EXT_HART_STATE => StateFunction::from_func_id(gprs.reg(A6)).map(SbiMessage::HartState),
            EXT_RESET => ResetFunction::from_regs(gprs.reg(A6), gprs.reg(A0), gprs.reg(A1))
                .map(SbiMessage::Reset),
            EXT_TEE => TeeFunction::from_regs(gprs.a_regs()).map(SbiMessage::Tee),
            _ => Err(Error::UnknownSbiExtension),
        }
    }

    /// Returns the register value for this `SbiMessage`.
    pub fn a7(&self) -> u64 {
        match self {
            SbiMessage::Base(_) => EXT_BASE,
            SbiMessage::PutChar(_) => EXT_PUT_CHAR,
            SbiMessage::HartState(_) => EXT_HART_STATE,
            SbiMessage::Reset(_) => EXT_RESET,
            SbiMessage::Tee(_) => EXT_TEE,
        }
    }

    /// Returns the register value for this `SbiMessage`.
    pub fn a6(&self) -> u64 {
        match self {
            SbiMessage::Base(_) => 0,      //TODO
            SbiMessage::HartState(_) => 0, //TODO
            SbiMessage::PutChar(_) => 0,
            SbiMessage::Reset(_) => 0,
            SbiMessage::Tee(f) => f.a6(),
        }
    }

    /// Returns the register value for this `SbiMessage`.
    pub fn a5(&self) -> u64 {
        match self {
            SbiMessage::Tee(f) => f.a5(),
            _ => 0,
        }
    }

    /// Returns the register value for this `SbiMessage`.
    pub fn a4(&self) -> u64 {
        match self {
            SbiMessage::Tee(f) => f.a4(),
            _ => 0,
        }
    }

    /// Returns the register value for this `SbiMessage`.
    pub fn a3(&self) -> u64 {
        match self {
            SbiMessage::Tee(f) => f.a3(),
            _ => 0,
        }
    }

    /// Returns the register value for this `SbiMessage`.
    pub fn a2(&self) -> u64 {
        match self {
            SbiMessage::Tee(f) => f.a2(),
            _ => 0,
        }
    }

    /// Returns the register value for this `SbiMessage`.
    pub fn a1(&self) -> u64 {
        match self {
            SbiMessage::Reset(r) => r.get_a1(),
            SbiMessage::Tee(f) => f.a1(),
            _ => 0,
        }
    }

    /// Returns the register value for this `SbiMessage`.
    pub fn a0(&self) -> u64 {
        match self {
            SbiMessage::Reset(r) => r.get_a0(),
            SbiMessage::PutChar(c) => *c,
            SbiMessage::Tee(f) => f.a0(),
            _ => 0,
        }
    }

    /// Returns the result returned in the SbiMessage. Intended for use after an SbiMessage has been
    /// handled by the firmware. Interprets the given registers based on the extension and function
    /// and returns the approprate result.
    ///
    /// # Example
    ///
    /// ```rust
    /// pub fn ecall_send(msg: &SbiMessage) -> Result<u64> {
    ///     let mut a0 = msg.a0(); // error code
    ///     let mut a1 = msg.a1(); // return value
    ///     unsafe {
    ///         // Safe, but relies on trusting the hypervisor or firmware.
    ///         asm!("ecall", inout("a0") a0, inout("a1")a1,
    ///                 in("a2")msg.a2(), in("a3") msg.a3(),
    ///                 in("a4")msg.a4(), in("a5") msg.a5(),
    ///                 in("a6")msg.a6(), in("a7") msg.a7());
    ///     }
    ///
    ///     msg.result(a0, a1)
    /// }
    /// ```
    pub fn result(&self, a0: u64, a1: u64) -> Result<u64> {
        match self {
            SbiMessage::Base(_) => {
                if a0 == 0 {
                    Ok(a1)
                } else {
                    Err(Error::InvalidParam) // TODO - set error
                }
            } //TODO
            SbiMessage::HartState(_) => Ok(a1), //TODO
            SbiMessage::PutChar(_) => Ok(0),
            SbiMessage::Reset(_) => Err(Error::InvalidParam),
            SbiMessage::Tee(f) => f.result(a0, a1),
        }
    }
}
