//! Agents in one process: each a kache store, an iroh endpoint (direct
//! addresses only) speaking the blobs and the lookups, and the S3
//! endpoint on loopback.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use drive_kache::{ForgetFn, Kache, Link, Members, Peer, ProvideFn, Store, Timeouts, peers, s3};
use drive_transfer::{Allow, Fetcher, Provider, Settings};
use futures::FutureExt;

pub struct Agent {
    pub name: String,
    pub kache: Arc<Kache>,
    pub provider: Arc<Provider>,
    pub http: SocketAddr,
    pub group: Arc<Mutex<Vec<Peer>>>,
}

impl Agent {
    pub fn url(&self) -> String {
        format!("http://{}", self.http)
    }

    pub fn peer(&self) -> Peer {
        Peer {
            id: self.provider.id(),
            addr: self.provider.addr(),
            name: self.name.clone(),
        }
    }
}

pub async fn agent(dir: &Path, name: &str, budget: u64, timeouts: Timeouts) -> Agent {
    let group: Arc<Mutex<Vec<Peer>>> = Arc::new(Mutex::new(Vec::new()));
    let g = group.clone();
    let members: Members = Arc::new(move || g.lock().unwrap().clone());
    let store = Store::open(&dir.join(name).join("kache"), budget).unwrap();
    let kache = Kache::new(store, members, timeouts);
    let allow: Allow = Arc::new(|_| true);
    let provider = Arc::new(
        Provider::start_with(
            &dir.join(name).join("transfer"),
            &Settings {
                relay: None,
                local_only: true,
            },
            allow,
            vec![peers::protocol(&kache)],
        )
        .await
        .unwrap(),
    );
    let pv = provider.clone();
    let provide: ProvideFn = Arc::new(move |path| {
        let pv = pv.clone();
        async move { pv.provide(&path).await.map(|p| p.hash) }.boxed()
    });
    let pf = provider.clone();
    let forget: ForgetFn = Arc::new(move |path| {
        let pf = pf.clone();
        async move { pf.forget(&path).await }.boxed()
    });
    kache.attach(Link {
        endpoint: provider.endpoint().clone(),
        fetcher: Arc::new(Fetcher::on(provider.endpoint().clone())),
        provide,
        forget,
    });
    let (http, _) = s3::serve(kache.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    Agent {
        name: name.to_string(),
        kache,
        provider,
        http,
        group,
    }
}

/// Put `a` and `b` in one build group.
pub fn group(a: &Agent, b: &Agent) {
    a.group.lock().unwrap().push(b.peer());
    b.group.lock().unwrap().push(a.peer());
}

/// An S3 client built as kache builds its own (OpenDAL's S3 service,
/// path-style, Content-MD5 on writes, dummy keys).
pub fn s3_client(a: &Agent) -> opendal::Operator {
    opendal_http_transport_reqwest::install_default();
    let b = opendal::services::S3::default()
        .bucket(drive_kache::BUCKET)
        .region("us-east-1")
        .endpoint(&a.url())
        .access_key_id("drive")
        .secret_access_key("drive")
        .checksum_algorithm("md5")
        .disable_config_load();
    opendal::Operator::new(b).unwrap()
}
