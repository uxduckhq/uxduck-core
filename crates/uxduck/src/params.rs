//! Parameter definitions on this computer (S67, ADR-0021 P2, P4, P5): the
//! signed catalogue, the sets downloaded from it, and the sets people add,
//! under `<data dir>/parameters/`. The engine asks through its
//! [`DefinitionsPort`]; each order runs on its own task and answers through
//! the engine handle.
//!
//! The last catalogue that verified is kept, so sets already downloaded
//! still describe their vehicles offline. A kept set is checked against the
//! catalogue's SHA-256 before it is read, as a downloaded one is.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use uxduck_contracts::dto::param_definitions::CatalogueSet;
use uxduck_core::domain::param_definitions::DefinitionSet;
use uxduck_engine::param_definitions::files::{
    DEFAULT_URL, MAX_SET_BYTES, NO_CATALOGUE_KEY, gunzip_set, read_set, sha256, usable, verify,
};
use uxduck_engine::param_definitions::{DefinitionsArrived, DefinitionsOrder, DefinitionsPort};

use crate::daemon::engine_task::EngineHandle;

/// The newest manifest seen, so an older signed one cannot be replayed.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Seen {
    published_at: String,
}

/// Where the catalogue comes from, and the key it must be signed with.
#[derive(Clone, Debug)]
pub struct Source {
    pub url: Option<String>,
    pub key: Option<String>,
}

impl Source {
    /// `UXDUCK_PARAM_CATALOGUE_URL` points elsewhere, and `none` turns the
    /// catalogue off. The public verification key is built into this binary;
    /// a development build takes `UXDUCK_PARAM_CATALOGUE_KEY` instead, for
    /// tests or a publisher's own catalogue.
    #[must_use]
    pub fn from_env() -> Self {
        let url = match crate::update::url_override("UXDUCK_PARAM_CATALOGUE_URL") {
            Some(url) if url == "none" => None,
            Some(url) => Some(url),
            None => Some(DEFAULT_URL.to_owned()),
        };
        let key = std::env::var("UXDUCK_PARAM_CATALOGUE_KEY")
            .ok()
            .filter(|_| cfg!(debug_assertions))
            .or_else(|| crate::update::PUBLIC_KEY.map(str::to_owned));
        Self { url, key }
    }
}

/// The engine's port: each order on its own task.
pub struct Host {
    dir: PathBuf,
    source: Source,
    engine: EngineHandle,
}

impl Host {
    #[must_use]
    pub fn new(data_dir: &Path, source: Source, engine: EngineHandle) -> Self {
        Self {
            dir: data_dir.join("parameters"),
            source,
            engine,
        }
    }
}

impl DefinitionsPort for Host {
    fn request(&mut self, order: DefinitionsOrder) {
        let dir = self.dir.clone();
        let source = self.source.clone();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let arrived = match order {
                DefinitionsOrder::Catalogue { .. } => Some(DefinitionsArrived::Catalogue(
                    catalogue(&dir, &source).await,
                )),
                DefinitionsOrder::Set(set) => Some(DefinitionsArrived::Set {
                    id: set.id.clone(),
                    result: catalogue_set(&dir, &set).await,
                }),
                DefinitionsOrder::Own { id } => {
                    let result = read_own(&dir, &id);
                    Some(DefinitionsArrived::Set { id, result })
                }
                DefinitionsOrder::StoreOwn { id, set } => {
                    if let Err(error) = write_own(&dir, &id, &set) {
                        tracing::warn!(%error, id, "cannot keep a definition set");
                    }
                    None
                }
                DefinitionsOrder::RemoveOwn { id } => {
                    let _ = std::fs::remove_file(own_path(&dir, &id));
                    None
                }
                DefinitionsOrder::FetchOwn { id, url } => {
                    let result = get(&url).await.and_then(|bytes| {
                        String::from_utf8(bytes).map_err(|_| format!("{url} is not text"))
                    });
                    Some(DefinitionsArrived::Fetched { id, result })
                }
            };
            if let Some(arrived) = arrived {
                let _ = engine.run(move |e| e.definitions_arrived(arrived)).await;
            }
        });
    }
}

/// The catalogue's sets: fetched and verified, or else the last one kept.
async fn catalogue(dir: &Path, source: &Source) -> Result<Vec<CatalogueSet>, String> {
    let url = source
        .url
        .as_deref()
        .ok_or("the definitions catalogue is off here")?;
    let key = source
        .key
        .as_deref()
        .filter(|key| !key.trim().is_empty())
        .ok_or(NO_CATALOGUE_KEY)?;
    let fetched = fetch_catalogue(dir, url, key).await;
    match fetched {
        Ok(sets) => Ok(sets),
        Err(why) => match kept_catalogue(dir, key) {
            Some(sets) => {
                tracing::info!(%why, "using the kept parameter definitions catalogue");
                Ok(sets)
            }
            None => Err(why),
        },
    }
}

async fn fetch_catalogue(dir: &Path, url: &str, key: &str) -> Result<Vec<CatalogueSet>, String> {
    let bytes = get(url).await?;
    let signature = get(&format!("{url}.minisig")).await?;
    let signature = String::from_utf8_lossy(&signature).into_owned();
    let catalogue = verify(&bytes, &signature, key)?;
    // Refuse a manifest older than the newest seen: an old signed one could
    // list sets since withdrawn.
    let seen_path = dir.join("seen.json");
    let seen: Seen = std::fs::read(&seen_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if catalogue.published_at < seen.published_at {
        return Err(format!(
            "the catalogue was published at {}, before one already seen ({}). It is ignored",
            catalogue.published_at, seen.published_at
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let _ = std::fs::write(
        &seen_path,
        serde_json::to_vec(&Seen {
            published_at: catalogue.published_at.clone(),
        })
        .unwrap_or_default(),
    );
    let _ = std::fs::write(dir.join("catalogue.json"), &bytes);
    let _ = std::fs::write(dir.join("catalogue.json.minisig"), signature);
    Ok(usable(catalogue))
}

fn kept_catalogue(dir: &Path, key: &str) -> Option<Vec<CatalogueSet>> {
    let bytes = std::fs::read(dir.join("catalogue.json")).ok()?;
    let signature = std::fs::read_to_string(dir.join("catalogue.json.minisig")).ok()?;
    verify(&bytes, &signature, key).ok().map(usable)
}

/// A catalogue set: kept if its bytes still match, else downloaded and kept.
async fn catalogue_set(dir: &Path, set: &CatalogueSet) -> Result<DefinitionSet, String> {
    let path = dir.join("sets").join(&set.file.name);
    if let Some(kept) = std::fs::read(&path)
        .ok()
        .filter(|bytes| sha256(bytes) == set.file.sha256)
    {
        return gunzip_set(&kept);
    }
    let bytes = get(&set.file.url).await?;
    let read = read_set(set, &bytes)?;
    let folder = dir.join("sets");
    std::fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let part = path.with_extension("part");
    std::fs::write(&part, &bytes)
        .and_then(|()| std::fs::rename(&part, &path))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(read)
}

fn own_path(dir: &Path, id: &str) -> PathBuf {
    // The engine mints the id; keep it a plain name all the same.
    let name: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    dir.join("own").join(format!("{name}.json.gz"))
}

fn write_own(dir: &Path, id: &str, set: &DefinitionSet) -> Result<(), String> {
    let path = own_path(dir, id);
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    }
    let json = serde_json::to_vec(set).map_err(|e| e.to_string())?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&json).map_err(|e| e.to_string())?;
    let bytes = encoder.finish().map_err(|e| e.to_string())?;
    let part = path.with_extension("part");
    std::fs::write(&part, bytes)
        .and_then(|()| std::fs::rename(&part, &path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn read_own(dir: &Path, id: &str) -> Result<DefinitionSet, String> {
    let path = own_path(dir, id);
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    gunzip_set(&bytes)
}

async fn get(url: &str) -> Result<Vec<u8>, String> {
    let client = crate::sim::catalogue::client()?;
    let response = client
        .get(url)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("cannot fetch {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("cannot fetch {url}: {}", response.status()));
    }
    if response.content_length().is_some_and(|n| n > MAX_SET_BYTES) {
        return Err(format!("{url} is too large"));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("cannot fetch {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_core::domain::param_definitions::{DefinitionSource, Firmware, ParamDefinition};

    fn set() -> DefinitionSet {
        DefinitionSet {
            source: DefinitionSource::Own {
                label: "My fork".into(),
            },
            firmware: Firmware::ArduCopter,
            version: None,
            commit: Some("2a3dc4b7".into()),
            parameters: vec![ParamDefinition {
                name: "MYFORK_GAIN".into(),
                display_name: "My fork's gain".into(),
                ..ParamDefinition::default()
            }],
        }
    }

    #[tokio::test]
    async fn disabled_catalogue_does_not_need_a_public_key() {
        let dir = tempfile::tempdir().unwrap();
        let source = Source {
            url: None,
            key: None,
        };
        assert_eq!(
            catalogue(dir.path(), &source).await.unwrap_err(),
            "the definitions catalogue is off here"
        );
    }

    #[tokio::test]
    async fn missing_or_blank_key_explains_public_verification_configuration() {
        let dir = tempfile::tempdir().unwrap();
        for key in [None, Some(String::new()), Some("  ".into())] {
            let source = Source {
                url: Some(DEFAULT_URL.into()),
                key,
            };
            assert_eq!(
                catalogue(dir.path(), &source).await.unwrap_err(),
                NO_CATALOGUE_KEY
            );
        }
    }

    #[test]
    fn a_persons_set_is_kept_read_back_and_removed() {
        let dir = std::env::temp_dir().join(format!("uxduck-params-{}", std::process::id()));
        write_own(&dir, "own-abc123", &set()).unwrap();
        assert_eq!(read_own(&dir, "own-abc123").unwrap(), set());
        assert_eq!(
            own_path(&dir, "../../etc/own"),
            dir.join("own").join("etcown.json.gz"),
            "an id never leaves the folder"
        );
        std::fs::remove_file(own_path(&dir, "own-abc123")).unwrap();
        assert!(read_own(&dir, "own-abc123").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
