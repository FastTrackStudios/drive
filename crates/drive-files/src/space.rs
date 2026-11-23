//! A space's registry, one document (`drive:space:<space>`):
//!
//! - `devices`: device id → its name, kind, OS, roles, who approved it;
//! - `storages`: storage id → its name, scheme, where (no credentials),
//!   and the device that runs it;
//! - `invites`: pairing code → who made it, when it expires;
//! - `holdings`: device id → blob hash → `local` or `pinned` (what each
//!   device keeps), so any app can show where a file's bytes are;
//! - `locations`: folders devices index in place (on their own disks, or
//!   in a storage they run), each with its latest index blob;
//! - `volumes`: each device's disks;
//! - `sites`: device or storage id → where it physically is (`home`,
//!   `studio`, `cloud`), for offsite copies;
//! - `policies`: protection rules (3-2-1 and the like) over locations;
//! - `devices.<id>.build_group`: which devices share kache build caches
//!   (docs/kache.md);
//! - `adopted`: folders taken over in place, by the logical path they
//!   show at in the Drive tree ([`crate::adopt`]);
//! - `nodes`: the iroh endpoint ids of the nodes that keep the space (a
//!   device's own endpoint is `iroh` in its `devices` entry).
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
    let seed = format!(
        "{}{}{:p}",
        crate::entropy(),
        N.fetch_add(1, Ordering::Relaxed),
        buf
    );
    let h = blake3::hash(seed.as_bytes());
    buf.copy_from_slice(&h.as_bytes()[..buf.len()]);
}

// ── devices ─────────────────────────────────────────────────────────────

pub fn add_device(doc: &LoroDoc, d: &Device) -> loro::LoroResult<()> {
    let m = doc.get_map("devices").ensure_mergeable_map(&d.id)?;
    put(
        &m,
        &[
            ("id", &d.id),
            ("name", &d.name),
            ("kind", &d.kind),
            ("os", &d.os),
            ("approved_by", &d.approved_by),
        ],
    )?;
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
            roles: s(&m, "roles")
                .split(',')
                .filter(|r| !r.is_empty())
                .map(str::to_string)
                .collect(),
            approved_by: s(&m, "approved_by"),
            added: n(&m, "added"),
        })
        .collect();
    out.sort_by_key(|d| d.added);
    out
}

/// A device's iroh endpoint (docs/transfer.md): its endpoint id, and its
/// address as it last said it (relay and direct addresses, JSON), next to
/// its entry in `devices`. Only devices with one serve files over iroh;
/// the others (agents from before) keep the node's `/fetch` path.
pub fn set_endpoint(doc: &LoroDoc, device: &str, id: &str, addr: &str) -> loro::LoroResult<()> {
    let m = doc.get_map("devices").ensure_mergeable_map(device)?;
    if s(&m, "iroh") != id {
        m.insert("iroh", id)?;
    }
    if s(&m, "iroh_addr") != addr {
        m.insert("iroh_addr", addr)?;
    }
    Ok(())
}

/// Device id → (endpoint id, address JSON), for devices that have one.
pub fn endpoints(doc: &LoroDoc) -> std::collections::HashMap<String, (String, String)> {
    let map = doc.get_map("devices");
    map.keys()
        .filter_map(|k| {
            let m = child(&map, &k)?;
            let id = s(&m, "iroh");
            (!id.is_empty()).then(|| (k.to_string(), (id, s(&m, "iroh_addr"))))
        })
        .collect()
}

/// A node that keeps this space, by its iroh endpoint id (`nodes`: id →
/// when it last said so): devices let it fetch their files.
pub fn set_node_endpoint(doc: &LoroDoc, id: &str, at: i64) -> loro::LoroResult<()> {
    doc.get_map("nodes").insert(id, at)
}

pub fn node_endpoints(doc: &LoroDoc) -> Vec<String> {
    let mut out: Vec<String> = doc.get_map("nodes").keys().map(|k| k.to_string()).collect();
    out.sort();
    out
}

/// Every endpoint id the space's devices and nodes have: who an agent
/// lets fetch its files.
pub fn allowed_endpoints(doc: &LoroDoc) -> std::collections::HashSet<String> {
    endpoints(doc)
        .into_values()
        .map(|(id, _)| id)
        .chain(node_endpoints(doc))
        .collect()
}

/// A device's build group (docs/kache.md): devices of one group share
/// their kache build caches (same OS and CPU, so the same keys: e.g.
/// `linux-x86_64`, `darwin-arm64`). Empty: none.
pub fn set_build_group(doc: &LoroDoc, device: &str, group: &str) -> loro::LoroResult<()> {
    let m = doc.get_map("devices").ensure_mergeable_map(device)?;
    if group.is_empty() {
        if m.get("build_group").is_some() {
            m.delete("build_group")?;
        }
    } else if s(&m, "build_group") != group {
        m.insert("build_group", group)?;
    }
    Ok(())
}

/// Device id → build group, for devices that have one.
pub fn build_groups(doc: &LoroDoc) -> std::collections::HashMap<String, String> {
    let map = doc.get_map("devices");
    map.keys()
        .filter_map(|k| {
            let g = s(&child(&map, &k)?, "build_group");
            (!g.is_empty()).then(|| (k.to_string(), g))
        })
        .collect()
}

/// Replace a device's roles (`storage-server`, `workstation`, `client`).
pub fn set_roles(doc: &LoroDoc, device: &str, roles: &[String]) -> loro::LoroResult<()> {
    doc.get_map("devices")
        .ensure_mergeable_map(device)?
        .insert("roles", roles.join(","))?;
    Ok(())
}

/// A folder of the file tree a device is working on now (`active`:
/// device → path → true): its files are kept on that device.
pub fn set_active(doc: &LoroDoc, device: &str, path: &str, on: bool) -> loro::LoroResult<()> {
    let m = doc.get_map("active").ensure_mergeable_map(device)?;
    if on {
        m.insert(path, true)?;
    } else if m.get(path).is_some() {
        m.delete(path)?;
    }
    Ok(())
}

pub fn active(doc: &LoroDoc, device: &str) -> Vec<String> {
    let mut out: Vec<String> = child(&doc.get_map("active"), device)
        .map(|m| m.keys().map(|k| k.to_string()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

/// Keep a file of the tree on a device (`pins`: device → tree id → true),
/// asked from anywhere (the Drive app): that device's agent fetches it and
/// never lets it go.
pub fn set_pin(doc: &LoroDoc, device: &str, file: &str, on: bool) -> loro::LoroResult<()> {
    let m = doc.get_map("pins").ensure_mergeable_map(device)?;
    if on {
        m.insert(file, true)?;
    } else if m.get(file).is_some() {
        m.delete(file)?;
    }
    Ok(())
}

/// The tree ids a device has been asked to keep.
pub fn pins(doc: &LoroDoc, device: &str) -> Vec<String> {
    let mut out: Vec<String> = child(&doc.get_map("pins"), device)
        .map(|m| m.keys().map(|k| k.to_string()).collect())
        .unwrap_or_default();
    out.sort();
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
    let code: String = b
        .iter()
        .map(|x| ALPHA[*x as usize % ALPHA.len()] as char)
        .collect();
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
    put(
        &m,
        &[
            ("id", &st.id),
            ("name", &st.name),
            ("scheme", &st.scheme),
            ("location", &st.location),
            ("device", &st.device),
        ],
    )?;
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

/// A storage that serves what is kept available to anyone, fast: an R2
/// (or S3) bucket with a public URL. Agents that run it copy every
/// available file into it, and share links send people straight there.
/// `url` is the bucket's public base (`https://media.example.com`); a blob
/// is at `{url}/{blob::key(hash)}`. Empty: no longer public.
pub fn set_public(doc: &LoroDoc, storage: &str, url: &str) -> loro::LoroResult<()> {
    let m = doc.get_map("public");
    if url.is_empty() {
        if m.get(storage).is_some() {
            m.delete(storage)?;
        }
        Ok(())
    } else {
        m.insert(storage, url.trim_end_matches('/'))
    }
}

/// Public storages: storage id → public base URL.
pub fn public(doc: &LoroDoc) -> std::collections::HashMap<String, String> {
    let m = doc.get_map("public");
    m.keys()
        .filter_map(|k| match m.get(&k) {
            Some(ValueOrContainer::Value(LoroValue::String(v))) => {
                Some((k.to_string(), v.to_string()))
            }
            _ => None,
        })
        .collect()
}

pub fn remove_storage(doc: &LoroDoc, id: &str) -> loro::LoroResult<()> {
    doc.get_map("storages").delete(id)
}

// ── locations ───────────────────────────────────────────────────────────

/// A folder a device indexes in place, and its latest published index.
#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    pub id: String,
    pub name: String,
    pub device: String,
    /// Its path on that device.
    pub path: String,
    /// The volume (disk) it is on: its mount point.
    pub volume: String,
    /// When set, the location is in this storage (an S3 bucket, a WebDAV
    /// share) and `path` is inside it; `device` runs that storage.
    pub storage: String,
    /// When set, this location is a storage server's mirror of that one.
    pub mirror_of: String,
    /// The latest index blob's hash (empty until the first walk).
    pub index: String,
    pub files: i64,
    pub bytes: i64,
    pub built: i64,
}

pub fn add_location(doc: &LoroDoc, l: &Location) -> loro::LoroResult<()> {
    let m = doc.get_map("locations").ensure_mergeable_map(&l.id)?;
    put(
        &m,
        &[
            ("id", &l.id),
            ("name", &l.name),
            ("device", &l.device),
            ("path", &l.path),
            ("volume", &l.volume),
            ("storage", &l.storage),
            ("mirror_of", &l.mirror_of),
        ],
    )?;
    Ok(())
}

/// Record a location's new index.
pub fn set_index(
    doc: &LoroDoc,
    location: &str,
    hash: &str,
    files: i64,
    bytes: i64,
) -> loro::LoroResult<()> {
    let m = doc.get_map("locations").ensure_mergeable_map(location)?;
    m.insert("index", hash)?;
    m.insert("files", files)?;
    m.insert("bytes", bytes)?;
    m.insert("built", crate::now_ms())?;
    Ok(())
}

/// Pause or resume a location: a paused one keeps its last index (still
/// searchable, still listed) but is never walked or watched again until
/// resumed. Nothing about its files changes either way.
pub fn set_paused(doc: &LoroDoc, location: &str, paused: bool) -> loro::LoroResult<()> {
    let m = doc.get_map("locations").ensure_mergeable_map(location)?;
    m.insert("paused", paused)
}

/// Whether a location is paused (see [`set_paused`]).
pub fn is_paused(doc: &LoroDoc, location: &str) -> bool {
    child(&doc.get_map("locations"), location)
        .and_then(|m| m.get("paused"))
        .and_then(|v| v.into_value().ok())
        .is_some_and(|v| matches!(v, loro::LoroValue::Bool(true)))
}

pub fn locations(doc: &LoroDoc) -> Vec<Location> {
    let map = doc.get_map("locations");
    let mut out: Vec<Location> = map
        .keys()
        .filter_map(|k| child(&map, &k))
        .map(|m| Location {
            id: s(&m, "id"),
            name: s(&m, "name"),
            device: s(&m, "device"),
            path: s(&m, "path"),
            volume: s(&m, "volume"),
            storage: s(&m, "storage"),
            mirror_of: s(&m, "mirror_of"),
            index: s(&m, "index"),
            files: n(&m, "files"),
            bytes: n(&m, "bytes"),
            built: n(&m, "built"),
        })
        .filter(|l| !l.id.is_empty())
        .collect();
    out.sort_by(|a, b| {
        (a.device.as_str(), a.name.as_str()).cmp(&(b.device.as_str(), b.name.as_str()))
    });
    out
}

pub fn remove_location(doc: &LoroDoc, id: &str) -> loro::LoroResult<()> {
    doc.get_map("locations").delete(id)
}

impl Location {
    /// The medium its bytes are on: the storage, or the device's disk. Two
    /// locations on one medium are one copy's worth of safety.
    pub fn medium(&self) -> String {
        if self.storage.is_empty() {
            format!("{}:{}", self.device, self.volume)
        } else {
            self.storage.clone()
        }
    }

    /// Who holds its bytes, for sites: the storage, or the device.
    pub fn holder(&self) -> &str {
        if self.storage.is_empty() {
            &self.device
        } else {
            &self.storage
        }
    }
}

// ── sites ───────────────────────────────────────────────────────────────

/// Say where a device or storage physically is.
pub fn set_site(doc: &LoroDoc, holder: &str, site: &str) -> loro::LoroResult<()> {
    doc.get_map("sites").insert(holder, site)
}

/// Every holder's site (unset holders are absent).
pub fn sites(doc: &LoroDoc) -> std::collections::HashMap<String, String> {
    let map = doc.get_map("sites");
    map.keys()
        .filter_map(|k| match map.get(&k) {
            Some(ValueOrContainer::Value(LoroValue::String(v))) => {
                Some((k.to_string(), v.to_string()))
            }
            _ => None,
        })
        .collect()
}

// ── policies ────────────────────────────────────────────────────────────

/// A protection rule: every file in `scope` (location ids; empty: every
/// location) should exist on at least `copies` media, `offsite` of them at
/// another site than the file's own. The 3-2-1 rule is copies 3 (the
/// original and two more), on 2 media, 1 offsite.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Policy {
    pub id: String,
    pub name: String,
    pub scope: Vec<String>,
    pub copies: i64,
    pub media: i64,
    pub offsite: i64,
}

pub fn set_policy(doc: &LoroDoc, p: &Policy) -> loro::LoroResult<()> {
    let m = doc.get_map("policies").ensure_mergeable_map(&p.id)?;
    put(
        &m,
        &[
            ("id", &p.id),
            ("name", &p.name),
            ("scope", &p.scope.join(",")),
        ],
    )?;
    m.insert("copies", p.copies)?;
    m.insert("media", p.media)?;
    m.insert("offsite", p.offsite)?;
    Ok(())
}

pub fn policies(doc: &LoroDoc) -> Vec<Policy> {
    let map = doc.get_map("policies");
    let mut out: Vec<Policy> = map
        .keys()
        .filter_map(|k| child(&map, &k))
        .map(|m| Policy {
            id: s(&m, "id"),
            name: s(&m, "name"),
            scope: s(&m, "scope")
                .split(',')
                .filter(|x| !x.is_empty())
                .map(str::to_string)
                .collect(),
            copies: n(&m, "copies"),
            media: n(&m, "media"),
            offsite: n(&m, "offsite"),
        })
        .filter(|p| !p.id.is_empty())
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn remove_policy(doc: &LoroDoc, id: &str) -> loro::LoroResult<()> {
    doc.get_map("policies").delete(id)
}

// ── volumes ─────────────────────────────────────────────────────────────

/// A disk a device has (its own, or one plugged in).
#[derive(Clone, Debug, PartialEq)]
pub struct Volume {
    pub device: String,
    pub name: String,
    pub mount: String,
    pub fs: String,
    pub total: i64,
    pub free: i64,
    pub removable: bool,
    /// When its device last saw it mounted.
    pub seen: i64,
}

/// A device's disks, as it sees them now; ones no longer mounted stay,
/// marked by their old `seen` (an unplugged drive keeps its files' index).
pub fn report_volumes(doc: &LoroDoc, device: &str, vols: &[Volume]) -> loro::LoroResult<()> {
    let map = doc.get_map("volumes");
    for v in vols {
        let m = map.ensure_mergeable_map(&format!("{device}:{}", v.mount))?;
        put(
            &m,
            &[
                ("device", device),
                ("name", &v.name),
                ("mount", &v.mount),
                ("fs", &v.fs),
            ],
        )?;
        m.insert("total", v.total)?;
        m.insert("free", v.free)?;
        m.insert("removable", v.removable)?;
        m.insert("seen", v.seen)?;
    }
    Ok(())
}

pub fn volumes(doc: &LoroDoc) -> Vec<Volume> {
    let map = doc.get_map("volumes");
    let mut out: Vec<Volume> = map
        .keys()
        .filter_map(|k| child(&map, &k))
        .map(|m| Volume {
            device: s(&m, "device"),
            name: s(&m, "name"),
            mount: s(&m, "mount"),
            fs: s(&m, "fs"),
            total: n(&m, "total"),
            free: n(&m, "free"),
            removable: matches!(
                m.get("removable"),
                Some(ValueOrContainer::Value(LoroValue::Bool(true)))
            ),
            seen: n(&m, "seen"),
        })
        .collect();
    out.sort_by(|a, b| {
        (a.device.as_str(), a.mount.as_str()).cmp(&(b.device.as_str(), b.mount.as_str()))
    });
    out
}

// ── holdings ────────────────────────────────────────────────────────────

/// What a device keeps, or a storage holds: `holder` → hash → state
/// (`local`, `pinned`, or for storages `stored`).
pub fn set_held(
    doc: &LoroDoc,
    holder: &str,
    hash: &str,
    state: Option<&str>,
) -> loro::LoroResult<()> {
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

        add_storage(
            &d,
            &Storage {
                id: "nas".into(),
                name: "NAS".into(),
                scheme: "fs".into(),
                location: "/mnt/nas".into(),
                device: "first".into(),
                added: 1,
            },
        )
        .unwrap();
        assert_eq!(storages(&d)[0].name, "NAS");

        let h = "b".repeat(64);
        set_held(&d, "second", &h, Some("pinned")).unwrap();
        set_held(&d, "nas", &h, Some("stored")).unwrap();
        let mut who = holders(&d, &h);
        who.sort();
        assert_eq!(
            who,
            [
                ("nas".to_string(), "stored".to_string()),
                ("second".to_string(), "pinned".to_string())
            ]
        );
        set_held(&d, "second", &h, None).unwrap();
        assert_eq!(holders(&d, &h).len(), 1);
    }

    #[test]
    fn endpoints_beside_devices_and_nodes() {
        let d = LoroDoc::new();
        add_device(&d, &dev("old")).unwrap();
        add_device(&d, &dev("new")).unwrap();
        set_endpoint(&d, "new", "e1", "{}").unwrap();
        set_node_endpoint(&d, "n1", 5).unwrap();
        let e = endpoints(&d);
        assert_eq!(e.len(), 1, "a device without one is left out");
        assert_eq!(e["new"], ("e1".to_string(), "{}".to_string()));
        // The device entry itself reads as before.
        assert_eq!(devices(&d).len(), 2);
        let allowed = allowed_endpoints(&d);
        assert!(allowed.contains("e1") && allowed.contains("n1") && allowed.len() == 2);
    }

    #[test]
    fn build_groups_beside_devices() {
        let d = LoroDoc::new();
        set_endpoint(&d, "pc", "e1", "{}").unwrap();
        set_build_group(&d, "pc", "linux-x86_64").unwrap();
        set_build_group(&d, "mac", "darwin-arm64").unwrap();
        set_build_group(&d, "vps", "linux-x86_64").unwrap();
        let g = build_groups(&d);
        assert_eq!(g["pc"], "linux-x86_64");
        assert_eq!(g.len(), 3);
        set_build_group(&d, "vps", "").unwrap();
        assert!(!build_groups(&d).contains_key("vps"));
        // The endpoint stays as it was.
        assert_eq!(endpoints(&d)["pc"].0, "e1");
    }

    #[test]
    fn pins_asked_from_anywhere() {
        let d = LoroDoc::new();
        set_pin(&d, "laptop", "12@3", true).unwrap();
        set_pin(&d, "laptop", "7@3", true).unwrap();
        set_pin(&d, "laptop", "12@3", false).unwrap();
        assert_eq!(pins(&d, "laptop"), ["7@3"]);
        assert!(pins(&d, "nas").is_empty());
    }
}
