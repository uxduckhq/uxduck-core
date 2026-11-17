//! Cameras' definition files on this computer (S411, ADR-0063 Y5): the
//! copy kept of each file by its version, under the data folder as
//! `camera-definitions/<key>.v<version>`, and the fetch of a file from an
//! `http` or `https` address a person agreed to.
//!
//! The address is the camera's to name, so it is fetched from with care:
//! within a time and a size, following only a few redirects that stay on
//! the same scheme, host and port, and saying nothing of what the address
//! answered beyond its status. The file itself is read by
//! `uxduck_mavlink::camera_definition`, which refuses what it must.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::daemon::engine_task::EngineHandle;

/// The most a definition file may hold as it is fetched, packed or not:
/// what a read over MAVLink FTP stops at too.
const MAX_BYTES: usize = uxduck_mavlink::camera_file::MAX_BYTES;
/// How long a fetch may take, from connecting to its last byte.
const LIMIT: Duration = Duration::from_secs(20);
/// How many redirects are followed.
const REDIRECTS: usize = 3;

const TOO_LARGE: &str =
    "The file is larger than 1 MB, the most UXDuck reads for a camera's definition.";

/// A fetch the engine asked for.
#[derive(Debug)]
pub struct Fetch {
    pub camera_id: String,
    pub uri: String,
}

/// Where a file is kept. The engine names it by a hash, checked here as
/// well, since it ends up in a path.
fn path(root: &Path, key: &str, version: u16) -> Option<PathBuf> {
    let plain = !key.is_empty() && key.len() <= 64 && key.chars().all(|c| c.is_ascii_hexdigit());
    plain.then(|| root.join(format!("{key}.v{version}")))
}

/// The copy kept under `key` with this version, if there is one.
pub fn kept(root: &Path, key: &str, version: u16) -> Option<Vec<u8>> {
    let file = fs::File::open(path(root, key, version)?).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_BYTES).then_some(bytes)
}

/// Keeps a file as it was fetched, in place of any other version kept
/// under `key`.
///
/// # Errors
/// Why it could not be kept.
pub fn keep(root: &Path, key: &str, version: u16, bytes: &[u8]) -> Result<(), String> {
    let path = path(root, key, version).ok_or_else(|| format!("no definition file {key}"))?;
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    // Whole or not at all: written beside it, then named.
    let partial = root.join(format!(".{key}"));
    fs::write(&partial, bytes)
        .and_then(|()| fs::rename(&partial, &path))
        .map_err(|e| e.to_string())?;
    let others = format!("{key}.v");
    for entry in fs::read_dir(root).map_err(|e| e.to_string())?.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(&others) && entry.path() != path {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// Fetches the files the engine asks for, each on a task of its own, and
/// gives what came of it back to the engine in order with everything else.
pub async fn work(mut fetches: mpsc::UnboundedReceiver<Fetch>, engine: EngineHandle) {
    while let Some(Fetch { camera_id, uri }) = fetches.recv().await {
        let engine = engine.clone();
        tokio::spawn(async move {
            let result = fetch(&uri, LIMIT).await;
            if let Err(why) = &result {
                tracing::info!(camera = %camera_id, %why, "a camera's definition file was not fetched");
            }
            engine.post(move |e| e.camera_definition_fetched(&camera_id, &uri, result));
        });
    }
}

/// Whether a redirect stays where the first request went.
fn same_place(first: &reqwest::Url, next: &reqwest::Url) -> bool {
    first.scheme() == next.scheme()
        && first.host_str() == next.host_str()
        && first.port_or_known_default() == next.port_or_known_default()
}

/// The bytes at an `http` or `https` address.
///
/// # Errors
/// Why there are none, as a sentence for people. It never quotes the
/// address, which may hold a password, nor anything the address answered
/// beyond its status.
pub async fn fetch(uri: &str, limit: Duration) -> Result<Vec<u8>, String> {
    let url = reqwest::Url::parse(uri)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
        .ok_or("The address is not one UXDuck can fetch from.")?;
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        let stays = attempt
            .previous()
            .first()
            .is_some_and(|first| same_place(first, attempt.url()));
        if !stays {
            attempt.error("a redirect to another place")
        } else if attempt.previous().len() > REDIRECTS {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    });
    let client = reqwest::Client::builder()
        .timeout(limit)
        .user_agent(concat!("uxduck/", env!("CARGO_PKG_VERSION")))
        .redirect(policy)
        .build()
        .map_err(|_| "UXDuck could not start the fetch.")?;
    let sent_on = || {
        format!(
            "The address sends UXDuck on to another host or scheme, or on more than {REDIRECTS} times, which it does not follow."
        )
    };
    let failed = |error: reqwest::Error| -> String {
        if error.is_timeout() {
            format!(
                "The address did not answer within {} seconds.",
                limit.as_secs()
            )
        } else if error.is_redirect() {
            sent_on()
        } else if error.is_connect() {
            "UXDuck could not connect to the address.".into()
        } else {
            "The file could not be fetched from the address.".into()
        }
    };
    let mut response = client.get(url).send().await.map_err(failed)?;
    let status = response.status();
    // A redirect that is not followed: to an address that is not a web
    // address at all, such as a file's.
    if status.is_redirection() {
        return Err(sent_on());
    }
    if !status.is_success() {
        return Err(format!("The address answered {status}."));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BYTES as u64)
    {
        return Err(TOO_LARGE.into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(failed)? {
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err(TOO_LARGE.into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    /// A server on this computer that answers each request with what
    /// `answer` makes of its path, and counts them. With its port.
    async fn server(
        answer: impl Fn(&str, u16) -> Vec<u8> + Send + Sync + 'static,
    ) -> (u16, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let asked = Arc::new(AtomicUsize::new(0));
        let count = asked.clone();
        let answer = Arc::new(answer);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let answer = answer.clone();
                let count = count.clone();
                tokio::spawn(async move {
                    let mut request = vec![0u8; 4096];
                    let read = stream.read(&mut request).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&request[..read]).into_owned();
                    let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    count.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(&answer(&path, port)).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (port, asked)
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn to(location: &str) -> Vec<u8> {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .into_bytes()
    }

    #[tokio::test]
    async fn a_file_at_an_address_is_fetched_whole() {
        let (port, asked) = server(|path, _| match path {
            "/camera.xml" => ok(b"<mavlinkcamera />"),
            _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecret"
                .to_vec(),
        })
        .await;
        let bytes = fetch(
            &format!("http://user:pass@127.0.0.1:{port}/camera.xml"),
            LIMIT,
        )
        .await;
        assert_eq!(bytes.unwrap(), b"<mavlinkcamera />");
        assert_eq!(asked.load(Ordering::SeqCst), 1);

        let missing = fetch(
            &format!("http://user:pass@127.0.0.1:{port}/none.xml"),
            LIMIT,
        )
        .await
        .unwrap_err();
        assert_eq!(missing, "The address answered 404 Not Found.");
        assert!(!missing.contains("secret") && !missing.contains("pass"));
    }

    #[tokio::test]
    async fn only_http_and_https_addresses_are_fetched_from() {
        for uri in [
            "file:///etc/passwd",
            "ftp://127.0.0.1/camera.xml",
            "mftp:///camera.xml",
            "http://",
            "camera.xml",
            "",
        ] {
            assert_eq!(
                fetch(uri, LIMIT).await.unwrap_err(),
                "The address is not one UXDuck can fetch from.",
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn a_file_past_the_size_limit_is_refused_whatever_its_length_says() {
        let (port, _) = server(|path, _| match path {
            // Says its length.
            "/said" => ok(&vec![b'x'; MAX_BYTES + 1]),
            // Says none, and goes on past the limit.
            "/unsaid" => {
                let mut out = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
                out.extend(std::iter::repeat_n(b'x', MAX_BYTES + 1));
                out
            }
            _ => ok(&vec![b'x'; MAX_BYTES]),
        })
        .await;
        for path in ["said", "unsaid"] {
            assert_eq!(
                fetch(&format!("http://127.0.0.1:{port}/{path}"), LIMIT)
                    .await
                    .unwrap_err(),
                TOO_LARGE,
                "{path}"
            );
        }
        let most = fetch(&format!("http://127.0.0.1:{port}/most"), LIMIT).await;
        assert_eq!(most.unwrap().len(), MAX_BYTES);
    }

    #[tokio::test]
    async fn a_few_redirects_on_the_same_host_are_followed_and_no_others() {
        let (port, asked) = server(|path, port| match path {
            "/camera.xml" => ok(b"here"),
            "/moved" | "/1" => to("/camera.xml"),
            "/3" => to("/2"),
            "/2" => to("/1"),
            "/4" => to("/3"),
            // The same computer by another name is another host.
            "/elsewhere" => to(&format!("http://localhost:{port}/camera.xml")),
            "/other-port" => to("http://127.0.0.1:1/camera.xml"),
            "/other-scheme" => to(&format!("https://127.0.0.1:{port}/camera.xml")),
            "/a-file" => to("file:///etc/passwd"),
            _ => to("/loop"),
        })
        .await;
        let at = |path: &str| format!("http://127.0.0.1:{port}/{path}");
        assert_eq!(fetch(&at("moved"), LIMIT).await.unwrap(), b"here");
        assert_eq!(fetch(&at("3"), LIMIT).await.unwrap(), b"here", "three");
        let refused = "The address sends UXDuck on to another host or scheme, or on more than 3 times, which it does not follow.";
        for path in [
            "4",
            "loop",
            "elsewhere",
            "other-port",
            "other-scheme",
            "a-file",
        ] {
            let before = asked.load(Ordering::SeqCst);
            assert_eq!(
                fetch(&at(path), LIMIT).await.unwrap_err(),
                refused,
                "{path}"
            );
            if path != "4" && path != "loop" {
                assert_eq!(
                    asked.load(Ordering::SeqCst),
                    before + 1,
                    "{path}: nothing is asked of the other place"
                );
            }
        }
    }

    #[tokio::test]
    async fn an_address_that_does_not_answer_in_time_is_given_up_on() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Takes the connection and says nothing.
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        let slow = fetch(
            &format!("http://127.0.0.1:{port}/camera.xml"),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(
            slow.unwrap_err(),
            "The address did not answer within 1 seconds."
        );
        // Nothing listens here.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        assert_eq!(
            fetch(&format!("http://127.0.0.1:{port}/camera.xml"), LIMIT)
                .await
                .unwrap_err(),
            "UXDuck could not connect to the address."
        );
    }

    #[test]
    fn a_kept_file_is_read_back_by_version_and_another_version_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("camera-definitions");
        let key = "0123456789abcdef0123456789abcdef";
        assert_eq!(kept(&root, key, 1), None);
        keep(&root, key, 1, b"one").unwrap();
        assert_eq!(kept(&root, key, 1).as_deref(), Some(&b"one"[..]));
        assert_eq!(kept(&root, key, 2), None, "another version is not this one");
        keep(&root, "ffff", 1, b"another camera's").unwrap();

        keep(&root, key, 2, b"two").unwrap();
        assert_eq!(kept(&root, key, 2).as_deref(), Some(&b"two"[..]));
        assert_eq!(kept(&root, key, 1), None, "replaced");
        assert_eq!(
            kept(&root, "ffff", 1).as_deref(),
            Some(&b"another camera's"[..])
        );
        let mut names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, [format!("{key}.v2"), "ffff.v1".to_owned()]);

        // A name that is not a hash never reaches a path.
        assert!(keep(&root, "../escape", 1, b"x").is_err());
        assert_eq!(kept(&root, "../escape", 1), None);
        assert!(keep(&root, "", 1, b"x").is_err());
        // A file grown past the limit on disk is not read.
        fs::write(root.join(format!("{key}.v2")), vec![0u8; MAX_BYTES + 1]).unwrap();
        assert_eq!(kept(&root, key, 2), None);
    }
}
