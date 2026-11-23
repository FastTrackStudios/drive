//! What the app takes from where it runs: the clock, randomness, the
//! node's address, HTTP to it, the clipboard and the page's scripts. One
//! set of calls for the browser build and the native one, over
//! `drive_dioxus::env`.

use drive_dioxus::env;

pub fn now_ms() -> f64 {
    env::now_ms()
}

/// A number in `0..1`.
pub fn random() -> f64 {
    let hex = env::random_hex32();
    u64::from_str_radix(hex.get(..13).unwrap_or("0"), 16).unwrap_or(0) as f64 / (1u64 << 52) as f64
}

/// A random id, 32 hex digits.
pub fn uuid() -> String {
    env::random_hex32().get(..32).unwrap_or_default().to_string()
}

/// (month 0–11, day, year) of a time in ms.
pub fn date_parts(ms: f64) -> (u32, u32, i64) {
    env::date_parts(ms)
}

/// The day of the week (0 Sunday) of a calendar date.
pub fn weekday(year: i64, month: u32, day: u32) -> usize {
    // Howard Hinnant's days_from_civil; 1970-01-01 was a Thursday.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days + 4).rem_euclid(7) as usize
}

pub fn enc(s: &str) -> String {
    env::encode_component(s)
}

/// The node's address (the page's origin in a browser).
pub fn origin() -> String {
    env::origin()
}

/// The node's host and port.
pub fn host() -> String {
    let o = origin();
    o.split_once("://").map_or(o.clone(), |(_, h)| h.to_string())
}

pub fn copy_text(text: &str) {
    env::copy_text(text);
}

/// A page script (nothing natively).
pub fn eval(js: &str) {
    env::eval(js);
}

/// One request to the node, signed in when this person is: its status
/// and the answer's bytes. A root path is the node's.
pub async fn request(
    method: &str,
    url: &str,
    content_type: Option<&str>,
    body: Option<Vec<u8>>,
) -> Option<(u16, Vec<u8>)> {
    let bearer = drive_dioxus::auth::bearer();
    let mut headers: Vec<(&str, &str)> = Vec::new();
    if let Some(b) = &bearer {
        headers.push(("authorization", b.as_str()));
    }
    if let Some(c) = content_type {
        headers.push(("content-type", c));
    }
    env::http_bytes(method, url, &headers, body).await
}

/// [`request`]'s answer as text.
pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekdays() {
        assert_eq!(weekday(2026, 11, 22), 0); // a Sunday
        assert_eq!(weekday(1970, 1, 1), 4);
        assert_eq!(weekday(2000, 2, 29), 2);
    }
}
