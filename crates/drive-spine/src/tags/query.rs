//! The search language: tags, names, paths, kinds and fields, combined.
//!
//! ```text
//! drums                    a tag by its name, shorthand or an alias, and every
//!                          tag under it; a word that names no tag matches the
//!                          names of things instead (so plain words search as
//!                          they always have)
//! drums snare              implicit AND
//! drums OR keys            OR (operators in any case: and, or, not)
//! NOT archived             NOT
//! (drums OR keys) NOT wip  parentheses
//! "bass guitar"            quotes, or underscores: bass_guitar
//! tag:mix                  only a tag (no name fallback)
//! name:take                the name contains it
//! path:Mixes/  path:*.wav  the path contains it, or matches the glob (* ?)
//! kind:audio               a kind (its broader kinds count: kind:audio finds MIDI)
//! special:untagged         no tags; special:empty: no tags and no fields
//! artist:jane              a field named Artist holding "jane"
//! ```
//!
//! Text matches are smartcase: all lower case matches any case; a capital
//! anywhere makes the match case-sensitive.
//!
//! Things with a hidden tag (Archived) stay out unless the search names
//! that tag.

use std::collections::BTreeSet;

use super::{Library, OnTarget};

/// A parsed search.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Term(Term),
}

/// One condition.
#[derive(Clone, Debug, PartialEq)]
pub enum Term {
    /// A tag by any of its names, else the thing's name.
    Word(String),
    /// Only a tag.
    Tag(String),
    Name(String),
    Path(String),
    Kind(String),
    Special(Special),
    Field {
        name: String,
        value: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Special {
    /// No tags.
    Untagged,
    /// No tags and no fields.
    Empty,
}

/// What is wrong with a search, and where (a character offset).
#[derive(Clone, Debug, PartialEq)]
pub struct ParseError {
    pub at: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

// ── lexing ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Open,
    Close,
    And,
    Or,
    Not,
    Term(Term),
}

fn err<T>(at: usize, message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError {
        at,
        message: message.into(),
    })
}

fn is_key(k: &str) -> bool {
    let mut c = k.chars();
    c.next().is_some_and(|f| f.is_alphabetic())
        && c.all(|x| x.is_alphanumeric() || x == '_' || x == '-')
}

fn term(key: Option<&str>, value: String, at: usize) -> Result<Term, ParseError> {
    let Some(key) = key else {
        return Ok(Term::Word(value));
    };
    Ok(match key.to_lowercase().as_str() {
        "tag" => Term::Tag(value),
        "name" => Term::Name(value),
        "path" => Term::Path(value),
        "kind" => Term::Kind(value),
        "special" => match value.to_lowercase().as_str() {
            "untagged" => Term::Special(Special::Untagged),
            "empty" => Term::Special(Special::Empty),
            other => {
                return err(
                    at,
                    format!("special:{other} is not one Drive knows (untagged, empty)"),
                );
            }
        },
        _ => Term::Field {
            name: key.replace('_', " "),
            value,
        },
    })
}

fn lex(q: &str) -> Result<Vec<(usize, Tok)>, ParseError> {
    let chars: Vec<char> = q.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let quoted = |i: &mut usize| -> Result<String, ParseError> {
        let start = *i;
        *i += 1;
        let mut s = String::new();
        while *i < chars.len() && chars[*i] != '"' {
            s.push(chars[*i]);
            *i += 1;
        }
        if *i >= chars.len() {
            return err(start, "a quote is not closed");
        }
        *i += 1;
        Ok(s)
    };
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let at = i;
        match c {
            '(' => {
                out.push((at, Tok::Open));
                i += 1;
            }
            ')' => {
                out.push((at, Tok::Close));
                i += 1;
            }
            '"' => {
                let s = quoted(&mut i)?;
                out.push((at, Tok::Term(Term::Word(s))));
            }
            _ => {
                let mut raw = String::new();
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], '(' | ')' | '"')
                {
                    raw.push(chars[i]);
                    i += 1;
                }
                // key:"quoted value"
                if let Some(key) = raw.strip_suffix(':')
                    && is_key(key)
                    && i < chars.len()
                    && chars[i] == '"'
                {
                    let v = quoted(&mut i)?;
                    out.push((at, Tok::Term(term(Some(key), v, at)?)));
                    continue;
                }
                if let Some((key, value)) = raw.split_once(':')
                    && is_key(key)
                {
                    if value.is_empty() {
                        return err(at, format!("{key}: needs something after it"));
                    }
                    out.push((at, Tok::Term(term(Some(key), value.to_string(), at)?)));
                    continue;
                }
                let tok = match raw.to_lowercase().as_str() {
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "not" => Tok::Not,
                    _ => Tok::Term(Term::Word(raw)),
                };
                out.push((at, tok));
            }
        }
    }
    Ok(out)
}

// ── parsing ─────────────────────────────────────────────────────────────

struct Parser {
    toks: Vec<(usize, Tok)>,
    pos: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(_, t)| t)
    }

    fn at(&self) -> usize {
        self.toks.get(self.pos).map_or(self.end, |(a, _)| *a)
    }

    fn or(&mut self) -> Result<Expr, ParseError> {
        let mut parts = vec![self.and()?];
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            if matches!(self.peek(), None | Some(Tok::Close | Tok::Or | Tok::And)) {
                return err(self.at(), "OR needs something after it");
            }
            parts.push(self.and()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Expr::Or(parts)
        })
    }

    fn and(&mut self) -> Result<Expr, ParseError> {
        let mut parts = vec![self.unary()?];
        loop {
            match self.peek() {
                None | Some(Tok::Close | Tok::Or) => break,
                Some(Tok::And) => {
                    self.pos += 1;
                    if matches!(self.peek(), None | Some(Tok::Close | Tok::Or | Tok::And)) {
                        return err(self.at(), "AND needs something after it");
                    }
                }
                _ => {}
            }
            parts.push(self.unary()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Expr::And(parts)
        })
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        match self.peek() {
            Some(Tok::Not) => {
                self.pos += 1;
                if matches!(self.peek(), None | Some(Tok::Close | Tok::Or | Tok::And)) {
                    return err(self.at(), "NOT needs something after it");
                }
                Ok(Expr::Not(Box::new(self.unary()?)))
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let at = self.at();
        match self.toks.get(self.pos).map(|(_, t)| t.clone()) {
            Some(Tok::Open) => {
                self.pos += 1;
                if self.peek() == Some(&Tok::Close) {
                    return err(at, "() has nothing inside");
                }
                let e = self.or()?;
                if self.peek() != Some(&Tok::Close) {
                    return err(at, "a ( is not closed");
                }
                self.pos += 1;
                Ok(e)
            }
            Some(Tok::Term(t)) => {
                self.pos += 1;
                Ok(Expr::Term(t))
            }
            Some(Tok::Close) => err(at, "a ) has no ("),
            Some(Tok::And) => err(at, "AND needs something before it"),
            Some(Tok::Or) => err(at, "OR needs something before it"),
            Some(Tok::Not) | None => err(at, "something is missing at the end"),
        }
    }
}

/// Parse a search (`None`: nothing to search for).
pub fn parse(q: &str) -> Result<Option<Expr>, ParseError> {
    let toks = lex(q)?;
    if toks.is_empty() {
        return Ok(None);
    }
    let mut p = Parser {
        toks,
        pos: 0,
        end: q.chars().count(),
    };
    let e = p.or()?;
    if p.pos < p.toks.len() {
        // Only a stray ) stops the top level early.
        return err(p.at(), "a ) has no (");
    }
    Ok(Some(e))
}

// ── matching ────────────────────────────────────────────────────────────

/// One thing to match: what it is called, where, what kind, and what is on
/// it.
#[derive(Clone, Copy, Debug)]
pub struct Subject<'a> {
    pub name: &'a str,
    /// Its path where it lives (in the Drive, or in its location).
    pub path: &'a str,
    /// Its kind's ids and names, and its broader kinds' (`midi`, `MIDI`,
    /// `audio`, `Audio`).
    pub kinds: &'a [String],
    pub on: &'a OnTarget,
}

/// Text matched smartcase: lower case matches any case.
#[derive(Clone, Debug, PartialEq)]
struct Text {
    needle: String,
    exact_case: bool,
}

impl Text {
    fn new(s: &str) -> Text {
        let exact_case = s.chars().any(char::is_uppercase);
        Text {
            needle: if exact_case {
                s.to_string()
            } else {
                s.to_lowercase()
            },
            exact_case,
        }
    }

    fn fold(&self, hay: &str) -> String {
        if self.exact_case {
            hay.to_string()
        } else {
            hay.to_lowercase()
        }
    }

    fn contained_in(&self, hay: &str) -> bool {
        self.fold(hay).contains(&self.needle)
    }

    fn globs(&self, hay: &str) -> bool {
        let p: Vec<char> = self.needle.chars().collect();
        let h: Vec<char> = self.fold(hay).chars().collect();
        glob(&p, &h)
    }
}

/// `*` any run (slashes too), `?` one character; the whole text must match.
fn glob(p: &[char], h: &[char]) -> bool {
    let (mut pi, mut hi) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while hi < h.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == h[hi]) {
            pi += 1;
            hi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, hi));
            pi += 1;
        } else if let Some((sp, sh)) = star {
            pi = sp + 1;
            hi = sh + 1;
            star = Some((sp, sh + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[derive(Clone, Debug, PartialEq)]
enum C {
    And(Vec<C>),
    Or(Vec<C>),
    Not(Box<C>),
    /// Any of these tags (each tag and everything under it).
    Tags(BTreeSet<String>),
    Name(Text),
    PathHas(Text),
    PathGlob(Text),
    Kind(String),
    Untagged,
    Empty,
    /// A field's name, and its value as typed and with underscores as spaces.
    Field(String, Text, Text),
}

fn spaced(s: &str) -> String {
    s.replace('_', " ").to_lowercase()
}

fn compile(e: &Expr, lib: &Library, named: &mut BTreeSet<String>) -> C {
    match e {
        Expr::And(v) => C::And(v.iter().map(|x| compile(x, lib, named)).collect()),
        Expr::Or(v) => C::Or(v.iter().map(|x| compile(x, lib, named)).collect()),
        Expr::Not(x) => C::Not(Box::new(compile(x, lib, named))),
        Expr::Term(t) => match t {
            Term::Word(w) | Term::Tag(w) => {
                let ids = lib.resolve(w);
                if ids.is_empty() {
                    return match t {
                        Term::Word(_) => C::Name(Text::new(w)),
                        _ => C::Tags(BTreeSet::new()),
                    };
                }
                named.extend(ids.iter().cloned());
                C::Tags(ids.iter().flat_map(|id| lib.descendants(id)).collect())
            }
            Term::Name(w) => C::Name(Text::new(w)),
            Term::Path(p) if p.contains(['*', '?']) => C::PathGlob(Text::new(p)),
            Term::Path(p) => C::PathHas(Text::new(p)),
            Term::Kind(k) => C::Kind(spaced(k)),
            Term::Special(Special::Untagged) => C::Untagged,
            Term::Special(Special::Empty) => C::Empty,
            Term::Field { name, value } => C::Field(
                spaced(name),
                Text::new(value),
                Text::new(&value.replace('_', " ")),
            ),
        },
    }
}

fn eval(c: &C, s: &Subject) -> bool {
    match c {
        C::And(v) => v.iter().all(|x| eval(x, s)),
        C::Or(v) => v.iter().any(|x| eval(x, s)),
        C::Not(x) => !eval(x, s),
        C::Tags(set) => s.on.tags.iter().any(|a| set.contains(&a.tag)),
        C::Name(t) => t.contained_in(s.name),
        C::PathHas(t) => t.contained_in(s.path),
        C::PathGlob(t) => t.globs(s.path),
        C::Kind(k) => s.kinds.iter().any(|x| spaced(x) == *k),
        C::Untagged => s.on.tags.is_empty(),
        C::Empty => s.on.is_empty(),
        C::Field(name, t, t2) => s.on.fields.iter().any(|f| {
            spaced(&f.name) == *name && (t.contained_in(&f.value) || t2.contained_in(&f.value))
        }),
    }
}

/// A search ready to run against one library.
#[derive(Clone, Debug, PartialEq)]
pub struct Search {
    c: C,
    expr: Expr,
    /// Hidden tags (and those under them) the search does not name: things
    /// with one stay out.
    hide: BTreeSet<String>,
}

impl Search {
    /// Parse and resolve a search (`Ok(None)`: it is empty).
    pub fn new(q: &str, lib: &Library) -> Result<Option<Search>, ParseError> {
        Ok(parse(q)?.map(|e| Search::from_expr(e, lib)))
    }

    pub fn from_expr(expr: Expr, lib: &Library) -> Search {
        let mut named = BTreeSet::new();
        let c = compile(&expr, lib, &mut named);
        let shown: BTreeSet<String> = named.iter().flat_map(|n| lib.descendants(n)).collect();
        let hide = lib
            .all()
            .into_iter()
            .filter(|t| t.hidden)
            .flat_map(|t| lib.descendants(&t.id))
            .filter(|t| !shown.contains(t))
            .collect();
        Search { c, expr, hide }
    }

    /// Things carrying a tag, or one under it (the Tags place).
    pub fn for_tag(id: &str, lib: &Library) -> Search {
        let mut s = Search::from_expr(Expr::Term(Term::Tag(id.to_string())), lib);
        s.c = C::Tags(lib.descendants(id));
        s.hide.retain(|h| !lib.descendants(id).contains(h));
        s
    }

    pub fn expr(&self) -> &Expr {
        &self.expr
    }

    pub fn matches(&self, s: &Subject) -> bool {
        eval(&self.c, s) && !s.on.tags.iter().any(|a| self.hide.contains(&a.tag))
    }

    /// Whether it asks about tags or fields at all (and not only names):
    /// plain words that name no tag are a name search, as before tags.
    pub fn is_plain(&self) -> bool {
        fn plain(c: &C) -> bool {
            match c {
                C::Name(_) => true,
                C::And(v) => v.iter().all(plain),
                _ => false,
            }
        }
        plain(&self.c)
    }

    /// What to ask a server's name index for, to find candidates the tags
    /// alone cannot (files on computers are found by name, path piece or
    /// kind, then matched here).
    pub fn hints(&self) -> Hints {
        let mut h = Hints::default();
        hint(&self.c, &mut h, false);
        h.words.sort();
        h.words.dedup();
        h.kinds.sort();
        h.kinds.dedup();
        h
    }
}

/// What a search needs fetched.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hints {
    /// Name pieces (lower case) to look files up by.
    pub words: Vec<String>,
    /// Kinds to list files of.
    pub kinds: Vec<String>,
    /// It asks about tags: fetch the tagged files.
    pub tagged: bool,
    /// It can match things no name or kind finds (NOT, untagged, fields):
    /// only what is at hand (the Drive, tagged files) is searched.
    pub open: bool,
}

fn hint(c: &C, h: &mut Hints, negated: bool) {
    match c {
        C::And(v) | C::Or(v) => v.iter().for_each(|x| hint(x, h, negated)),
        C::Not(x) => {
            h.open = true;
            hint(x, h, !negated);
        }
        C::Tags(_) => h.tagged = true,
        C::Name(t) | C::PathHas(t) if !negated => h.words.push(t.needle.to_lowercase()),
        C::PathGlob(t) if !negated => {
            // The longest run without wildcards (`.wav` of `*.wav`).
            let best = t
                .needle
                .split(['*', '?'])
                .map(|p| p.rsplit('/').next().unwrap_or(p))
                .max_by_key(|p| p.len())
                .unwrap_or_default();
            if best.len() >= 2 {
                h.words.push(best.to_lowercase());
            } else {
                h.open = true;
            }
        }
        C::Kind(k) if !negated => h.kinds.push(k.clone()),
        C::Untagged | C::Empty | C::Field(..) => {
            h.open = true;
            h.tagged = true;
        }
        _ => h.open = true,
    }
}

/// A kind's words for [`Subject::kinds`]: its id and name, and its broader
/// kind's.
pub fn kind_words(kinds: &crate::kinds::Kinds, kind: &crate::kinds::Kind) -> Vec<String> {
    let mut v = vec![kind.id.clone(), kind.name.clone()];
    if !kind.parent.is_empty() {
        let p = kinds.get(&kind.parent);
        v.push(p.id.clone());
        v.push(p.name.clone());
    }
    v
}

/// The extensions of the kinds a `kind:` hint names (by id or name), and
/// the kinds under them.
pub fn kind_extensions(kinds: &crate::kinds::Kinds, hint: &str) -> Vec<String> {
    let h = spaced(hint);
    kinds
        .all()
        .iter()
        .filter(|k| spaced(&k.id) == h || spaced(&k.name) == h)
        .flat_map(|k| kinds.extensions_of(&k.id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags::{self, Assigned, Color, Field, FieldKind, NewTag, Target};
    use loro::LoroDoc;

    fn w(s: &str) -> Expr {
        Expr::Term(Term::Word(s.into()))
    }

    #[test]
    fn parses_the_grammar() {
        assert_eq!(parse("").unwrap(), None);
        assert_eq!(parse("   ").unwrap(), None);
        assert_eq!(parse("drums").unwrap(), Some(w("drums")));
        assert_eq!(
            parse("drums snare").unwrap(),
            Some(Expr::And(vec![w("drums"), w("snare")]))
        );
        assert_eq!(
            parse("drums AND snare").unwrap(),
            parse("drums snare").unwrap()
        );
        assert_eq!(
            parse("a or b and c").unwrap(),
            Some(Expr::Or(vec![w("a"), Expr::And(vec![w("b"), w("c")])]))
        );
        assert_eq!(
            parse("not a b").unwrap(),
            Some(Expr::And(vec![Expr::Not(Box::new(w("a"))), w("b")]))
        );
        assert_eq!(
            parse("(a OR b) NOT c").unwrap(),
            Some(Expr::And(vec![
                Expr::Or(vec![w("a"), w("b")]),
                Expr::Not(Box::new(w("c")))
            ]))
        );
        assert_eq!(
            parse("NOT NOT a").unwrap(),
            Some(Expr::Not(Box::new(Expr::Not(Box::new(w("a"))))))
        );
        assert_eq!(parse("((a))").unwrap(), Some(w("a")));
        assert_eq!(parse("\"bass guitar\"").unwrap(), Some(w("bass guitar")));
        // A quoted operator is a word.
        assert_eq!(parse("\"or\"").unwrap(), Some(w("or")));
    }

    #[test]
    fn parses_prefixes() {
        assert_eq!(
            parse("path:*.wav").unwrap(),
            Some(Expr::Term(Term::Path("*.wav".into())))
        );
        assert_eq!(
            parse("path:\"Song A/Mixes\"").unwrap(),
            Some(Expr::Term(Term::Path("Song A/Mixes".into())))
        );
        assert_eq!(
            parse("KIND:audio").unwrap(),
            Some(Expr::Term(Term::Kind("audio".into())))
        );
        assert_eq!(
            parse("special:untagged").unwrap(),
            Some(Expr::Term(Term::Special(Special::Untagged)))
        );
        assert_eq!(
            parse("special:Empty").unwrap(),
            Some(Expr::Term(Term::Special(Special::Empty)))
        );
        assert_eq!(
            parse("tag:mix name:take").unwrap(),
            Some(Expr::And(vec![
                Expr::Term(Term::Tag("mix".into())),
                Expr::Term(Term::Name("take".into()))
            ]))
        );
        assert_eq!(
            parse("date_created:2026-11").unwrap(),
            Some(Expr::Term(Term::Field {
                name: "date created".into(),
                value: "2026-11".into()
            }))
        );
        // Not a key: a time, a leading colon.
        assert_eq!(parse("12:30").unwrap(), Some(w("12:30")));
        assert_eq!(parse(":x").unwrap(), Some(w(":x")));
        // Brackets end a value.
        assert_eq!(
            parse("(kind:audio)").unwrap(),
            Some(Expr::Term(Term::Kind("audio".into())))
        );
    }

    #[test]
    fn says_what_is_wrong() {
        for (q, msg) in [
            ("a AND", "AND needs something after it"),
            ("AND a", "AND needs something before it"),
            ("a OR", "OR needs something after it"),
            ("OR a", "OR needs something before it"),
            ("NOT", "NOT needs something after it"),
            ("a NOT", "NOT needs something after it"),
            ("(a", "a ( is not closed"),
            ("a)", "a ) has no ("),
            ("()", "() has nothing inside"),
            ("\"open", "a quote is not closed"),
            ("path:", "path: needs something after it"),
            (
                "special:nope",
                "special:nope is not one Drive knows (untagged, empty)",
            ),
        ] {
            assert_eq!(parse(q).unwrap_err().message, msg, "{q}");
        }
        assert_eq!(parse("abc )").unwrap_err().at, 4);
    }

    #[test]
    fn globs() {
        let g = |p: &str, h: &str| {
            glob(
                &p.chars().collect::<Vec<_>>(),
                &h.chars().collect::<Vec<_>>(),
            )
        };
        assert!(g("*.wav", "Song A/Mixes/v2.wav"));
        assert!(!g("*.wav", "Song A/Mixes/v2.wav.asd"));
        assert!(g("*/mixes/*", "song a/mixes/v2.wav"));
        assert!(g("v?.wav", "v2.wav"));
        assert!(!g("v?.wav", "v10.wav"));
        assert!(g("*", ""));
        assert!(g("**", "a/b"));
        assert!(g("a*b*c", "aXXbYYc"));
        assert!(!g("a*b*c", "aXXbYY"));
    }

    struct Fixture {
        lib: Library,
    }

    /// Instruments ⊃ Drums ⊃ Snare; Keys (shorthand k, alias piano);
    /// two Freddys; Archived built in.
    fn fixture() -> (LoroDoc, Fixture, [String; 5]) {
        let d = LoroDoc::new();
        let new = |name: &str, parents: &[String]| {
            tags::create(
                &d,
                NewTag {
                    name,
                    parents,
                    ..NewTag::default()
                },
                "cody",
                1,
            )
            .unwrap()
        };
        let inst = new("Instruments", &[]);
        let drums = new("Drums", std::slice::from_ref(&inst));
        let snare = new("Snare", std::slice::from_ref(&drums));
        let keys = new("Keys", std::slice::from_ref(&inst));
        tags::set_shorthand(&d, &keys, "k").unwrap();
        tags::add_alias(&d, &keys, "Piano").unwrap();
        let bass = new("Bass Guitar", &[]);
        tags::set_color(&d, &bass, Color::Blue).unwrap();
        d.commit();
        let lib = Library::load(&d);
        (d, Fixture { lib }, [inst, drums, snare, keys, bass])
    }

    fn on(tags: &[&str], fields: &[(&str, &str)]) -> OnTarget {
        OnTarget {
            tags: tags
                .iter()
                .map(|t| Assigned {
                    tag: t.to_string(),
                    by: String::new(),
                    at: 0,
                })
                .collect(),
            fields: fields
                .iter()
                .map(|(n, v)| Field {
                    id: n.to_string(),
                    name: n.to_string(),
                    kind: FieldKind::Text,
                    value: v.to_string(),
                    by: String::new(),
                    at: 0,
                })
                .collect(),
        }
    }

    fn hit(q: &str, lib: &Library, name: &str, path: &str, kinds: &[&str], o: &OnTarget) -> bool {
        let kinds: Vec<String> = kinds.iter().map(|k| k.to_string()).collect();
        Search::new(q, lib).unwrap().unwrap().matches(&Subject {
            name,
            path,
            kinds: &kinds,
            on: o,
        })
    }

    #[test]
    fn tags_find_their_children_and_aliases() {
        let (_d, f, [_inst, drums, snare, keys, bass]) = fixture();
        let l = &f.lib;
        let snare_file = on(&[&snare], &[]);
        let keys_file = on(&[&keys], &[]);
        let bass_file = on(&[&bass], &[]);
        let h = |q: &str, o: &OnTarget| hit(q, l, "take.wav", "a/take.wav", &["audio"], o);
        // A parent finds its descendants.
        assert!(h("drums", &snare_file));
        assert!(h("instruments", &snare_file));
        assert!(h("INSTRUMENTS", &keys_file));
        assert!(!h("drums", &keys_file));
        // Shorthand and alias, any case.
        assert!(h("k", &keys_file));
        assert!(h("piano", &keys_file));
        assert!(h("PIANO", &keys_file));
        // Spaces by quotes or underscores.
        assert!(h("\"bass guitar\"", &bass_file));
        assert!(h("bass_guitar", &bass_file));
        assert!(h("tag:bass_guitar", &bass_file));
        // Operators.
        assert!(h("drums OR keys", &keys_file));
        assert!(!h("drums keys", &keys_file));
        assert!(h("instruments NOT drums", &keys_file));
        assert!(!h("instruments NOT drums", &snare_file));
        assert!(h("(drums OR bass_guitar) not keys", &bass_file));
        // A tag: term that names no tag finds nothing (no name fallback).
        assert!(!h("tag:take", &snare_file));
        let _ = drums;
    }

    #[test]
    fn words_that_name_no_tag_search_names() {
        let (_d, f, _) = fixture();
        let none = on(&[], &[]);
        let h = |q: &str, name: &str| hit(q, &f.lib, name, name, &[], &none);
        assert!(h("kick", "Kick 01.wav"));
        assert!(h("kick 01", "Kick 01.wav"));
        assert!(!h("kick 02", "Kick 01.wav"));
        // Smartcase.
        assert!(h("Kick", "Kick 01.wav"));
        assert!(!h("KICK", "Kick 01.wav"));
        assert!(h("kick_01", "kick_01.wav"));
        assert!(Search::new("kick 01", &f.lib).unwrap().unwrap().is_plain());
        assert!(
            !Search::new("kick drums", &f.lib)
                .unwrap()
                .unwrap()
                .is_plain()
        );
        assert!(
            !Search::new("kick OR snare_x", &f.lib)
                .unwrap()
                .unwrap()
                .is_plain()
        );
    }

    #[test]
    fn paths_kinds_specials_and_fields() {
        let (_d, f, [_, _, snare, ..]) = fixture();
        let l = &f.lib;
        let none = on(&[], &[]);
        let fielded = on(
            &[],
            &[("Artist", "Jane Doe"), ("Date created", "2026-11-21")],
        );
        let tagged = on(&[&snare], &[]);
        let p = "Song A/Mixes/v2.wav";
        let h =
            |q: &str, o: &OnTarget| hit(q, l, "v2.wav", p, &["midi", "MIDI", "audio", "Audio"], o);
        assert!(h("path:*.wav", &none));
        assert!(!h("path:*.aif", &none));
        assert!(h("path:mixes", &none));
        assert!(h("path:Mixes", &none));
        assert!(!h("path:MIXES", &none));
        assert!(h("path:\"song a/*\"", &none));
        assert!(h("path:*/Mixes/*.wav", &none));
        assert!(h("kind:audio", &none));
        assert!(h("kind:Audio", &none));
        assert!(h("kind:midi", &none));
        assert!(!h("kind:video", &none));
        assert!(h("special:untagged", &none));
        assert!(h("special:untagged", &fielded));
        assert!(!h("special:untagged", &tagged));
        assert!(h("special:empty", &none));
        assert!(!h("special:empty", &fielded));
        assert!(h("artist:jane", &fielded));
        assert!(h("artist:\"jane doe\"", &fielded));
        assert!(h("artist:jane_doe", &fielded));
        assert!(!h("artist:Bob", &fielded));
        assert!(h("date_created:2026-11", &fielded));
        assert!(!h("title:jane", &fielded));
        assert!(h("snare kind:audio path:*.wav", &tagged));
    }

    #[test]
    fn hidden_tags_stay_out_unless_named() {
        let (_d, f, [_, drums, ..]) = fixture();
        let l = &f.lib;
        let archived = on(&[&drums, tags::ARCHIVED], &[]);
        let h = |q: &str| hit(q, l, "old.wav", "old.wav", &[], &archived);
        assert!(!h("drums"));
        assert!(!h("old"));
        assert!(h("archived"));
        assert!(h("drums archived"));
        assert!(h("tag:archived"));
        // The Tags place for Archived shows them.
        let kinds: Vec<String> = Vec::new();
        assert!(Search::for_tag(tags::ARCHIVED, l).matches(&Subject {
            name: "old.wav",
            path: "old.wav",
            kinds: &kinds,
            on: &archived,
        }));
        assert!(!Search::for_tag(&drums, l).matches(&Subject {
            name: "old.wav",
            path: "old.wav",
            kinds: &kinds,
            on: &archived,
        }));
    }

    #[test]
    fn hints_say_what_to_fetch() {
        let (_d, f, _) = fixture();
        let hints = |q: &str| Search::new(q, &f.lib).unwrap().unwrap().hints();
        let h = hints("kick 01");
        assert_eq!(h.words, ["01", "kick"]);
        assert!(!h.tagged && !h.open);
        let h = hints("drums path:*.wav kind:audio");
        assert!(h.tagged);
        assert_eq!(h.words, [".wav"]);
        assert_eq!(h.kinds, ["audio"]);
        let h = hints("NOT drums");
        assert!(h.open && h.tagged && h.words.is_empty());
        assert!(hints("special:untagged").open);
    }

    #[test]
    fn kinds_by_id_or_name() {
        let kinds = crate::kinds::Kinds::from(Vec::new());
        let midi = kinds.get("midi");
        assert_eq!(kind_words(&kinds, midi), ["midi", "MIDI", "audio", "Audio"]);
        let exts = kind_extensions(&kinds, "Audio");
        assert!(exts.contains(&"wav".to_string()) && exts.contains(&"mid".to_string()));
        assert_eq!(
            kind_extensions(&kinds, "sample_instrument"),
            kind_extensions(&kinds, "instrument")
        );
    }

    #[test]
    fn a_target_round_trips_its_key() {
        for t in [
            Target::File("12@3".into()),
            Target::Content("abc".into()),
            Target::Doc("studio:song:x".into()),
            Target::Project("p1".into()),
        ] {
            assert_eq!(Target::parse(&t.key()), Some(t));
        }
        assert_eq!(Target::parse("file:"), None);
        assert_eq!(Target::parse("what:x"), None);
    }
}
