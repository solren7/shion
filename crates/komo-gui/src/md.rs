//! Markdown → HTML for agent replies.
//!
//! Agent output can contain text pulled from web pages (an indirect-injection
//! surface), and this HTML goes straight into the webview via
//! `dangerous_inner_html`. So raw HTML embedded in the markdown is neutralized —
//! rendered as literal text rather than passed through — while markdown syntax
//! still renders. Everything pulldown-cmark emits as `Text` is HTML-escaped by
//! `push_html`, so mapping raw-HTML events to `Text` is the whole defense.

use pulldown_cmark::{Event, Options, Parser, html};

/// Render `md` to a safe HTML fragment (no raw/inline HTML passthrough).
pub fn to_html(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(md, opts).map(|ev| match ev {
        Event::Html(raw) => Event::Text(raw),
        Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_basic_markdown() {
        let html = to_html("**bold** and `code`");
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<code>code</code>"));
    }

    #[test]
    fn raw_html_is_escaped_not_passed_through() {
        let html = to_html("<script>alert(1)</script>\n\nhi");
        assert!(
            !html.contains("<script>"),
            "raw HTML must not reach the webview: {html}"
        );
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn inline_html_is_escaped() {
        let html = to_html("a <img src=x onerror=alert(1)> b");
        assert!(
            !html.contains("<img"),
            "inline HTML must be escaped: {html}"
        );
    }
}
