//! Render the agent's messages from Markdown to HTML for the admin UI.
//!
//! The text comes from a model, so it is untrusted: raw HTML in the message is shown as
//! text, images are reduced to their alt text, and links keep only `http`, `https` and
//! `mailto` targets. Everything else is escaped by the renderer.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

/// Whether a link target is safe to put in an `href`.
fn safe_link(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
}

/// Render Markdown to a safe HTML fragment.
pub fn render(markdown: &str) -> String {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    // Depth of links being dropped (their text is kept) and of images (alt text is kept).
    let mut dropped_links = 0usize;
    let events = Parser::new_ext(markdown, options).filter_map(move |event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Some(Event::Text(raw)),
        Event::Start(Tag::Link { ref dest_url, .. }) if !safe_link(dest_url) => {
            dropped_links += 1;
            None
        }
        Event::End(TagEnd::Link) if dropped_links > 0 => {
            dropped_links -= 1;
            None
        }
        Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => None,
        Event::Start(Tag::HtmlBlock) | Event::End(TagEnd::HtmlBlock) => None,
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            id,
            ..
        }) => Some(Event::Start(Tag::Link {
            link_type,
            dest_url,
            // Titles are not needed and are one less attribute to carry model text.
            title: CowStr::Borrowed(""),
            id,
        })),
        other => Some(other),
    });
    let mut out = String::with_capacity(markdown.len() * 3 / 2);
    html::push_html(&mut out, events);
    out
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn lists_and_emphasis_become_html() {
        let html = render("Done:\n\n- **Header** is dark blue\n- Added `contact.html`\n");
        assert!(html.contains("<ul>"));
        assert!(html.contains("<li><strong>Header</strong> is dark blue</li>"));
        assert!(html.contains("<code>contact.html</code>"));
    }

    #[test]
    fn raw_html_is_shown_as_text() {
        let html = render("Hi <script>alert(1)</script>\n\n<img src=x onerror=alert(1)>\n");
        assert!(!html.contains("<script"));
        assert!(!html.contains("<img"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&lt;img"));
    }

    #[test]
    fn only_web_and_mail_links_survive() {
        let html = render(
            "[ok](https://example.com \"t\") [bad](javascript:alert(1)) [data](data:text/html,x) ![alt](https://example.com/a.png)",
        );
        assert!(html.contains(r#"<a href="https://example.com">ok</a>"#));
        assert!(!html.contains("javascript:"));
        assert!(!html.contains("data:"));
        assert!(html.contains("bad"));
        assert!(!html.contains("<img"));
        assert!(html.contains("alt"));
    }

    #[test]
    fn attribute_breakouts_are_escaped() {
        let html = render("[x](https://example.com/\"onmouseover=\"alert(1))");
        assert!(!html.contains("\"onmouseover"));
    }
}
