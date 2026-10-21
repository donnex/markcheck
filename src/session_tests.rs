use super::*;

#[test]
fn a_job_of_a_job_control_shell_can_be_stopped() {
    assert!(stop_takes_effect(200, 100, 100));
}

#[test]
fn the_session_leaders_group_cannot_be_stopped() {
    assert!(!stop_takes_effect(100, 100, 50));
    assert!(!stop_takes_effect(100, 100, 100));
}

#[test]
fn a_parent_in_another_session_cannot_resume_it() {
    assert!(!stop_takes_effect(200, 100, 50));
}

#[test]
fn a_failed_lookup_leaves_the_stop_to_be_tried() {
    assert!(stop_takes_effect(100, -1, 50));
    assert!(stop_takes_effect(200, 100, -1));
}

#[test]
fn the_test_process_reads_its_own_ids() {
    // SAFETY: plain-integer libc calls touching no memory.
    let expected = unsafe {
        stop_takes_effect(
            libc::getpgrp(),
            libc::getsid(0),
            libc::getsid(libc::getppid()),
        )
    };
    assert_eq!(can_stop_for_shell(), expected);
}
