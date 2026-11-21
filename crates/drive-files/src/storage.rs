//! A storage's full configuration, credentials included. It lives only on
//! the device that runs the storage (in that device's own config); the
//! space registry gets [`StorageConfig::public`], never this.

use opendal::Operator;
use serde::{Deserialize, Serialize};

use crate::blob::BlobStore;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "lowercase")]
pub enum StorageConfig {
    /// A folder on this device's disks (a drive, a NAS mount).
    Fs { root: String },
    /// S3 or anything that speaks it: R2, B2, MinIO, Garage, RustFS, Wasabi.
    S3 {
        bucket: String,
        #[serde(default)]
        region: String,
        #[serde(default)]
        endpoint: String,
        access_key_id: String,
        secret_access_key: String,
        #[serde(default)]
        root: String,
    },
    /// A WebDAV server (Nextcloud, OxiCloud, Stalwart, a NAS).
    Webdav {
        endpoint: String,
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: String,
        #[serde(default)]
        root: String,
    },
    /// In memory (tests).
    Memory,
}

impl StorageConfig {
    pub fn scheme(&self) -> &'static str {
        match self {
            StorageConfig::Fs { .. } => "fs",
            StorageConfig::S3 { .. } => "s3",
            StorageConfig::Webdav { .. } => "webdav",
            StorageConfig::Memory => "memory",
        }
    }

    /// Where it is, with no secrets: safe for the registry.
    pub fn public(&self) -> String {
        match self {
            StorageConfig::Fs { root } => root.clone(),
            StorageConfig::S3 { bucket, endpoint, root, .. } => {
                let host = if endpoint.is_empty() { "aws".to_string() } else { endpoint.clone() };
                format!("{host} · {bucket}/{root}").trim_end_matches('/').to_string()
            }
            StorageConfig::Webdav { endpoint, root, .. } => format!("{endpoint}/{root}").trim_end_matches('/').to_string(),
            StorageConfig::Memory => "memory".into(),
        }
    }

    pub fn operator(&self) -> opendal::Result<Operator> {
        use opendal::services;
        Ok(match self {
            StorageConfig::Fs { root } => Operator::new(services::Fs::default().root(root))?,
            StorageConfig::S3 { bucket, region, endpoint, access_key_id, secret_access_key, root } => {
                let mut b = services::S3::default()
                    .bucket(bucket)
                    .access_key_id(access_key_id)
                    .secret_access_key(secret_access_key)
                    .root(if root.is_empty() { "/" } else { root });
                b = b.region(if region.is_empty() { "auto" } else { region });
                if !endpoint.is_empty() {
                    b = b.endpoint(endpoint);
                }
                Operator::new(b)?
            }
            StorageConfig::Webdav { endpoint, username, password, root } => {
                let mut b = services::Webdav::default().endpoint(endpoint).root(if root.is_empty() { "/" } else { root });
                if !username.is_empty() {
                    b = b.username(username).password(password);
                }
                Operator::new(b)?
            }
            StorageConfig::Memory => Operator::new(services::Memory::default())?,
        })
    }

    pub fn blobs(&self) -> opendal::Result<BlobStore> {
        Ok(BlobStore::new(self.operator()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_never_reach_the_public_form() {
        let c = StorageConfig::S3 {
            bucket: "stems".into(),
            region: String::new(),
            endpoint: "https://r2.example".into(),
            access_key_id: "AKIA-SECRET".into(),
            secret_access_key: "very-secret".into(),
            root: "drive".into(),
        };
        let p = c.public();
        assert!(!p.contains("SECRET") && !p.contains("secret"));
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<StorageConfig>(&json).unwrap(), c);
        assert!(c.operator().is_ok());
    }
}
