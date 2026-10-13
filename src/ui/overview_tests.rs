use super::*;

#[test]
fn item_indent_grows_with_depth() {
    // With a list header, items get a base level plus one per depth.
    assert_eq!(item_indent(0, true), "  ");
    assert_eq!(item_indent(1, true), "    ");
    assert_eq!(item_indent(2, true), "      ");
}

#[test]
fn item_indent_drops_base_without_list_header() {
    // Single-list: no base level, so top-level items sit flush.
    assert_eq!(item_indent(0, false), "");
    assert_eq!(item_indent(1, false), "  ");
    assert_eq!(item_indent(2, false), "    ");
}

fn sub(level: u8, text: &str) -> SubHeading {
    SubHeading {
        level,
        text: text.to_string(),
    }
}

#[test]
fn section_divider_line_truncates_a_long_heading_with_an_ellipsis() {
    let heading = sub(3, "A very long sub-section heading that will not fit");
    let line = section_divider_line(&heading, true, 20);
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.contains('…'), "truncated with an ellipsis: {text:?}");
    assert!(
        !text.contains("will not fit"),
        "the tail of the heading was actually cut: {text:?}"
    );
}

#[test]
fn section_divider_line_leaves_a_short_heading_untouched() {
    let heading = sub(3, "Short");
    let line = section_divider_line(&heading, true, 40);
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.contains("Short"));
    assert!(!text.contains('…'));
}

#[test]
fn new_sub_headings_returns_the_divergent_tail() {
    let outer = vec![sub(3, "A")];
    let inner = vec![sub(3, "A"), sub(4, "B")];
    // Entering the group: everything is new.
    assert_eq!(new_sub_headings(&[], &outer), &outer[..]);
    // Descending into a deeper level: only the new deeper heading.
    assert_eq!(new_sub_headings(&outer, &inner), &inner[1..]);
    // Continuing the same group: nothing new (no divider redrawn).
    assert_eq!(new_sub_headings(&inner, &inner), &[] as &[SubHeading]);
    // Switching to a sibling replaces the tail from the divergence point.
    let sibling = vec![sub(3, "C")];
    assert_eq!(new_sub_headings(&inner, &sibling), &sibling[..]);
    // Leaving the sub-section entirely: no new headings to draw.
    assert_eq!(new_sub_headings(&outer, &[]), &[] as &[SubHeading]);
}
