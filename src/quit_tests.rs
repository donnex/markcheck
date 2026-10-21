use super::*;
use crossterm::event::KeyCode;

#[derive(Default)]
struct FakeSync {
    busy: bool,
    flush_starts: bool,
    lands_on_wait: bool,
    dies_on_term: bool,
    landing: Vec<SyncOutcome>,
    unresolved: Option<SyncOutcome>,
    log: Vec<String>,
}

impl SyncWorker for FakeSync {
    fn flush(&mut self) {
        self.log.push("flush".to_string());
        self.busy |= self.flush_starts;
    }

    fn is_busy(&self) -> bool {
        self.busy
    }

    fn abandon(&mut self, force: bool) {
        self.log.push(format!("abandon force={force}"));
        if force || self.dies_on_term {
            self.busy = false;
        }
    }

    fn wait_unless(&mut self, timeout: Duration, give_up: &dyn Fn() -> bool) -> Vec<SyncOutcome> {
        self.log
            .push(format!("wait {}s give_up={}", timeout.as_secs(), give_up()));
        if self.busy && self.lands_on_wait && !give_up() {
            self.busy = false;
            return std::mem::take(&mut self.landing);
        }
        Vec::new()
    }

    fn unresolved(&self) -> Option<&SyncOutcome> {
        self.unresolved.as_ref()
    }
}

fn busy() -> FakeSync {
    FakeSync {
        busy: true,
        ..FakeSync::default()
    }
}

fn state() -> AppState {
    let dir = crate::test_support::unique_temp_path("quit", "state", None);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("list.md");
    std::fs::write(&path, "## A\n\n- [ ] one\n").unwrap();
    AppState::new(crate::parser::parse_document(path).unwrap())
}

const CALM: &dyn Fn() -> bool = &|| false;
const HURRIED: &dyn Fn() -> bool = &|| true;

#[test]
fn abandoning_stops_at_sigterm_when_the_worker_unwinds() {
    let mut sync = FakeSync {
        dies_on_term: true,
        ..busy()
    };
    abandon(&mut sync, CALM);
    assert_eq!(sync.log, ["abandon force=false", "wait 2s give_up=false"]);
}

#[test]
fn abandoning_escalates_to_sigkill_when_sigterm_is_not_enough() {
    let mut sync = busy();
    abandon(&mut sync, CALM);
    assert_eq!(
        sync.log,
        [
            "abandon force=false",
            "wait 2s give_up=false",
            "abandon force=true",
            "wait 2s give_up=false",
        ]
    );
    assert!(!sync.busy);
}

#[test]
fn a_hurried_abandon_goes_straight_to_sigkill() {
    let mut sync = busy();
    abandon(&mut sync, HURRIED);
    assert_eq!(sync.log, ["abandon force=true", "wait 2s give_up=false"]);
}

#[test]
fn finishing_an_idle_sync_reports_the_problem_still_standing() {
    let mut sync = FakeSync {
        unresolved: Some(SyncOutcome::Failed("boom".to_string())),
        ..FakeSync::default()
    };
    let report = finish(&mut sync, CALM);
    assert!(sync.log.is_empty(), "nothing to abandon: {:?}", sync.log);
    assert_eq!(
        report,
        git_sync::exit_report(Some(&SyncOutcome::Failed("boom".to_string())), false)
    );
    assert!(report.unwrap().contains("boom"));
    assert_eq!(finish(&mut FakeSync::default(), CALM), None);
}

#[test]
fn finishing_a_running_sync_abandons_it_first() {
    let mut sync = FakeSync {
        dies_on_term: true,
        ..busy()
    };
    assert_eq!(finish(&mut sync, CALM), None);
    assert_eq!(sync.log[0], "abandon force=false");
}

#[test]
fn settling_waits_for_the_sync_before_abandoning_anything() {
    let mut sync = FakeSync {
        lands_on_wait: true,
        ..busy()
    };
    assert_eq!(settle(&mut sync, CALM), None);
    assert_eq!(sync.log, ["wait 5s give_up=false"]);
}

#[test]
fn stopping_without_the_tui_flushes_and_says_when_it_waits() {
    let mut sync = FakeSync {
        flush_starts: true,
        lands_on_wait: true,
        ..FakeSync::default()
    };
    let mut told = false;
    assert_eq!(stop_without_tui(&mut sync, CALM, || told = true), None);
    assert!(told);
    assert_eq!(sync.log, ["flush", "wait 5s give_up=false"]);

    let mut idle = FakeSync::default();
    let mut told = false;
    stop_without_tui(&mut idle, CALM, || told = true);
    assert!(!told, "nothing to wait for");
}

#[test]
fn stopping_without_the_tui_reports_a_sync_it_had_to_abandon() {
    let mut sync = busy();
    let report = stop_without_tui(&mut sync, HURRIED, || {});
    assert_eq!(
        sync.log,
        [
            "flush",
            "wait 5s give_up=true",
            "abandon force=true",
            "wait 2s give_up=false",
        ]
    );
    assert_eq!(report, None);
}

#[test]
fn q_stops_at_once_without_a_running_sync() {
    let mut state = state();
    let mut quit = Quit::default();
    assert!(quit.begin(None::<&mut FakeSync>, &mut state));
    let mut idle = FakeSync::default();
    assert!(quit.begin(Some(&mut idle), &mut state));
    assert_eq!(idle.log, ["flush"]);
    assert!(!quit.is_waiting());
    assert!(!quit.landed(false));
}

#[test]
fn q_with_a_sync_running_waits_for_it_with_a_notice() {
    let mut state = state();
    let mut quit = Quit::default();
    let mut sync = FakeSync {
        flush_starts: true,
        ..FakeSync::default()
    };
    assert!(!quit.begin(Some(&mut sync), &mut state));
    assert!(quit.is_waiting());
    assert_eq!(state.status_message.as_deref(), Some(WAITING_FOR_SYNC));
    assert!(!state.status_is_error);
    assert!(!quit.landed(true));
    assert!(quit.landed(false));
}

#[test]
fn while_waiting_only_a_second_q_abandons() {
    let mut state = state();
    let quit = Quit { waiting: true };
    state.set_status("something else".to_string());
    assert!(!quit.key(KeyCode::Char('x'), &mut state));
    assert_eq!(state.status_message.as_deref(), Some(WAITING_FOR_SYNC));
    assert!(quit.key(KeyCode::Char('q'), &mut state));
}

#[test]
fn suspending_with_no_sync_running_draws_nothing() {
    let mut state = state();
    let mut sync = FakeSync::default();
    let mut drew = false;
    let running = settle_before_suspend(&mut state, &mut sync, CALM, |_| {
        drew = true;
        Ok(())
    })
    .unwrap();
    assert!(!running);
    assert!(!drew);
    assert_eq!(sync.log, ["flush"]);
}

#[test]
fn suspending_shows_the_wait_then_applies_what_landed() {
    let mut state = state();
    let mut sync = FakeSync {
        flush_starts: true,
        lands_on_wait: true,
        landing: vec![SyncOutcome::Failed("boom".to_string())],
        ..FakeSync::default()
    };
    let mut shown = None;
    let running = settle_before_suspend(&mut state, &mut sync, CALM, |state| {
        shown = state.status_message.clone();
        assert!(state.git_sync.busy);
        assert!(!state.git_sync.waiting);
        Ok(())
    })
    .unwrap();
    assert!(!running);
    assert!(shown.is_some_and(|shown| shown.contains("suspending")));
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("boom"))
    );
}

#[test]
fn suspending_gives_up_on_a_sync_that_keeps_running() {
    let mut state = state();
    let mut sync = busy();
    let running = settle_before_suspend(&mut state, &mut sync, HURRIED, |_| Ok(())).unwrap();
    assert!(running);
    assert_eq!(state.status_message, None, "the wait notice is cleared");
    assert_eq!(sync.log, ["flush", "wait 5s give_up=true"]);
}

#[test]
fn suspending_stops_when_the_wait_cannot_be_drawn() {
    let mut state = state();
    let mut sync = busy();
    let drawn = settle_before_suspend(&mut state, &mut sync, CALM, |_| {
        anyhow::bail!("terminal gone")
    });
    assert_eq!(drawn.unwrap_err().to_string(), "terminal gone");
    assert_eq!(sync.log, ["flush"]);
}

#[test]
fn the_real_worker_answers_the_quit_sequence() {
    let dir = crate::test_support::unique_temp_path("quit", "repo", None);
    std::fs::create_dir_all(&dir).unwrap();
    let status = crate::test_support::git()
        .args(["init", "-q"])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(status.success());
    let file = dir.join("list.md");
    std::fs::write(&file, "- [ ] one\n").unwrap();
    let mut sync = GitSync::detect(&file).unwrap();
    SyncWorker::flush(&mut sync);
    assert!(!SyncWorker::is_busy(&sync));
    assert!(sync.wait_unless(QUIT_WAIT, CALM).is_empty());
    SyncWorker::abandon(&mut sync, false);
    assert_eq!(SyncWorker::unresolved(&sync), None);
    assert_eq!(finish(&mut sync, CALM), None);
}
