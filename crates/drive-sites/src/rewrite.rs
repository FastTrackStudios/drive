//! A built site moved under a path. dodeca writes root-absolute URLs
//! (`/theory/`, `/main.css`, `/search/meta`), but a Drive site lives under
//! a prefix: `/sites/<space>/<id>/` on the node, `<public URL>/sites/<id>/`
//! in a bucket. This rewrites the URLs in its HTML and CSS, and adds a
//! small script so what the search widget fetches and links (paths stored
//! inside its index) gets the prefix too.
//!
//! `explicit_index`: a bucket does not turn `/theory/` into
//! `/theory/index.html`, so links name the file.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

/// How URLs in a built site are placed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Placement {
    /// `/sites/home/wiki` (no trailing slash); empty: the site is at the root.
    pub prefix: String,
    /// Name `index.html` in links to folders (static buckets need it).
    pub explicit_index: bool,
}

impl Placement {
    /// The prefix a base URL implies (`https://x.example/sites/wiki/` →
    /// `/sites/wiki`).
    pub fn from_base_url(base_url: &str, explicit_index: bool) -> Placement {
        let path = match base_url.split_once("://") {
            Some((_, rest)) => rest.find('/').map_or("", |i| &rest[i..]),
            None => base_url,
        };
        Placement {
            prefix: path.trim_end_matches('/').to_string(),
            explicit_index,
        }
    }

    fn is_identity(&self) -> bool {
        self.prefix.is_empty() && !self.explicit_index
    }

    /// One root-absolute URL, placed.
    pub fn url(&self, u: &str) -> String {
        if !u.starts_with('/') || u.starts_with("//") {
            return u.to_string();
        }
        if !self.prefix.is_empty()
            && (u == self.prefix || u.starts_with(&format!("{}/", self.prefix)))
        {
            return u.to_string();
        }
        let cut = u.find(['?', '#']).unwrap_or(u.len());
        let (path, rest) = u.split_at(cut);
        let mut path = path.to_string();
        if self.explicit_index {
            let last = path.rsplit('/').next().unwrap_or("");
            if path.ends_with('/') {
                path.push_str("index.html");
            } else if !last.contains('.') {
                path.push_str("/index.html");
            }
        }
        format!("{}{path}{rest}", self.prefix)
    }

    /// The script that places URLs the search widget makes at run time: what
    /// it fetches (index files: the prefix only) and the result links it
    /// adds (pages: `index.html` too when the placement names it).
    fn shim(&self) -> String {
        format!(
            "<script>(function(){{var B={b},X={x};function f(u,x){{if(typeof u!==\"string\"||u.charAt(0)!==\"/\"||u.charAt(1)===\"/\")return u;if(B&&(u===B||u.indexOf(B+\"/\")===0))return u;var i=u.search(/[?#]/),p=i<0?u:u.slice(0,i),r=i<0?\"\":u.slice(i);if(x){{var l=p.slice(p.lastIndexOf(\"/\")+1);if(p.charAt(p.length-1)===\"/\")p+=\"index.html\";else if(l.indexOf(\".\")<0)p+=\"/index.html\";}}return B+p+r}}var F=window.fetch;window.fetch=function(i,o){{return F.call(this,f(i,false),o)}};function fix(n){{if(!n.querySelectorAll)return;var a=[].slice.call(n.querySelectorAll(\"a[href]\"));if(n.matches&&n.matches(\"a[href]\"))a.push(n);a.forEach(function(e){{var h=e.getAttribute(\"href\"),g=f(h,X);if(g!==h)e.setAttribute(\"href\",g)}})}}new MutationObserver(function(m){{m.forEach(function(r){{r.addedNodes.forEach(fix)}})}}).observe(document.documentElement,{{childList:true,subtree:true}})}})();</script>",
            b = script_str(&self.prefix),
            x = self.explicit_index,
        )
    }
}

/// A JSON string literal that is safe inside an inline `<script>`: `<`
/// cannot start `</script>`, and U+2028/2029 cannot end a line.
fn script_str(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".into())
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

static ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(\s(?:href|src|action|poster|data-src)\s*=\s*)("([^"]*)"|'([^']*)')"#)
        .expect("a regex")
});
static SRCSET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(\ssrcset\s*=\s*)("([^"]*)"|'([^']*)')"#).expect("a regex"));
static CSS_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"url\(\s*(["']?)(/[^"')]*)(["']?)\s*\)"#).expect("a regex"));
static HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<head(\s[^>]*)?>").expect("a regex"));

fn css(p: &Placement, text: &str) -> String {
    CSS_URL
        .replace_all(text, |c: &regex::Captures| {
            format!("url({}{}{})", &c[1], p.url(&c[2]), &c[3])
        })
        .into_owned()
}

fn html(p: &Placement, text: &str) -> String {
    let t = ATTR.replace_all(text, |c: &regex::Captures| {
        let (q, v) = match (c.get(3), c.get(4)) {
            (Some(v), _) => ('"', v.as_str()),
            (_, Some(v)) => ('\'', v.as_str()),
            _ => return c[0].to_string(),
        };
        format!("{}{q}{}{q}", &c[1], p.url(v))
    });
    let t = SRCSET.replace_all(&t, |c: &regex::Captures| {
        let (q, v) = match (c.get(3), c.get(4)) {
            (Some(v), _) => ('"', v.as_str()),
            (_, Some(v)) => ('\'', v.as_str()),
            _ => return c[0].to_string(),
        };
        let placed: Vec<String> = v
            .split(',')
            .map(|item| {
                let item = item.trim();
                match item.split_once(char::is_whitespace) {
                    Some((u, d)) => format!("{} {}", p.url(u), d.trim()),
                    None => p.url(item),
                }
            })
            .collect();
        format!("{}{q}{}{q}", &c[1], placed.join(", "))
    });
    let t = css(p, &t);
    let shim = p.shim();
    match HEAD.find(&t) {
        Some(m) => format!("{}{shim}{}", &t[..m.end()], &t[m.end()..]),
        None => format!("{shim}{t}"),
    }
}

/// Place one output file (HTML and CSS change; anything else is as it was).
pub fn file(p: &Placement, path: &str, bytes: &[u8]) -> Vec<u8> {
    if p.is_identity() {
        return bytes.to_vec();
    }
    let lower = path.to_ascii_lowercase();
    let is_html = lower.ends_with(".html") || lower.ends_with(".htm");
    let is_css = lower.ends_with(".css");
    if !is_html && !is_css {
        return bytes.to_vec();
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    if is_html {
        html(p, text).into_bytes()
    } else {
        css(p, text).into_bytes()
    }
}

/// Place a whole output.
pub fn site(p: &Placement, files: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    files
        .iter()
        .map(|(k, v)| (k.clone(), file(p, k, v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_get_the_prefix_once() {
        let p = Placement::from_base_url("/sites/home/wiki/", false);
        assert_eq!(p.prefix, "/sites/home/wiki");
        assert_eq!(p.url("/theory/"), "/sites/home/wiki/theory/");
        assert_eq!(
            p.url("/sites/home/wiki/theory/"),
            "/sites/home/wiki/theory/"
        );
        assert_eq!(p.url("https://x.example/a"), "https://x.example/a");
        assert_eq!(p.url("//cdn.example/a"), "//cdn.example/a");
        assert_eq!(p.url("#top"), "#top");
        let b = Placement::from_base_url("https://media.example.com/sites/wiki", true);
        assert_eq!(b.prefix, "/sites/wiki");
        assert_eq!(b.url("/"), "/sites/wiki/index.html");
        assert_eq!(b.url("/theory"), "/sites/wiki/theory/index.html");
        assert_eq!(
            b.url("/theory/?q=1#x"),
            "/sites/wiki/theory/index.html?q=1#x"
        );
        assert_eq!(b.url("/main.123.css"), "/sites/wiki/main.123.css");
        assert!(Placement::from_base_url("https://wiki.example.com/", false).is_identity());
    }

    #[test]
    fn html_and_css_are_placed() {
        let p = Placement::from_base_url("/s/w", false);
        let page = r#"<html><head><link rel="stylesheet" href="/main.css"></head><body><a href='/theory'>T</a> <a href="https://x.y/">x</a><img src="/img/a.webp" srcset="/img/a-320.webp 320w, /img/a-640.webp 640w"><div style="background:url(/bg.png)"></div></body></html>"#;
        let out = String::from_utf8(file(&p, "index.html", page.as_bytes())).unwrap();
        assert!(out.contains(r#"href="/s/w/main.css""#), "{out}");
        assert!(out.contains("href='/s/w/theory'"), "{out}");
        assert!(out.contains(r#"href="https://x.y/""#), "{out}");
        assert!(
            out.contains(r#"srcset="/s/w/img/a-320.webp 320w, /s/w/img/a-640.webp 640w""#),
            "{out}"
        );
        assert!(out.contains("url(/s/w/bg.png)"), "{out}");
        assert!(out.starts_with("<html><head><script>"), "{out}");
        let sheet =
            String::from_utf8(file(&p, "main.css", b"a{background:url('/x.png')}")).unwrap();
        assert_eq!(sheet, "a{background:url('/s/w/x.png')}");
        assert_eq!(file(&p, "search/meta", b"/x"), b"/x");
    }

    #[test]
    fn shim_prefix_cannot_close_the_script() {
        let p = Placement {
            prefix: "/sites/a</script><script>alert(1)</script>\u{2028}".into(),
            explicit_index: false,
        };
        let shim = p.shim();
        assert_eq!(shim.matches("</script>").count(), 1, "{shim}");
        assert!(shim.ends_with("</script>"), "{shim}");
        assert!(!shim.contains('\u{2028}'), "{shim}");
        assert!(shim.contains(r#"var B="/sites/a\u003c/script>"#), "{shim}");
    }
}
