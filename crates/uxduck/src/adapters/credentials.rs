//! The daemon's credential store: a JSON file readable only by the user
//! (0600) in the data directory, behind the engine's `Credentials` port. The
//! OS keychain can replace it behind the same port (review.md S3).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use sha2::{Digest, Sha256};

static WRITE: Mutex<()> = Mutex::new(());

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

    fn load(&self) -> Result<BTreeMap<String, String>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("Cannot read {}: {e}", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(format!("Cannot read {}: {error}", self.path.display())),
        }
    }

    /// Read fresh on every call, so a key set a moment ago is used at once.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        let mut all = self.load().ok()?;
        if name == uxduck_engine::ACCOUNT_TOKEN {
            let api = self.api.as_deref()?;
            let token = all.remove(&account_key(api)).or_else(|| {
                (self.api == self.legacy_api)
                    .then(|| all.remove(name))
                    .flatten()
            })?;
            // Shared local data never follows a different person's sign-in.
            let owner = all.get(&self.scoped("workspace.owner"));
            let person = all.get(&self.scoped("account.person"));
            if owner.is_some() && owner != person {
                return None;
            }
            if owner.is_none()
                && all
                    .get(&self.scoped("workspace.legacy"))
                    .is_none_or(|pending| *pending != fingerprint(&token))
            {
                return None;
            }
            return Some(token);
        }
        all.remove(name)
    }

    fn scoped(&self, name: &str) -> String {
        format!(
            "{name}:{}",
            self.api.as_deref().unwrap_or("none").trim_end_matches('/')
        )
    }

    /// The active person's token. Background data services use `get(TOKEN)`
    /// instead, which additionally checks the dataset's immutable owner.
    pub fn account_session(&self) -> Option<String> {
        self.api.as_ref()?;
        let all = self.load().ok()?;
        all.get(&self.scoped("account.token"))
            .cloned()
            .or_else(|| {
                (self.api == self.legacy_api)
                    .then(|| all.get(uxduck_engine::ACCOUNT_TOKEN).cloned())
                    .flatten()
            })
            .filter(|token| !token.is_empty())
    }

    pub fn account_operator(&self) -> Option<String> {
        self.load()
            .ok()?
            .get(&self.scoped("account.operator"))
            .cloned()
    }

    pub fn bind_operator(&self, operator: Option<&str>) -> Result<(), String> {
        let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
        let mut all = self.load()?;
        let key = self.scoped("account.operator");
        if let Some(operator) = operator {
            all.insert(key, operator.into());
        } else {
            all.remove(&key);
        }
        self.save(&all).map_err(|e| e.to_string())
    }

    pub fn workspace_needs_identity(&self) -> bool {
        self.load().is_ok_and(|all| {
            !all.contains_key(&self.scoped("workspace.owner"))
                && all.contains_key(&self.scoped("workspace.legacy"))
        })
    }

    /// Preserve evidence of the legacy workspace before a person can switch.
    /// An unreachable server must not make the next sign-in its new owner.
    pub fn protect_legacy_workspace(&self) -> Result<(), String> {
        let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
        let mut all = self.load()?;
        if !all.contains_key(&self.scoped("workspace.owner"))
            && !all.contains_key(&self.scoped("workspace.legacy"))
            && let Some(token) = self.account_session()
        {
            all.insert(self.scoped("workspace.legacy"), fingerprint(&token));
            self.save(&all).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Identity comes from the authenticated /v1/me response, never email input.
    pub fn remember_person(&self, token: &str, id: &str) -> Result<bool, String> {
        let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
        if self.account_session().as_deref() != Some(token) {
            return Ok(false);
        }
        let mut all = self.load()?;
        all.insert(self.scoped("account.person"), id.into());
        let owner = self.scoped("workspace.owner");
        let legacy = self.scoped("workspace.legacy");
        if !all.contains_key(&owner)
            && all
                .get(&legacy)
                .is_none_or(|old| *old == fingerprint(token))
        {
            all.insert(owner.clone(), id.into());
            all.remove(&legacy);
        }
        let owns = all.get(&owner).is_some_and(|owner| owner == id);
        self.save(&all).map_err(|e| e.to_string())?;
        Ok(owns)
    }

    /// Move the pre-selector token into its original server's slot before
    /// background tasks start. It can never become another server's token.
    pub fn migrate_account_token(&self) -> Result<(), String> {
        let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
        let mut all = self.load()?;
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
        let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
        let mut all = self.load()?;
        let scoped;
        let name = if name == uxduck_engine::ACCOUNT_TOKEN {
            let api = self
                .api
                .as_deref()
                .ok_or("This UXDuck has no account server.")?;
            // A new credential is unverified until its own /v1/me completes.
            all.remove(&self.scoped("account.person"));
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

fn fingerprint(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn account_key(api: &str) -> String {
    format!("account.token:{}", api.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_persons_login_never_becomes_a_shared_data_export_credential() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileCredentials::new(dir.path());
        store.api = Some("https://api.uxduck.test".into());
        store
            .set(uxduck_engine::ACCOUNT_TOKEN, "ana-token")
            .unwrap();
        assert!(
            store.get(uxduck_engine::ACCOUNT_TOKEN).is_none(),
            "unverified tokens cannot export shared data"
        );
        assert!(store.remember_person("ana-token", "ana").unwrap());
        store
            .set(uxduck_engine::ACCOUNT_TOKEN, "ben-token")
            .unwrap();
        assert!(store.get(uxduck_engine::ACCOUNT_TOKEN).is_none());
        assert!(!store.remember_person("ben-token", "ben").unwrap());
        assert_eq!(store.account_session().as_deref(), Some("ben-token"));
        assert!(store.get(uxduck_engine::ACCOUNT_TOKEN).is_none());
        store
            .set(uxduck_engine::ACCOUNT_TOKEN, "ana-new-token")
            .unwrap();
        assert!(store.remember_person("ana-new-token", "ana").unwrap());
        assert_eq!(
            store.get(uxduck_engine::ACCOUNT_TOKEN).as_deref(),
            Some("ana-new-token")
        );
    }

    #[test]
    fn legacy_ownership_and_corrupt_credentials_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileCredentials::new(dir.path());
        store.api = Some("https://api.uxduck.test".into());
        store
            .set(uxduck_engine::ACCOUNT_TOKEN, "legacy-token")
            .unwrap();
        store.protect_legacy_workspace().unwrap();
        store
            .set(uxduck_engine::ACCOUNT_TOKEN, "another-person")
            .unwrap();
        assert!(!store.remember_person("another-person", "ben").unwrap());
        assert!(store.get(uxduck_engine::ACCOUNT_TOKEN).is_none());
        std::fs::write(&store.path, "broken").unwrap();
        assert!(store.get(uxduck_engine::ACCOUNT_TOKEN).is_none());
        assert!(
            store
                .set(uxduck_engine::ACCOUNT_TOKEN, "cannot-overwrite")
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&store.path).unwrap(), "broken");
    }

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
        staging
            .remember_person("staging-token", "staging-person")
            .unwrap();
        assert!(!production.has(uxduck_engine::ACCOUNT_TOKEN));
        production
            .set(uxduck_engine::ACCOUNT_TOKEN, "production-token")
            .unwrap();
        production
            .remember_person("production-token", "production-person")
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
        credentials.protect_legacy_workspace().unwrap();
        assert_eq!(
            credentials.get(uxduck_engine::ACCOUNT_TOKEN).as_deref(),
            Some("old")
        );
        assert!(
            !credentials
                .load()
                .unwrap()
                .contains_key(uxduck_engine::ACCOUNT_TOKEN)
        );
    }
}
