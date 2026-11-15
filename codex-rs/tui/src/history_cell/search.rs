//! Compact web activity summaries with complete details in the transcript and raw output.

use super::activity_preview::DETAIL_PREVIEW_LINES;
use super::*;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::terminal_hyperlinks::adaptive_wrap_hyperlink_lines;

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;

fn web_search_action_detail(action: &WebSearchAction) -> String {
    match action {
        WebSearchAction::Search { query, queries } => query
            .clone()
            .filter(|q| !q.is_empty())
            .unwrap_or_else(|| queries.as_deref().unwrap_or_default().join(", ")),
        WebSearchAction::OpenPage { url } => url.clone().unwrap_or_default(),
        WebSearchAction::FindInPage { url, pattern } => match (
            pattern.as_deref().filter(|pattern| !pattern.is_empty()),
            url.as_deref().filter(|url| !url.is_empty()),
        ) {
            (Some(pattern), Some(url)) => format!("'{pattern}' in {url}"),
            (Some(pattern), None) => format!("'{pattern}'"),
            (None, Some(url)) => url.to_string(),
            (None, None) => String::new(),
        },
        WebSearchAction::Other => String::new(),
    }
}

/// Response text as terminal cells: content-reference delimiters and control characters do not
/// print.
fn printable(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\u{e200}' => '[',
            '\u{e202}' => ':',
            '\u{e201}' => ']',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect()
}

fn preview_line(index: usize, text: &str) -> Line<'static> {
    // Bound allocation and grapheme scanning before clipping to display width.
    let end = text.floor_char_boundary(16 * 1024);
    let mut shown = printable(&text[..end]);
    if end < text.len() {
        shown.push('…');
    }
    Line::from(vec![
        if index == 0 { "  └ " } else { "    " }.dim(),
        shown.dim(),
    ])
}

fn web_search_detail(action: Option<&WebSearchAction>, query: &str) -> String {
    let detail = action.map(web_search_action_detail).unwrap_or_default();
    if detail.is_empty() {
        query.to_string()
    } else {
        detail
    }
}

#[derive(Debug)]
pub(crate) struct WebSearchCell {
    call_id: String,
    query: String,
    action: Option<WebSearchAction>,
    /// Text the search returned to the model, when the item carries it.
    output: Option<String>,
    start_time: Instant,
    completed: bool,
    animations_enabled: bool,
}

impl WebSearchCell {
    pub(crate) fn new(
        call_id: String,
        query: String,
        action: Option<WebSearchAction>,
        animations_enabled: bool,
    ) -> Self {
        Self {
            call_id,
            query,
            action,
            output: None,
            start_time: Instant::now(),
            completed: false,
            animations_enabled,
        }
    }

    pub(crate) fn call_id(&self) -> &str {
        &self.call_id
    }

    pub(crate) fn with_output(mut self, output: Option<String>) -> Self {
        self.output = output;
        self
    }

    pub(crate) fn update(
        &mut self,
        action: WebSearchAction,
        query: String,
        output: Option<String>,
    ) {
        self.action = Some(action);
        self.query = query;
        self.output = output;
    }

    pub(crate) fn complete(&mut self) {
        self.completed = true;
    }

    pub(crate) fn is_completed(&self) -> bool {
        self.completed
    }

    fn summary(&self) -> Line<'static> {
        let detail = web_search_detail(self.action.as_ref(), &self.query);
        let (header, separator) = match (&self.action, self.completed) {
            (Some(WebSearchAction::OpenPage { .. }), completed) => (
                match (completed, detail.is_empty()) {
                    (true, true) => "Opened page",
                    (true, false) => "Opened",
                    (false, true) => "Opening page",
                    (false, false) => "Opening",
                },
                " ",
            ),
            (Some(WebSearchAction::FindInPage { pattern, .. }), completed) => {
                let has_pattern = pattern.as_ref().is_some_and(|pattern| !pattern.is_empty());
                (
                    match (completed, has_pattern) {
                        (true, true) => "Searched for",
                        (true, false) => "Searched page",
                        (false, true) => "Searching for",
                        (false, false) => "Searching page",
                    },
                    " ",
                )
            }
            (None | Some(WebSearchAction::Other), false) if detail.is_empty() => {
                ("Browsing the web", " ")
            }
            (Some(WebSearchAction::Search { .. } | WebSearchAction::Other) | None, completed) => (
                if completed {
                    "Searched the web"
                } else {
                    "Searching the web"
                },
                " for ",
            ),
        };
        let mut line = Line::from(header.bold());
        if !detail.is_empty() {
            line.extend([separator.into(), detail.into()]);
        }
        line
    }

    fn header(&self) -> Line<'static> {
        let bullet = if self.completed {
            "•".dim()
        } else {
            activity_indicator(
                Some(self.start_time),
                MotionMode::from_animations_enabled(self.animations_enabled),
                ReducedMotionIndicator::StaticBullet,
            )
            .unwrap_or_else(|| "•".dim())
        };
        let mut line = Line::from(vec![bullet, " ".into()]);
        let mut summary = self.summary();
        for span in &mut summary.spans {
            span.content = span.content.replace(['\n', '\r', '\t'], " ").into();
        }
        line.extend(summary);
        line
    }

    fn output(&self) -> Option<&str> {
        self.output
            .as_deref()
            .filter(|output| !output.trim().is_empty())
    }
}

impl HistoryCell for WebSearchCell {
    fn tool_call_labels(&self) -> Vec<String> {
        vec![self.summary().to_string()]
    }

    fn activity_ids(&self) -> Vec<String> {
        // A search without a response has nothing to disclose and keeps a plain row.
        self.output()
            .map(|_| format!("web:{}", self.call_id))
            .into_iter()
            .collect()
    }

    fn activity_disclosure(&self, width: u16) -> Option<ActivityDisclosure> {
        let output = self.output()?;
        let overflows = |line: Line<'_>| line.width() > usize::from(width);
        let clipped = output
            .lines()
            .take(DETAIL_PREVIEW_LINES)
            .filter(|text| overflows(preview_line(/*index*/ 0, text)))
            .count();
        let hidden = output.lines().count().saturating_sub(DETAIL_PREVIEW_LINES) + clipped;
        if hidden > 0 {
            return Some(ActivityDisclosure::OutputLines(hidden));
        }
        overflows(self.header()).then_some(ActivityDisclosure::Generic)
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let preview = self
            .output()
            .into_iter()
            .flat_map(str::lines)
            .take(DETAIL_PREVIEW_LINES)
            .enumerate()
            .map(|(index, text)| preview_line(index, text));
        std::iter::once(self.header())
            .chain(preview)
            .map(|line| truncate_line_with_ellipsis_if_overflow(line, width as usize))
            .collect()
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        visible_lines(self.transcript_hyperlink_lines(width))
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        let mut lines = PrefixedWrappedHistoryCell::new(self.summary(), vec!["• ".dim()], "  ")
            .display_hyperlink_lines(width);
        if let Some(output) = self.output() {
            let response = output
                .lines()
                .map(|text| Line::from(printable(text)))
                .collect();
            lines.extend(adaptive_wrap_hyperlink_lines(
                &plain_hyperlink_lines(response),
                RtOptions::new(usize::from(width).max(/*other*/ 1))
                    .initial_indent("  └ ".dim().into())
                    .subsequent_indent("    ".into()),
            ));
        }
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![self.summary()];
        lines.extend(self.output().into_iter().flat_map(raw_lines_from_source));
        plain_lines(lines)
    }
}

pub(crate) fn new_active_web_search_call(
    call_id: String,
    query: String,
    animations_enabled: bool,
) -> WebSearchCell {
    WebSearchCell::new(call_id, query, /*action*/ None, animations_enabled)
}

pub(crate) fn new_web_search_call(
    call_id: String,
    query: String,
    action: WebSearchAction,
) -> WebSearchCell {
    let mut cell = WebSearchCell::new(
        call_id,
        query,
        Some(action),
        /*animations_enabled*/ false,
    );
    cell.complete();
    cell
}
