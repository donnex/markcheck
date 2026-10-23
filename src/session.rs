//! Starting a child process in a session of its own.

use std::io;
use std::process::{Child, Command};

/// Spawns `cmd` in a new session, which is also a new process group, with no
/// controlling terminal. Signals aimed at markcheck's job, such as the
/// `SIGHUP` of a closed terminal or the interrupt key, don't reach it, and
/// the whole group can be signalled at once.
pub(crate) fn spawn_in_new_session(mut cmd: Command) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    // SAFETY: runs in the forked child before exec, where only
    // async-signal-safe calls are allowed; `setsid` is one and touches no
    // memory shared with the parent.
    unsafe {
        cmd.pre_exec(|| {
            // Fails only for a process group leader, which a freshly forked
            // child never is, so this branch can't be reached in a test.
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Whether a `SIGTSTP` sent to markcheck would stop it. The kernel discards
/// one sent to an orphaned process group: one started by a shell without job
/// control, a terminal or tmux running markcheck itself, or `ssh -t`. See
/// [`stop_takes_effect`].
pub(crate) fn can_stop_for_shell() -> bool {
    use rustix::process::{Pid, getpgrp, getppid, getsid};
    let raw = |sid: rustix::io::Result<Pid>| sid.map_or(-1, Pid::as_raw_pid);
    stop_takes_effect(
        getpgrp().as_raw_pid(),
        raw(getsid(None)),
        getppid().map_or(-1, |parent| raw(getsid(Some(parent)))),
    )
}

/// Whether a process in `group` and `session`, whose parent is in
/// `parent_session`, can be stopped: its group isn't the session leader's,
/// and its parent, there to resume it, shares its session. A failed lookup
/// (`-1`) assumes it can, leaving the stop to be tried.
fn stop_takes_effect(
    group: libc::pid_t,
    session: libc::pid_t,
    parent_session: libc::pid_t,
) -> bool {
    if session == -1 {
        return true;
    }
    group != session && (parent_session == -1 || parent_session == session)
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
