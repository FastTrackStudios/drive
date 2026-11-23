//! What a page has that a native app has otherwise: the clock, randomness,
//! a key-value store shared by every app on the address, the page's
//! location (and a reload), cookies and HTTP. In a browser the page's own;
//! natively ([`native`]) a file beside the client's copies, the node's
//! HTTP address and a restart of the app.

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::*;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

/// `encodeURIComponent`: everything but `A-Z a-z 0-9 - _ . ! ~ * ' ( )`
/// percent-encoded as UTF-8.
pub fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `decodeURIComponent`, leaving a malformed escape as it is.
pub fn decode_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(h) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(h);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// A query parameter of `search` (`?a=1&b=2`), decoded.
pub fn query_param(search: &str, name: &str) -> Option<String> {
    search.trim_start_matches('?').split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| decode_component(v))
    })
}

/// (month 0–11, day of the month, year) of a time in ms since the epoch,
/// in UTC.
pub fn date_parts_utc(ms: f64) -> (u32, u32, i64) {
    let days = (ms / 86_400_000.0).floor() as i64;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (m - 1, d, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_round_trip_like_the_browser() {
        assert_eq!(encode_component("a b/c?d=é"), "a%20b%2Fc%3Fd%3D%C3%A9");
        assert_eq!(decode_component("a%20b%2Fc%3Fd%3D%C3%A9"), "a b/c?d=é");
        assert_eq!(decode_component("100%"), "100%");
        assert_eq!(query_param("?org=my%20org&x=1", "org").as_deref(), Some("my org"));
    }

    #[test]
    fn dates_in_utc() {
        // 2026-11-22T00:00:00Z
        assert_eq!(date_parts_utc(1_795_305_600_000.0), (10, 22, 2026));
        assert_eq!(date_parts_utc(0.0), (0, 1, 1970));
    }
}
