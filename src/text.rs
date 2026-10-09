//! Outbound text shaping: markdown -> `org.matrix.custom.html` HTML, and
//! char-boundary chunking so one long answer arrives as readable events.
//!
//! `md_to_html` renders markdown and flattens any inline raw HTML to text,
//! so model output can never inject markup the homeserver would honor.
use pulldown_cmark::{Event, Options, Parser};

/// Render markdown to Matrix's `org.matrix.custom.html` subset.
/// Raw `<tag>` input is shown literally, never passed through.
pub fn md_to_html(text: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(text, opts).map(|event| match event {
        Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
        other => other,
    });
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, parser);
    out.trim_end().to_string()
}

/// Largest char-boundary prefix of `s` within `limit` chars.
fn chunk_paragraph(paragraph: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = paragraph;
    while !rest.is_empty() {
        if rest.chars().count() <= limit {
            chunks.push(rest.to_string());
            break;
        }
        let byte_limit = rest
            .char_indices()
            .nth(limit)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let mut split_at = byte_limit;
        if let Some(space) = rest[..byte_limit].rfind(' ') {
            if space > byte_limit / 3 {
                split_at = space;
            }
        }
        chunks.push(rest[..split_at].trim_end().to_string());
        rest = rest[split_at..].trim_start();
    }
    chunks
}

/// Split an answer into events of at most `limit` chars each, preferring
/// blank-line paragraph boundaries. Never returns an empty vec when the
/// input is non-empty.
pub fn split_message(content: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for paragraph in content.split("\n\n") {
        let candidate = if current.is_empty() {
            paragraph.to_string()
        } else {
            format!("{current}\n\n{paragraph}")
        };
        if candidate.chars().count() <= limit {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            chunks.push(current.clone());
            current.clear();
        }
        for piece in chunk_paragraph(paragraph, limit) {
            if current.is_empty() {
                current = piece;
            } else {
                let candidate = format!("{current}\n\n{piece}");
                if candidate.chars().count() <= limit {
                    current = candidate;
                } else {
                    chunks.push(std::mem::take(&mut current));
                    current = piece;
                }
            }
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() && !content.is_empty() {
        chunks.push(content.chars().take(limit).collect());
    }
    chunks
}

/// Strip the quoted-reply fallback a Matrix client prepends to reply
/// bodies: leading `> <@mxid> quoted` lines, then a blank line. A body
/// that is only quotes (no blank-line-separated text) is not a fallback
/// and is returned unchanged.
pub fn strip_reply_fallback(body: &str) -> &str {
    if !body.starts_with('>') {
        return body;
    }
    let mut rest = body;
    loop {
        if rest.is_empty() {
            return body;
        }
        let line = rest.lines().next().unwrap_or("");
        if line.starts_with('>') {
            rest = &rest[(line.len() + 1).min(rest.len())..];
            continue;
        }
        if line.trim().is_empty() {
            return rest[line.len()..].trim_start_matches(['\n', '\r']);
        }
        // Quote lines with no blank-line separator are prose, not fallback.
        return body;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_becomes_html() {
        let html = md_to_html("**bold** `code` [t](https://u)");
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("<a href=\"https://u\">"));
    }

    #[test]
    fn raw_html_is_literal() {
        let html = md_to_html("a <b>not bold</b>");
        assert!(!html.contains("<b>"));
        assert!(html.contains("&lt;b&gt;"));
    }

    #[test]
    fn chunking_respects_chars() {
        let chunks = split_message(&"word ".repeat(4000), 8000);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.chars().count() <= 8000));
    }

    #[test]
    fn fallback_stripped() {
        assert_eq!(
            strip_reply_fallback("> <@a:b> quoted\n\nreal"),
            "real"
        );
        assert_eq!(strip_reply_fallback("plain"), "plain");
        assert_eq!(strip_reply_fallback("> not a reply"), "> not a reply");
    }
}
