//! The daemon's credential store: a JSON file readable only by the user
//! (0600) in the data directory, behind the engine's `Credentials` port. The
//! OS keychain can replace it behind the same port (review.md S3).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use uxduck_engine::runtime::Credentials;

#[derive(Clone, Debug)]
pub struct FileCredentials {
    path: PathBuf,
    api: Option<String>,
    legacy_api: Option<String>,
}

impl FileCredentials {
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("credentials.json"),
            api: crate::dev_api::api_url(),
            legacy_api: crate::dev_api::legacy_api_url(),
        }
    }

    fn load(&self) -> BTreeMap<String, String> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Read fresh on every call, so a key set a moment ago is used at once.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        let mut all = self.load();
        if name == uxduck_engine::ACCOUNT_TOKEN {
            let api = self.api.as_deref()?;
            return all.remove(&account_key(api)).or_else(|| {
                (self.api == self.legacy_api)
                    .then(|| all.remove(name))
                    .flatten()
            });
        }
        all.remove(name)
    }

    /// Move the pre-selector token into its original server's slot before
    /// background tasks start. It can never become another server's token.
    pub fn migrate_account_token(&self) -> Result<(), String> {
        let mut all = self.load();
        let Some(api) = self.legacy_api.as_deref() else {
            return Ok(());
        };
        let Some(token) = all.remove(uxduck_engine::ACCOUNT_TOKEN) else {
            return Ok(());
        };
        all.entry(account_key(api)).or_insert(token);
        self.save(&all)
            .map_err(|e| format!("cannot migrate {}: {e}", self.path.display()))
    }

    fn save(&self, all: &BTreeMap<String, String>) -> std::io::Result<()> {
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(all).unwrap_or_default())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(temp, &self.path)
    }
}

impl Credentials for FileCredentials {
    fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let mut all = self.load();
        let scoped;
        let name = if name == uxduck_engine::ACCOUNT_TOKEN {
            let api = self
                .api
                .as_deref()
                .ok_or("This UXDuck has no account server.")?;
            scoped = account_key(api);
            if self.api == self.legacy_api {
                all.remove(name);
            }
            &scoped
        } else {
            name
        };
        if value.is_empty() {
            all.remove(name);
        } else {
            all.insert(name.to_owned(), value.to_owned());
        }
        self.save(&all)
            .map_err(|error| format!("cannot write {}: {error}", self.path.display()))
    }

    fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

fn account_key(api: &str) -> String {
    format!("account.token:{}", api.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_reads_and_removes_privately() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileCredentials::new(dir.path());
        assert!(!store.has("openai.key"));
        store.set("openai.key", "sk-1").unwrap();
        assert_eq!(
            FileCredentials::new(dir.path())
                .get("openai.key")
                .as_deref(),
            Some("sk-1")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("credentials.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.set("openai.key", "").unwrap();
        assert!(!store.has("openai.key"));
    }

    #[test]
    fn account_tokens_are_isolated_and_sign_out_only_removes_the_active_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut staging = FileCredentials::new(dir.path());
        staging.api = Some("https://api-staging.uxduck.com".into());
        let mut production = staging.clone();
        production.api = Some("https://api.uxduck.com".into());
        staging
            .set(uxduck_engine::ACCOUNT_TOKEN, "staging-token")
            .unwrap();
        assert!(!production.has(uxduck_engine::ACCOUNT_TOKEN));
        production
            .set(uxduck_engine::ACCOUNT_TOKEN, "production-token")
            .unwrap();
        assert_eq!(
            staging.get(uxduck_engine::ACCOUNT_TOKEN).as_deref(),
            Some("staging-token")
        );
        assert_eq!(
            production.get(uxduck_engine::ACCOUNT_TOKEN).as_deref(),
            Some("production-token")
        );
        production.set(uxduck_engine::ACCOUNT_TOKEN, "").unwrap();
        assert!(!production.has(uxduck_engine::ACCOUNT_TOKEN));
        assert!(staging.has(uxduck_engine::ACCOUNT_TOKEN));
    }

    #[test]
    fn legacy_tokens_are_migrated_to_the_startup_server_even_with_a_pending_choice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("credentials.json"),
            r#"{"account.token":"old","openai.key":"key"}"#,
        )
        .unwrap();
        let mut credentials = FileCredentials::new(dir.path());
        credentials.legacy_api = Some("https://api-staging.uxduck.com".into());
        credentials.api = Some("https://api.uxduck.com".into());
        credentials.migrate_account_token().unwrap();
        assert!(!credentials.has(uxduck_engine::ACCOUNT_TOKEN));
        assert_eq!(credentials.get("openai.key").as_deref(), Some("key"));
        credentials.api = credentials.legacy_api.clone();
        assert_eq!(
            credentials.get(uxduck_engine::ACCOUNT_TOKEN).as_deref(),
            Some("old")
        );
        assert!(
            !credentials
                .load()
                .contains_key(uxduck_engine::ACCOUNT_TOKEN)
        );
    }
}
