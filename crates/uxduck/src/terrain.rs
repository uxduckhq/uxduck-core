//! Terrain on this computer (S23, ADR-0026 T3): the files the cache keeps
//! under `<data dir>/terrain/`, by their path in the bucket, and fetches
//! from `https://terrain.uxduck.com`. Nothing is checked here; the engine's
//! cache checks every file against the signed manifest before it is used
//! or kept.

use std::path::{Path, PathBuf};
use std::time::Duration;

use uxduck_engine::terrain::TerrainPort;
use uxduck_terrain::manifest::DEFAULT_BASE_URL;
use uxduck_terrain::{Arrived, FetchError, Order};

use crate::daemon::engine_task::EngineHandle;

/// No terrain file is near this large.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Where terrain comes from, and the key its manifest must be signed with.
#[derive(Clone, Debug)]
pub struct Source {
    pub base_url: String,
    pub key: Option<String>,
}

impl Source {
    /// `UXDUCK_TERRAIN_URL` serves it from elsewhere (a test fixture), and
    /// `none` turns terrain off (tests fetch nothing). The public key is
    /// the release key built into this binary; a development build takes
    /// `UXDUCK_TERRAIN_KEY` instead, and the development daemon (`dev`)
    /// the checkout's copy of the release key.
    ///
    /// # Errors
    /// For the development daemon with terrain on and no key, which would
    /// answer no heights: it says how to get the key.
    pub fn from_env(dev: bool) -> Result<Option<Self>, String> {
        let base_url = match crate::update::url_override("UXDUCK_TERRAIN_URL") {
            Some(url) if url == "none" => return Ok(None),
            Some(url) if !url.trim().is_empty() => url,
            _ => DEFAULT_BASE_URL.to_owned(),
        };
        let key = std::env::var("UXDUCK_TERRAIN_KEY")
            .ok()
            .filter(|_| cfg!(debug_assertions))
            .or_else(|| {
                if dev {
                    crate::update::development_key()
                } else {
                    crate::update::PUBLIC_KEY.map(str::to_owned)
                }
            });
        if dev && key.is_none() {
            return Err(
                "terrain needs the release public key: run `cargo xtask release-key` \
                 (it reads it from Infisical once), or set UXDUCK_TERRAIN_URL=none to run \
                 without terrain"
                    .into(),
            );
        }
        Ok(Some(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            key,
        }))
    }
}

/// The engine's port: each order on its own task.
pub struct Host {
    dir: PathBuf,
    base_url: String,
    engine: EngineHandle,
}

impl Host {
    #[must_use]
    pub fn new(data_dir: &Path, base_url: String, engine: EngineHandle) -> Self {
        Self {
            dir: data_dir.join("terrain"),
            base_url,
            engine,
        }
    }
}

/// A key as a path under the folder; `None` for one that would leave it.
fn path(dir: &Path, key: &str) -> Option<PathBuf> {
    let plain = !key.is_empty()
        && key.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        });
    plain.then(|| dir.join(key))
}

impl TerrainPort for Host {
    fn order(&mut self, order: Order) {
        let dir = self.dir.clone();
        let base_url = self.base_url.clone();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let arrived = match order {
                Order::Read { key } => {
                    let bytes = path(&dir, &key).and_then(|p| std::fs::read(p).ok());
                    Some(Arrived::Read { key, bytes })
                }
                Order::Fetch { key } => {
                    let result = fetch(&format!("{base_url}/{key}")).await;
                    Some(Arrived::Fetched { key, result })
                }
                Order::Write { key, bytes } => {
                    if let Err(error) = write(&dir, &key, &bytes) {
                        tracing::warn!(%error, key, "cannot keep a terrain file");
                    }
                    None
                }
                Order::Remove { prefix } => {
                    if let Some(folder) = path(&dir, prefix.trim_end_matches('/')) {
                        let _ = std::fs::remove_dir_all(folder);
                    }
                    None
                }
            };
            if let Some(arrived) = arrived {
                let _ = engine.run(move |e| e.terrain_arrived(arrived)).await;
            }
        });
    }
}

fn write(dir: &Path, key: &str, bytes: &[u8]) -> Result<(), String> {
    let path = path(dir, key).ok_or_else(|| format!("{key} is not a terrain file"))?;
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    }
    let part = path.with_extension("part");
    std::fs::write(&part, bytes)
        .and_then(|()| std::fs::rename(&part, &path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

async fn fetch(url: &str) -> Result<Vec<u8>, FetchError> {
    let failed = |offline: bool, message: String| FetchError { offline, message };
    let client = crate::sim::catalogue::client().map_err(|e| failed(false, e))?;
    let response = client
        .get(url)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| {
            failed(
                e.is_connect() || e.is_timeout(),
                format!("cannot fetch {url}: {e}"),
            )
        })?;
    if !response.status().is_success() {
        return Err(failed(
            false,
            format!("cannot fetch {url}: {}", response.status()),
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_FILE_BYTES)
    {
        return Err(failed(false, format!("{url} is too large")));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| failed(e.is_timeout(), format!("cannot fetch {url}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_stay_in_the_folder() {
        let dir = Path::new("/data/terrain");
        assert_eq!(
            path(dir, "alos/2026-10-14/N53W005/34.uxdt"),
            Some(dir.join("alos/2026-10-14/N53W005/34.uxdt"))
        );
        assert_eq!(path(dir, "manifest.json"), Some(dir.join("manifest.json")));
        assert_eq!(path(dir, "../etc/passwd"), None);
        assert_eq!(path(dir, "alos//x"), None);
        assert_eq!(path(dir, "/etc/passwd"), None);
        assert_eq!(path(dir, ""), None);
    }

    #[test]
    fn a_file_is_kept_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "alos/r/N53W005/00.uxdt", b"x").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("alos/r/N53W005/00.uxdt")).unwrap(),
            b"x"
        );
        assert!(write(dir.path(), "../x", b"x").is_err());
    }
}
