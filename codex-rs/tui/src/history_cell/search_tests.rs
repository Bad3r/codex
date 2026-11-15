use super::*;
use pretty_assertions::assert_eq;

#[test]
fn web_action_labels_and_missing_details() {
    let url = "https://example.com/docs";
    let actions = [
        WebSearchAction::OpenPage {
            url: Some(url.into()),
        },
        WebSearchAction::OpenPage { url: None },
        WebSearchAction::FindInPage {
            url: Some(url.into()),
            pattern: Some("needle".into()),
        },
        WebSearchAction::FindInPage {
            url: None,
            pattern: Some("needle".into()),
        },
        WebSearchAction::FindInPage {
            url: Some(url.into()),
            pattern: Some(String::new()),
        },
        WebSearchAction::FindInPage {
            url: None,
            pattern: None,
        },
    ];
    let rendered = actions
        .into_iter()
        .flat_map(|action| {
            new_web_search_call("call".into(), String::new(), action)
                .display_lines(/*width*/ 80)
        })
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn web_action_compact_display_preserves_transcript_and_raw_details() {
    let url = "https://example.com/docs/very-long-page?section=narrowing#details";
    let cell = new_web_search_call(
        "call".into(),
        String::new(),
        WebSearchAction::FindInPage {
            url: Some(url.into()),
            pattern: Some("日本語 🦀 needle".into()),
        },
    );
    let display = cell.display_lines(/*width*/ 32);
    assert_eq!(display.len(), 1);
    assert!(display[0].width() <= 32);
    insta::assert_snapshot!(display[0].to_string());
    let full = format!("Searched for '日本語 🦀 needle' in {url}");
    assert_eq!(cell.raw_lines(), vec![Line::from(full.clone())]);
    assert_eq!(
        cell.transcript_lines(/*width*/ 200)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![format!("• {full}")]
    );
}

#[test]
fn pending_web_action_and_legacy_query() {
    let cell = new_active_web_search_call(
        "call".into(),
        String::new(),
        /*animations_enabled*/ false,
    );
    insta::assert_snapshot!(cell.display_lines(/*width*/ 80)[0].to_string(), @"• Browsing the web");
    let legacy = new_web_search_call("call".into(), "old query".into(), WebSearchAction::Other);
    assert_eq!(
        legacy.raw_lines(),
        vec![Line::from("Searched the web for old query")]
    );
}

#[test]
fn batched_search_retains_each_query() {
    let cell = new_web_search_call(
        "call".into(),
        String::new(),
        WebSearchAction::Search {
            query: None,
            queries: Some(vec!["first query".into(), "second query".into()]),
        },
    );
    assert_eq!(
        cell.raw_lines(),
        vec![Line::from("Searched the web for first query, second query")]
    );
    insta::assert_snapshot!(cell.display_lines(/*width*/ 80)[0].to_string(), @"• Searched the web for first query, second query");
}

fn opened_page(output: Option<&str>) -> WebSearchCell {
    new_web_search_call(
        "call".into(),
        String::new(),
        WebSearchAction::OpenPage {
            url: Some("https://example.com/docs".into()),
        },
    )
    .with_output(output.map(str::to_owned))
}

fn rendered(lines: Vec<Line<'static>>) -> String {
    lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn web_response_previews_its_first_lines_and_discloses_the_rest() {
    let output = "Docs (https://example.com/docs)\n\u{e200}cite\u{e202}turn0view0\u{e201} Total lines: 3\nL0: first\tcolumn\nL1: \u{1b}[31msecond\nL2: third\n";
    let cell = opened_page(Some(output));

    assert_eq!(cell.activity_ids(), ["web:call"]);
    assert_eq!(
        cell.activity_disclosure(/*width*/ 80),
        Some(ActivityDisclosure::OutputLines(2))
    );
    insta::assert_snapshot!(rendered(cell.display_lines(/*width*/ 80)), @r"
    • Opened https://example.com/docs
      └ Docs (https://example.com/docs)
        [cite:turn0view0] Total lines: 3
        L0: first column
    ");
    insta::assert_snapshot!(rendered(cell.transcript_lines(/*width*/ 80)), @r"
    • Opened https://example.com/docs
      └ Docs (https://example.com/docs)
        [cite:turn0view0] Total lines: 3
        L0: first column
        L1:  [31msecond
        L2: third
    ");
    // Raw mode keeps the response as the model received it.
    let mut raw = vec![Line::from("Opened https://example.com/docs")];
    raw.extend(output.lines().map(Line::from));
    assert_eq!(cell.raw_lines(), raw);
}

#[test]
fn web_response_disclosure_counts_clipped_preview_lines() {
    let cell = opened_page(Some("short\nA preview line wider than the viewport\nlast"));
    // Three lines fit the preview; only the clipped one has more to reveal.
    assert_eq!(
        cell.activity_disclosure(/*width*/ 24),
        Some(ActivityDisclosure::OutputLines(1))
    );
    let display = cell.display_lines(/*width*/ 24);
    assert_eq!(display.len(), 4);
    assert!(display.iter().all(|line| line.width() <= 24));
    // The header is the only clipped row once the response fits.
    assert_eq!(
        opened_page(Some("short")).activity_disclosure(/*width*/ 24),
        Some(ActivityDisclosure::Generic)
    );
    assert_eq!(
        opened_page(Some("short")).activity_disclosure(/*width*/ 80),
        None
    );
}

#[test]
fn web_search_without_a_response_keeps_a_plain_row() {
    for output in [None, Some(""), Some(" \n\n")] {
        let cell = opened_page(output);
        assert_eq!(cell.activity_ids(), Vec::<String>::new());
        assert_eq!(cell.activity_disclosure(/*width*/ 80), None);
        assert_eq!(
            rendered(cell.display_lines(/*width*/ 80)),
            "• Opened https://example.com/docs"
        );
        assert_eq!(
            rendered(cell.transcript_lines(/*width*/ 80)),
            "• Opened https://example.com/docs"
        );
        assert_eq!(
            cell.raw_lines(),
            vec![Line::from("Opened https://example.com/docs")]
        );
    }
}

#[test]
fn live_completion_and_replay_show_the_same_response() {
    use crate::test_support::PathBufExt;
    use crate::test_support::test_path_buf;
    use crate::thread_transcript::RawReasoningVisibility;
    use crate::thread_transcript::thread_items_to_transcript_cells;
    use codex_app_server_protocol::ThreadItemPayload;
    use codex_app_server_protocol::WebSearchItem;

    let action = WebSearchAction::Search {
        query: Some("fixture".into()),
        queries: None,
    };
    let mut live = new_active_web_search_call(
        "call".into(),
        String::new(),
        /*animations_enabled*/ false,
    );
    assert_eq!(live.activity_ids(), Vec::<String>::new());
    live.update(action.clone(), "fixture".into(), Some("a\nb\nc\nd".into()));
    live.complete();

    let replayed = thread_items_to_transcript_cells(
        /*thread_id*/ None,
        &test_path_buf("/workspace").abs(),
        [ThreadItemPayload::WebSearch(WebSearchItem {
            id: "call".into(),
            query: "fixture".into(),
            action: Some(action),
            results: None,
            output: Some("a\nb\nc\nd".into()),
        })],
        RawReasoningVisibility::Hidden,
        /*config*/ None,
    );
    assert_eq!(replayed.len(), 1);
    let replayed = &replayed[0];
    assert_eq!(replayed.activity_ids(), live.activity_ids());
    assert_eq!(
        replayed.activity_disclosure(/*width*/ 80),
        Some(ActivityDisclosure::OutputLines(1))
    );
    assert_eq!(
        replayed.display_lines(/*width*/ 80),
        live.display_lines(/*width*/ 80)
    );
    assert_eq!(
        replayed.transcript_lines(/*width*/ 80),
        live.transcript_lines(/*width*/ 80)
    );
}

#[test]
fn web_response_folds_and_unfolds_in_the_conversation() {
    use crate::transcript_view::TranscriptView;
    use crossterm::event::KeyCode;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use std::sync::Arc;

    let output = (0..8)
        .map(|line| format!("L{line}: response line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let cells: Vec<Arc<dyn HistoryCell>> = vec![Arc::new(opened_page(Some(&output)))];
    let mut view = TranscriptView::default();
    let mut screen = |view: &mut TranscriptView| {
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 72, /*height*/ 14,
        );
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer, &cells);
        (area.top()..area.bottom())
            .map(|y| {
                (area.left()..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let folded = screen(&mut view);
    assert!(folded.contains("L2: response line 2"), "{folded}");
    assert!(!folded.contains("L3: response line 3"), "{folded}");
    assert!(folded.contains("+ 5 lines"), "{folded}");

    view.handle_key(KeyCode::F(4).into(), &cells);
    assert!(view.is_activity_focused());
    view.handle_key(KeyCode::Enter.into(), &cells);
    let unfolded = screen(&mut view);
    assert!(unfolded.contains("L7: response line 7"), "{unfolded}");
    assert!(unfolded.contains("− Show less"), "{unfolded}");

    view.handle_key(KeyCode::Enter.into(), &cells);
    let refolded = screen(&mut view);
    assert!(!refolded.contains("L3: response line 3"), "{refolded}");
    assert!(refolded.contains("+ 5 lines"), "{refolded}");
}
