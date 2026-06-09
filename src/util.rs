//! Small process helper: spawn another copy of ourselves, fully detached, so a
//! short-lived hook can start a long-lived broker/daemon that outlives it.

use crate::config;
use std::process::Stdio;

pub fn spawn_detached(args: &[&str]) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "claude-mesh".into());
    let _ = std::fs::create_dir_all(config::base_dir());
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config::log_file())
        .ok();

    let mut cmd = std::process::Command::new(exe);
    cmd.args(args).stdin(Stdio::null());

    match out {
        Some(f) => {
            let f2 = f.try_clone().expect("clone log handle");
            cmd.stdout(Stdio::from(f)).stderr(Stdio::from(f2));
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    // Detach from the parent's process group so it survives the hook exiting.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let _ = cmd.spawn();
}
