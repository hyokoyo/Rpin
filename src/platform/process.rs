use std::fmt;

use windows::Win32::Foundation::{ERROR_SUCCESS, GetLastError, HANDLE, SetLastError};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    GET_GUI_RESOURCES_FLAGS, GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetGuiResources,
};

use crate::logging::{self, Level};

pub fn log_memory(stage: &str, level: Level) {
    if logging::current_level() < level {
        return;
    }
    match resources() {
        Ok(sample) => logging::write(level, &format!("{stage}: {sample}")),
        Err(err) => logging::warn(format!("{stage}: querying process resources failed: {err}")),
    }
}

pub struct Resources {
    memory: PROCESS_MEMORY_COUNTERS_EX,
    gdi_objects: Option<u32>,
    user_objects: Option<u32>,
}

impl fmt::Display for Resources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "private_bytes={}, working_set_bytes={}, peak_working_set_bytes={}, gdi_objects={:?}, user_objects={:?}",
            self.memory.PrivateUsage,
            self.memory.WorkingSetSize,
            self.memory.PeakWorkingSetSize,
            self.gdi_objects,
            self.user_objects
        )
    }
}

pub fn resources() -> windows::core::Result<Resources> {
    let mut memory = PROCESS_MEMORY_COUNTERS_EX::default();
    let size = std::mem::size_of_val(&memory) as u32;
    memory.cb = size;
    // SAFETY: passing the EX size lets Windows fill the full allocation.
    let process = unsafe { GetCurrentProcess() };
    unsafe {
        GetProcessMemoryInfo(
            process,
            (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            size,
        )?;
    }
    Ok(Resources {
        memory,
        gdi_objects: gui_objects(process, GR_GDIOBJECTS),
        user_objects: gui_objects(process, GR_USEROBJECTS),
    })
}

fn gui_objects(process: HANDLE, flags: GET_GUI_RESOURCES_FLAGS) -> Option<u32> {
    // SAFETY: process is the current process and flags identify a GUI resource class.
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let count = GetGuiResources(process, flags);
        (count != 0 || GetLastError() == ERROR_SUCCESS).then_some(count)
    }
}
