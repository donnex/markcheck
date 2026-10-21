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
