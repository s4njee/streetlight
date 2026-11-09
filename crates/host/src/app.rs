//! Track only the process group launched for a stream, with an explicit exit policy.
use crate::config::App;
use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct RunningApp {
    child: Option<Child>,
    stop_on_disconnect: bool,
    pub id: String,
    pub pid: u32,
}

impl RunningApp {
    pub fn launch(app: &App, output: &str, width: u32, height: u32) -> Result<Option<Self>> {
        let Some(command) = &app.command else {
            return Ok(None);
        };
        let child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .env("STREETLIGHT_OUTPUT", output)
            .env("STREETLIGHT_WIDTH", width.to_string())
            .env("STREETLIGHT_HEIGHT", height.to_string())
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .with_context(|| format!("launch {}", app.name))?;
        let pid = child.id();
        tracing::info!(
            "launched app '{}' process group {pid}, stop_on_disconnect={}",
            app.id,
            app.stop_on_disconnect
        );
        Ok(Some(Self {
            child: Some(child),
            stop_on_disconnect: app.stop_on_disconnect,
            id: app.id.clone(),
            pid,
        }))
    }

    pub fn running(&mut self) -> bool {
        if self.stop_on_disconnect {
            // Preserve the group leader as an unreaped child until group teardown,
            // so its numeric process-group ID cannot be recycled into another app.
            !exited_without_reaping(self.pid)
        } else {
            self.child
                .as_mut()
                .is_some_and(|c| c.try_wait().ok().flatten().is_none())
        }
    }
}

impl Drop for RunningApp {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if self.stop_on_disconnect {
            // The group was created by us; never signal a user-selected PID or app name.
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                if exited_without_reaping(self.pid) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            tracing::info!("stopped app '{}' process group {}", self.id, self.pid);
        } else {
            // Reap our child without keeping the streaming/control task blocked.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

fn exited_without_reaping(pid: u32) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == 0
            && info.assume_init().si_pid() != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app(stop: bool) -> App {
        App {
            id: "test".into(),
            name: "test".into(),
            command: Some("exec sleep 30".into()),
            stop_on_disconnect: stop,
        }
    }
    #[test]
    fn exited_managed_leader_stays_reserved_until_group_cleanup() {
        let mut spec = app(true);
        spec.command = Some("exit 0".into());
        let mut a = RunningApp::launch(&spec, "Virtual-test", 1920, 1080)
            .unwrap()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while a.running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!a.running());
        let pid = a.pid;
        assert_eq!(
            unsafe { libc::kill(pid as i32, 0) },
            0,
            "leader must not be reaped/recycled before group teardown"
        );
        drop(a);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }

    #[test]
    fn stopped_policy_terminates_only_its_owned_child_and_leave_policy_preserves_it() {
        let a = RunningApp::launch(&app(true), "Virtual-test", 1920, 1080)
            .unwrap()
            .unwrap();
        let pid = a.pid;
        drop(a);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        let a = RunningApp::launch(&app(false), "Virtual-test", 1920, 1080)
            .unwrap()
            .unwrap();
        let pid = a.pid;
        drop(a);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
}
