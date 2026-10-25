//! Own the optional broker and its forked children, including forced teardown.
use crate::{GatewayError, Result};
#[cfg(windows)]
pub struct Tree(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
// The owned kernel job handle has no thread affinity; only its owner closes it.
unsafe impl Send for Tree {}
#[cfg(windows)]
impl Tree {
    pub fn attach(child: &tokio::process::Child) -> Result<Self> {
        use windows_sys::Win32::{Foundation::CloseHandle, System::JobObjects::*};
        let process = child
            .raw_handle()
            .ok_or_else(|| GatewayError::new("PLUGIN_ERROR"))?;
        // The broker receives no launch commands until assignment completes,
        // so every subsequently forked plugin inherits this owned job.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(GatewayError::new("PLUGIN_ERROR"));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(handle, process.cast()) == 0
            {
                CloseHandle(handle);
                return Err(GatewayError::new("PLUGIN_ERROR"));
            }
            Ok(Self(handle))
        }
    }
    pub fn kill(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }
}
#[cfg(windows)]
impl Drop for Tree {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
#[cfg(unix)]
pub struct Tree(i32, std::sync::atomic::AtomicBool);
#[cfg(unix)]
impl Tree {
    pub fn attach(child: &tokio::process::Child) -> Result<Self> {
        Ok(Self(
            child
                .id()
                .ok_or_else(|| GatewayError::new("PLUGIN_ERROR"))? as i32,
            std::sync::atomic::AtomicBool::new(false),
        ))
    }
    pub fn kill(&self) {
        if self.1.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}
#[cfg(unix)]
impl Drop for Tree {
    fn drop(&mut self) {
        self.kill();
    }
}
