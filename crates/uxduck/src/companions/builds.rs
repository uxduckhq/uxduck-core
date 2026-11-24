//! The builds Install and Update copy to a companion computer (S297,
//! ADR-0061 B5). Each release lists them in its own signed manifest,
//! `releases/v<version>/<channel>.json` beside its files, as artifacts of
//! kind `companion`: one `uxduck` for each Linux target, in a `tar.gz`.
//! The manifest's signature is checked here with the release key, then the
//! build's size and SHA-256 against it, and the computer checks the SHA-256
//! again before it changes anything. Both are kept under
//! `<data dir>/companions/builds/<version>/`, so a ground station in a field
//! with no internet installs what it downloaded before, the manifest's
//! signature checked again each time.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use uxduck_contracts::dto::release::ReleaseArtifact;

/// The artifact kind of a companion build in a release's manifest.
pub(crate) const KIND: &str = "companion";

/// Where releases are published, each in `v<version>/`.
const RELEASES: &str = "https://app.uxduck.com/releases";

/// The most of a manifest read before its signature is checked: what an
/// untrusted server can make UXDuck hold. A release's manifest lists a few
/// dozen files in a few kilobytes.
const MANIFEST_MAX: usize = 1024 * 1024;

/// How long a download may send nothing before it is given up: a server
/// that stops sending would otherwise hold the Install open with no end.
const STALLED: Duration = Duration::from_secs(60);

/// A build kept on this computer, checked against the signed manifest.
pub(crate) struct Build {
    pub path: PathBuf,
    pub sha256: String,
    /// What fetching it did, for people.
    pub said: String,
}

/// The channel a version is published on.
fn channel(version: &str) -> &'static str {
    if version.contains('-') {
        "beta"
    } else {
        "stable"
    }
}

/// Where builds come from and the key their manifests are signed with.
pub(crate) struct Source {
    /// The folder holding each release's `v<version>/`.
    pub base: String,
    /// The minisign public key, its base64 line.
    pub key: String,
}

impl Source {
    /// The releases, checked with the release key. A development build
    /// takes `UXDUCK_COMPANION_BUILDS_URL` and `UXDUCK_COMPANION_BUILDS_KEY`
    /// in their place, for releases signed with a key of their own.
    pub(crate) fn releases() -> Result<Self, String> {
        let base = match crate::update::url_override("UXDUCK_COMPANION_BUILDS_URL") {
            Some(url) if url == "none" => {
                return Err("This UXDuck was started with UXDUCK_COMPANION_BUILDS_URL=none, so it downloads no builds for companion computers.".into());
            }
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => RELEASES.to_owned(),
        };
        let key = std::env::var("UXDUCK_COMPANION_BUILDS_KEY")
            .ok()
            .filter(|_| cfg!(debug_assertions))
            .unwrap_or_else(crate::update::data_key);
        Ok(Self { base, key })
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(STALLED)
        .read_timeout(STALLED)
        .user_agent(concat!("uxduck/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("Cannot prepare the download: {e}"))
}

async fn fetch_small(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if bytes.len() + chunk.len() > MANIFEST_MAX {
            return Err(format!(
                "it is larger than the {} KiB a release's list of files can be",
                MANIFEST_MAX / 1024
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// The SHA-256 of the file at `path`, when it can be read.
fn sha256(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Some(format!(
        "{:x}",
        uxduck_core::encoding::Hex(&hash.finalize())
    ))
}

/// The signed manifest of `version`: downloaded and kept, or the one kept
/// before when it cannot be downloaded.
async fn manifest(
    client: &reqwest::Client,
    base: &str,
    dir: &Path,
    version: &str,
) -> Result<(Vec<u8>, String, bool), String> {
    let name = format!("{}.json", channel(version));
    let url = format!("{base}/v{version}/{name}");
    let fetched = async {
        let bytes = fetch_small(client, &url).await?;
        let signature = fetch_small(client, &format!("{url}.minisig")).await?;
        Ok::<_, String>((bytes, signature))
    }
    .await;
    match fetched {
        Ok((bytes, signature)) => {
            let signature = String::from_utf8(signature)
                .map_err(|_| format!("The signature at {url}.minisig is not text."))?;
            std::fs::create_dir_all(dir)
                .and_then(|()| std::fs::write(dir.join(&name), &bytes))
                .and_then(|()| std::fs::write(dir.join(format!("{name}.minisig")), &signature))
                .map_err(|e| {
                    format!(
                        "Cannot keep UXDuck {version}'s list of builds in {}: {e}",
                        dir.display()
                    )
                })?;
            Ok((bytes, signature, true))
        }
        Err(why) => match (
            std::fs::read(dir.join(&name)),
            std::fs::read_to_string(dir.join(format!("{name}.minisig"))),
        ) {
            (Ok(bytes), Ok(signature)) => Ok((bytes, signature, false)),
            _ => Err(format!(
                "Cannot download UXDuck {version}'s signed list of builds from {url}: {why}. Connect this computer to the internet and try again."
            )),
        },
    }
}

/// Downloads `artifact` to `path`, refusing more than its signed size and
/// anything whose SHA-256 differs.
async fn download(
    client: &reqwest::Client,
    artifact: &ReleaseArtifact,
    path: &Path,
) -> Result<(), String> {
    let partial = path.with_extension("part");
    let mut response = client
        .get(&artifact.url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let mut out = tokio::fs::File::create(&partial)
        .await
        .map_err(|e| format!("{}: {e}", partial.display()))?;
    let mut hash = Sha256::new();
    let mut have = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        have += chunk.len() as u64;
        if have > artifact.size {
            let _ = std::fs::remove_file(&partial);
            return Err(format!(
                "it is larger than the {} bytes the release signed",
                artifact.size
            ));
        }
        hash.update(&chunk);
        out.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    out.sync_all().await.map_err(|e| e.to_string())?;
    drop(out);
    let got = format!("{:x}", uxduck_core::encoding::Hex(&hash.finalize()));
    if have != artifact.size || got != artifact.sha256 {
        let _ = std::fs::remove_file(&partial);
        return Err("it is not the file the release signed".into());
    }
    std::fs::rename(&partial, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// UXDuck `version`'s build for `target` computers, from its release,
/// checked against the release's signed manifest.
pub(crate) async fn build(
    data_dir: &Path,
    source: &Source,
    version: &str,
    target: &str,
) -> Result<Build, String> {
    let dir = data_dir.join("companions").join("builds").join(version);
    let client = client()?;
    let (bytes, signature, fresh) = manifest(&client, &source.base, &dir, version).await?;
    let manifest = crate::update::verify(&bytes, &signature, &source.key).map_err(|e| {
        format!("UXDuck {version}'s list of builds does not verify, so nothing was copied: {e}")
    })?;
    if manifest.version != version || manifest.channel != channel(version) {
        return Err(format!(
            "The list of builds published for UXDuck {version} is signed for {} {}, so nothing was copied.",
            manifest.channel, manifest.version
        ));
    }
    let artifact = manifest
        .artifacts
        .iter()
        .find(|a| a.kind == KIND && a.target == target)
        .ok_or_else(|| {
            format!("UXDuck {version} has no build for {target} companion computers.")
        })?;
    let file = Path::new(&artifact.file);
    if file.components().count() != 1 || file.file_name().is_none() {
        return Err(format!(
            "UXDuck {version}'s list of builds names {} as a file, which UXDuck refuses.",
            artifact.file
        ));
    }
    let path = dir.join(file);
    if sha256(&path).as_deref() == Some(artifact.sha256.as_str()) {
        return Ok(Build {
            path,
            sha256: artifact.sha256.clone(),
            said: format!(
                "Used {} downloaded before, {} bytes. Its SHA-256 matches the release's signed list{}.",
                artifact.file,
                artifact.size,
                if fresh {
                    ""
                } else {
                    ", kept from an earlier download because this computer could not reach the release"
                }
            ),
        });
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    download(&client, artifact, &path).await.map_err(|why| {
        format!(
            "Cannot download {} from {}: {why}.",
            artifact.file, artifact.url
        )
    })?;
    Ok(Build {
        path,
        sha256: artifact.sha256.clone(),
        said: format!(
            "Downloaded {}, {} bytes. Its SHA-256 matches the release's signed list.",
            artifact.file, artifact.size
        ),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use std::collections::HashMap;

    use axum::response::IntoResponse as _;
    use uxduck_contracts::dto::release::{ReleaseArtifact, ReleaseManifest};

    use super::{Source, build};

    const TARGET: &str = "aarch64-unknown-linux-gnu";

    fn sha(bytes: &[u8]) -> String {
        use sha2::Digest as _;
        format!(
            "{:x}",
            uxduck_core::encoding::Hex(&sha2::Sha256::digest(bytes))
        )
    }

    /// A release signed for `version` by a new key, published under
    /// `v<at>/`, whose list names `listed` as its build and whose server
    /// sends `sent`.
    async fn release(version: &str, at: &str, listed: &[u8], sent: &[u8]) -> Source {
        let key = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let public = key.pk.to_box().unwrap().into_string();
        let public = public.lines().nth(1).unwrap().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let file = format!("uxduck-{version}-{TARGET}.companion.tar.gz");
        let manifest = ReleaseManifest {
            schema: 1,
            channel: "stable".into(),
            version: version.into(),
            published_at: "2026-11-23T00:00:00Z".into(),
            rollback: false,
            notes_url: None,
            desktop_app: None,
            artifacts: vec![ReleaseArtifact {
                target: TARGET.into(),
                kind: super::KIND.into(),
                file: file.clone(),
                url: format!("{base}/v{at}/{file}"),
                sha256: sha(listed),
                size: listed.len() as u64,
            }],
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let signature = minisign::sign(None, &key.sk, std::io::Cursor::new(&bytes), None, None)
            .unwrap()
            .into_string();
        let files: HashMap<String, Vec<u8>> = [
            (format!("/v{at}/stable.json"), bytes),
            (
                format!("/v{at}/stable.json.minisig"),
                signature.into_bytes(),
            ),
            (format!("/v{at}/{file}"), sent.to_vec()),
        ]
        .into();
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let files = files.clone();
            async move {
                match files.get(uri.path()) {
                    Some(body) => body.clone().into_response(),
                    None => axum::http::StatusCode::NOT_FOUND.into_response(),
                }
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Source { base, key: public }
    }

    fn walk(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path.display().to_string());
            }
        }
        out
    }

    #[tokio::test]
    async fn a_build_is_downloaded_checked_and_kept_for_a_field_with_no_internet() {
        let data = tempfile::tempdir().unwrap();
        let source = release("0.1.0", "0.1.0", b"the build", b"the build").await;
        let got = build(data.path(), &source, "0.1.0", TARGET).await.unwrap();
        assert_eq!(std::fs::read(&got.path).unwrap(), b"the build");
        assert_eq!(got.sha256, sha(b"the build"));
        assert!(got.said.starts_with("Downloaded"), "{}", got.said);
        let again = build(data.path(), &source, "0.1.0", TARGET).await.unwrap();
        assert!(again.said.starts_with("Used"), "{}", again.said);
        let offline = Source {
            base: "http://127.0.0.1:1".into(),
            key: source.key.clone(),
        };
        let kept = build(data.path(), &offline, "0.1.0", TARGET).await.unwrap();
        assert!(
            kept.said.contains("could not reach the release"),
            "{}",
            kept.said
        );
        let forged = Source {
            base: "http://127.0.0.1:1".into(),
            key: release("0.1.0", "0.1.0", b"x", b"x").await.key,
        };
        let why = build(data.path(), &forged, "0.1.0", TARGET)
            .await
            .err()
            .unwrap();
        assert!(why.contains("does not verify"), "{why}");
        let why = build(data.path(), &source, "0.1.0", "x86_64-unknown-linux-gnu")
            .await
            .err()
            .unwrap();
        assert!(
            why.contains("has no build for x86_64-unknown-linux-gnu"),
            "{why}"
        );
        assert!(!why.contains(';'));
    }

    #[tokio::test]
    async fn a_build_that_is_not_the_signed_one_is_refused_and_not_kept() {
        let data = tempfile::tempdir().unwrap();
        let source = release("0.1.0", "0.1.0", b"the build", b"the bulid").await;
        let why = build(data.path(), &source, "0.1.0", TARGET)
            .await
            .err()
            .unwrap();
        assert!(why.contains("not the file the release signed"), "{why}");
        let longer = release("0.2.0", "0.2.0", b"the build", b"the build and more").await;
        let why = build(data.path(), &longer, "0.2.0", TARGET)
            .await
            .err()
            .unwrap();
        assert!(why.contains("larger than"), "{why}");
        let kept = walk(data.path());
        assert!(
            kept.iter().all(|p| std::path::Path::new(p)
                .extension()
                .is_some_and(|e| e == "json" || e == "minisig")),
            "{kept:?}"
        );
    }

    #[tokio::test]
    async fn a_list_signed_for_another_version_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let source = release("0.1.0", "0.3.0", b"b", b"b").await;
        let why = build(data.path(), &source, "0.3.0", TARGET)
            .await
            .err()
            .unwrap();
        assert!(why.contains("signed for stable 0.1.0"), "{why}");
        let why = build(data.path(), &source, "0.4.0", TARGET)
            .await
            .err()
            .unwrap();
        assert!(
            why.contains("Cannot download UXDuck 0.4.0's signed list"),
            "{why}"
        );
    }
}
