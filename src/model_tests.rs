use super::*;

fn checkbox(line_number: LineNumber, completed: bool) -> Item {
    Item {
        ordinal: None,
        line_number,
        depth: 0,
        section: vec![],
        display_text: "task".to_string(),
        body: vec![],
        header: None,
        code_spans: vec![],
        code_blocks: vec![],
        kind: ItemKind::Checkbox(if completed {
            TaskState::Done
        } else {
            TaskState::NotStarted
        }),
    }
}

fn display_only(line_number: LineNumber) -> Item {
    Item {
        ordinal: None,
        line_number,
        depth: 0,
        section: vec![],
        display_text: "heading text".to_string(),
        body: vec![],
        header: None,
        code_spans: vec![],
        code_blocks: vec![],
        kind: ItemKind::DisplayOnly,
    }
}

#[test]
fn search_text_combines_header_body_and_code_blocks() {
    // search_text folds in the title, the body (display_text),
    // and fenced command blocks — the last aren't part of display_text.
    let item = Item {
        ordinal: None,
        line_number: 1,
        depth: 0,
        section: vec![],
        display_text: "restart the service".to_string(),
        body: vec![],
        header: Some("Reboot".to_string()),
        code_spans: vec![],
        code_blocks: vec!["systemctl restart svc".to_string()],
        kind: ItemKind::Checkbox(TaskState::NotStarted),
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
        ordinal: None,
        line_number,
        depth,
        section: vec![],
        display_text: "item".to_string(),
        body: vec![],
        header: None,
        code_spans: vec![],
        code_blocks: vec![],
        kind,
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
    let mut item = checkbox(line, false);
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
        items: vec![checkbox(1, false), checkbox(2, false)],
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
fn list_checkbox_stats_excludes_display_only_items() {
    let list = List {
        title: "List".to_string(),
        banner: None,
        items: vec![
            display_only(1),
            checkbox(2, true),
            checkbox(3, false),
            checkbox(4, true),
        ],
    };
    assert_eq!(list.checkbox_stats(), (2, 3));
}

#[test]
fn started_tasks_count_as_not_done_in_stats() {
    let mut list = List {
        title: "S".to_string(),
        banner: None,
        items: vec![checkbox(1, true), checkbox(2, false), checkbox(3, false)],
    };
    list.items[1].kind = ItemKind::Checkbox(TaskState::Started);
    // 3 checkboxes total, only the Done one counts as done.
    assert_eq!(list.checkbox_stats(), (1, 3));
}

#[test]
fn checkbox_progress_counts_done_started_total() {
    // The progress bar needs the started count too.
    let mut list = List {
        title: "S".to_string(),
        banner: None,
        items: vec![checkbox(1, true), checkbox(2, false), checkbox(3, false)],
    };
    list.items[1].kind = ItemKind::Checkbox(TaskState::Started);
    assert_eq!(list.checkbox_progress(), (1, 1, 3));
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
        assert_eq!(Palette::detect(choice).depth, depth);
    }
    assert_eq!(ColorChoice::Auto.forced_depth(), None);
    assert_eq!(
        Palette::detect(ColorChoice::Auto).depth,
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
        let code = Palette::for_depth(depth).code;
        assert!(code.fg.is_some(), "{depth:?}: code fg left to the theme");
        assert!(code.bg.is_some(), "{depth:?}: code has no background");
    }
}

#[test]
fn mono_palette_has_no_colors() {
    let p = Palette::for_depth(ColorDepth::Mono);
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
        Palette::for_depth(ColorDepth::TrueColor).code.bg,
        Some(Color::Rgb(..))
    ));
    assert!(matches!(
        Palette::for_depth(ColorDepth::Palette256).code.bg,
        Some(Color::Indexed(_))
    ));
    assert_eq!(
        Palette::for_depth(ColorDepth::Basic).code.bg,
        Some(Color::DarkGray),
        "basic fallback: a subtle gray code background"
    );
    // The error role (missing-title placeholder) resolves per depth.
    assert!(matches!(
        Palette::for_depth(ColorDepth::TrueColor).error,
        Color::Rgb(..)
    ));
    assert!(matches!(
        Palette::for_depth(ColorDepth::Palette256).error,
        Color::Indexed(_)
    ));
    assert_eq!(Palette::for_depth(ColorDepth::Basic).error, Color::Red);
    // The warning role (list banner) resolves per depth.
    assert!(matches!(
        Palette::for_depth(ColorDepth::TrueColor).warning,
        Color::Rgb(..)
    ));
    assert!(matches!(
        Palette::for_depth(ColorDepth::Palette256).warning,
        Color::Indexed(_)
    ));
    assert_eq!(Palette::for_depth(ColorDepth::Basic).warning, Color::Yellow);
}

#[test]
fn for_depth_records_the_depth() {
    assert_eq!(
        Palette::for_depth(ColorDepth::TrueColor).depth,
        ColorDepth::TrueColor
    );
    assert_eq!(
        Palette::for_depth(ColorDepth::Palette256).depth,
        ColorDepth::Palette256
    );
    assert_eq!(
        Palette::for_depth(ColorDepth::Basic).depth,
        ColorDepth::Basic
    );
}

#[test]
fn progress_rgb_hits_the_two_stops() {
    // Yellow at 0% done, green at 100%.
    assert_eq!(progress_rgb_parts(0.0), STARTED_RGB);
    assert_eq!(progress_rgb_parts(1.0), DONE_RGB);
}

#[test]
fn progress_rgb_is_monotonic_between_stops() {
    // Half-way is strictly between yellow and green on the channels that move
    // in one direction (r falls and b falls yellow→green).
    let (r, _g, b) = progress_rgb_parts(0.5);
    assert!(r < STARTED_RGB.0 && r > DONE_RGB.0);
    assert!(b < STARTED_RGB.2 && b > DONE_RGB.2);
}

#[test]
fn progress_rgb_clamps_out_of_range_ratios() {
    assert_eq!(progress_rgb_parts(-1.0), STARTED_RGB);
    assert_eq!(progress_rgb_parts(2.0), DONE_RGB);
}

#[test]
fn truecolor_progress_color_fades_with_completion() {
    let p = Palette::for_depth(ColorDepth::TrueColor);
    let (yr, yg, yb) = STARTED_RGB;
    let (er, eg, eb) = DONE_RGB;
    // Starts at yellow (not gray), ends at green.
    assert_eq!(p.progress_color(0, 4).fg, Some(Color::Rgb(yr, yg, yb)));
    assert_eq!(p.progress_color(4, 4).fg, Some(Color::Rgb(er, eg, eb)));
    // Half done is a blend strictly between the yellow and green stops.
    let (mr, mg, mb) = progress_rgb_parts(0.5);
    assert_eq!(p.progress_color(2, 4).fg, Some(Color::Rgb(mr, mg, mb)));
    assert_ne!((mr, mg, mb), STARTED_RGB);
    assert_ne!((mr, mg, mb), DONE_RGB);
}

#[test]
fn empty_document_counter_reads_complete() {
    // total == 0 is treated as fully done (green), matching the old
    // done == total semantics rather than the 0%-done yellow.
    let p = Palette::for_depth(ColorDepth::TrueColor);
    let (er, eg, eb) = DONE_RGB;
    assert_eq!(p.progress_color(0, 0).fg, Some(Color::Rgb(er, eg, eb)));
}

#[test]
fn basic_progress_color_steps_through_named_colors() {
    let p = Palette::for_depth(ColorDepth::Basic);
    assert_eq!(p.progress_color(0, 4).fg, Some(Color::Yellow)); // was DarkGray
    assert_eq!(p.progress_color(1, 4).fg, Some(Color::Yellow));
    assert_eq!(p.progress_color(2, 4).fg, Some(Color::Green));
    let full = p.progress_color(4, 4);
    assert_eq!(full.fg, Some(Color::Green));
    assert!(full.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn palette256_progress_color_uses_indexed_colors() {
    let p = Palette::for_depth(ColorDepth::Palette256);
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
fn document_checkbox_stats_sums_across_lists() {
    let document = Document {
        file_path: PathBuf::from("test.md"),
        title: None,
        has_default_list: false,
        lists: vec![
            List {
                title: "A".to_string(),
                banner: None,
                items: vec![checkbox(1, true), checkbox(2, false)],
            },
            List {
                title: "B".to_string(),
                banner: None,
                items: vec![display_only(3), checkbox(4, true)],
            },
        ],
        preambles: Default::default(),
        unshown: Default::default(),
        raw_lines: vec![],
    };
    assert_eq!(document.checkbox_stats(), (2, 3));
}

#[test]
fn the_progress_ramp_runs_between_the_truecolor_started_and_done_colors() {
    let palette = Palette::truecolor();
    let (r, g, b) = progress_rgb_parts(0.0);
    assert_eq!(palette.started, Color::Rgb(r, g, b));
    let (r, g, b) = progress_rgb_parts(1.0);
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
