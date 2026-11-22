//! Where the app is mounted, and so where its node answers.
//!
//! An app at the root of its node (`/`) calls the node at the root
//! (`/orgs`, `/sync`). An app mounted under a path (`/studio`) calls it
//! under `<mount>/api` (`/studio/api/orgs`), which the node serves for
//! every app it mounts. That keeps the whole app under its one path, so a
//! site can hand that path to the node and keep everything else its own:
//! `example.com/studio/*` to the node, the rest the site's.
//!
//! The app says where it is mounted once, before it renders
//! ([`set`]); unset, it is the root.

use std::cell::RefCell;

thread_local! {
    static MOUNT: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Say where the app is mounted: `"/studio"`, `"studio"` or `""` (the root).
pub fn set(path: &str) {
    let p = path.trim_matches('/');
    let p = if p.is_empty() {
        String::new()
    } else {
        format!("/{p}")
    };
    MOUNT.with(|m| *m.borrow_mut() = p);
}

/// The app's mount: `""` at the root, else `/studio`.
pub fn get() -> String {
    MOUNT.with(|m| m.borrow().clone())
}

/// A path of the app itself (`/auth/callback` → `/studio/auth/callback`).
pub fn page(path: &str) -> String {
    format!("{}{path}", get())
}

/// A node path as this app reaches it (`/orgs` → `/studio/api/orgs`).
/// Anything not a root path (a full URL) and a path already under the
/// app's API stay as they are.
pub fn api(path: &str) -> String {
    let m = get();
    if m.is_empty() || !path.starts_with('/') {
        return path.to_string();
    }
    let base = format!("{m}/api");
    if path == base
        || path.starts_with(&format!("{base}/"))
        || path.starts_with(&format!("{base}?"))
    {
        return path.to_string();
    }
    format!("{base}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_calls_the_node_at_the_root() {
        set("");
        assert_eq!(api("/orgs"), "/orgs");
        assert_eq!(page("/auth/callback"), "/auth/callback");
    }

    #[test]
    fn a_mounted_app_calls_it_under_its_mount() {
        set("studio/");
        assert_eq!(get(), "/studio");
        assert_eq!(api("/orgs"), "/studio/api/orgs");
        assert_eq!(api("/search?q=x"), "/studio/api/search?q=x");
        assert_eq!(api("/studio/api/orgs"), "/studio/api/orgs");
        assert_eq!(
            api("https://auth.example/token"),
            "https://auth.example/token"
        );
        assert_eq!(page("/auth/callback"), "/studio/auth/callback");
        set("");
    }
}
