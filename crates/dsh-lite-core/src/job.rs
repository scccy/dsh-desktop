//! Keeps the sponsored `dsh` process inside a Windows job object.
//!
//! `stop()` alone is not enough: it only runs on the paths we control. If the
//! shell is killed (Task Manager, a crash, a debugger stop), the child would keep
//! running, holding its port and possibly a token. A job object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` ties the whole tree to this process: when
//! our handle closes — for any reason at all — Windows terminates it.
//!
//! The handle is owned by [`DshChild`](crate::launcher::DshChild), so the child
//! lives exactly as long as the shell keeps it.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::process::Child;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
use windows_sys::Win32::System::JobObjects::SetInformationJobObject;
use windows_sys::Win32::System::JobObjects::TerminateJobObject;

/// A job object that owns one child process tree.
///
/// The kernel handle is stored as `isize`: `HANDLE` in `windows-sys` is
/// `*mut c_void`, which is not `Send`, and a `DshChild` has to travel into the
/// shell's global state (the FFI library keeps it behind a `Mutex`).
#[derive(Debug)]
pub struct ChildJob {
    handle: isize,
}

impl ChildJob {
    /// Creates a job that kills everything inside it when the handle closes, and
    /// puts `child` (plus anything it spawns later) inside.
    pub fn attach(child: &Child) -> io::Result<Self> {
        // Safe usage: every call below is checked, and the handle is owned by the
        // returned value (closed in `Drop`) or closed explicitly on failure.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        let job = Self {
            handle: handle as isize,
        };

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job.handle as HANDLE,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }

        // Windows 8 and newer allow nested jobs, so this works even when the
        // shell itself already runs inside one (CI runners, some launchers).
        let assigned = unsafe {
            AssignProcessToJobObject(job.handle as HANDLE, child.as_raw_handle() as HANDLE)
        };
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(job)
    }

    /// Terminates every process in the job immediately.
    ///
    /// Used by `stop()` before the polite shutdown, so a child that ignores the
    /// request cannot take its helpers with it.
    pub fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.handle as HANDLE, 1);
        }
    }
}

impl Drop for ChildJob {
    fn drop(&mut self) {
        // Closing the handle is what triggers KILL_ON_JOB_CLOSE.
        unsafe {
            CloseHandle(self.handle as HANDLE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// A child that outlives its parent unless the job does its job.
    fn spawn_sleeper() -> Child {
        let mut command = Command::new("cmd.exe");
        command
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn().expect("spawn the sleeper")
    }

    #[test]
    fn attaching_succeeds_for_a_normal_child() {
        let mut child = spawn_sleeper();
        let job = ChildJob::attach(&child).expect("attach the job");
        job.terminate();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().expect("poll").is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        panic!("the child survived TerminateJobObject");
    }

    #[test]
    fn dropping_the_job_kills_its_child_without_an_explicit_stop() {
        // This is the crash case: nothing calls stop(), the handle just closes.
        let mut child = spawn_sleeper();
        {
            let _job = ChildJob::attach(&child).expect("attach the job");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().expect("poll").is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        panic!("the child survived the job handle closing");
    }
}
