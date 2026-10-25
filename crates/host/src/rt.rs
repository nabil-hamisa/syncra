//! Process/thread setup for real-time work on Windows.

use windows::core::w;
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Threading::{
    AvSetMmThreadCharacteristicsW, GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

/// Call once at startup, before any DXGI work.
pub fn init_process() {
    unsafe {
        // DuplicateOutput1 requires per-monitor-v2 DPI awareness.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // 1 ms scheduler granularity for sleeps and waits.
        timeBeginPeriod(1);
        let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    }
}

/// Register the calling thread with MMCSS under the given task ("Games", "Pro Audio").
pub fn mmcss_current_thread(task: MmcssTask) {
    let name = match task {
        MmcssTask::Games => w!("Games"),
        MmcssTask::ProAudio => w!("Pro Audio"),
    };
    let mut index = 0u32;
    unsafe {
        if let Err(e) = AvSetMmThreadCharacteristicsW(name, &mut index) {
            tracing::warn!("MMCSS registration failed: {e}");
        }
    }
}

#[allow(dead_code)]
pub enum MmcssTask {
    Games,
    ProAudio,
}
