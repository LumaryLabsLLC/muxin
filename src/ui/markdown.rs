//! Markdown, simplified for reading: paragraphs, headings, lists, quotes,
//! code blocks and tables (as written), with bold, italics, inline code and
//! links inside a line. Images and HTML are left out. Used for what GitHub
//! and the assistant write.

use std::ops::Range;

use gpui::{AnyElement, ElementId, FontStyle, FontWeight, HighlightStyle, InteractiveText, SharedString, StyledText, UnderlineStyle, div, prelude::*, px};

use super::theme::{Theme, mono_font};

/// What a stretch of a line is.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Kind {
    Bold,
    Italic,
    Code,
    /// Where it goes.
    Link(String),
}

/// A line as plain text, and which stretches of it are something more.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Rich {
    pub text: String,
    /// In order, never overlapping (see `flatten`).
    pub spans: Vec<(Range<usize>, Kind)>,
}

/// `text` as blocks, each `width` wide (text only wraps in a width it knows
/// up front). `id` names it among the page's other texts, for its links.
/// With nothing to show, `empty` is shown instead (nothing at all when
/// that's empty too).
pub(super) fn markdown(theme: &Theme, text: &str, width: f32, id: impl Into<SharedString>, empty: &str) -> AnyElement {
    let id: SharedString = id.into();
    let text = strip_comments(text);
    let mut blocks: Vec<AnyElement> = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut code: Option<Vec<String>> = None;
    let mut table: Vec<String> = Vec::new();
    let line_of = |text: &str, blocks: &Vec<AnyElement>| line(theme, ElementId::NamedInteger(id.clone(), blocks.len() as u64), rich(text));
    let mono_block = |lines: Vec<String>| {
        div()
            .w(px(width))
            .p(px(10.))
            .rounded(crate::ui::kit::corner(6.))
            .bg(theme.element)
            .font_family(mono_font())
            .text_size(px(12.))
            .text_color(theme.text_muted)
            .overflow_hidden()
            .children(lines.into_iter().map(|l| div().child(if l.is_empty() { " ".to_string() } else { l })))
            .into_any_element()
    };
    let flush = |paragraph: &mut Vec<String>, table: &mut Vec<String>, blocks: &mut Vec<AnyElement>| {
        if !paragraph.is_empty() {
            let text = paragraph.join(" ");
            blocks.push(div().w(px(width)).text_size(px(13.)).text_color(theme.text).child(line_of(&text, blocks)).into_any_element());
            paragraph.clear();
        }
        if !table.is_empty() {
            blocks.push(mono_block(std::mem::take(table)));
        }
    };
    for raw in text.lines() {
        if raw.trim_start().starts_with("```") {
            match code.take() {
                Some(lines) => blocks.push(mono_block(lines)),
                None => {
                    flush(&mut paragraph, &mut table, &mut blocks);
                    code = Some(Vec::new());
                }
            }
            continue;
        }
        if let Some(lines) = code.as_mut() {
            lines.push(raw.to_string());
            continue;
        }
        let t = raw.trim();
        if t.starts_with('|') {
            // A table, as it was written: its columns line up in a monospace font.
            if !paragraph.is_empty() {
                flush(&mut paragraph, &mut table, &mut blocks);
            }
            if !is_table_rule(t) {
                table.push(rich(t).text);
            }
            continue;
        }
        if t.is_empty() {
            flush(&mut paragraph, &mut table, &mut blocks);
        } else if let Some(h) = ["###### ", "##### ", "#### ", "### ", "## ", "# "].iter().find_map(|p| t.strip_prefix(p)) {
            flush(&mut paragraph, &mut table, &mut blocks);
            blocks.push(div().w(px(width)).pt(px(4.)).text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(line_of(h, &blocks)).into_any_element());
        } else if let Some(item) = list_item(t) {
            flush(&mut paragraph, &mut table, &mut blocks);
            blocks.push(
                div()
                    .w(px(width))
                    .flex()
                    .gap(px(8.))
                    .text_size(px(13.))
                    .text_color(theme.text)
                    .child(div().flex_none().text_color(theme.text_placeholder).child(item.0))
                    .child(div().flex_1().min_w(px(0.)).child(line_of(&item.1, &blocks)))
                    .into_any_element(),
            );
        } else if let Some(q) = t.strip_prefix('>') {
            flush(&mut paragraph, &mut table, &mut blocks);
            blocks.push(div().w(px(width)).pl(px(10.)).border_l_2().border_color(theme.border).text_size(px(13.)).text_color(theme.text_muted).child(line_of(q.trim(), &blocks)).into_any_element());
        } else {
            if !table.is_empty() {
                flush(&mut paragraph, &mut table, &mut blocks);
            }
            paragraph.push(t.to_string());
        }
    }
    if let Some(lines) = code {
        // Still being written, or never closed.
        blocks.push(mono_block(lines));
    }
    flush(&mut paragraph, &mut table, &mut blocks);
    if blocks.is_empty() && !empty.is_empty() {
        blocks.push(div().text_size(px(12.5)).text_color(theme.text_placeholder).child(empty.to_string()).into_any_element());
    }
    div().flex().flex_col().gap(px(8.)).children(blocks).into_any_element()
}

/// "|---|:--:|": the line under a table's heading.
fn is_table_rule(t: &str) -> bool {
    t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// One line with its bold, code and links.
fn line(theme: &Theme, id: ElementId, rich: Rich) -> AnyElement {
    if rich.spans.is_empty() {
        return div().child(rich.text).into_any_element();
    }
    let highlights: Vec<(Range<usize>, HighlightStyle)> = rich
        .spans
        .iter()
        .map(|(range, kind)| {
            let style = match kind {
                Kind::Bold => HighlightStyle { font_weight: Some(FontWeight::SEMIBOLD), ..Default::default() },
                Kind::Italic => HighlightStyle { font_style: Some(FontStyle::Italic), ..Default::default() },
                Kind::Code => HighlightStyle { background_color: Some(theme.element), ..Default::default() },
                Kind::Link(_) => HighlightStyle { color: Some(theme.text_accent), underline: Some(UnderlineStyle { thickness: px(1.), color: None, wavy: false }), ..Default::default() },
            };
            (range.clone(), style)
        })
        .collect();
    let links: Vec<(Range<usize>, String)> = rich.spans.iter().filter_map(|(range, kind)| if let Kind::Link(url) = kind { Some((range.clone(), url.clone())) } else { None }).collect();
    let styled = StyledText::new(rich.text).with_highlights(highlights);
    if links.is_empty() {
        return styled.into_any_element();
    }
    let ranges = links.iter().map(|(range, _)| range.clone()).collect();
    InteractiveText::new(id, styled)
        .on_click(ranges, move |ix, _, cx| {
            if let Some((_, url)) = links.get(ix) {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

pub(super) fn strip_comments(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// "- [x] done" -> ("☑", "done"), "1. step" -> ("1.", "step"), "* x" -> ("•", "x").
pub(super) fn list_item(t: &str) -> Option<(String, String)> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(marker) {
            return Some(match rest {
                r if r.starts_with("[ ] ") => ("☐".into(), r[4..].to_string()),
                r if r.starts_with("[x] ") || r.starts_with("[X] ") => ("☑".into(), r[4..].to_string()),
                r => ("•".into(), r.to_string()),
            });
        }
    }
    let (num, rest) = t.split_once(". ")?;
    (num.len() <= 3 && num.chars().all(|c| c.is_ascii_digit())).then(|| (format!("{num}."), rest.to_string()))
}

/// A line as plain text: links as their text, images as their description,
/// the marks of emphasis and code dropped.
#[cfg(test)]
pub(super) fn inline(text: &str) -> String {
    rich(text).text
}

/// A link Muxin will open: the web and mail, nothing that runs a program.
fn opens(url: &str) -> bool {
    ["https://", "http://", "mailto:"].iter().any(|p| url.starts_with(p))
}

/// A line's plain text and what its marks made of it.
pub(super) fn rich(text: &str) -> Rich {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut spans: Vec<(Range<usize>, Kind)> = Vec::new();
    // Where bold and italics began, in `out`.
    let (mut bold, mut italic): (Option<usize>, Option<usize>) = (None, None);
    let find = |from: usize, what: char| chars[from..].iter().position(|c| *c == what).map(|p| from + p);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let image = c == '!' && chars.get(i + 1) == Some(&'[');
        if c == '[' || image {
            let open = if image { i + 1 } else { i };
            if let Some(close) = find(open, ']')
                && chars.get(close + 1) == Some(&'(')
                && let Some(end) = find(close, ')')
            {
                let label: String = chars[open + 1..close].iter().collect();
                if image {
                    out.push_str(&format!("[image{}]", if label.is_empty() { String::new() } else { format!(": {label}") }));
                } else {
                    let url: String = chars[close + 2..end].iter().collect();
                    let url = url.split_whitespace().next().unwrap_or("").to_string();
                    let start = out.len();
                    // The label may have marks of its own.
                    out.push_str(&rich(&label).text);
                    if opens(&url) && out.len() > start {
                        spans.push((start..out.len(), Kind::Link(url)));
                    }
                }
                i = end + 1;
                continue;
            }
        }
        if c == '<' {
            // <https://example.com> is a link; anything else is HTML (<br>, <img ...>, <details>), dropped.
            if let Some(end) = find(i, '>') {
                let inner: String = chars[i + 1..end].iter().collect();
                if opens(&inner) && !inner.contains(char::is_whitespace) {
                    let start = out.len();
                    out.push_str(&inner);
                    spans.push((start..out.len(), Kind::Link(inner)));
                } else if inner.starts_with("img") {
                    out.push_str("[image]");
                }
                i = end + 1;
                continue;
            }
        }
        if c == '`' {
            // Inline code stays as written (it may contain <angle brackets> and *stars*).
            if let Some(end) = find(i + 1, '`') {
                let start = out.len();
                out.extend(&chars[i + 1..end]);
                if out.len() > start {
                    spans.push((start..out.len(), Kind::Code));
                }
                i = end + 1;
                continue;
            }
            i += 1;
            continue;
        }
        if (c == '*' && chars.get(i + 1) == Some(&'*')) || (c == '_' && chars.get(i + 1) == Some(&'_')) {
            match bold.take() {
                Some(start) if out.len() > start => spans.push((start..out.len(), Kind::Bold)),
                Some(_) => {}
                None => bold = Some(out.len()),
            }
            i += 2;
            continue;
        }
        if c == '*' {
            match italic.take() {
                Some(start) if out.len() > start => spans.push((start..out.len(), Kind::Italic)),
                Some(_) => {}
                None => italic = Some(out.len()),
            }
            i += 1;
            continue;
        }
        // A web address written out is a link to it.
        if c == 'h' && (i == 0 || !chars[i - 1].is_alphanumeric()) {
            let rest: String = chars[i..].iter().take_while(|c| !c.is_whitespace()).collect();
            if rest.starts_with("https://") || rest.starts_with("http://") {
                // Not the full stop or bracket that follows it.
                let url = rest.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '"', '\'']);
                if url.len() > "https://".len() {
                    let start = out.len();
                    out.push_str(url);
                    spans.push((start..out.len(), Kind::Link(url.to_string())));
                    i += url.chars().count();
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    Rich { text: out, spans: flatten(spans) }
}

/// Spans in order with none inside another: a link wins over code, code over
/// bold, bold over italics, and what's left of the outer one keeps its look
/// around the inner.
fn flatten(spans: Vec<(Range<usize>, Kind)>) -> Vec<(Range<usize>, Kind)> {
    let rank = |k: &Kind| match k {
        Kind::Link(_) => 0,
        Kind::Code => 1,
        Kind::Bold => 2,
        Kind::Italic => 3,
    };
    let mut by_rank = spans;
    by_rank.sort_by_key(|(range, kind)| (rank(kind), range.start));
    let mut kept: Vec<(Range<usize>, Kind)> = Vec::new();
    for (range, kind) in by_rank {
        // What of this span no stronger one has taken.
        let mut pieces = vec![range];
        for (taken, _) in &kept {
            pieces = pieces
                .into_iter()
                .flat_map(|p| {
                    if taken.end <= p.start || taken.start >= p.end {
                        vec![p]
                    } else {
                        vec![p.start..taken.start.max(p.start), taken.end.min(p.end)..p.end]
                    }
                })
                .filter(|p| p.start < p.end)
                .collect();
        }
        kept.extend(pieces.into_iter().map(|p| (p, kind.clone())));
    }
    kept.sort_by_key(|(range, _)| range.start);
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplifies_markdown() {
        assert_eq!(inline("See [the docs](https://x.y) and **bold** `code`"), "See the docs and bold code");
        assert_eq!(inline("commands like `:tag <tag_name>` work<br>"), "commands like :tag <tag_name> work");
        assert_eq!(inline("![screenshot](a.png)"), "[image: screenshot]");
        assert_eq!(inline("__both__ ways and a lone * star"), "both ways and a lone  star");
        assert_eq!(list_item("- [x] done"), Some(("☑".into(), "done".into())));
        assert_eq!(list_item("12. step"), Some(("12.".into(), "step".into())));
        assert_eq!(strip_comments("a<!-- hidden -->b"), "ab");
    }

    fn part<'a>(rich: &'a Rich, n: usize) -> (&'a str, &'a Kind) {
        (&rich.text[rich.spans[n].0.clone()], &rich.spans[n].1)
    }

    #[test]
    fn a_line_keeps_what_its_marks_meant() {
        let r = rich("See [the docs](https://x.y/a) and **bold** `code` and *more*");
        assert_eq!(r.text, "See the docs and bold code and more");
        assert_eq!(part(&r, 0), ("the docs", &Kind::Link("https://x.y/a".into())));
        assert_eq!(part(&r, 1), ("bold", &Kind::Bold));
        assert_eq!(part(&r, 2), ("code", &Kind::Code));
        assert_eq!(part(&r, 3), ("more", &Kind::Italic));
        assert_eq!(r.spans.len(), 4);
        // Plain text has none, and marks left open mark nothing.
        assert_eq!(rich("nothing special, 2 * 3 = 6"), Rich { text: "nothing special, 2  3 = 6".into(), spans: vec![] });
        assert!(rich("**never closed").spans.is_empty());
    }

    #[test]
    fn an_address_written_out_is_a_link_without_what_follows_it() {
        let r = rich("Opened https://github.com/acme/api/pull/12. Checks are running (see http://ci.acme.dev/1).");
        assert_eq!(part(&r, 0), ("https://github.com/acme/api/pull/12", &Kind::Link("https://github.com/acme/api/pull/12".into())));
        assert_eq!(part(&r, 1), ("http://ci.acme.dev/1", &Kind::Link("http://ci.acme.dev/1".into())));
        assert!(r.text.ends_with("(see http://ci.acme.dev/1)."));
        let r = rich("<https://muxin.dev> and oauth://not-a-link and with`https://in.code`");
        assert_eq!(part(&r, 0), ("https://muxin.dev", &Kind::Link("https://muxin.dev".into())));
        assert_eq!(part(&r, 1), ("https://in.code", &Kind::Code));
        assert_eq!(r.spans.len(), 2);
    }

    #[test]
    fn only_the_web_and_mail_open() {
        for url in ["javascript:void", "file:///etc/passwd", "muxin://join/x", "/relative", ""] {
            let r = rich(&format!("[click]({url})"));
            assert_eq!((r.text.as_str(), r.spans.len()), ("click", 0), "{url}");
        }
        assert_eq!(rich("[mail](mailto:a@b.c)").spans.len(), 1);
    }

    #[test]
    fn marks_inside_marks_dont_overlap() {
        // Bold around a link around code: each stretch is one thing, in order.
        let r = rich("**see [the `api` docs](https://x.y) now**");
        assert_eq!(r.text, "see the api docs now");
        let parts: Vec<(&str, &Kind)> = (0..r.spans.len()).map(|n| part(&r, n)).collect();
        let link = Kind::Link("https://x.y".into());
        assert_eq!(parts, [("see ", &Kind::Bold), ("the api docs", &link), (" now", &Kind::Bold)]);
        for pair in r.spans.windows(2) {
            assert!(pair[0].0.end <= pair[1].0.start, "{:?}", r.spans);
        }
        // Every stretch falls on whole characters.
        let r = rich("**héllo** `wörld` [ünï](https://x.y)");
        assert!(r.spans.iter().all(|(range, _)| r.text.is_char_boundary(range.start) && r.text.is_char_boundary(range.end)));
    }

    #[test]
    fn a_tables_rule_is_told_from_its_rows() {
        assert!(is_table_rule("|---|:--:|"));
        assert!(is_table_rule("| --- | --- |"));
        assert!(!is_table_rule("| a | b |"));
        assert!(!is_table_rule("|  |  |"));
    }
}
