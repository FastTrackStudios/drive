//! Templates: `{name}` `{ext}` `{year}` `{month}` `{day}` `{kind}`
//! `{project}` `{device}` filled from a file's facts. Unknown tokens stay
//! as written, so a typo shows in the dry run instead of vanishing.

/// What a template is filled from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tokens {
    /// The name without its extension.
    pub name: String,
    pub ext: String,
    /// Milliseconds: the date `{year}` `{month}` `{day}` are of.
    pub date: i64,
    pub kind: String,
    pub project: String,
    pub device: String,
}

pub const ALL: [&str; 8] = [
    "{name}",
    "{ext}",
    "{year}",
    "{month}",
    "{day}",
    "{kind}",
    "{project}",
    "{device}",
];

/// Fill a template.
pub fn render(template: &str, t: &Tokens) -> String {
    let (y, m, d) = civil(t.date);
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        let Some(j) = after.find('}') else {
            out.push_str(after);
            rest = "";
            break;
        };
        let token = &after[..=j];
        match token {
            "{name}" => out.push_str(&t.name),
            "{ext}" => out.push_str(&t.ext),
            "{year}" => out.push_str(&format!("{y:04}")),
            "{month}" => out.push_str(&format!("{m:02}")),
            "{day}" => out.push_str(&format!("{d:02}")),
            "{kind}" => out.push_str(&t.kind),
            "{project}" => out.push_str(&t.project),
            "{device}" => out.push_str(&t.device),
            other => out.push_str(other),
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    out
}

/// A name template's output as a file name: `{name}.{ext}` style
/// templates with an empty `{ext}` lose the trailing dot.
pub fn file_name(template: &str, t: &Tokens) -> String {
    let s = render(template, t);
    let s = s.trim().trim_end_matches('.');
    crate::names::clean_piece(s)
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian, UTC
/// (Howard Hinnant's `civil_from_days`).
pub fn civil(ms: i64) -> (i64, u32, u32) {
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD` → milliseconds at its midnight, UTC.
pub fn day_ms(day: &str) -> Option<i64> {
    let mut it = day.trim().splitn(3, '-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        let ms = day_ms("2026-11-21").unwrap();
        assert_eq!(civil(ms), (2026, 11, 21));
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(
            civil(day_ms("2024-02-29").unwrap() + 3_600_000),
            (2024, 2, 29)
        );
        assert!(day_ms("2026-13-01").is_none());
    }

    #[test]
    fn tokens_fill() {
        let t = Tokens {
            name: "Take 3".into(),
            ext: "wav".into(),
            date: day_ms("2026-03-07").unwrap(),
            kind: "Audio".into(),
            project: "Song A".into(),
            device: "Voyager".into(),
        };
        assert_eq!(
            render("{year}/{month}/{project} - {name} ({device}).{ext}", &t),
            "2026/03/Song A - Take 3 (Voyager).wav"
        );
        assert_eq!(render("{kind} {nope} {", &t), "Audio {nope} {");
        assert_eq!(file_name("{name}/x.{ext}", &t), "Take 3-x.wav");
        let no_ext = Tokens {
            ext: String::new(),
            ..t
        };
        assert_eq!(file_name("{name}.{ext}", &no_ext), "Take 3");
    }
}
