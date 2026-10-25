//! What survives a software restart (not a power cycle): the update job, the crash counter and
//! the "restart into AI mode" flag. Laid out exactly like the C firmware's (at the start of RTC
//! slow memory), so each firmware understands a restart from the other: after an update from
//! one to the other the power stays on, and the job is the same.

use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

/// Every magic the C firmware used here is this value, except the job's.
pub const MAGIC: u32 = 0xB0B0_A1A1;
pub const JOB_MAGIC: u32 = 0x0DA7_E0DA;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Job {
    pub magic: u32,
    pub size: u32,
    pub url: [u8; 192],
    pub sha256: [u8; 65],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rtc {
    /// The C firmware's `Ota::pending` (0x50000000).
    pub job: Job,
    /// `AiRestart`'s `crashes`, `crashMagic` and `restartIntoAi`.
    pub crashes: u32,
    pub crash_magic: u32,
    pub restart_into_ai: u32,
}

#[link_section = ".rtc_noinit"]
static mut RTC: MaybeUninit<Rtc> = MaybeUninit::uninit();

/// Where the C firmware has it; the layout only matches there.
const C_ADDRESS: usize = 0x5000_0000;

pub fn read() -> Rtc {
    // Plain data: any bit pattern is a valid `Rtc`.
    unsafe { addr_of_mut!(RTC).read_volatile().assume_init() }
}

pub fn update(f: impl FnOnce(&mut Rtc)) {
    let mut r = read();
    f(&mut r);
    unsafe { addr_of_mut!(RTC).write_volatile(MaybeUninit::new(r)) };
}

pub fn check_layout() {
    let at = addr_of_mut!(RTC) as usize;
    if at != C_ADDRESS {
        log::error!("rtc: state at {at:#x}, not {C_ADDRESS:#x}: restarts into the C firmware will switch it off");
    }
}
