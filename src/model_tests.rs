use super::*;
use crate::test_support::fixture::{checkbox, display_only, document_with_missing_file, item};

#[test]
fn search_text_combines_header_body_and_code_blocks() {
    // search_text folds in the title, the body (display_text),
    // and fenced command blocks — the last aren't part of display_text.
    let item = Item {
        header: Some("Reboot".to_string()),
        code_blocks: vec!["systemctl restart svc".into()],
        ..checkbox(1, "restart the service", false)
    };
    let text = item.search_text();
    assert!(text.contains("Reboot"), "includes the header");
    assert!(text.contains("restart the service"), "includes the body");
    assert!(
        text.contains("systemctl restart svc"),
        "includes code block"
    );
}

fn item_at(line_number: LineNumber, depth: usize, kind: ItemKind) -> Item {
    Item {
        depth,
        ..item(line_number, "item", kind)
    }
}

// An info parent (depth 0) with a two-item checkbox sub-list (depth 1),
// plus a following top-level item that must *not* be treated as a child.
fn info_parent_list(child_a: TaskState, child_b: TaskState) -> List {
    List {
        title: "L".to_string(),
        banner: None,
        items: vec![
            item_at(1, 0, ItemKind::DisplayOnly),
            item_at(2, 1, ItemKind::Checkbox(child_a)),
            item_at(3, 1, ItemKind::Checkbox(child_b)),
            item_at(4, 0, ItemKind::Checkbox(TaskState::NotStarted)),
        ],
    }
}

#[test]
fn descendants_are_the_deeper_run_after_an_item() {
    let list = info_parent_list(TaskState::NotStarted, TaskState::NotStarted);
    // Items 1 and 2 are the parent's sub-list; item 3 (depth 0) is not.
    assert_eq!(list.descendants(0), vec![1, 2]);
    // A leaf checkbox has no descendants; the trailing top-level item too.
    assert_eq!(list.descendants(1), Vec::<usize>::new());
    assert_eq!(list.descendants(3), Vec::<usize>::new());
}

fn item_with_section(line: LineNumber, section: Vec<SubHeading>) -> Item {
    let mut item = checkbox(line, "task", false);
    item.section = section;
    item
}

fn h(level: u8, text: &str) -> SubHeading {
    SubHeading {
        level,
        text: text.to_string(),
    }
}

#[test]
fn sub_section_starts_are_the_divider_positions() {
    // A shallower path that's an ancestor of the previous one is not a new
    // start.
    let list = List {
        title: "L".to_string(),
        banner: None,
        items: vec![
            item_with_section(1, vec![]),          // 0: under H2, no divider
            item_with_section(2, vec![h(3, "A")]), // 1: starts A
            item_with_section(3, vec![h(3, "A")]), // 2: same group
            item_with_section(4, vec![h(3, "A"), h(4, "B")]), // 3: starts B under A
            item_with_section(5, vec![h(3, "A")]), // 4: back to A, no new divider
            item_with_section(6, vec![h(3, "C")]), // 5: starts C
        ],
    };
    assert_eq!(list.sub_section_starts(), vec![1, 3, 5]);

    // A list with no sub-sections has no starts.
    let flat = List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task", false), checkbox(2, "task", false)],
    };
    assert!(flat.sub_section_starts().is_empty());
}

#[test]
fn sublist_slot_numbers_each_sublist_distinctly() {
    // Every parent (item with children) gets its own ordinal, so
    // distinct sub-lists — even at the same depth — get distinct colors.
    let list = List {
        title: "L".to_string(),
        banner: None,
        items: vec![
            item_at(1, 0, ItemKind::Checkbox(TaskState::NotStarted)), // 0: parent A
            item_at(2, 1, ItemKind::Checkbox(TaskState::NotStarted)), // 1: parent (has child)
            item_at(3, 2, ItemKind::Checkbox(TaskState::NotStarted)), // 2: leaf
            item_at(4, 0, ItemKind::Checkbox(TaskState::NotStarted)), // 3: parent B
            item_at(5, 1, ItemKind::Checkbox(TaskState::NotStarted)), // 4: leaf
        ],
    };
    assert_eq!(list.sublist_slot(0), 0, "first parent → slot 0");
    assert_eq!(list.sublist_slot(1), 1, "nested parent → slot 1");
    assert_eq!(list.sublist_slot(3), 2, "second top-level parent → slot 2");
}

#[test]
fn info_parent_state_is_done_only_when_all_children_done() {
    let all_done = info_parent_list(TaskState::Done, TaskState::Done);
    assert_eq!(all_done.info_parent_state(0), Some(TaskState::Done));

    let one_left = info_parent_list(TaskState::Done, TaskState::NotStarted);
    assert_eq!(one_left.info_parent_state(0), Some(TaskState::Started));
}

#[test]
fn info_parent_state_is_started_when_any_child_active() {
    // A single started child is enough — no child-count rule.
    let started = info_parent_list(TaskState::Started, TaskState::NotStarted);
    assert_eq!(started.info_parent_state(0), Some(TaskState::Started));

    // A done child among not-started ones is also "under way".
    let one_done = info_parent_list(TaskState::Done, TaskState::NotStarted);
    assert_eq!(one_done.info_parent_state(0), Some(TaskState::Started));
}

#[test]
fn info_parent_state_is_none_when_nothing_begun_or_no_children() {
    let untouched = info_parent_list(TaskState::NotStarted, TaskState::NotStarted);
    assert_eq!(untouched.info_parent_state(0), None);

    // An info item whose only descendant is another info item — no
    // checkbox children — has no aggregate state.
    let no_tasks = List {
        title: "L".to_string(),
        banner: None,
        items: vec![
            item_at(1, 0, ItemKind::DisplayOnly),
            item_at(2, 1, ItemKind::DisplayOnly),
        ],
    };
    assert_eq!(no_tasks.info_parent_state(0), None);

    // A checkbox item never reports an aggregate state; it carries its own.
    let with_child = info_parent_list(TaskState::Done, TaskState::Done);
    assert_eq!(with_child.info_parent_state(1), None);
}

#[test]
fn info_parent_state_counts_deeper_descendants_too() {
    // A grandchild (depth 2) still counts toward the parent (depth 0).
    let list = List {
        title: "L".to_string(),
        banner: None,
        items: vec![
            item_at(1, 0, ItemKind::DisplayOnly),
            item_at(2, 1, ItemKind::Checkbox(TaskState::Done)),
            item_at(3, 2, ItemKind::Checkbox(TaskState::NotStarted)),
        ],
    };
    assert_eq!(list.info_parent_state(0), Some(TaskState::Started));
}

#[test]
fn item_links_match_the_per_item_scans() {
    use ItemKind::{Checkbox, DisplayOnly};
    use TaskState::{Done, NotStarted, Started};
    // Well-formed nesting plus malformed jumps (0 → 2, a shallower item
    // between a parent and its child) and info parents in every state.
    let shapes = [
        (0, DisplayOnly),
        (1, Checkbox(Done)),
        (2, DisplayOnly),
        (3, Checkbox(Started)),
        (1, Checkbox(NotStarted)),
        (0, Checkbox(NotStarted)),
        (2, DisplayOnly),
        (3, Checkbox(Done)),
        (1, DisplayOnly),
        (0, DisplayOnly),
        (1, Checkbox(NotStarted)),
        (0, DisplayOnly),
        (1, DisplayOnly),
        (0, DisplayOnly),
    ];
    let list = List {
        title: "L".to_string(),
        banner: None,
        items: shapes
            .iter()
            .enumerate()
            .map(|(i, &(depth, kind))| item_at(i + 1, depth, kind))
            .collect(),
    };
    let links = list.item_links();
    assert_eq!(links.len(), list.items.len());
    for (i, link) in links.iter().enumerate() {
        let mut chain = Vec::new();
        let mut up = link.parent;
        while let Some(j) = up {
            chain.push(j);
            up = links[j].parent;
        }
        chain.reverse();
        assert_eq!(chain, list.parent_chain(i), "chain of {i}");
        assert_eq!(link.sublist_slot, list.sublist_slot(i), "slot of {i}");
        assert_eq!(link.info_state, list.info_parent_state(i), "state of {i}");
    }
}

#[test]
fn list_progress_excludes_display_only_items() {
    let list = List {
        title: "List".to_string(),
        banner: None,
        items: vec![
            display_only(1, "heading text"),
            checkbox(2, "task", true),
            checkbox(3, "task", false),
            checkbox(4, "task", true),
        ],
    };
    assert_eq!(
        list.progress(),
        Progress {
            done: 2,
            open: 1,
            ..Progress::default()
        }
    );
}

#[test]
fn started_tasks_count_as_not_done_in_stats() {
    let mut list = List {
        title: "S".to_string(),
        banner: None,
        items: vec![
            checkbox(1, "task", true),
            checkbox(2, "task", false),
            checkbox(3, "task", false),
        ],
    };
    list.items[1].kind = ItemKind::Checkbox(TaskState::Started);
    // 3 checkboxes total, only the Done one counts as done.
    let progress = list.progress();
    assert_eq!((progress.done, progress.total()), (1, 3));
}

#[test]
fn progress_counts_each_state() {
    // The progress bar needs the started count too.
    let mut list = List {
        title: "S".to_string(),
        banner: None,
        items: vec![
            checkbox(1, "task", true),
            checkbox(2, "task", false),
            checkbox(3, "task", false),
        ],
    };
    list.items[1].kind = ItemKind::Checkbox(TaskState::Started);
    list.items.push(checkbox(4, "task", false));
    list.items[3].kind = ItemKind::Checkbox(TaskState::Skipped);
    let progress = list.progress();
    assert_eq!(
        progress,
        Progress {
            done: 1,
            started: 1,
            skipped: 1,
            open: 1,
        }
    );
    assert_eq!(progress.total(), 3, "skipped is outside the total");
    assert_eq!(progress.resolved(), 2);
    assert_eq!(progress.tasks(), 4);
    assert!(!progress.is_finished());
}

#[test]
fn depth_from_env_prefers_truecolor_then_256_then_basic() {
    assert_eq!(
        depth_from_env(None, Some("truecolor"), Some("xterm-256color")),
        ColorDepth::TrueColor
    );
    assert_eq!(
        depth_from_env(None, Some("24bit"), None),
        ColorDepth::TrueColor
    );
    assert_eq!(
        depth_from_env(None, None, Some("xterm-256color")),
        ColorDepth::Palette256
    );
    assert_eq!(depth_from_env(None, None, Some("xterm")), ColorDepth::Basic);
    assert_eq!(depth_from_env(None, None, None), ColorDepth::Basic);
}

#[test]
fn depth_from_env_knows_truecolor_terms_without_colorterm() {
    for term in [
        "xterm-kitty",
        "alacritty",
        "foot",
        "foot-extra",
        "wezterm",
        "contour",
        "xterm-ghostty",
        "xterm-direct",
        "alacritty-direct",
    ] {
        assert_eq!(
            depth_from_env(None, None, Some(term)),
            ColorDepth::TrueColor,
            "TERM={term}"
        );
    }
    assert_eq!(
        depth_from_env(Some("1"), None, Some("xterm-kitty")),
        ColorDepth::Mono
    );
    assert_eq!(
        depth_from_env(None, None, Some("screen")),
        ColorDepth::Basic
    );
}

#[test]
fn palette_detect_honours_a_forced_color_choice() {
    for (choice, depth) in [
        (ColorChoice::TrueColor, ColorDepth::TrueColor),
        (ColorChoice::Palette256, ColorDepth::Palette256),
        (ColorChoice::Basic, ColorDepth::Basic),
    ] {
        assert_eq!(choice.forced_depth(), Some(depth));
        assert_eq!(Palette::detect(choice, Theme::Dark).depth, depth);
        assert_eq!(Palette::detect(choice, Theme::Light).depth, depth);
    }
    assert_eq!(ColorChoice::Auto.forced_depth(), None);
    assert_eq!(
        Palette::detect(ColorChoice::Auto, Theme::Dark).depth,
        detect_color_depth()
    );
}

#[test]
fn depth_from_env_honours_a_non_empty_no_color() {
    assert_eq!(
        depth_from_env(Some("1"), Some("truecolor"), Some("xterm-256color")),
        ColorDepth::Mono
    );
    assert_eq!(
        depth_from_env(Some(""), Some("truecolor"), None),
        ColorDepth::TrueColor,
        "an empty NO_COLOR is ignored, per no-color.org"
    );
}

#[test]
fn code_style_pairs_an_explicit_fg_with_its_bg_on_every_color_tier() {
    for depth in [
        ColorDepth::TrueColor,
        ColorDepth::Palette256,
        ColorDepth::Basic,
    ] {
        let code = Palette::themed(depth, Theme::Dark).code;
        assert!(code.fg.is_some(), "{depth:?}: code fg left to the theme");
        assert!(code.bg.is_some(), "{depth:?}: code has no background");
    }
}

#[test]
fn error_style_is_the_error_color_or_bold_reverse_without_color() {
    for depth in [
        ColorDepth::TrueColor,
        ColorDepth::Palette256,
        ColorDepth::Basic,
    ] {
        let p = Palette::themed(depth, Theme::Dark);
        assert_eq!(p.error_style(), Style::default().fg(p.error), "{depth:?}");
    }
    let mono = Palette::mono().error_style();
    assert_eq!(mono.fg, None);
    assert!(
        mono.add_modifier
            .contains(Modifier::BOLD | Modifier::REVERSED)
    );
}

#[test]
fn mono_palette_has_no_colors() {
    let p = Palette::themed(ColorDepth::Mono, Theme::Dark);
    assert!(p.is_mono());
    for c in [p.done, p.started, p.current, p.note, p.error, p.warning] {
        assert_eq!(c, Color::Reset);
    }
    assert_eq!(p.code.fg, None);
    assert_eq!(p.code.bg, None);
    assert!(p.code.add_modifier.contains(Modifier::REVERSED));
    assert_eq!(p.depth_color(3), Color::Reset);
    assert_eq!(p.progress_color(1, 4), Style::default());
    assert!(p.progress_color(4, 4).add_modifier.contains(Modifier::BOLD));
}

#[test]
fn palette_for_depth_uses_the_matching_color_kind() {
    assert!(matches!(
        Palette::themed(ColorDepth::TrueColor, Theme::Dark).code.bg,
        Some(Color::Rgb(..))
    ));
    assert!(matches!(
        Palette::themed(ColorDepth::Palette256, Theme::Dark).code.bg,
        Some(Color::Indexed(_))
    ));
    assert_eq!(
        Palette::themed(ColorDepth::Basic, Theme::Dark).code.bg,
        Some(Color::DarkGray),
        "basic fallback: a subtle gray code background"
    );
    assert!(matches!(
        Palette::themed(ColorDepth::TrueColor, Theme::Dark).error,
        Color::Rgb(..)
    ));
    assert!(matches!(
        Palette::themed(ColorDepth::Palette256, Theme::Dark).error,
        Color::Indexed(_)
    ));
    assert_eq!(
        Palette::themed(ColorDepth::Basic, Theme::Dark).error,
        Color::Red
    );
    // The warning role (list banner) resolves per depth.
    assert!(matches!(
        Palette::themed(ColorDepth::TrueColor, Theme::Dark).warning,
        Color::Rgb(..)
    ));
    assert!(matches!(
        Palette::themed(ColorDepth::Palette256, Theme::Dark).warning,
        Color::Indexed(_)
    ));
    assert_eq!(
        Palette::themed(ColorDepth::Basic, Theme::Dark).warning,
        Color::Yellow
    );
}

#[test]
fn for_depth_records_the_depth() {
    assert_eq!(
        Palette::themed(ColorDepth::TrueColor, Theme::Dark).depth,
        ColorDepth::TrueColor
    );
    assert_eq!(
        Palette::themed(ColorDepth::Palette256, Theme::Dark).depth,
        ColorDepth::Palette256
    );
    assert_eq!(
        Palette::themed(ColorDepth::Basic, Theme::Dark).depth,
        ColorDepth::Basic
    );
}

#[test]
fn progress_rgb_hits_the_two_stops() {
    // Yellow at 0% done, green at 100%.
    assert_eq!(progress_rgb_parts(STARTED_RGB, DONE_RGB, 0.0), STARTED_RGB);
    assert_eq!(progress_rgb_parts(STARTED_RGB, DONE_RGB, 1.0), DONE_RGB);
}

#[test]
fn progress_rgb_is_monotonic_between_stops() {
    // Half-way is strictly between yellow and green on the channels that move
    // in one direction (r falls and b falls yellow→green).
    let (r, _g, b) = progress_rgb_parts(STARTED_RGB, DONE_RGB, 0.5);
    assert!(r < STARTED_RGB.0 && r > DONE_RGB.0);
    assert!(b < STARTED_RGB.2 && b > DONE_RGB.2);
}

#[test]
fn progress_rgb_clamps_out_of_range_ratios() {
    assert_eq!(progress_rgb_parts(STARTED_RGB, DONE_RGB, -1.0), STARTED_RGB);
    assert_eq!(progress_rgb_parts(STARTED_RGB, DONE_RGB, 2.0), DONE_RGB);
}

#[test]
fn truecolor_progress_color_fades_with_completion() {
    let p = Palette::themed(ColorDepth::TrueColor, Theme::Dark);
    let (yr, yg, yb) = STARTED_RGB;
    let (er, eg, eb) = DONE_RGB;
    // Starts at yellow (not gray), ends at green.
    assert_eq!(p.progress_color(0, 4).fg, Some(Color::Rgb(yr, yg, yb)));
    assert_eq!(p.progress_color(4, 4).fg, Some(Color::Rgb(er, eg, eb)));
    // Half done is a blend strictly between the yellow and green stops.
    let (mr, mg, mb) = progress_rgb_parts(STARTED_RGB, DONE_RGB, 0.5);
    assert_eq!(p.progress_color(2, 4).fg, Some(Color::Rgb(mr, mg, mb)));
    assert_ne!((mr, mg, mb), STARTED_RGB);
    assert_ne!((mr, mg, mb), DONE_RGB);
}

#[test]
fn empty_document_counter_reads_complete() {
    // total == 0 is treated as fully done (green), matching the old
    // done == total semantics rather than the 0%-done yellow.
    let p = Palette::themed(ColorDepth::TrueColor, Theme::Dark);
    let (er, eg, eb) = DONE_RGB;
    assert_eq!(p.progress_color(0, 0).fg, Some(Color::Rgb(er, eg, eb)));
}

#[test]
fn basic_progress_color_steps_through_named_colors() {
    let p = Palette::themed(ColorDepth::Basic, Theme::Dark);
    assert_eq!(p.progress_color(0, 4).fg, Some(Color::Yellow));
    assert_eq!(p.progress_color(1, 4).fg, Some(Color::Yellow));
    assert_eq!(p.progress_color(2, 4).fg, Some(Color::Green));
    let full = p.progress_color(4, 4);
    assert_eq!(full.fg, Some(Color::Green));
    assert!(full.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn palette256_progress_color_uses_indexed_colors() {
    let p = Palette::themed(ColorDepth::Palette256, Theme::Dark);
    for (done, total) in [(0, 4), (2, 4), (4, 4)] {
        assert!(matches!(
            p.progress_color(done, total).fg,
            Some(Color::Indexed(_))
        ));
    }
}

#[test]
fn rgb_to_256_maps_primaries_and_extremes() {
    assert_eq!(rgb_to_256(0, 0, 0), 16); // cube origin (black)
    assert_eq!(rgb_to_256(255, 255, 255), 231); // cube far corner (white)
    assert_eq!(rgb_to_256(255, 0, 0), 196); // pure red cube cell
    // A near-gray triple snaps into the 232–255 grayscale ramp.
    assert!(rgb_to_256(0x80, 0x80, 0x80) >= 232);
}

#[test]
fn document_progress_sums_across_lists() {
    let document = document_with_missing_file(vec![
        List {
            title: "A".to_string(),
            banner: None,
            items: vec![checkbox(1, "task", true), checkbox(2, "task", false)],
        },
        List {
            title: "B".to_string(),
            banner: None,
            items: vec![display_only(3, "heading text"), checkbox(4, "task", true)],
        },
    ]);
    assert_eq!(
        document.progress(),
        Progress {
            done: 2,
            open: 1,
            ..Progress::default()
        }
    );
}

#[test]
fn the_progress_ramp_runs_between_the_truecolor_started_and_done_colors() {
    let palette = Palette::truecolor();
    let (r, g, b) = progress_rgb_parts(STARTED_RGB, DONE_RGB, 0.0);
    assert_eq!(palette.started, Color::Rgb(r, g, b));
    let (r, g, b) = progress_rgb_parts(STARTED_RGB, DONE_RGB, 1.0);
    assert_eq!(palette.done, Color::Rgb(r, g, b));
}

#[test]
fn auto_copies_to_both_clipboards_over_ssh_only() {
    assert_eq!(
        ClipboardChoice::Auto.route(false),
        CopyRoute::SystemThenOsc52
    );
    assert_eq!(ClipboardChoice::Auto.route(true), CopyRoute::Both);
    assert_eq!(ClipboardChoice::System.route(true), CopyRoute::SystemOnly);
    assert_eq!(ClipboardChoice::Osc52.route(false), CopyRoute::Osc52Only);
}

#[test]
fn progress_is_finished_once_every_task_is_resolved() {
    assert!(!Progress::default().is_finished(), "no tasks at all");
    let skipped_only = Progress {
        skipped: 2,
        ..Progress::default()
    };
    assert!(skipped_only.is_finished());
    let one_started = Progress {
        done: 3,
        started: 1,
        ..Progress::default()
    };
    assert!(!one_started.is_finished());
}

const ALL_STATES: [TaskState; 4] = [
    TaskState::NotStarted,
    TaskState::Started,
    TaskState::Done,
    TaskState::Skipped,
];

#[test]
fn toggle_goes_to_the_target_or_back_to_not_started() {
    for from in ALL_STATES {
        for target in [TaskState::Started, TaskState::Done, TaskState::Skipped] {
            let expected = if from == target {
                TaskState::NotStarted
            } else {
                target
            };
            assert_eq!(from.toggle(target), expected, "{from:?} toward {target:?}");
        }
    }
}

#[test]
fn toggle_message_names_the_state_entered_or_left() {
    let cases = [
        (TaskState::NotStarted, TaskState::Done, "Marked done"),
        (TaskState::Started, TaskState::Done, "Marked done"),
        (TaskState::Done, TaskState::NotStarted, "Marked not done"),
        (TaskState::NotStarted, TaskState::Started, "Marked started"),
        (
            TaskState::Started,
            TaskState::NotStarted,
            "Marked not started",
        ),
        (TaskState::Done, TaskState::Skipped, "Marked skipped"),
        (
            TaskState::Skipped,
            TaskState::NotStarted,
            "Marked not skipped",
        ),
    ];
    for (from, to, message) in cases {
        assert_eq!(from.toggle_message(to), message, "{from:?} to {to:?}");
    }
}

#[test]
fn every_state_has_a_name_and_a_commit_verb() {
    let named: Vec<_> = ALL_STATES
        .iter()
        .map(|state| (state.marked(), state.commit_verb()))
        .collect();
    assert_eq!(
        named,
        [
            ("not done", "Uncheck"),
            ("started", "Start"),
            ("done", "Check"),
            ("skipped", "Skip"),
        ]
    );
}

#[test]
fn auto_icons_are_ascii_only_on_the_linux_console() {
    let done = |choice: IconChoice, term| choice.icon_set(term).done;
    assert_eq!(done(IconChoice::Auto, Some("linux")), IconSet::ascii().done);
    for term in [None, Some("xterm-256color"), Some("screen"), Some("")] {
        assert_eq!(
            done(IconChoice::Auto, term),
            IconSet::unicode().done,
            "{term:?}"
        );
    }
    for term in [None, Some("linux")] {
        assert_eq!(done(IconChoice::Unicode, term), IconSet::unicode().done);
        assert_eq!(done(IconChoice::Nerd, term), IconSet::nerd().done);
        assert_eq!(done(IconChoice::Ascii, term), IconSet::ascii().done);
    }
}

#[test]
fn the_deprecated_nerd_font_key_maps_onto_an_icon_choice() {
    assert_eq!(IconChoice::from_nerd_font(true), IconChoice::Nerd);
    assert_eq!(IconChoice::from_nerd_font(false), IconChoice::Auto);
}

#[test]
fn the_ascii_set_is_plain_ascii_with_console_box_lines() {
    let set = IconSet::ascii();
    let glyphs = [
        set.done,
        set.pending,
        set.started,
        set.skipped,
        set.current,
        set.note,
        set.file,
        set.list,
        set.update,
        set.sync,
        set.dot_current,
        set.dot_done,
        set.dot_started,
        set.dot_started_mono,
        set.dot_skipped,
        set.dot_pending,
        set.scroll_thumb,
        set.scroll_up,
        set.scroll_down,
        set.bar_filled,
        set.bar_empty,
        set.continuation,
        set.crumb,
    ];
    for glyph in glyphs {
        assert!(glyph.is_ascii(), "{glyph:?} is not ASCII");
    }
    // The heavy and rounded lines aren't in a console font; light and
    // double ones are.
    assert_eq!(set.task_border, BorderType::Double);
    assert_eq!(set.note_border, BorderType::Plain);
}

#[test]
fn every_icon_set_keeps_its_markers_one_width_and_its_chrome_one_cell() {
    for set in [IconSet::unicode(), IconSet::nerd(), IconSet::ascii()] {
        let width = |glyph: &str| glyph.chars().count();
        let marker = width(set.pending);
        for glyph in [set.done, set.started, set.skipped] {
            assert_eq!(width(glyph), marker, "{glyph:?} next to {:?}", set.pending);
        }
        for glyph in [set.current, set.note] {
            assert!(
                width(glyph) <= marker,
                "{glyph:?} wider than {:?}",
                set.pending
            );
        }
        for glyph in [
            set.dot_current,
            set.dot_done,
            set.dot_started,
            set.dot_started_mono,
            set.dot_skipped,
            set.dot_pending,
            set.scroll_thumb,
            set.scroll_up,
            set.scroll_down,
            set.bar_filled,
            set.bar_empty,
            set.continuation,
        ] {
            assert_eq!(width(glyph), 1, "{glyph:?} must fill one cell");
        }
    }
}

/// The sRGB value of a truecolor or xterm-256 color (cube or gray ramp).
fn color_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(i) if i >= 232 => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
        Color::Indexed(i) if i >= 16 => {
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let i = i - 16;
            (
                LEVELS[(i / 36) as usize],
                LEVELS[(i / 6 % 6) as usize],
                LEVELS[(i % 6) as usize],
            )
        }
        other => panic!("no fixed RGB for {other:?}"),
    }
}

/// WCAG 2 contrast ratio between two sRGB colors.
fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let luminance = |(r, g, b): (u8, u8, u8)| {
        let channel = |c: u8| {
            let c = c as f64 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    };
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[test]
fn the_contrast_helper_matches_known_ratios() {
    assert!((contrast((0, 0, 0), (255, 255, 255)) - 21.0).abs() < 0.01);
    assert!((contrast((255, 255, 255), (255, 255, 255)) - 1.0).abs() < 0.01);
    assert_eq!(color_rgb(Color::Indexed(231)), (255, 255, 255));
    assert_eq!(color_rgb(Color::Indexed(244)), (128, 128, 128));
}

#[test]
fn every_light_palette_accent_reaches_4_5_to_1_on_white() {
    const WHITE: (u8, u8, u8) = (255, 255, 255);
    for depth in [ColorDepth::TrueColor, ColorDepth::Palette256] {
        let p = Palette::themed(depth, Theme::Light);
        assert!(p.is_light());
        let mut accents = vec![
            ("done", p.done),
            ("started", p.started),
            ("current", p.current),
            ("note", p.note),
            ("error", p.error),
            ("warning", p.warning),
        ];
        accents.extend((0..4).map(|slot| ("depth guide", p.depth_color(slot))));
        accents.extend((0..=10).filter_map(|done| {
            let fg = p.progress_color(done, 10).fg?;
            Some(("progress", fg))
        }));
        for (role, color) in accents {
            let ratio = contrast(color_rgb(color), WHITE);
            assert!(
                ratio >= 4.5,
                "{depth:?} {role} {color:?} is only {ratio:.2}:1 on white"
            );
        }
        let pill = contrast(color_rgb(p.on_current), color_rgb(p.current));
        assert!(pill >= 4.5, "{depth:?} tab pill is only {pill:.2}:1");
    }
}

#[test]
fn the_light_theme_only_changes_the_truecolor_and_256_palettes() {
    for depth in [ColorDepth::Basic, ColorDepth::Mono] {
        let dark = Palette::themed(depth, Theme::Dark);
        let light = Palette::themed(depth, Theme::Light);
        assert!(!light.is_light());
        assert_eq!(
            (light.done, light.started, light.current, light.note),
            (dark.done, dark.started, dark.current, dark.note)
        );
        assert_eq!(light.depth_color(1), dark.depth_color(1));
        assert_eq!(light.progress_color(1, 2), dark.progress_color(1, 2));
    }
    for depth in [ColorDepth::TrueColor, ColorDepth::Palette256] {
        let dark = Palette::themed(depth, Theme::Dark);
        let light = Palette::themed(depth, Theme::Light);
        assert_eq!(light.depth, depth);
        assert_ne!(light.done, dark.done);
        assert_eq!(light.code, dark.code, "the dark code chip reads on both");
    }
}

#[test]
fn the_light_256_progress_counter_steps_from_started_to_done() {
    let p = Palette::palette256_light();
    assert_eq!(p.progress_color(1, 4).fg, Some(p.started));
    assert_eq!(p.progress_color(2, 4).fg, Some(p.done));
    let full = p.progress_color(4, 4);
    assert_eq!(full.fg, Some(p.done));
    assert!(full.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn the_light_truecolor_progress_ramp_runs_between_its_own_stops() {
    let p = Palette::truecolor_light();
    assert_eq!(p.progress_color(0, 4).fg, Some(p.started));
    assert_eq!(p.progress_color(4, 4).fg, Some(p.done));
    assert_eq!(rgb(LIGHT_STARTED_RGB), p.started);
}

#[test]
fn the_theme_defaults_to_dark() {
    assert_eq!(Theme::default(), Theme::Dark);
}

#[test]
fn the_multiplexer_comes_from_tmux_then_a_screen_term() {
    let detect = Multiplexer::detect;
    assert_eq!(
        detect(Some("/tmp/tmux-1000/default,1,0"), Some("screen")),
        Multiplexer::Tmux
    );
    assert_eq!(detect(Some("x"), None), Multiplexer::Tmux);
    assert_eq!(
        detect(None, Some("screen.xterm-256color")),
        Multiplexer::Screen
    );
    assert_eq!(detect(Some(""), Some("screen")), Multiplexer::Screen);
    assert_eq!(detect(None, Some("xterm-256color")), Multiplexer::None);
    assert_eq!(detect(None, None), Multiplexer::None);
}

fn block(text: &str, kind: CodeKind) -> CodeBlock {
    CodeBlock {
        text: text.to_string(),
        kind,
    }
}

#[test]
fn a_fence_info_string_names_its_code_kind_by_its_first_word() {
    for info in [
        "text",
        "output",
        "plaintext",
        "TEXT",
        "text {.wide}",
        "output,raw",
    ] {
        assert_eq!(CodeKind::from_info(info), CodeKind::Output, "{info}");
    }
    for info in ["console", "shell-session", "Console title=run"] {
        assert_eq!(CodeKind::from_info(info), CodeKind::Console, "{info}");
    }
    for info in ["", "sh", "bash", "textile", "shell"] {
        assert_eq!(CodeKind::from_info(info), CodeKind::Command, "{info}");
    }
}

#[test]
fn only_a_command_or_a_transcript_with_a_prompt_has_copy_text() {
    assert_eq!(
        block("make build\nmake test", CodeKind::Command).copy_text(),
        Some("make build\nmake test".to_string())
    );
    assert_eq!(block("Active: running", CodeKind::Output).copy_text(), None);
    assert_eq!(
        block(
            "$ example-cli migrate\nApplied 3 migrations.\n# example-cli restart\n$ \n$x",
            CodeKind::Console
        )
        .copy_text(),
        Some("example-cli migrate\nexample-cli restart".to_string()),
        "prompt lines without their prompt; an empty prompt and `$x` are output"
    );
    assert_eq!(
        block("Applied 3 migrations.", CodeKind::Console).copy_text(),
        None
    );
}

#[test]
fn a_block_from_a_string_is_a_command_and_compares_with_it() {
    let command = CodeBlock::from("make");
    assert_eq!(command.kind, CodeKind::Command);
    assert_eq!(command, "make".to_string());
    assert_ne!(block("make", CodeKind::Output), "make".to_string());
}

#[test]
fn expected_output_alone_is_not_a_command_block() {
    let mut item = checkbox(1, "check", false);
    assert!(!item.has_command_blocks());
    item.code_blocks = vec![block("Active: running", CodeKind::Output)];
    assert!(!item.has_command_blocks());
    item.code_blocks.push("systemctl status".into());
    assert!(item.has_command_blocks());
}
