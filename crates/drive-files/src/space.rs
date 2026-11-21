//! A space's registry, one document (`drive:space:<space>`):
//!
//! - `devices`: device id → its name, kind, OS, roles, who approved it;
//! - `storages`: storage id → its name, scheme, where (no credentials),
//!   and the device that runs it;
//! - `invites`: pairing code → who made it, when it expires;
//! - `holdings`: device id → blob hash → `local` or `pinned` (what each
//!   device keeps), so any app can show where a file's bytes are.
//!
//! Prototype note: approval is recorded, not yet signed with device keys.

use loro::{LoroDoc, LoroMap, LoroValue, ValueOrContainer};

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub os: String,
    pub roles: Vec<String>,
    pub approved_by: String,
    pub added: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Storage {
    pub id: String,
    pub name: String,
    /// The OpenDAL scheme: `fs`, `s3`, `webdav`, …
    pub scheme: String,
    /// Where, without secrets: a path, `bucket/root`, a host.
    pub location: String,
    /// The device that runs it (holds its credentials).
    pub device: String,
    pub added: i64,
}

fn s(m: &LoroMap, k: &str) -> String {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::String(v))) => v.to_string(),
        _ => String::new(),
    }
}

fn n(m: &LoroMap, k: &str) -> i64 {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::I64(v))) => v,
        Some(ValueOrContainer::Value(LoroValue::Double(v))) => v as i64,
        _ => 0,
    }
}

fn child(m: &LoroMap, k: &str) -> Option<LoroMap> {
    match m.get(k) {
        Some(ValueOrContainer::Container(c)) => c.into_map().ok(),
        _ => None,
    }
}

fn put(m: &LoroMap, pairs: &[(&str, &str)]) -> loro::LoroResult<()> {
    for (k, v) in pairs {
        m.insert(k, *v)?;
    }
    Ok(())
}

/// A new random id (hex).
pub fn new_id() -> String {
    let mut b = [0u8; 8];
    getrandom_fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn getrandom_fill(buf: &mut [u8]) {
    // BLAKE3 of the time and a counter: unique enough for ids here.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let seed = format!("{:?}{}{:p}", std::time::SystemTime::now(), N.fetch_add(1, Ordering::Relaxed), buf);
    let h = blake3::hash(seed.as_bytes());
    buf.copy_from_slice(&h.as_bytes()[..buf.len()]);
}

// ── devices ─────────────────────────────────────────────────────────────

pub fn add_device(doc: &LoroDoc, d: &Device) -> loro::LoroResult<()> {
    let m = doc.get_map("devices").ensure_mergeable_map(&d.id)?;
    put(&m, &[("id", &d.id), ("name", &d.name), ("kind", &d.kind), ("os", &d.os), ("approved_by", &d.approved_by)])?;
    m.insert("roles", d.roles.join(","))?;
    m.insert("added", d.added)?;
    Ok(())
}

pub fn devices(doc: &LoroDoc) -> Vec<Device> {
    let map = doc.get_map("devices");
    let mut out: Vec<Device> = map
        .keys()
        .filter_map(|k| child(&map, &k))
        .map(|m| Device {
            id: s(&m, "id"),
            name: s(&m, "name"),
            kind: s(&m, "kind"),
            os: s(&m, "os"),
            roles: s(&m, "roles").split(',').filter(|r| !r.is_empty()).map(str::to_string).collect(),
            approved_by: s(&m, "approved_by"),
            added: n(&m, "added"),
        })
        .collect();
    out.sort_by_key(|d| d.added);
    out
}

pub fn remove_device(doc: &LoroDoc, id: &str) -> loro::LoroResult<()> {
    doc.get_map("devices").delete(id)?;
    doc.get_map("holdings").delete(id)
}

// ── pairing ─────────────────────────────────────────────────────────────

/// A pairing code (six letters and digits, no look-alikes), valid ten
/// minutes, made by an approved device.
pub fn invite(doc: &LoroDoc, by_device: &str) -> loro::LoroResult<String> {
    const ALPHA: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut b = [0u8; 6];
    getrandom_fill(&mut b);
    let code: String = b.iter().map(|x| ALPHA[*x as usize % ALPHA.len()] as char).collect();
    let m = doc.get_map("invites").ensure_mergeable_map(&code)?;
    m.insert("by", by_device)?;
    m.insert("expires", crate::now_ms() + 10 * 60 * 1000)?;
    Ok(code)
}

#[derive(Debug, PartialEq)]
pub enum JoinError {
    NoSuchCode,
    Expired,
}

/// Join with a code: the new device is added, approved by whoever made
/// the code, and the code is used up.
pub fn join(doc: &LoroDoc, code: &str, mut d: Device) -> Result<Device, JoinError> {
    let code = code.trim().to_ascii_uppercase();
    let invites = doc.get_map("invites");
    let inv = child(&invites, &code).ok_or(JoinError::NoSuchCode)?;
    if n(&inv, "expires") < crate::now_ms() {
        let _ = invites.delete(&code);
        return Err(JoinError::Expired);
    }
    d.approved_by = s(&inv, "by");
    let _ = add_device(doc, &d);
    let _ = invites.delete(&code);
    Ok(d)
}

// ── storage ─────────────────────────────────────────────────────────────

pub fn add_storage(doc: &LoroDoc, st: &Storage) -> loro::LoroResult<()> {
    let m = doc.get_map("storages").ensure_mergeable_map(&st.id)?;
    put(&m, &[("id", &st.id), ("name", &st.name), ("scheme", &st.scheme), ("location", &st.location), ("device", &st.device)])?;
    m.insert("added", st.added)?;
    Ok(())
}

pub fn storages(doc: &LoroDoc) -> Vec<Storage> {
    let map = doc.get_map("storages");
    let mut out: Vec<Storage> = map
        .keys()
        .filter_map(|k| child(&map, &k))
        .map(|m| Storage {
            id: s(&m, "id"),
            name: s(&m, "name"),
            scheme: s(&m, "scheme"),
            location: s(&m, "location"),
            device: s(&m, "device"),
            added: n(&m, "added"),
        })
        .collect();
    out.sort_by_key(|x| x.added);
    out
}

pub fn remove_storage(doc: &LoroDoc, id: &str) -> loro::LoroResult<()> {
    doc.get_map("storages").delete(id)
}

// ── holdings ────────────────────────────────────────────────────────────

/// What a device keeps, or a storage holds: `holder` → hash → state
/// (`local`, `pinned`, or for storages `stored`).
pub fn set_held(doc: &LoroDoc, holder: &str, hash: &str, state: Option<&str>) -> loro::LoroResult<()> {
    let m = doc.get_map("holdings").ensure_mergeable_map(holder)?;
    match state {
        Some(st) => {
            if s(&m, hash) != st {
                m.insert(hash, st)?;
            }
        }
        None => {
            if m.get(hash).is_some() {
                m.delete(hash)?;
            }
        }
    }
    Ok(())
}

/// Who holds a blob, and how: (holder, state).
pub fn holders(doc: &LoroDoc, hash: &str) -> Vec<(String, String)> {
    let map = doc.get_map("holdings");
    map.keys()
        .filter_map(|k| child(&map, &k).map(|m| (k.to_string(), s(&m, hash))))
        .filter(|(_, st)| !st.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            kind: "laptop".into(),
            os: "linux".into(),
            roles: vec!["client".into()],
            approved_by: String::new(),
            added: crate::now_ms(),
        }
    }

    #[test]
    fn pairing_storage_and_holdings() {
        let d = LoroDoc::new();
        add_device(&d, &dev("first")).unwrap();
        let code = invite(&d, "first").unwrap();
        assert_eq!(code.len(), 6);
        let joined = join(&d, &code.to_lowercase(), dev("second")).unwrap();
        assert_eq!(joined.approved_by, "first");
        assert_eq!(devices(&d).len(), 2);
        assert_eq!(join(&d, &code, dev("third")), Err(JoinError::NoSuchCode));

        add_storage(&d, &Storage { id: "nas".into(), name: "NAS".into(), scheme: "fs".into(), location: "/mnt/nas".into(), device: "first".into(), added: 1 }).unwrap();
        assert_eq!(storages(&d)[0].name, "NAS");

        let h = "b".repeat(64);
        set_held(&d, "second", &h, Some("pinned")).unwrap();
        set_held(&d, "nas", &h, Some("stored")).unwrap();
        let mut who = holders(&d, &h);
        who.sort();
        assert_eq!(who, [("nas".to_string(), "stored".to_string()), ("second".to_string(), "pinned".to_string())]);
        set_held(&d, "second", &h, None).unwrap();
        assert_eq!(holders(&d, &h).len(), 1);
    }
}
