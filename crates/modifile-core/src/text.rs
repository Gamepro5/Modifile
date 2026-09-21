//! Turning mod descriptions into something a plain-text UI can show.
//!
//! CurseForge publishes descriptions as HTML, Modrinth as Markdown, GitHub as
//! whatever is in the README. None of that is worth a rendering engine: this is
//! a mod manager, the description is there to answer "what is this and do I
//! want it", and a detail page links out for the full thing.
//!
//! So both are reduced to readable plain text. The goal is not fidelity, it is
//! that paragraphs stay paragraphs and nothing shows up as raw markup.

/// Strip HTML to readable text.
///
/// Handles the shapes a mod description actually uses — paragraphs, lists,
/// line breaks, headings, links — and throws away the rest. `<script>` and
/// `<style>` contents are dropped entirely rather than printed, which is the
/// difference between a description and a wall of CSS.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut chars = html.chars().peekable();
    // Set while inside an element whose *contents* are not text.
    let mut skipping: Option<&'static str> = None;

    while let Some(c) = chars.next() {
        if c != '<' {
            if skipping.is_none() {
                out.push(c);
            }
            continue;
        }

        // Read the tag.
        let mut tag = String::new();
        for c in chars.by_ref() {
            if c == '>' {
                break;
            }
            tag.push(c);
        }

        let lowered = tag.trim().to_ascii_lowercase();
        let name: String = lowered
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        let closing = lowered.starts_with('/');

        if let Some(open) = skipping {
            if closing && name == open {
                skipping = None;
            }
            continue;
        }
        if !closing && matches!(name.as_str(), "script" | "style" | "head") {
            skipping = Some(match name.as_str() {
                "script" => "script",
                "style" => "style",
                _ => "head",
            });
            continue;
        }

        // Anything that ends a line ends a line.
        match name.as_str() {
            "br" => out.push('\n'),
            // A list item is one line, and its bullet belongs to the opening
            // tag. Breaking on the close too would put a blank line between
            // every pair of items.
            "li" => {
                if !closing {
                    out.push('\n');
                    out.push_str("• ");
                }
            }
            "p" | "div" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol"
            | "table" | "blockquote" | "section" | "pre" => out.push('\n'),
            _ => {}
        }
    }

    tidy(&decode_entities(&out))
}

/// Strip Markdown to readable text.
///
/// Deliberately conservative: it removes the markers that read as noise and
/// leaves everything else alone. Mangling a description is worse than showing
/// a stray asterisk.
pub fn markdown_to_text(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut in_fence = false;

    for line in md.lines() {
        let trimmed = line.trim();

        // Fenced code blocks are dropped: an installation snippet is not a
        // description, and it is the bulkiest thing in most READMEs.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        // A line that is only badges or images carries nothing readable.
        let without_images = strip_images(trimmed);
        if trimmed.starts_with('!') && without_images.trim().is_empty() {
            continue;
        }

        let mut line = without_images;
        line = strip_links(&line);
        line = line.trim_start_matches('#').trim_start().to_string();
        // Bullets keep their meaning but not their spelling.
        if let Some(rest) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
        {
            line = format!("• {rest}");
        }
        line = line.replace("**", "").replace("__", "").replace('`', "");
        // A horizontal rule is a paragraph break, not a row of dashes.
        if line.chars().all(|c| c == '-' || c == '=' || c == '*') && line.len() > 2 {
            line = String::new();
        }

        out.push_str(&line);
        out.push('\n');
    }

    tidy(&out)
}

/// `![alt](url)` -> `alt`, which is usually empty for a badge.
fn strip_images(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '!' && bytes.get(i + 1) == Some(&'[') {
            if let Some((alt, end)) = read_bracketed(&bytes, i + 1) {
                if bytes.get(end) == Some(&'(') {
                    if let Some((_, close)) = read_parens(&bytes, end) {
                        out.push_str(&alt);
                        i = close;
                        continue;
                    }
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// `[text](url)` -> `text`.
fn strip_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '[' {
            if let Some((label, end)) = read_bracketed(&bytes, i) {
                if bytes.get(end) == Some(&'(') {
                    if let Some((_, close)) = read_parens(&bytes, end) {
                        out.push_str(&label);
                        i = close;
                        continue;
                    }
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Read `[...]` starting at `start`, returning the contents and the index just
/// past the closing bracket.
fn read_bracketed(chars: &[char], start: usize) -> Option<(String, usize)> {
    if chars.get(start) != Some(&'[') {
        return None;
    }
    let mut depth = 0;
    let mut body = String::new();
    for (offset, c) in chars.iter().enumerate().skip(start) {
        match c {
            '[' => {
                depth += 1;
                if depth > 1 {
                    body.push(*c);
                }
            }
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some((body, offset + 1));
                }
                body.push(*c);
            }
            _ => body.push(*c),
        }
    }
    None
}

fn read_parens(chars: &[char], start: usize) -> Option<(String, usize)> {
    if chars.get(start) != Some(&'(') {
        return None;
    }
    let mut depth = 0;
    let mut body = String::new();
    for (offset, c) in chars.iter().enumerate().skip(start) {
        match c {
            '(' => {
                depth += 1;
                if depth > 1 {
                    body.push(*c);
                }
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((body, offset + 1));
                }
                body.push(*c);
            }
            _ => body.push(*c),
        }
    }
    None
}

/// The handful of entities that actually appear in mod descriptions.
fn decode_entities(text: &str) -> String {
    let mut out = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…");
    // Numeric entities, which storefront editors emit freely.
    while let Some(start) = out.find("&#") {
        let Some(end) = out[start..].find(';').map(|i| start + i) else {
            break;
        };
        let digits = &out[start + 2..end];
        let decoded = digits
            .strip_prefix('x')
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .or_else(|| digits.parse::<u32>().ok())
            .and_then(char::from_u32);
        match decoded {
            Some(c) => out.replace_range(start..=end, &c.to_string()),
            // Unrecognised: drop it rather than loop forever on it.
            None => out.replace_range(start..=end, ""),
        }
    }
    out
}

/// Collapse runs of blank lines and trailing spaces.
fn tidy(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end().to_string();
        // At most one blank line in a row.
        if line.trim().is_empty() && lines.last().is_some_and(|l| l.trim().is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Cut to a length that fits, on a word boundary, with an ellipsis.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    let end = cut.rfind(' ').unwrap_or(cut.len());
    format!("{}…", cut[..end].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_paragraphs() {
        // Paragraphs stay separated by a blank line, which is the whole point
        // of keeping the block tags rather than stripping every tag alike.
        let html = "<p>Adds <b>sodium</b> rendering.</p><p>Fast &amp; small.</p>";
        assert_eq!(
            html_to_text(html),
            "Adds sodium rendering.\n\nFast & small."
        );
    }

    #[test]
    fn html_lists_keep_their_bullets() {
        let html = "<ul><li>One</li><li>Two</li></ul>";
        assert_eq!(html_to_text(html), "• One\n• Two");
    }

    #[test]
    fn script_and_style_contents_are_not_description() {
        // The bug this guards: stripping only the tags leaves the CSS, and the
        // description reads as a stylesheet.
        let html = "<style>.a{color:red}</style><p>Real text</p><script>evil()</script>";
        assert_eq!(html_to_text(html), "Real text");
    }

    #[test]
    fn numeric_entities_decode_and_terminate() {
        // An unrecognised entity used to spin forever here.
        assert_eq!(html_to_text("<p>caf&#233;</p>"), "café");
        assert_eq!(html_to_text("<p>a&#x41;b</p>"), "aAb");
        assert_eq!(html_to_text("<p>x&#999999999;y</p>"), "xy");
    }

    #[test]
    fn markdown_loses_its_markers() {
        let md = "# Sodium\n\nA **fast** renderer.\n\n- one\n- two\n";
        assert_eq!(
            markdown_to_text(md),
            "Sodium\n\nA fast renderer.\n\n• one\n• two"
        );
    }

    #[test]
    fn markdown_badges_and_code_blocks_are_dropped() {
        let md = "![](https://img.shields.io/badge)\n\n# Title\n\n```sh\ncargo build\n```\n\nBody.";
        assert_eq!(markdown_to_text(md), "Title\n\nBody.");
    }

    #[test]
    fn markdown_links_keep_their_label() {
        assert_eq!(
            markdown_to_text("See [the docs](https://example.com) now."),
            "See the docs now."
        );
        // An image inside a link is the common badge-with-link shape.
        assert_eq!(markdown_to_text("[![](a.png)](https://b.com)").trim(), "");
    }

    #[test]
    fn truncation_stops_on_a_word() {
        assert_eq!(truncate("one two three", 100), "one two three");
        assert_eq!(truncate("one two three", 8), "one two…");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(html_to_text(""), "");
        assert_eq!(markdown_to_text(""), "");
    }
}
