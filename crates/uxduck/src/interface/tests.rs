#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::Request as HttpRequest;
use sha2::{Digest, Sha256};
use uxduck_contracts::dto::interface::{InterfaceArchive, InterfaceEntry, InterfaceManifest};

use super::store::{InstallError, Store, hex};
use super::*;

const VITE: &str = "http://localhost:5173";

type Build = Vec<(&'static str, Vec<u8>)>;

fn build(version: &str, extra: &[(&'static str, &str)]) -> Build {
    let mut files: Build = vec![
        (
            "index.html",
            format!("<!doctype html><p>{version}</p>").into_bytes(),
        ),
        (
            "version.json",
            format!("{{\"id\":\"{version}\",\"builtAt\":\"2026-10-11T00:00:00Z\"}}").into_bytes(),
        ),
    ];
    for (name, body) in extra {
        files.push((name, body.as_bytes().to_vec()));
    }
    files
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// A `tar.zst` of the files, and an entry for it.
fn archive(version: &str, at: &str, files: &Build) -> (Vec<u8>, InterfaceEntry) {
    let mut tar = tar::Builder::new(Vec::new());
    for (name, body) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, body.as_slice()).unwrap();
    }
    let tar = tar.into_inner().unwrap();
    let compressed = zstd::stream::encode_all(tar.as_slice(), 3).unwrap();
    let entry = InterfaceEntry {
        version: version.into(),
        published_at: at.into(),
        protocols: vec![PROTOCOL_VERSION],
        archive: InterfaceArchive {
            url: format!("/{version}.tar.zst"),
            size: compressed.len() as u64,
            sha256: sha(&compressed),
        },
        files: files
            .iter()
            .map(|(name, body)| ((*name).to_owned(), sha(body)))
            .collect(),
    };
    (compressed, entry)
}

/// An archive whose one entry is named `name` as given, unchecked.
fn raw_archive(name: &[u8], body: &[u8]) -> Vec<u8> {
    let mut header = tar::Header::new_old();
    header.as_old_mut().name[..name.len()].copy_from_slice(name);
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    let mut tar = tar::Builder::new(Vec::new());
    tar.append(&header, body).unwrap();
    zstd::stream::encode_all(tar.into_inner().unwrap().as_slice(), 3).unwrap()
}

fn install(store: &Store, bytes: &[u8], entry: &InterfaceEntry) -> Result<Kept, InstallError> {
    let path = store.download_path(&entry.version);
    std::fs::write(&path, bytes).unwrap();
    store.install(entry, &path)
}

#[test]
fn an_archive_installs_only_when_it_and_every_file_check_out() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let files = build("v1", &[("assets/a-1.js", "a")]);
    let (bytes, entry) = archive("v1", "2026-10-10T00:00:00Z", &files);

    let mut wrong = entry.clone();
    wrong.archive.sha256 = "00".repeat(32);
    assert!(matches!(
        install(&store, &bytes, &wrong),
        Err(InstallError::ArchiveHash)
    ));
    let mut short = entry.clone();
    short.archive.size -= 1;
    assert!(matches!(
        install(&store, &bytes, &short),
        Err(InstallError::Size { .. })
    ));
    let mut big = entry.clone();
    big.archive.size = MAX_ARCHIVE_BYTES + 1;
    assert!(matches!(
        install(&store, &bytes, &big),
        Err(InstallError::TooLarge(_))
    ));
    let mut one = entry.clone();
    one.files.insert("assets/a-1.js".into(), sha(b"b"));
    assert!(matches!(
        install(&store, &bytes, &one),
        Err(InstallError::FileHash(name)) if name == "assets/a-1.js"
    ));
    let mut unlisted = entry.clone();
    unlisted.files.remove("assets/a-1.js");
    assert!(matches!(
        install(&store, &bytes, &unlisted),
        Err(InstallError::Unlisted(_))
    ));
    let mut missing = entry.clone();
    missing.files.insert("gone.txt".into(), sha(b""));
    assert!(matches!(
        install(&store, &bytes, &missing),
        Err(InstallError::Missing(_))
    ));
    let mut traversal = entry.clone();
    traversal.version = "../escape".into();
    assert!(matches!(
        install(&store, &bytes, &traversal),
        Err(InstallError::BadVersion(_))
    ));
    assert!(store.kept().is_empty(), "nothing failed is kept");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(leftovers.is_empty(), "no partial work left: {leftovers:?}");

    let kept = install(&store, &bytes, &entry).unwrap();
    assert_eq!(std::fs::read(kept.dir.join("assets/a-1.js")).unwrap(), b"a");
    assert_eq!(store.kept().len(), 1);
    assert!(
        !store.download_path("v1").exists(),
        "the archive is deleted"
    );
}

#[test]
fn a_path_that_leaves_the_folder_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("interface")).unwrap();
    for name in [&b"../evil.js"[..], b"/etc/evil", b"assets/../../evil"] {
        let bytes = raw_archive(name, b"x");
        let entry = InterfaceEntry {
            version: "v1".into(),
            published_at: "2026-10-10T00:00:00Z".into(),
            protocols: vec![PROTOCOL_VERSION],
            archive: InterfaceArchive {
                url: String::new(),
                size: bytes.len() as u64,
                sha256: sha(&bytes),
            },
            files: BTreeMap::from([(String::from_utf8_lossy(name).into_owned(), sha(b"x"))]),
        };
        let result = install(&store, &bytes, &entry);
        assert!(
            matches!(result, Err(InstallError::Escapes(_))),
            "{result:?}"
        );
    }
    assert!(!dir.path().join("evil.js").exists());
    assert!(store.kept().is_empty());
}

#[test]
fn an_interrupted_install_is_cleaned_up_on_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (bytes, entry) = archive("v1", "2026-10-10T00:00:00Z", &build("v1", &[]));
    install(&store, &bytes, &entry).unwrap();
    std::fs::create_dir_all(dir.path().join("v2.partial/assets")).unwrap();
    std::fs::write(dir.path().join("v3.download"), b"half").unwrap();
    // Renamed into place, but the power went before its entry was written.
    std::fs::create_dir_all(dir.path().join("v4")).unwrap();
    std::fs::write(dir.path().join("v5.json"), b"{}").unwrap();
    store.set_current("v1").unwrap();

    let store = Store::open(dir.path()).unwrap();
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["current", "v1", "v1.json"]);
    assert_eq!(store.current().as_deref(), Some("v1"));
}

// A small server for the manifest and archives.

#[derive(Clone, Default)]
struct Served(Arc<Mutex<BTreeMap<String, Vec<u8>>>>);

impl Served {
    fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.0.lock().unwrap().insert(path.to_owned(), body.into());
    }
}

async fn server(served: Served) -> String {
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let served = served.clone();
        async move {
            match served.0.lock().unwrap().get(uri.path()) {
                Some(body) => (StatusCode::OK, body.clone()).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

struct Publisher {
    served: Served,
    base: String,
    key: minisign::KeyPair,
}

impl Publisher {
    async fn new() -> Self {
        let served = Served::default();
        let base = server(served.clone()).await;
        Self {
            served,
            base,
            key: minisign::KeyPair::generate_unencrypted_keypair().unwrap(),
        }
    }

    fn public_key(&self) -> String {
        self.key.pk.to_base64()
    }

    /// Puts each archive and a production manifest naming them, signed
    /// with `key`.
    fn publish_signed(
        &self,
        at: &str,
        builds: &[(Vec<u8>, InterfaceEntry)],
        key: &minisign::KeyPair,
    ) {
        self.publish_as("production", "production", at, builds, key);
    }

    /// Puts a manifest saying it is `channel`'s where `host` serves its
    /// channel: production at `/manifest.json`, staging at
    /// `/staging/manifest.json`.
    fn publish_as(
        &self,
        host: &str,
        channel: &str,
        at: &str,
        builds: &[(Vec<u8>, InterfaceEntry)],
        key: &minisign::KeyPair,
    ) {
        let mut entries = Vec::new();
        for (bytes, entry) in builds {
            let mut entry = entry.clone();
            self.served.put(&entry.archive.url, bytes.clone());
            entry.archive.url = format!("{}{}", self.base, entry.archive.url);
            entries.push(entry);
        }
        let manifest = serde_json::to_vec(&InterfaceManifest {
            schema: 1,
            channel: channel.into(),
            published_at: at.into(),
            entries,
        })
        .unwrap();
        let signature = minisign::sign(None, &key.sk, manifest.as_slice(), None, None)
            .unwrap()
            .into_string();
        let path = if host == "staging" {
            "/staging/manifest.json"
        } else {
            "/manifest.json"
        };
        self.served.put(path, manifest);
        self.served.put(&format!("{path}.minisig"), signature);
    }

    fn publish(&self, at: &str, builds: &[(Vec<u8>, InterfaceEntry)]) {
        self.publish_signed(at, builds, &self.key);
    }

    fn config(&self) -> Config {
        Config {
            url: Some(format!("{}/manifest.json", self.base)),
            staging_url: Some(format!("{}/staging/manifest.json", self.base)),
            key: Some(self.public_key()),
            dev: None,
            first_check: Duration::ZERO,
            every: Duration::from_hours(1),
            ready_timeout: Duration::from_millis(200),
        }
    }
}

fn shipped(dir: &Path) -> PathBuf {
    let ui = dir.join("ui");
    std::fs::create_dir_all(ui.join("assets")).unwrap();
    std::fs::write(ui.join("index.html"), "<p>shipped</p>").unwrap();
    std::fs::write(
        ui.join("version.json"),
        r#"{"id":"shipped","builtAt":"2026-10-01T00:00:00Z"}"#,
    )
    .unwrap();
    std::fs::write(ui.join("assets/shipped-1.js"), "shipped").unwrap();
    ui
}

async fn get(interfaces: &Arc<Interfaces>, path: &str) -> (StatusCode, String, String) {
    let request = HttpRequest::builder()
        .uri(path)
        .header("sec-fetch-dest", "document")
        .body(Body::empty())
        .unwrap();
    let response = serve(State(interfaces.clone()), request).await;
    let status = response.status();
    let cache = response
        .headers()
        .get(header::CACHE_CONTROL)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned(), cache)
}

#[tokio::test]
async fn online_it_follows_the_manifest_and_an_open_page_keeps_its_chunks() {
    let publisher = Publisher::new().await;
    let data = tempfile::tempdir().unwrap();
    let ui = shipped(data.path());
    let v1 = archive(
        "v1",
        "2026-10-10T00:00:00Z",
        &build("v1", &[("assets/lazy-1.js", "one"), ("favicon.svg", "1")]),
    );
    publisher.publish("2026-10-10T00:00:00Z", std::slice::from_ref(&v1));
    let interfaces = Arc::new(Interfaces::open(data.path(), Some(ui), publisher.config()).unwrap());
    assert_eq!(
        interfaces.status().version,
        "shipped",
        "the built-in copy first"
    );
    assert!(interfaces.status().built_in);

    interfaces.check().await;
    let status = interfaces.status();
    assert_eq!(status.version, "v1", "{status:?}");
    assert_eq!(status.problem, None);
    assert!(!status.built_in);
    let (code, body, cache) = get(&interfaces, "/").await;
    assert_eq!(
        (code, body.as_str(), cache.as_str()),
        (StatusCode::OK, "<!doctype html><p>v1</p>", "no-cache")
    );
    assert_eq!(
        get(&interfaces, "/vehicles/alpha").await.1,
        "<!doctype html><p>v1</p>"
    );

    let v2 = archive(
        "v2",
        "2026-10-11T00:00:00Z",
        &build("v2", &[("assets/lazy-2.js", "two"), ("favicon.svg", "2")]),
    );
    publisher.publish("2026-10-11T00:00:00Z", &[v2.clone(), v1.clone()]);
    interfaces.check().await;
    assert_eq!(interfaces.status().version, "v2");
    assert_eq!(
        get(&interfaces, "/index.html").await.1,
        "<!doctype html><p>v2</p>"
    );
    // A page still on v1 lazily loads a chunk only v1 has.
    let (code, body, cache) = get(&interfaces, "/assets/lazy-1.js").await;
    assert_eq!((code, body.as_str()), (StatusCode::OK, "one"));
    assert_eq!(cache, "public, max-age=31536000, immutable");
    assert_eq!(get(&interfaces, "/assets/shipped-1.js").await.1, "shipped");
    // Unhashed files come from the current one only.
    let (_, favicon, cache) = get(&interfaces, "/favicon.svg").await;
    assert_eq!((favicon.as_str(), cache.as_str()), ("2", "no-cache"));
    assert_eq!(get(&interfaces, "/nope.txt").await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        get(&interfaces, "/assets/../v1.json").await.0,
        StatusCode::NOT_FOUND
    );

    // A rollback is a newer manifest naming the older entry.
    publisher.publish("2026-10-12T00:00:00Z", std::slice::from_ref(&v1));
    interfaces.check().await;
    assert_eq!(interfaces.status().version, "v1");
    // A page cached from v2 is not told its index is unchanged, though v1's
    // file is older.
    let request = HttpRequest::builder()
        .uri("/")
        .header(header::IF_NONE_MATCH, "\"v2\"")
        .header(header::IF_MODIFIED_SINCE, "Sun, 01 Jan 2090 00:00:00 GMT")
        .body(Body::empty())
        .unwrap();
    let response = serve(State(interfaces.clone()), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::ETAG], "\"v1\"");
    let request = HttpRequest::builder()
        .uri("/")
        .header(header::IF_NONE_MATCH, "\"v1\"")
        .body(Body::empty())
        .unwrap();
    let response = serve(State(interfaces.clone()), request).await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);

    // Offline: the next start serves the one it last followed.
    let offline = Config {
        url: Some("http://127.0.0.1:9/manifest.json".into()),
        ..publisher.config()
    };
    let again =
        Arc::new(Interfaces::open(data.path(), Some(shipped(data.path())), offline).unwrap());
    assert_eq!(again.status().version, "v1");
    again.check().await;
    let status = again.status();
    assert_eq!(status.version, "v1");
    assert_eq!(
        status.problem.as_deref(),
        Some("Couldn't check for a new interface: no connection")
    );
}

#[tokio::test]
async fn a_bad_signature_a_portal_or_a_replayed_manifest_changes_nothing() {
    let publisher = Publisher::new().await;
    let data = tempfile::tempdir().unwrap();
    let v1 = archive("v1", "2026-10-10T00:00:00Z", &build("v1", &[]));
    let v2 = archive("v2", "2026-10-11T00:00:00Z", &build("v2", &[]));
    let interfaces = Arc::new(Interfaces::open(data.path(), None, publisher.config()).unwrap());
    assert_eq!(
        get(&interfaces, "/").await.0,
        StatusCode::NOT_FOUND,
        "nothing to serve yet"
    );
    publisher.publish("2026-10-11T00:00:00Z", std::slice::from_ref(&v2));
    interfaces.check().await;
    assert_eq!(interfaces.status().version, "v2");

    let other = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    publisher.publish_signed("2026-10-12T00:00:00Z", std::slice::from_ref(&v1), &other);
    let fresh = Arc::new(Interfaces::open(data.path(), None, publisher.config()).unwrap());
    fresh.check().await;
    assert_eq!(fresh.status().version, "v2");
    assert_eq!(
        fresh.status().problem.as_deref(),
        Some("Couldn't check for a new interface: the answer wasn't signed by UXDuck")
    );

    // A captive portal answers with its own page.
    publisher
        .served
        .put("/manifest.json", "<html>Sign in to the Wi-Fi</html>");
    fresh.check().await;
    assert_eq!(fresh.status().version, "v2");
    assert!(fresh.status().problem.unwrap().contains("signed"));

    // An old signed manifest, replayed.
    publisher.publish("2026-10-10T00:00:00Z", std::slice::from_ref(&v1));
    fresh.check().await;
    assert_eq!(fresh.status().version, "v2");
    assert_eq!(
        fresh.status().problem.as_deref(),
        Some("Couldn't check for a new interface: the answer was older than one already followed")
    );
}

#[tokio::test]
async fn a_broken_archive_keeps_what_was_served() {
    let publisher = Publisher::new().await;
    let data = tempfile::tempdir().unwrap();
    let (bytes, mut entry) = archive("v1", "2026-10-10T00:00:00Z", &build("v1", &[]));
    entry
        .files
        .insert("index.html".into(), sha(b"something else"));
    publisher.publish("2026-10-10T00:00:00Z", &[(bytes, entry)]);
    let interfaces = Arc::new(
        Interfaces::open(data.path(), Some(shipped(data.path())), publisher.config()).unwrap(),
    );
    interfaces.check().await;
    let status = interfaces.status();
    assert_eq!(status.version, "shipped");
    assert_eq!(
        status.problem.as_deref(),
        Some(
            "Couldn't install a new interface: index.html in the archive does not match its checksum"
        )
    );
    assert!(
        Store::open(data.path().join("interface"))
            .unwrap()
            .kept()
            .is_empty()
    );
}

#[tokio::test]
async fn three_interfaces_are_kept() {
    let publisher = Publisher::new().await;
    let data = tempfile::tempdir().unwrap();
    let interfaces = Arc::new(Interfaces::open(data.path(), None, publisher.config()).unwrap());
    let mut all = Vec::new();
    for day in 1..=5 {
        let version = format!("v{day}");
        let at = format!("2026-10-0{day}T00:00:00Z");
        let built = archive(&version, &at, &build("v", &[]));
        all.insert(0, built);
        publisher.publish(&at, &all);
        interfaces.check().await;
        assert_eq!(interfaces.status().version, version);
    }
    let kept: Vec<String> = Store::open(data.path().join("interface"))
        .unwrap()
        .kept()
        .into_iter()
        .map(|k| k.entry.version)
        .collect();
    assert_eq!(kept, ["v5", "v4", "v3"]);
}

#[tokio::test]
async fn an_interface_that_fails_to_start_is_marked_bad_and_the_previous_one_served() {
    let publisher = Publisher::new().await;
    let data = tempfile::tempdir().unwrap();
    let v1 = archive("v1", "2026-10-10T00:00:00Z", &build("v1", &[]));
    let v2 = archive("v2", "2026-10-11T00:00:00Z", &build("v2", &[]));
    let v3 = archive("v3", "2026-10-12T00:00:00Z", &build("v3", &[]));
    let interfaces = Arc::new(Interfaces::open(data.path(), None, publisher.config()).unwrap());
    publisher.publish("2026-10-10T00:00:00Z", std::slice::from_ref(&v1));
    interfaces.check().await;
    publisher.publish("2026-10-11T00:00:00Z", &[v2.clone(), v1.clone()]);
    interfaces.check().await;
    assert_eq!(interfaces.status().version, "v2");

    // The page reports that it could not start.
    interfaces.failed("v2", "a page could not start it: SyntaxError");
    assert_eq!(interfaces.status().version, "v1");
    let store = Store::open(data.path().join("interface")).unwrap();
    assert!(store.bad().contains_key("v2"));
    // The same manifest again: v2 stays out.
    interfaces.check().await;
    assert_eq!(interfaces.status().version, "v1");
    // And after a restart, offline.
    let offline = Config {
        url: Some("http://127.0.0.1:9/manifest.json".into()),
        ..publisher.config()
    };
    assert_eq!(
        Interfaces::open(data.path(), None, offline)
            .unwrap()
            .status()
            .version,
        "v1"
    );

    // A page that loads v3 and never says it is ready.
    publisher.publish("2026-10-12T00:00:00Z", &[v3, v2, v1]);
    interfaces.check().await;
    assert_eq!(interfaces.status().version, "v3");
    let _ = get(&interfaces, "/").await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    interfaces.overdue();
    assert_eq!(interfaces.status().version, "v1");
    assert!(store.bad().contains_key("v3"));

    // A version a page said was ready is never marked bad.
    interfaces.ready("v1");
    interfaces.failed("v1", "a later error");
    assert_eq!(interfaces.status().version, "v1");
}

#[tokio::test]
async fn updates_off_pins_the_current_interface() {
    let publisher = Publisher::new().await;
    let data = tempfile::tempdir().unwrap();
    let v1 = archive("v1", "2026-10-10T00:00:00Z", &build("v1", &[]));
    publisher.publish("2026-10-10T00:00:00Z", std::slice::from_ref(&v1));
    set_updates(data.path(), "off").unwrap();
    let interfaces = Arc::new(Interfaces::open(data.path(), None, publisher.config()).unwrap());
    interfaces.check().await;
    let status = interfaces.status();
    assert!(!status.updates);
    assert_eq!(status.version, "");
    set_updates(data.path(), "on").unwrap();
    interfaces.check().await;
    assert_eq!(interfaces.status().version, "v1");
    assert!(set_updates(data.path(), "maybe").is_err());
}

#[tokio::test]
async fn staging_is_followed_only_when_chosen_and_signed_as_staging() {
    let publisher = Publisher::new().await;
    let data = tempfile::tempdir().unwrap();
    let v1 = archive("v1", "2026-10-12T00:00:00Z", &build("v1", &[]));
    let v2 = archive("v2", "2026-10-11T00:00:00Z", &build("v2", &[]));
    publisher.publish("2026-10-12T00:00:00Z", std::slice::from_ref(&v1));
    // Staging publishes at its own times, here before production's last.
    publisher.publish_as(
        "staging",
        "staging",
        "2026-10-11T00:00:00Z",
        std::slice::from_ref(&v2),
        &publisher.key,
    );
    let interfaces = Arc::new(Interfaces::open(data.path(), None, publisher.config()).unwrap());
    interfaces.check().await;
    let status = interfaces.status();
    assert_eq!(status.version, "v1", "production unless chosen");
    assert_eq!(status.channel, None);

    set_channel(data.path(), "staging").unwrap();
    interfaces.check().await;
    let status = interfaces.status();
    assert_eq!(status.problem, None);
    assert_eq!(status.version, "v2");
    assert_eq!(status.channel.as_deref(), Some("staging"));
    assert!(status.updates);

    // Production's manifest where staging's belongs, signed: refused.
    publisher.publish_as(
        "staging",
        "production",
        "2026-10-13T00:00:00Z",
        std::slice::from_ref(&v1),
        &publisher.key,
    );
    interfaces.check().await;
    let status = interfaces.status();
    assert_eq!(status.version, "v2");
    assert_eq!(
        status.problem.as_deref(),
        Some(
            "Couldn't check for a new interface: the answer was for the production channel, not staging"
        )
    );

    // Pinning keeps the channel, and the next start keeps both.
    set_updates(data.path(), "off").unwrap();
    let again = Arc::new(Interfaces::open(data.path(), None, publisher.config()).unwrap());
    assert_eq!(again.status().channel.as_deref(), Some("staging"));
    assert!(!again.status().updates);
    set_updates(data.path(), "on").unwrap();

    set_channel(data.path(), "production").unwrap();
    again.check().await;
    let status = again.status();
    assert_eq!(status.problem, None);
    assert_eq!(status.version, "v1");
    assert_eq!(status.channel, None);
    assert!(set_channel(data.path(), "beta").is_err());
}

#[tokio::test]
async fn an_entry_for_a_newer_protocol_asks_for_a_newer_uxduck() {
    let publisher = Publisher::new().await;
    let data = tempfile::tempdir().unwrap();
    let v1 = archive("v1", "2026-10-10T00:00:00Z", &build("v1", &[]));
    let (bytes, mut entry) = archive("next", "2026-10-11T00:00:00Z", &build("next", &[]));
    entry.protocols = vec![PROTOCOL_VERSION + 1];
    publisher.publish("2026-10-11T00:00:00Z", &[(bytes, entry), v1]);
    let interfaces = Arc::new(Interfaces::open(data.path(), None, publisher.config()).unwrap());
    interfaces.check().await;
    let status = interfaces.status();
    assert_eq!(status.version, "v1");
    assert!(status.update_uxduck);
}

#[test]
fn development_keeps_and_fetches_nothing() {
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        dev: Some(VITE.into()),
        ..Config::from_env(Some(VITE.into()))
    };
    let interfaces = Interfaces::open(data.path(), None, config).unwrap();
    assert!(!interfaces.status().updates);
    assert!(!data.path().join("interface").exists());
}

#[tokio::test]
async fn a_missing_app_distinguishes_development_from_missing_files() {
    let data = tempfile::tempdir().unwrap();
    let missing = data.path().join("missing-ui");
    for dev in [Some(VITE.to_owned()), None] {
        let config = Config {
            url: None,
            dev: dev.clone(),
            ..Config::from_env(dev.clone())
        };
        let interfaces =
            Arc::new(Interfaces::open(data.path(), Some(missing.clone()), config).unwrap());
        let (status, body, _) = get(&interfaces, "/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        if dev.is_some() {
            assert!(body.contains("Running in development"));
            assert!(body.contains("<a href=\"http://localhost:5173/\">"));
            assert!(!body.contains("pnpm dev"));
        } else {
            assert!(body.contains("The app files are missing"));
            assert!(body.contains("--ui-dir"));
            assert!(!body.contains("uxduck open"));
        }
    }
}

#[cfg(not(feature = "embed-ui"))]
#[tokio::test]
async fn a_build_without_the_app_says_it_is_development() {
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        url: None,
        dev: None,
        ..Config::from_env(None)
    };
    let interfaces = Arc::new(Interfaces::open(data.path(), None, config).unwrap());
    let (status, body, _) = get(&interfaces, "/").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Running in development"));
    assert!(body.contains("is not bundled in development"));
    assert!(!body.contains("pnpm dev"));
    assert!(!body.contains("uxduck open"));
}
