//! Keep transcription fast on laptops.
//!
//! Windows applies "power throttling" (EcoQoS) to processes it considers background work: no
//! visible window, not in the foreground. Under the *Better battery* power mode that means the
//! threads are pinned to efficiency cores at low clocks, and a 3 s dictation that should take
//! well under a second can take 30 s. Dictate is a tray app, so it is always "background" by
//! that definition even though the user is waiting on it. Opt out explicitly.

use tracing::{debug, warn};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, SetPriorityClass, SetProcessInformation, SetThreadInformation,
    SetThreadPriority, ProcessPowerThrottling, ThreadPowerThrottling, ABOVE_NORMAL_PRIORITY_CLASS,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, THREAD_POWER_THROTTLING_CURRENT_VERSION,
    THREAD_POWER_THROTTLING_EXECUTION_SPEED, THREAD_POWER_THROTTLING_STATE, THREAD_PRIORITY_ABOVE_NORMAL,
};

/// Call once at start-up: disables EcoQoS for the whole process and raises its priority class.
pub fn opt_out_of_power_throttling() {
    unsafe {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0, // control the flag, and set it to "off"
        };
        match SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        ) {
            Ok(()) => debug!("process power throttling (EcoQoS) disabled"),
            Err(e) => warn!(error = %e, "could not disable process power throttling"),
        }
        if let Err(e) = SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS) {
            warn!(error = %e, "could not raise process priority class");
        }
    }
}

/// Call from a latency-critical worker thread (transcription): above-normal priority and no
/// per-thread power throttling.
pub fn boost_current_thread() {
    unsafe {
        let thread = GetCurrentThread();
        let _ = SetThreadPriority(thread, THREAD_PRIORITY_ABOVE_NORMAL);
        let state = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        if let Err(e) = SetThreadInformation(
            thread,
            ThreadPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        ) {
            debug!(error = %e, "could not disable thread power throttling");
        }
    }
}
