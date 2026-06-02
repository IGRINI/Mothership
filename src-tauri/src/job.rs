//! Tie the sidecar's lifetime to the host's.
//!
//! On Windows, if the host process dies (crash, kill, normal exit) a child it
//! spawned is *not* automatically reaped — and our sidecar holds the database
//! plus the whole provider-adapter subtree. We put the sidecar in a Job Object
//! with `KILL_ON_JOB_CLOSE`: when the host exits and the job handle closes, the
//! OS terminates the sidecar and every process it spawned (adapters inherit the
//! job by default). The host holds one job for its whole life and assigns each
//! spawned sidecar to it.
//!
//! On other platforms this is a no-op for now; the Unix reaper story lives with
//! the process-sandbox work.

pub use imp::JobHandle;

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::mem::size_of;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    /// A Job Object configured to kill all member processes when it closes.
    pub struct JobHandle(HANDLE);

    // A kernel handle is just an opaque value; moving it between threads is safe.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl JobHandle {
        /// Creates a kill-on-close job, or `None` if the OS call fails (we then
        /// run without the safety net rather than refusing to start).
        pub fn create() -> Option<Self> {
            unsafe {
                let job = CreateJobObjectW(None, None).ok()?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .is_err()
                {
                    let _ = CloseHandle(job);
                    return None;
                }
                Some(Self(job))
            }
        }

        /// Adds a running process (by pid) to the job. Best-effort: a failure
        /// just means that process won't be auto-killed with the host.
        pub fn assign(&self, pid: u32) {
            unsafe {
                if let Ok(process) = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)
                {
                    let _ = AssignProcessToJobObject(self.0, process);
                    let _ = CloseHandle(process);
                }
            }
        }
    }

    impl Drop for JobHandle {
        fn drop(&mut self) {
            // Closing the last handle triggers KILL_ON_JOB_CLOSE.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// No-op placeholder; process-tree teardown on Unix/macOS is handled by the
    /// (future) process-sandbox reaper, not here.
    pub struct JobHandle;

    impl JobHandle {
        pub fn create() -> Option<Self> {
            Some(Self)
        }

        pub fn assign(&self, _pid: u32) {}
    }
}
