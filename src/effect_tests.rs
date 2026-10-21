use super::*;
use crate::test_support::{RecordingHost, unique_temp_path};

fn state() -> AppState {
    let path = unique_temp_path("effect", "checklist", Some("md"));
    std::fs::write(
        &path,
        "# Title\n\n## List\n\n- [ ] Run `make`\n- [ ] Done\n",
    )
    .unwrap();
    AppState::new(crate::parser::parse_document(path).unwrap())
}

fn sync(description: &str) -> PendingSync {
    PendingSync {
        content: String::new(),
        content_hash: [0; 32],
        previous_content: None,
        description: description.to_string(),
        immediate: false,
        base: crate::model::SyncBase::Edited { epoch: 0 },
    }
}

#[test]
fn an_empty_queue_runs_nothing() {
    let mut state = state();
    let drained = drain(&mut state, &mut RecordingHost::default()).unwrap();
    assert_eq!(drained, Drained::default());
}

#[test]
fn every_effect_reaches_the_host_in_order_and_quit_is_reported() {
    let mut state = state();
    state.effects = vec![
        Effect::OpenEditor { line: Some(5) },
        Effect::Suspend,
        Effect::Redraw,
        Effect::OpenLink("https://example.com".to_string()),
        Effect::CancelGitSync,
        Effect::CheckRemote,
        Effect::Quit,
        Effect::GitSync(sync("after quit")),
    ];
    let mut host = RecordingHost::default();
    let drained = drain(&mut state, &mut host).unwrap();
    assert_eq!(
        drained,
        Drained {
            ran_any: true,
            quit: true
        }
    );
    assert_eq!(host.editor_lines, vec![Some(5)]);
    assert_eq!(host.suspends, 1);
    assert_eq!(host.redraws, 1);
    assert_eq!(host.links, vec!["https://example.com".to_string()]);
    assert_eq!(host.cancelled_syncs, 1);
    assert_eq!(host.remote_checks, 1);
    assert_eq!(
        host.syncs
            .iter()
            .map(|s| s.description.as_str())
            .collect::<Vec<_>>(),
        vec!["after quit"],
        "a sync queued alongside quit is still handed over"
    );
    assert!(state.effects.is_empty());
}

#[test]
fn a_copy_runs_against_the_clipboard_and_reports_its_outcome() {
    let mut state = state();
    clipboard::fake::with(|c| {
        c.system_available = true;
        c.system_attempts.clear();
    });
    state.effects.push(Effect::Copy {
        text: "make".to_string(),
        primary: true,
        kind: CopyKind::Command,
    });
    drain(&mut state, &mut RecordingHost::default()).unwrap();
    clipboard::fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("make".to_string(), true)]);
    });
    assert_eq!(
        state.status_message.as_deref(),
        Some("Copied to clipboard: make")
    );
}

/// Queues a copy from inside the host, as a link with no browser does.
struct LinkFallbackHost;

impl Host for LinkFallbackHost {
    fn open_editor(&mut self, _: &mut AppState, _: Option<LineNumber>) -> anyhow::Result<()> {
        anyhow::bail!("editor failed")
    }

    fn suspend(&mut self, _: &mut AppState) -> anyhow::Result<()> {
        Ok(())
    }

    fn redraw(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn open_link(&mut self, state: &mut AppState, url: &str) -> anyhow::Result<()> {
        state.copy_link_instead(url);
        Ok(())
    }

    fn git_sync(&mut self, _: PendingSync) {}

    fn cancel_git_sync(&mut self) -> bool {
        false
    }

    fn check_remote(&mut self) {}
}

#[test]
fn effects_queued_while_draining_run_in_the_same_drain() {
    let mut state = state();
    clipboard::fake::with(|c| c.system_available = true);
    state
        .effects
        .push(Effect::OpenLink("https://example.com".to_string()));
    drain(&mut state, &mut LinkFallbackHost).unwrap();
    assert!(state.effects.is_empty());
    assert_eq!(
        state.status_message.as_deref(),
        Some("No browser here — link copied to clipboard")
    );
}

#[test]
fn a_host_failure_stops_the_drain_and_is_returned() {
    let mut state = state();
    state.effects = vec![Effect::OpenEditor { line: None }, Effect::Quit];
    let err = drain(&mut state, &mut LinkFallbackHost).unwrap_err();
    assert_eq!(err.to_string(), "editor failed");
    assert_eq!(state.effects, vec![Effect::Quit]);
}
