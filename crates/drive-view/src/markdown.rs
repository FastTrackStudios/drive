//! Markdown to HTML for the viewer, safe to put in a page: raw HTML in the
//! file is shown as text, and links and images only go where a link may
//! (`http`, `https`, `mailto`, the same site, an anchor).

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

fn safe_url(url: &str) -> bool {
    let u = url.trim_start().to_ascii_lowercase();
    match u.split_once(':') {
        // A scheme comes before any `/`, `?` or `#`.
        Some((scheme, _)) if !scheme.contains(['/', '?', '#']) => {
            matches!(scheme, "http" | "https" | "mailto")
        }
        _ => true,
    }
}

/// The HTML of a markdown text.
pub fn to_html(md: &str) -> String {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let events = Parser::new_ext(md, opts).map(|e| match e {
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) if !safe_url(&dest_url) => Event::Start(Tag::Link {
            link_type,
            dest_url: CowStr::Borrowed("#"),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) if !safe_url(&dest_url) => Event::Start(Tag::Image {
            link_type,
            dest_url: CowStr::Borrowed(""),
            title,
            id,
        }),
        e => e,
    });
    let mut out = String::with_capacity(md.len() * 3 / 2);
    html::push_html(&mut out, events);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_markdown() {
        let h = to_html("# Set\n\n- [x] Amazing Grace\n\n|a|b|\n|-|-|\n|1|2|\n");
        assert!(h.contains("<h1>Set</h1>"));
        assert!(h.contains("checkbox"));
        assert!(h.contains("<table>"));
    }

    #[test]
    fn raw_html_and_script_links_stay_harmless() {
        let h = to_html(
            "<script>alert(1)</script>\n\n[x](javascript:alert(1)) [y](JaVaScRiPt:alert(1)) ![i](data:text/html,hi) [ok](/s/a?x=1:2) [m](mailto:a@b.c)",
        );
        assert!(!h.contains("<script>"), "{h}");
        assert!(h.contains("&lt;script&gt;"));
        assert!(!h.to_lowercase().contains("javascript:"), "{h}");
        assert!(!h.contains("data:text"), "{h}");
        assert!(h.contains(r#"href="/s/a?x=1:2""#));
        assert!(h.contains("mailto:a@b.c"));
    }
}
