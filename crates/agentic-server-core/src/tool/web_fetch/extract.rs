//! Plain-text extraction for fetched HTML, without an HTML parser dependency.
//!
//! The model needs readable text, not a DOM: scripts, styles, and markup are
//! dropped, every block boundary becomes one line break, character references
//! are decoded, and whitespace is collapsed outside `<pre>`, whose content is
//! kept verbatim. The output is deterministic and never larger than a small
//! multiple of the input.

use std::borrow::Cow;

/// Text and title extracted from an HTML document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ExtractedText {
    pub(crate) title: Option<String>,
    pub(crate) text: String,
}

/// Bytes of a `<title>` kept: a title is a label, not content, and the page
/// chooses its length.
const MAX_TITLE_BYTES: usize = 512;

/// Longest character reference decoded, in characters after the `&`; the
/// search for a reference's `;` never looks past it.
const MAX_REFERENCE_CHARS: usize = 12;

/// Elements whose content never reaches the model.
const SKIPPED_ELEMENTS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "iframe", "canvas", "object",
];

/// Elements that start or end a line of text.
const BLOCK_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "dd",
    "details",
    "dialog",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "html",
    "li",
    "main",
    "nav",
    "ol",
    "option",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "tfoot",
    "thead",
    "tr",
    "ul",
];

struct Tag<'a> {
    name: Cow<'a, str>,
    closing: bool,
    /// Bytes the tag occupies in the input, from `<` through `>`.
    len: usize,
}

/// Parse the tag at the start of `input` (which begins with `<`). Returns
/// `None` when the `<` does not open a tag, so it is kept as text.
fn parse_tag(input: &str) -> Option<Tag<'_>> {
    let body = input.strip_prefix('<')?;
    let (closing, body) = match body.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let name_len = body
        .char_indices()
        .take_while(|(index, ch)| {
            if *index == 0 {
                ch.is_ascii_alphabetic()
            } else {
                ch.is_ascii_alphanumeric() || *ch == '-'
            }
        })
        .count();
    if name_len == 0 {
        return None;
    }
    let name = &body[..name_len];
    let name = if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(name.to_ascii_lowercase())
    } else {
        Cow::Borrowed(name)
    };
    // The tag ends at the first `>` outside a quoted attribute value.
    let mut quote: Option<char> = None;
    let mut end = input.len();
    for (index, ch) in input.char_indices().skip(1) {
        match (quote, ch) {
            (Some(open), close) if open == close => quote = None,
            (None, '"' | '\'') => quote = Some(ch),
            (None, '>') => {
                end = index + 1;
                break;
            }
            _ => {}
        }
    }
    Some(Tag {
        name,
        closing,
        len: end,
    })
}

/// Split `input` at the closing tag of `name` (case-insensitive), returning the
/// enclosed text and what follows the closing tag. Without a closing tag the
/// whole input is enclosed.
fn take_until_close<'a>(input: &'a str, name: &str) -> (&'a str, &'a str) {
    let mut search_from = 0;
    while let Some(offset) = input[search_from..].find("</") {
        let start = search_from + offset;
        let candidate = &input[start + 2..];
        let matches = candidate
            .get(..name.len())
            .is_some_and(|found| found.eq_ignore_ascii_case(name))
            && candidate[name.len()..]
                .chars()
                .next()
                .is_none_or(|next| next == '>' || next.is_whitespace());
        if matches {
            let after = candidate.find('>').map_or("", |end| &candidate[end + 1..]);
            return (&input[..start], after);
        }
        search_from = start + 2;
    }
    (input, "")
}

/// Append a text run, collapsing whitespace unless `preformatted`.
fn push_text(out: &mut String, text: &str, preformatted: bool) {
    let decoded = decode_entities(text);
    if preformatted {
        out.push_str(&decoded);
        return;
    }
    let mut pending_space = false;
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() && !out.ends_with(['\n', ' ']) {
            out.push(' ');
        }
        pending_space = false;
        out.push(ch);
    }
    if pending_space && !out.is_empty() && !out.ends_with(['\n', ' ']) {
        out.push(' ');
    }
}

/// End the current line, once: consecutive block boundaries never produce
/// blank lines, and a line never ends in collapsed whitespace.
fn push_line_break(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Separate inline cells with one space, never at a line start.
fn push_separator(out: &mut String) {
    if !out.is_empty() && !out.ends_with(['\n', ' ']) {
        out.push(' ');
    }
}

/// The longest prefix of `text` within `limit` bytes that ends on a character
/// boundary.
pub(super) fn truncate_to_char_boundary(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Convert an HTML document to plain text and pick out its `<title>`.
#[must_use]
pub(crate) fn html_to_text(html: &str) -> ExtractedText {
    let mut out = String::with_capacity(html.len() / 2);
    let mut title = None;
    let mut rest = html;
    let mut pre_depth = 0usize;
    while let Some(lt) = rest.find('<') {
        push_text(&mut out, &rest[..lt], pre_depth > 0);
        rest = &rest[lt..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            rest = rest.find('>').map_or("", |end| &rest[end + 1..]);
            continue;
        }
        let Some(tag) = parse_tag(rest) else {
            out.push('<');
            rest = &rest[1..];
            continue;
        };
        rest = &rest[tag.len..];
        let name = tag.name.as_ref();
        if tag.closing {
            if name == "pre" {
                pre_depth = pre_depth.saturating_sub(1);
            }
            if BLOCK_ELEMENTS.contains(&name) {
                push_line_break(&mut out);
            }
            continue;
        }
        if name == "title" && title.is_none() {
            let (inner, after) = take_until_close(rest, "title");
            let mut heading = String::new();
            push_text(&mut heading, inner, false);
            let heading = truncate_to_char_boundary(heading.trim(), MAX_TITLE_BYTES);
            title = (!heading.is_empty()).then(|| heading.to_owned());
            rest = after;
        } else if SKIPPED_ELEMENTS.contains(&name) {
            rest = take_until_close(rest, name).1;
        } else if name == "br" || name == "pre" || BLOCK_ELEMENTS.contains(&name) {
            if name == "pre" {
                pre_depth += 1;
            }
            push_line_break(&mut out);
        } else if matches!(name, "td" | "th") {
            push_separator(&mut out);
        }
    }
    push_text(&mut out, rest, pre_depth > 0);
    ExtractedText {
        title,
        text: out.trim().to_owned(),
    }
}

/// Decode HTML character references. Unknown references are kept verbatim.
#[must_use]
pub(crate) fn decode_entities(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        // Look for the `;` within the reference length only, so a run of
        // ampersands costs a bounded amount of work per ampersand.
        let semicolon = rest[1..]
            .char_indices()
            .take(MAX_REFERENCE_CHARS + 1)
            .find(|(_, ch)| *ch == ';')
            .map(|(index, _)| index + 1);
        let Some(semicolon) = semicolon else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let reference = &rest[1..semicolon];
        if let Some(decoded) = decode_reference(reference) {
            out.push(decoded);
            rest = &rest[semicolon + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

fn decode_reference(reference: &str) -> Option<char> {
    if let Some(number) = reference.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse::<u32>().ok()?,
        };
        return char::from_u32(code).filter(|ch| *ch != '\0');
    }
    let decoded = match reference {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "bull" => '•',
        "middot" => '·',
        "times" => '×',
        "deg" => '°',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "sect" => '§',
        "para" => '¶',
        _ => return None,
    };
    Some(decoded)
}

/// The charset an HTML document declares in a `<meta>` tag, read from its
/// first 2 KiB, for bodies whose `Content-Type` carries none.
#[must_use]
pub(crate) fn sniff_html_charset(bytes: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(offset) = head[search_from..].find("charset=") {
        let value_start = search_from + offset + "charset=".len();
        let value = head[value_start..]
            .trim_start_matches(['"', '\'', ' '])
            .split(|ch: char| ch == '"' || ch == '\'' || ch == ';' || ch == '>' || ch == '/' || ch.is_whitespace())
            .next()
            .unwrap_or_default();
        let inside_meta = head[..value_start]
            .rfind("<meta")
            .is_some_and(|meta| !head[meta..value_start].contains('>'));
        if !value.is_empty() && inside_meta {
            return Some(value.to_owned());
        }
        search_from = value_start;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_title_and_block_structure() {
        let html = r"<!DOCTYPE html><html><head><title> The &amp; Title </title>
            <style>body { color: red }</style><script>var x = '<p>not text</p>';</script></head>
            <body><h1>Heading</h1><p>First   paragraph with <b>bold</b> and <a href='/x'>a link</a>.</p>
            <ul><li>one</li><li>two</li></ul><table><tr><td>a</td><td>b</td></tr></table>
            <!-- a comment --><noscript>enable js</noscript><p>Last &lt;p&gt; &#169; &#x41;</p></body></html>";
        let extracted = html_to_text(html);
        assert_eq!(extracted.title.as_deref(), Some("The & Title"));
        assert_eq!(
            extracted.text,
            "Heading\nFirst paragraph with bold and a link.\none\ntwo\na b\nLast <p> © A"
        );
    }

    #[test]
    fn preformatted_text_keeps_its_whitespace() {
        let html = "<p>intro</p><pre>line 1\n    indented\n\n\nline 3</pre><p>outro</p>";
        assert_eq!(
            html_to_text(html).text,
            "intro\nline 1\n    indented\n\n\nline 3\noutro"
        );
    }

    #[test]
    fn tags_with_quoted_angle_brackets_and_lone_brackets_survive() {
        let html = r#"<p data-x="a > b">1 < 2 and 3 > 2</p><p>&unknown; &#0; &#xD800;</p>"#;
        assert_eq!(html_to_text(html).text, "1 < 2 and 3 > 2\n&unknown; &#0; &#xD800;");
    }

    #[test]
    fn unterminated_skipped_elements_and_comments_do_not_leak_text() {
        assert_eq!(html_to_text("<p>visible</p><script>var secret = 1;").text, "visible");
        assert_eq!(html_to_text("<p>visible</p><!-- never closed").text, "visible");
        assert_eq!(html_to_text("<P>Upper<BR>case</P>").text, "Upper\ncase");
    }

    #[test]
    fn plain_text_without_markup_is_collapsed_not_lost() {
        assert_eq!(html_to_text("  just   some\n\n\n text  ").text, "just some text");
        assert_eq!(html_to_text("").text, "");
        assert_eq!(html_to_text("<br><br><br>").text, "");
    }

    #[test]
    fn entities_decode_named_numeric_and_keep_unknown() {
        assert_eq!(
            decode_entities("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x26; &nbsp;f"),
            "a & b <c> \"d\" 'e' & \u{a0}f"
        );
        assert_eq!(decode_entities("no entities here"), "no entities here");
        assert_eq!(
            decode_entities("&notanentity; & alone &amp"),
            "&notanentity; & alone &amp"
        );
        assert_eq!(
            decode_entities("&#1114112;"),
            "&#1114112;",
            "out of range code points are kept"
        );
    }

    #[test]
    fn ampersand_floods_decode_in_linear_time() {
        // Each ampersand looks at most a reference length ahead, so a flood
        // costs a bounded amount per character; the old scan to the next `;`
        // was quadratic and took tens of seconds at this size.
        let flood = "&".repeat(300 * 1024);
        let started = std::time::Instant::now();
        assert_eq!(decode_entities(&flood), flood);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let mixed = format!("{}{}&lt;", "&amp;".repeat(1000), "&".repeat(100_000));
        let decoded = decode_entities(&mixed);
        assert!(decoded.starts_with("&&&&") && decoded.ends_with("&<"));
        assert_eq!(decoded.len(), 1000 + 100_000 + 1);
    }

    #[test]
    fn sniffs_meta_charset_in_either_form() {
        assert_eq!(
            sniff_html_charset(br#"<html><head><meta charset="ISO-8859-1"><title>x</title>"#).as_deref(),
            Some("iso-8859-1")
        );
        assert_eq!(
            sniff_html_charset(br#"<meta http-equiv="Content-Type" content="text/html; charset=Shift_JIS">"#)
                .as_deref(),
            Some("shift_jis")
        );
        assert_eq!(sniff_html_charset(b"<p>charset=utf-8 appears in text only</p>"), None);
        assert_eq!(
            sniff_html_charset(b"<meta name=\"x\"><p>charset=utf-8 after the meta tag closed</p>"),
            None
        );
        assert_eq!(sniff_html_charset(b"<html><body>no charset</body></html>"), None);
    }

    #[test]
    fn titles_are_capped_on_a_character_boundary() {
        let html = format!("<title>{}</title><p>body</p>", "é".repeat(MAX_TITLE_BYTES));
        let extracted = html_to_text(&html);
        let title = extracted.title.expect("title");
        assert_eq!(title.len(), MAX_TITLE_BYTES);
        assert_eq!(title, "é".repeat(MAX_TITLE_BYTES / 2));
        assert_eq!(extracted.text, "body");
        assert_eq!(truncate_to_char_boundary("aéb", 2), "a");
        assert_eq!(truncate_to_char_boundary("aéb", 3), "aé");
        assert_eq!(truncate_to_char_boundary("aéb", 10), "aéb");
    }
}
