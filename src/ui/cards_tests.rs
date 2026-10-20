use super::*;
use crate::model::TextStyle;

fn code_bg(color: Color) -> Style {
    Style::default().bg(color)
}

#[test]
fn code_spans_carry_an_explicit_fg_on_every_color_tier() {
    let item = body_item(
        vec![
            BodySpan::Text("Run ".into()),
            BodySpan::Code("deploy --now".into()),
        ],
        vec!["make all"],
        "Run deploy --now",
    );
    for palette in [
        Palette::truecolor(),
        Palette::palette256(),
        Palette::basic(),
    ] {
        let code_styles: Vec<Style> = body_lines(&item, 40, palette.code)
            .into_iter()
            .flat_map(|l| {
                let line_style = l.style;
                l.spans.into_iter().map(move |s| line_style.patch(s.style))
            })
            .filter(|s| s.bg.is_some())
            .collect();
        assert!(code_styles.len() > 1, "inline chip and fenced box rendered");
        assert!(
            code_styles.iter().all(|s| s.fg.is_some()),
            "{:?}: code left to the theme's fg",
            palette.depth
        );
    }
}

#[test]
fn mono_renders_code_reversed_and_started_dots_as_half_dots() {
    let item = body_item(vec![BodySpan::Code("deploy".into())], vec![], "deploy");
    let mono = Palette::mono();
    let (lines, rows) = body_layout(&item, 40, mono.code);
    assert!(lines.iter().flat_map(|l| &l.spans).any(|s| {
        s.content.contains("deploy") && s.style.add_modifier.contains(Modifier::REVERSED)
    }));
    assert!(copy_texts(&rows).contains(&"deploy"), "still copyable");

    let list = List {
        title: "L".to_string(),
        banner: None,
        items: vec![
            kind_item(ItemKind::Checkbox(TaskState::NotStarted)),
            kind_item(ItemKind::Checkbox(TaskState::Done)),
            kind_item(ItemKind::Checkbox(TaskState::Started)),
        ],
    };
    let glyphs = |pal| -> Vec<String> {
        position_dots(&list, 0, pal)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect()
    };
    assert_eq!(glyphs(mono)[2..4], ["●", "◐"]);
    assert_eq!(glyphs(Palette::basic())[2..4], ["●", "●"]);
}

#[test]
fn wrap_exact_fit_stays_one_line() {
    assert_eq!(wrap_text("ab cd", 5), vec!["ab cd".to_string()]);
}

#[test]
fn wrap_splits_on_word_boundaries() {
    assert_eq!(
        wrap_text("alpha beta gamma", 11),
        vec!["alpha beta".to_string(), "gamma".to_string()]
    );
}

#[test]
fn wrap_hard_splits_overlong_word() {
    assert_eq!(
        wrap_text("abcdefghij", 4),
        vec!["abcd".to_string(), "efgh".to_string(), "ij".to_string()]
    );
}

#[test]
fn wrap_empty_text_yields_single_empty_line() {
    assert_eq!(wrap_text("", 10), vec![String::new()]);
}

/// Renders `block` as a fenced code box at `width` and returns the
/// painted rows, so assertions see real terminal cells — the only level
/// at which a wide-character width bug is visible, since the offending
/// row has the *same* character count either way.
fn code_box_rows(block: &str, width: u16) -> Vec<String> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;

    let lines = code_block_lines(block, width as usize, code_bg(Color::DarkGray), None);
    let mut terminal = Terminal::new(TestBackend::new(width, lines.len() as u16)).unwrap();
    terminal
        .draw(|f| f.render_widget(Paragraph::new(lines), f.area()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(width as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect()
}

#[test]
fn code_block_box_closes_on_rows_with_wide_characters() {
    // Wide characters must not push the closing `│` off the row.
    for block in ["abcde", "日本語テスト", "mixed 日本 text"] {
        let rows = code_box_rows(block, 24);
        for row in &rows {
            assert!(
                row.trim_end().ends_with('│')
                    || row.trim_end().ends_with('┐')
                    || row.trim_end().ends_with('┘'),
                "every box row must close its right edge, got {row:?} for {block:?}"
            );
        }
    }
}

#[test]
fn hard_wrap_breaks_on_display_width_not_character_count() {
    // Six wide characters are 12 cells, so they can't share an 8-cell
    // line the way six ASCII characters can.
    assert_eq!(hard_wrap("abcdefgh", 8), vec!["abcdefgh".to_string()]);
    assert_eq!(
        hard_wrap("日本語テスト", 8),
        vec!["日本語テ".to_string(), "スト".to_string()],
        "four wide characters fill an 8-cell line exactly"
    );
    // A glyph wider than the whole line still makes progress.
    assert_eq!(
        hard_wrap("日本", 1),
        vec!["日".to_string(), "本".to_string()]
    );
    assert_eq!(hard_wrap("", 4), vec![String::new()]);
}

fn body_item(body: Vec<BodySpan>, blocks: Vec<&str>, display: &str) -> Item {
    Item {
        ordinal: None,
        line_number: 1,
        depth: 0,
        section: vec![],
        display_text: display.to_string(),
        body,
        header: None,
        code_spans: vec![],
        code_blocks: blocks.into_iter().map(str::to_string).collect(),
        kind: ItemKind::Checkbox(TaskState::NotStarted),
    }
}

fn line_text(line: &Line) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[test]
fn step_number_leads_the_title_when_there_is_one_else_the_body() {
    let mut item = body_item(vec![], vec![], "take a snapshot");
    item.ordinal = Some("3.".to_string());
    assert_eq!(body_tokens(&item).0[0].0, "3.");
    let side: Vec<String> = side_card_lines(&item, 40).iter().map(line_text).collect();
    assert_eq!(side, vec!["3. take a snapshot"]);
    assert!(card_title_rows(&item, "i", 40).is_empty());

    item.header = Some("Snapshot".to_string());
    assert_eq!(card_title_rows(&item, "i", 40), vec!["i 3. Snapshot"]);
    assert_eq!(body_tokens(&item).0[0].0, "take");
    let side: Vec<String> = side_card_lines(&item, 40).iter().map(line_text).collect();
    assert_eq!(side, vec!["3. Snapshot", "", "take a snapshot"]);
}

fn kind_item(kind: ItemKind) -> Item {
    let mut item = body_item(vec![], vec![], "item");
    item.kind = kind;
    item
}

#[test]
fn accent_color_falls_back_to_note_blue_for_idle_info_cards() {
    let pal = Palette::truecolor();
    let info = kind_item(ItemKind::DisplayOnly);
    // An idle info card's frame takes the note blue (matching its icon)
    // instead of the default, while an active sub-list keeps the state accent.
    assert_eq!(accent_color(&info, None, pal), Some(pal.note));
    assert_eq!(
        accent_color(&info, Some(TaskState::Started), pal),
        Some(pal.started)
    );
    assert_eq!(
        accent_color(&info, Some(TaskState::Done), pal),
        Some(pal.done)
    );
    // Tasks are unaffected: a pending task frame stays default (None).
    let pending = kind_item(ItemKind::Checkbox(TaskState::NotStarted));
    assert_eq!(accent_color(&pending, None, pal), None);
    let done = kind_item(ItemKind::Checkbox(TaskState::Done));
    assert_eq!(accent_color(&done, None, pal), Some(pal.done));
}

#[test]
fn position_dots_tint_info_items_apart_from_pending_tasks() {
    let pal = Palette::truecolor();
    let list = List {
        title: "L".to_string(),
        banner: None,
        items: vec![
            kind_item(ItemKind::DisplayOnly),
            kind_item(ItemKind::Checkbox(TaskState::NotStarted)),
        ],
    };
    // current = the pending task (index 1), so neither dot is the ◉ marker.
    let dots = position_dots(&list, 1, pal);
    let info = &dots.spans[1]; // spans[0] is the leading pad space
    let pending = &dots.spans[2];
    assert_eq!(info.content, "·");
    assert_eq!(info.style.fg, Some(pal.note), "info dot is note-blue");
    assert_eq!(pending.content, "◉", "the pending task is current here");
    // Make the info item non-current so the pending dot shows its dim ·.
    let dots = position_dots(&list, 0, pal);
    let pending = &dots.spans[2];
    assert_eq!(
        pending.style.fg, None,
        "a pending task stays a dim ·, not blue"
    );
}

/// Every (text, background) pair across all lines, for content+style
/// assertions (code is styled with a background).
fn span_pairs(lines: &[Line]) -> Vec<(String, Option<Color>)> {
    lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| (s.content.to_string(), s.style.bg)))
        .collect()
}

#[test]
fn body_lines_style_inline_code_distinctly() {
    let item = body_item(
        vec![
            BodySpan::Text("run".to_string()),
            BodySpan::Code("cmd".to_string()),
        ],
        vec![],
        "run cmd",
    );
    let pairs = span_pairs(&body_lines(&item, 40, code_bg(Color::Magenta)));
    assert!(
        pairs
            .iter()
            .any(|(t, bg)| t.contains("cmd") && *bg == Some(Color::Magenta)),
        "inline code sits on the code background (padded chip)"
    );
    assert!(
        pairs.iter().any(|(t, bg)| t == "run" && bg.is_none()),
        "prose keeps the base style (no code background)"
    );
}

#[test]
fn body_lines_keep_a_paragraph_separator_between_words() {
    let item = body_item(
        vec![
            BodySpan::Text("Restart the service".to_string()),
            BodySpan::Text(" ".to_string()),
            BodySpan::Text("Then check".to_string()),
        ],
        vec![],
        "Restart the service Then check",
    );
    let text: String = body_lines(&item, 40, code_bg(Color::Magenta))
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(text.contains("service Then"), "got {text:?}");
    assert!(!text.contains("serviceThen"), "got {text:?}");
}

/// Every (text, modifiers) pair across all lines, for inline-style
/// assertions (emphasis/strong/strikethrough/link).
fn span_mods(lines: &[Line]) -> Vec<(String, Modifier)> {
    lines
        .iter()
        .flat_map(|l| {
            l.spans
                .iter()
                .map(|s| (s.content.to_string(), s.style.add_modifier))
        })
        .collect()
}

#[test]
fn body_lines_style_emphasis_strong_and_strikethrough() {
    let item = body_item(
        vec![
            BodySpan::Styled {
                text: "italicword".to_string(),
                style: TextStyle {
                    emphasis: true,
                    ..Default::default()
                },
            },
            BodySpan::Text(" plain ".to_string()),
            BodySpan::Styled {
                text: "boldword".to_string(),
                style: TextStyle {
                    strong: true,
                    ..Default::default()
                },
            },
            BodySpan::Text(" ".to_string()),
            BodySpan::Styled {
                text: "crossed".to_string(),
                style: TextStyle {
                    strikethrough: true,
                    ..Default::default()
                },
            },
        ],
        vec![],
        "italicword plain boldword crossed",
    );
    let mods = span_mods(&body_lines(&item, 60, code_bg(Color::Magenta)));
    assert!(
        mods.iter()
            .any(|(t, m)| t == "italicword" && m.contains(Modifier::ITALIC))
    );
    assert!(
        mods.iter()
            .any(|(t, m)| t == "boldword" && m.contains(Modifier::BOLD))
    );
    assert!(
        mods.iter()
            .any(|(t, m)| t == "crossed" && m.contains(Modifier::CROSSED_OUT))
    );
}

#[test]
fn body_lines_render_link_as_underlined_text_without_inline_url() {
    // A single link shows only its underlined text; the URL is not
    // rendered inline (it's in the link panel below the card), and a lone link
    // gets no `[N]` marker.
    let item = body_item(
        vec![
            BodySpan::Text("see ".to_string()),
            BodySpan::Link {
                text: "runbook".to_string(),
                url: "https://example.com/rb".to_string(),
            },
        ],
        vec![],
        "see runbook",
    );
    let lines = body_lines(&item, 60, code_bg(Color::Magenta));
    let whole: String = line_texts(&lines).join(" ");
    assert!(whole.contains("runbook"), "link text shown: {whole:?}");
    assert!(
        !whole.contains("example.com"),
        "URL not shown inline: {whole:?}"
    );
    assert!(!whole.contains('['), "no marker for a lone link: {whole:?}");
    let mods = span_mods(&lines);
    assert!(
        mods.iter()
            .any(|(t, m)| t == "runbook" && m.contains(Modifier::UNDERLINED))
    );
}

#[test]
fn body_lines_tag_multiple_links_with_dim_numbered_markers() {
    // With 2+ links, each link text is followed by a dim `[N]`
    // marker (document order) so it maps to the link panel's numbered list.
    let item = body_item(
        vec![
            BodySpan::Text("see ".to_string()),
            BodySpan::Link {
                text: "runbook".to_string(),
                url: "https://example.com/rb".to_string(),
            },
            BodySpan::Text(" and ".to_string()),
            BodySpan::Link {
                text: "wiki".to_string(),
                url: "https://example.com/wiki".to_string(),
            },
        ],
        vec![],
        "see runbook and wiki",
    );
    let lines = body_lines(&item, 60, code_bg(Color::Magenta));
    let whole: String = line_texts(&lines).join(" ");
    assert!(whole.contains("runbook [1]"), "first marker: {whole:?}");
    assert!(whole.contains("wiki [2]"), "second marker: {whole:?}");
    assert!(!whole.contains("example.com"), "no inline URL: {whole:?}");
    let mods = span_mods(&lines);
    assert!(
        mods.iter()
            .any(|(t, m)| t == "[1]" && m.contains(Modifier::DIM)),
        "marker is dim: {mods:?}"
    );
}

#[test]
fn punctuation_after_a_styled_span_stays_attached() {
    // A comma right after an emphasized word must not get a spurious
    // space (the styled span is a separate run from the following text).
    let item = body_item(
        vec![
            BodySpan::Text("say ".to_string()),
            BodySpan::Styled {
                text: "hi".to_string(),
                style: TextStyle {
                    emphasis: true,
                    ..Default::default()
                },
            },
            BodySpan::Text(", ok".to_string()),
        ],
        vec![],
        "say hi, ok",
    );
    let whole = line_texts(&body_lines(&item, 40, code_bg(Color::Magenta))).join(" ");
    assert!(
        whole.contains("hi,"),
        "comma attaches to the word: {whole:?}"
    );
    assert!(!whole.contains("hi ,"), "no spurious space: {whole:?}");
}

/// Content of each line, concatenated per line.
fn line_texts(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn trailing_inline_command_breaks_to_its_own_line() {
    // A trailing inline code span drops onto its own line, with a
    // blank line between it and the prose above.
    let item = body_item(
        vec![
            BodySpan::Text("run this".to_string()),
            BodySpan::Code("cmd".to_string()),
        ],
        vec![],
        "run this cmd",
    );
    let lines = body_lines(&item, 40, code_bg(Color::Magenta));
    let texts = line_texts(&lines);
    assert_eq!(texts.len(), 3, "prose, blank, command: {texts:?}");
    assert_eq!(texts[0], "run this");
    assert!(texts[1].trim().is_empty(), "blank separator");
    assert!(texts[2].contains("cmd"), "command on its own line");
    // The command line is the styled code chip.
    assert!(
        lines[2]
            .spans
            .iter()
            .any(|s| s.style.bg == Some(Color::Magenta)),
        "command line is a code chip"
    );
}

#[test]
fn inline_code_followed_by_text_stays_inline() {
    // Code that is not the last span keeps flowing — no break.
    let item = body_item(
        vec![
            BodySpan::Text("run".to_string()),
            BodySpan::Code("x".to_string()),
            BodySpan::Text("and wait".to_string()),
        ],
        vec![],
        "run x and wait",
    );
    let lines = body_lines(&item, 40, code_bg(Color::Magenta));
    let texts = line_texts(&lines);
    assert!(
        !texts.iter().any(|t| t.trim().is_empty()),
        "no blank separator; code stays inline: {texts:?}"
    );
    let code_line: String = lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.style.bg == Some(Color::Magenta)))
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .unwrap();
    assert!(
        code_line.contains("run") || code_line.contains("wait"),
        "code shares a line with prose: {code_line:?}"
    );
}

#[test]
fn body_of_only_a_command_has_no_leading_blank() {
    // With nothing above it, the command needs no separator.
    let item = body_item(
        vec![BodySpan::Code("refresh-cache".to_string())],
        vec![],
        "refresh-cache",
    );
    let lines = body_lines(&item, 40, code_bg(Color::Magenta));
    let texts = line_texts(&lines);
    assert_eq!(texts.len(), 1, "just the command line: {texts:?}");
    assert!(texts[0].contains("refresh-cache"));
}

#[test]
fn body_lines_box_fenced_code_left_aligned_with_code_background() {
    let item = body_item(
        vec![BodySpan::Text("do this".to_string())],
        vec!["first line\nsecond line"],
        "do this",
    );
    let lines = body_lines(&item, 40, code_bg(Color::Magenta));
    // Each code line's content appears inside a boxed, code-background,
    // left-aligned line.
    for expected in ["first line", "second line"] {
        let found = lines.iter().any(|line| {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let is_code_bg = line.style.bg == Some(Color::Magenta)
                || (!line.spans.is_empty()
                    && line
                        .spans
                        .iter()
                        .all(|s| s.style.bg == Some(Color::Magenta)));
            text.contains(expected) && line.alignment == Some(Alignment::Left) && is_code_bg
        });
        assert!(found, "boxed code line for {expected:?} not found");
    }
    // The box borders are present.
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        all.contains('┌') && all.contains('└'),
        "box drawn around code"
    );
}

#[test]
fn code_block_box_spans_full_width_even_when_short() {
    // A short one-line block still fills the full card-body width, so
    // every box row (borders, blanks, content) is exactly `width` wide.
    let width = 30usize;
    let lines = code_block_lines("hi", width, code_bg(Color::Magenta), None);
    assert!(lines.len() >= 5, "top + blank + content + blank + bottom");
    for line in &lines {
        let w: usize = line.spans.iter().flat_map(|s| s.content.chars()).count();
        assert_eq!(w, width, "every box row spans the full width");
    }
    // The whole box is painted on the code background.
    assert!(
        lines.iter().all(|l| l.style.bg == Some(Color::Magenta)),
        "background reaches both edges"
    );
}

#[test]
fn narrow_code_block_falls_back_without_a_box() {
    // Below the box minimum the fallback is still plain left-aligned
    // code lines (no border), unchanged by the full-width change.
    let lines = code_block_lines("hello world", 4, code_bg(Color::Magenta), None);
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(!all.contains('┌'), "no box at tiny widths");
}

#[test]
fn short_prose_centered_long_prose_left_aligned() {
    let short = body_item(vec![BodySpan::Text("do it".to_string())], vec![], "do it");
    for line in body_lines(&short, 40, code_bg(Color::Magenta)) {
        if !line.spans.is_empty() {
            assert_eq!(
                line.alignment,
                Some(Alignment::Center),
                "short prose centered"
            );
        }
    }
    let long_text = "word ".repeat(60);
    let long = body_item(vec![BodySpan::Text(long_text.clone())], vec![], &long_text);
    let long_lines = body_lines(&long, 20, code_bg(Color::Magenta));
    assert!(long_lines.len() > PROSE_CENTER_MAX_LINES);
    for line in long_lines {
        assert_eq!(
            line.alignment,
            Some(Alignment::Left),
            "long prose left-aligned"
        );
    }
}

#[test]
fn body_lines_fall_back_to_display_text_when_body_empty() {
    let item = body_item(vec![], vec![], "plain text");
    let pairs = span_pairs(&body_lines(&item, 40, code_bg(Color::Magenta)));
    let text: String = pairs
        .iter()
        .map(|(t, _)| t.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains("plain"));
    assert!(text.contains("text"));
}

#[test]
fn current_card_width_is_center_pct_in_stack_and_full_when_narrow() {
    // In the stack tier the center card takes CENTER_CARD_PCT of
    // the width; below NARROW_THRESHOLD it's the full width.
    assert_eq!(current_card_width(Rect::new(0, 0, 100, 40)), 72); // 100 * 72%
    assert_eq!(current_card_width(Rect::new(0, 0, 120, 40)), 86); // 120 * 72%
    assert_eq!(
        current_card_width(Rect::new(0, 0, 50, 40)),
        50,
        "narrow tier (< NARROW_THRESHOLD) uses the full width"
    );
    assert_eq!(current_card_width(Rect::new(0, 0, 81, 40)), 58); // 81 * 72% = 58
}

#[test]
fn desired_card_height_grows_with_content_and_clamps() {
    let area = Rect::new(0, 0, 100, 40); // max ≈ 24 rows (60%)
    let short = body_item(vec![BodySpan::Text("do it".to_string())], vec![], "do it");
    assert_eq!(
        desired_card_height(&short, area, code_bg(Color::Magenta)),
        MIN_CARD_HEIGHT,
        "short task uses the minimum height"
    );

    let long_text = "word ".repeat(400);
    let long = body_item(vec![BodySpan::Text(long_text.clone())], vec![], &long_text);
    assert_eq!(
        desired_card_height(&long, area, code_bg(Color::Magenta)),
        (area.height * 3 / 5).max(MIN_CARD_HEIGHT),
        "long task clamps to the max height"
    );
}

#[test]
fn top_pad_centers_and_saturates() {
    assert_eq!(top_pad(2, 10), 4); // (10-2)/2
    assert_eq!(top_pad(3, 10), 3); // floor of 3.5
    assert_eq!(top_pad(10, 10), 0); // exact fit
    assert_eq!(top_pad(12, 10), 0); // overflow: no negative pad
}

#[test]
fn wrap_text_measures_wide_glyphs_in_cells() {
    let lines = wrap_text("漢字漢字漢 ab", 4);
    assert_eq!(lines, vec!["漢字", "漢字", "漢", "ab"]);
    assert!(lines.iter().all(|l| display_width(l) <= 4));
}

#[test]
fn breadcrumb_front_truncation_fits_wide_glyphs() {
    let parent = body_item(vec![], vec![], "漢字漢字漢字");
    let mut child = body_item(vec![], vec![], "child");
    child.line_number = 2;
    child.depth = 1;
    let list = List {
        title: "L".to_string(),
        banner: None,
        items: vec![parent, child],
    };
    let line = breadcrumb_line(&list, 0, 1, 6, Palette::truecolor())
        .expect("nested item has a breadcrumb");
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(display_width(&text) <= 6, "fits the width: {text}");
    assert!(text.starts_with('…') && text.ends_with('›'), "{text}");
}

#[test]
fn hard_wrap_splits_ascii_at_the_line_width() {
    assert_eq!(
        hard_wrap("abcdef", 4),
        vec!["abcd".to_string(), "ef".to_string()]
    );
    assert_eq!(hard_wrap("", 4), vec![String::new()]);
}

#[test]
fn wrapped_code_rows_carry_a_continuation_marker_and_real_breaks_do_not() {
    let block = "rm -rf /var/lib/example-app/cache/sessions\nsystemctl restart example-app";
    let rows = code_box_rows(block, 30);
    let content: Vec<&String> = rows
        .iter()
        .filter(|r| r.starts_with('│') && !r.trim_matches(['│', ' ']).is_empty())
        .collect();
    assert_eq!(content.len(), 4, "two lines, each wrapped once: {rows:?}");
    assert!(
        content[0].ends_with("↩│"),
        "first piece continues: {rows:?}"
    );
    assert!(content[1].ends_with(" │"), "end of the real line: {rows:?}");
    assert!(
        content[2].ends_with("↩│"),
        "second line continues: {rows:?}"
    );
    assert!(
        content[3].ends_with(" │"),
        "last row has no marker: {rows:?}"
    );
    assert!(
        !code_box_rows("short\nlines", 30)
            .iter()
            .any(|r| r.contains('↩')),
        "real line breaks render without a marker"
    );
}

#[test]
fn continuation_marker_is_dimmed() {
    let lines = code_block_lines("abcdefghij", 8, code_bg(Color::DarkGray), None);
    let marker = lines
        .iter()
        .flat_map(|l| &l.spans)
        .find(|s| s.content == CONTINUATION)
        .expect("a wrapped row carries the marker");
    assert!(marker.style.add_modifier.contains(Modifier::DIM));
}

#[test]
fn tab_indented_block_keeps_its_indentation_and_a_straight_border() {
    let block = "deploy:\n\tbuild\n\t\tpush";
    let width = 24;
    let lines = code_block_lines(block, width, code_bg(Color::DarkGray), None);
    let texts = line_texts(&lines);
    assert!(texts.iter().all(|t| !t.contains('\t')), "{texts:?}");
    assert!(texts.contains(&format!("│ {:<20} │", "deploy:")));
    assert!(texts.contains(&format!("│ {:<20} │", "    build")));
    assert!(texts.contains(&format!("│ {:<20} │", "        push")));
    for line in &lines {
        assert_eq!(line.width(), width, "ragged right edge: {texts:?}");
    }
}

#[test]
fn tabbed_commands_show_spaces_but_copy_the_real_tabs() {
    let block = "cat <<-EOF\n\tline\nEOF";
    let chip = "printf\tx";
    let item = body_item(
        vec![
            BodySpan::Text("Run".to_string()),
            BodySpan::Code(chip.to_string()),
            BodySpan::Text("first.".to_string()),
        ],
        vec![block],
        "Run it first.",
    );
    let (lines, rows) = body_layout(&item, 40, code_bg(Color::Magenta));
    let texts = line_texts(&lines);
    assert!(texts.iter().all(|t| !t.contains('\t')), "{texts:?}");
    assert!(texts[0].contains(" printf x "), "{texts:?}");
    assert!(
        texts.iter().any(|t| t.starts_with("│     line ")),
        "{texts:?}"
    );
    let copies = copy_texts(&rows);
    assert!(copies.contains(&chip), "{copies:?}");
    assert!(copies.contains(&block), "{copies:?}");
}

#[test]
fn narrow_unboxed_code_still_marks_wrapped_rows() {
    let texts = |w| line_texts(&code_block_lines("abcdefg\nxy", w, Style::default(), None));
    assert_eq!(texts(4), vec!["abc↩", "def↩", "g", "xy"]);
    assert_eq!(texts(1).len(), 9, "no room for a marker at width 1");
    assert!(!texts(1).concat().contains('↩'));
}

#[test]
fn hard_split_trailing_command_marks_its_continued_rows() {
    let cmd = "rm -rf /var/lib/example-app/cache/sessions";
    let item = body_item(
        vec![
            BodySpan::Text("Clear".to_string()),
            BodySpan::Code(cmd.to_string()),
        ],
        vec![],
        "Clear",
    );
    let (lines, rows) = body_layout(&item, 20, code_bg(Color::Magenta));
    let texts = line_texts(&lines);
    let chip: Vec<usize> = (0..lines.len()).filter(|&r| !rows[r].is_empty()).collect();
    assert!(chip.len() >= 3, "the command is split: {texts:?}");
    let (last, continued) = chip.split_last().unwrap();
    for &r in continued {
        assert!(
            texts[r].trim_end().ends_with('↩'),
            "row {r} continues: {texts:?}"
        );
        assert!(lines[r].width() <= 20, "marker fits the row: {texts:?}");
    }
    assert!(
        !texts[*last].contains('↩'),
        "final row has no marker: {texts:?}"
    );
    let joined: String = chip.iter().map(|&r| texts[r].replace('↩', "")).collect();
    assert_eq!(joined.trim(), cmd, "no character lost to the marker");
    for &r in &chip {
        assert!(
            rows[r].iter().all(|(_, t)| &**t == cmd),
            "every row copies the command"
        );
    }
}

// --- Click-to-copy code mapping. `body_layout` returns, per output
// line, the copy targets on it (index-aligned with the lines).

fn copy_texts(rows: &[Vec<CopyTarget>]) -> Vec<&str> {
    rows.iter().flatten().map(|(_, t)| &**t).collect()
}

/// The text copied by a click at `col` on body row `row`, if any.
fn copy_at(rows: &[Vec<CopyTarget>], row: usize, col: u16) -> Option<&str> {
    rows[row]
        .iter()
        .find(|(cols, _)| cols.contains(&col))
        .map(|(_, t)| &**t)
}

#[test]
fn several_inline_commands_get_numbered_markers_for_y() {
    let mut item = body_item(
        vec![
            BodySpan::Text("Run".to_string()),
            BodySpan::Code("stop-svc".to_string()),
            BodySpan::Text("then".to_string()),
            BodySpan::Code("start-svc".to_string()),
        ],
        vec![],
        "Run stop-svc then start-svc",
    );
    item.code_spans = vec!["stop-svc".to_string(), "start-svc".to_string()];
    let whole = line_texts(&body_lines(&item, 60, code_bg(Color::Magenta))).join("\n");
    assert!(whole.contains(" stop-svc [1]"), "{whole:?}");
    assert!(
        whole.contains(" start-svc [2]"),
        "the trailing command is numbered too: {whole:?}"
    );

    // A lone command needs no number.
    item.body.truncate(2);
    item.code_spans.truncate(1);
    let whole = line_texts(&body_lines(&item, 60, code_bg(Color::Magenta))).join("\n");
    assert!(!whole.contains("[1]"), "{whole:?}");
}

#[test]
fn several_fenced_blocks_carry_their_number_in_the_top_border() {
    let mut item = body_item(vec![], vec!["on-primary", "on-replica"], "");
    item.code_spans = vec!["ignored".to_string(), "too".to_string()];
    let rows = line_texts(&body_lines(&item, 30, code_bg(Color::Magenta)));
    let tops: Vec<&String> = rows.iter().filter(|r| r.starts_with('┌')).collect();
    assert_eq!(tops.len(), 2);
    assert!(tops[0].starts_with("┌─[1]─"), "{:?}", tops[0]);
    assert!(tops[1].starts_with("┌─[2]─"), "{:?}", tops[1]);
    assert!(tops.iter().all(|t| t.ends_with('┐')));
    assert_eq!(
        display_width(tops[0]),
        30,
        "the marker doesn't widen the box"
    );
    assert!(
        !rows.iter().any(|r| r.contains("ignored")),
        "inline spans aren't numbered when blocks are the candidates"
    );

    // Too narrow for a box: the number gets its own row above the code.
    let narrow = code_block_lines("x", 4, code_bg(Color::Magenta), Some(2));
    assert_eq!(
        line_texts(&narrow),
        vec!["[2]".to_string(), "x".to_string()]
    );
    // A box too small for its number keeps a plain border.
    let tiny = code_block_lines("x", 6, code_bg(Color::Magenta), Some(10));
    assert_eq!(line_texts(&tiny)[0], "┌────┐");
}

#[test]
fn body_layout_maps_each_chip_on_a_row_to_its_own_command() {
    let item = body_item(
        vec![
            BodySpan::Text("Run".to_string()),
            BodySpan::Code("stop-svc".to_string()),
            BodySpan::Text("then".to_string()),
            BodySpan::Code("start-svc".to_string()),
            BodySpan::Text("now.".to_string()),
        ],
        vec![],
        "Run stop-svc then start-svc now.",
    );
    let width = 60u16;
    let (lines, rows) = body_layout(&item, width, code_bg(Color::Magenta));
    assert_eq!(lines.len(), 1, "fits on one row");
    let rendered: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    let offset = (width as usize / 2).saturating_sub(lines[0].width() / 2);
    let col_of = |needle: &str| (offset + rendered.find(needle).unwrap()) as u16;
    assert_eq!(copy_at(&rows, 0, col_of("stop-svc")), Some("stop-svc"));
    assert_eq!(copy_at(&rows, 0, col_of("start-svc")), Some("start-svc"));
    assert_eq!(
        copy_at(&rows, 0, col_of("then")),
        None,
        "prose has no target"
    );
    assert_eq!(copy_at(&rows, 0, col_of("Run")), None);
}

#[test]
fn body_layout_maps_every_row_of_a_hard_split_chip_to_the_full_command() {
    let cmd = "curl -fsSL https://example.com/install.sh --output /tmp/install.sh";
    let item = body_item(
        vec![
            BodySpan::Text("Fetch".to_string()),
            BodySpan::Code(cmd.to_string()),
            BodySpan::Text("first.".to_string()),
        ],
        vec![],
        "Fetch it first.",
    );
    let (lines, rows) = body_layout(&item, 30, code_bg(Color::Magenta));
    let chip_rows: Vec<usize> = (0..lines.len()).filter(|&r| !rows[r].is_empty()).collect();
    assert!(
        chip_rows.len() >= 3,
        "the chip is split over rows: {rows:?}"
    );
    for r in chip_rows {
        for (cols, text) in &rows[r] {
            assert_eq!(&**text, cmd, "row {r} copies the whole command");
            assert!(cols.end <= 30, "target stays inside the body");
        }
    }
}

#[test]
fn body_layout_lines_and_code_rows_are_aligned() {
    let item = body_item(vec![], vec!["kubectl apply"], "");
    let (lines, code_rows) = body_layout(&item, 40, code_bg(Color::Magenta));
    assert_eq!(lines.len(), code_rows.len());
}

#[test]
fn body_layout_maps_fenced_block_rows_to_block_text() {
    let item = body_item(vec![], vec!["kubectl apply"], "");
    let (_, code_rows) = body_layout(&item, 40, code_bg(Color::Magenta));
    assert!(
        copy_texts(&code_rows).contains(&"kubectl apply"),
        "a fenced-block row copies the clean block text: {code_rows:?}"
    );
}

#[test]
fn body_layout_maps_trailing_command_row() {
    // A trailing inline command drops onto its own row.
    let item = body_item(
        vec![
            BodySpan::Text("run this".to_string()),
            BodySpan::Code("do-it".to_string()),
        ],
        vec![],
        "run this do-it",
    );
    let (_, code_rows) = body_layout(&item, 40, code_bg(Color::Magenta));
    assert!(
        copy_texts(&code_rows).contains(&"do-it"),
        "trailing command row copies the command: {code_rows:?}"
    );
}

#[test]
fn body_layout_maps_inline_code_in_prose() {
    // Code with text after it stays inline (not trailing), so it rides a
    // flowed prose row; that row still copies the span.
    let item = body_item(
        vec![
            BodySpan::Text("first".to_string()),
            BodySpan::Code("cmd".to_string()),
            BodySpan::Text("then done".to_string()),
        ],
        vec![],
        "first cmd then done",
    );
    let (_, code_rows) = body_layout(&item, 40, code_bg(Color::Magenta));
    assert!(
        copy_texts(&code_rows).contains(&"cmd"),
        "inline code row copies the span: {code_rows:?}"
    );
}

#[test]
fn body_layout_prose_rows_carry_no_code() {
    let item = body_item(
        vec![BodySpan::Text("just prose here".to_string())],
        vec![],
        "just prose here",
    );
    let (_, code_rows) = body_layout(&item, 40, code_bg(Color::Magenta));
    assert!(
        code_rows.iter().all(Vec::is_empty),
        "prose-only body has no code rows: {code_rows:?}"
    );
}

fn heading_rows(texts: &[(&str, bool)]) -> Heading {
    Heading {
        title: true,
        banner: vec!["banner one".to_string(), "banner two".to_string()],
        preamble: texts
            .iter()
            .map(|(text, alert)| PreambleRow {
                text: text.to_string(),
                style: Style::default(),
                alert: *alert,
            })
            .collect(),
        breadcrumb: Some("Section".to_string()),
        gap: true,
    }
}

#[test]
fn fit_heading_shrinks_the_card_before_dropping_any_heading_row() {
    let mut heading = heading_rows(&[("prose", false)]);
    assert_eq!(fit_heading(&mut heading, 20, 18, 40), 14);
    assert_eq!(heading.rows(), 6, "everything kept");
    let mut heading = heading_rows(&[("prose", false)]);
    assert_eq!(fit_heading(&mut heading, 40, 10, 40), 10, "room to spare");
}

#[test]
fn fit_heading_drops_the_gap_breadcrumb_title_then_prose_before_alerts() {
    let full = [("alert", true), ("prose", false), ("more prose", false)];
    let mut heading = heading_rows(&full);
    assert_eq!(fit_heading(&mut heading, 14, 12, 40), MIN_CARD_HEIGHT);
    assert!(!heading.gap && heading.breadcrumb.is_none());
    assert!(heading.title, "the title stays while there is room");
    assert_eq!(heading.rows(), 6);
    assert!(heading.preamble[2].text.ends_with("more prose"));

    let mut heading = heading_rows(&full);
    fit_heading(&mut heading, 12, 12, 40);
    assert!(!heading.title);
    let texts: Vec<&str> = heading.preamble.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(texts, ["alert", "prose…"], "the last prose row went first");

    let mut heading = heading_rows(&full);
    fit_heading(&mut heading, 11, 12, 40);
    let texts: Vec<&str> = heading.preamble.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(texts, ["alert…"], "alerts outlast prose");
    assert_eq!(heading.banner.len(), 2);
}

#[test]
fn fit_heading_cuts_the_banner_last() {
    let mut heading = heading_rows(&[("alert", true)]);
    fit_heading(&mut heading, 9, 12, 40);
    assert!(heading.preamble.is_empty());
    assert_eq!(heading.banner, ["banner one…"]);
    let mut heading = heading_rows(&[("alert", true)]);
    assert_eq!(fit_heading(&mut heading, 5, 5, 40), 5);
    assert_eq!(
        heading.rows(),
        0,
        "a terminal shorter than a card keeps only the card"
    );
}

#[test]
fn body_layout_rows_of_one_command_share_its_text() {
    let block = (0..200)
        .map(|i| format!("echo line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let long_cmd = "x".repeat(400);
    let item = body_item(
        vec![
            BodySpan::Text("run".to_string()),
            BodySpan::Code(long_cmd.clone()),
        ],
        vec![&block],
        &format!("run {long_cmd}"),
    );
    let (_, code_rows) = body_layout(&item, 40, code_bg(Color::Magenta));
    let targets: Vec<&Rc<str>> = code_rows.iter().flatten().map(|(_, t)| t).collect();
    let block_rows: Vec<&Rc<str>> = targets.iter().copied().filter(|t| ***t == *block).collect();
    let cmd_rows: Vec<&Rc<str>> = targets
        .iter()
        .copied()
        .filter(|t| ***t == *long_cmd)
        .collect();
    assert!(block_rows.len() >= 200, "every block row is a target");
    assert!(
        cmd_rows.len() > 1,
        "the long command wraps onto several rows"
    );
    assert!(block_rows.iter().all(|t| Rc::ptr_eq(t, block_rows[0])));
    assert!(cmd_rows.iter().all(|t| Rc::ptr_eq(t, cmd_rows[0])));
}
#[test]
fn a_huge_inline_command_lays_out_quickly_and_shares_its_text() {
    let cmd = "y".repeat(200_000);
    let area = Rect::new(0, 0, 120, 40);
    for body in [
        vec![BodySpan::Text("run".into()), BodySpan::Code(cmd.clone())],
        vec![
            BodySpan::Code(cmd.clone()),
            BodySpan::Text("then check".into()),
        ],
    ] {
        let item = body_item(body, vec![], "run");
        let start = std::time::Instant::now();
        let height = desired_card_height(&item, area, code_bg(Color::Magenta));
        let (lines, code_rows) = body_layout(&item, 80, code_bg(Color::Magenta));
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "a 200 KB command took {elapsed:?} to lay out"
        );
        assert_eq!(
            height,
            area.height * 3 / 5,
            "the card clamps to the max height"
        );
        assert!(lines.len() > 2_000, "the command wraps onto many rows");
        let targets: Vec<&Rc<str>> = code_rows.iter().flatten().map(|(_, t)| t).collect();
        assert!(
            targets.len() > 2_000,
            "every row of the command is a target"
        );
        assert_eq!(&**targets[0], cmd.as_str());
        assert!(targets.iter().all(|t| Rc::ptr_eq(t, targets[0])));
    }
}
