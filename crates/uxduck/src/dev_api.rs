//! Development server selection. A restart applies one immutable API target
//! to every daemon task; saved choices are ignored by release executables.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use uxduck_contracts::dto::account::{AccountEnvironment, AccountEnvironmentState};

const STAGING: &str = "https://api-staging.uxduck.com";
const PRODUCTION: &str = "https://api.uxduck.com";
const LOCAL: &str = "http://localhost:45103";
const FILE: &str = "dev-api.json";
static ACTIVE: OnceLock<Active> = OnceLock::new();

#[derive(Debug)]
struct Active {
    api: Option<String>,
    startup: Option<String>,
    local: String,
    path: PathBuf,
}

pub fn startup_url() -> Option<String> {
    startup_target(
        std::env::var("UXDUCK_API_URL").ok().as_deref(),
        cfg!(debug_assertions),
        &std::env::var("UXDUCK_LOCAL_API_URL").unwrap_or_else(|_| LOCAL.into()),
    )
}

fn startup_target(configured: Option<&str>, development: bool, local: &str) -> Option<String> {
    match configured {
        Some("none" | "") => None,
        Some(value) => Some(value.trim_end_matches('/').to_owned()),
        None if development => Some(local.trim_end_matches('/').to_owned()),
        None => Some(PRODUCTION.into()),
    }
}

fn target(
    selected: Option<AccountEnvironment>,
    startup: Option<&str>,
    local: &str,
) -> Option<String> {
    match selected {
        Some(AccountEnvironment::Local) => Some(local.to_owned()),
        Some(AccountEnvironment::Staging) => Some(STAGING.into()),
        Some(AccountEnvironment::Production) => Some(PRODUCTION.into()),
        None => startup.map(str::to_owned),
    }
}

fn local_url(configured: Option<&str>) -> Result<String, String> {
    let text = configured.unwrap_or(LOCAL).trim_end_matches('/');
    let url = reqwest::Url::parse(text)
        .map_err(|_| "UXDUCK_LOCAL_API_URL must be an HTTP or HTTPS loopback URL.".to_owned())?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !loopback
        || !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("UXDUCK_LOCAL_API_URL must use localhost or a loopback address over HTTP or HTTPS, without credentials, a query or a fragment.".into());
    }
    Ok(text.to_owned())
}

fn read(path: &Path) -> Result<Option<AccountEnvironment>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "Cannot read {}: {e}. Remove it to restore the startup server.",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Cannot read {}: {e}", path.display())),
    }
}

pub fn initialize(data_dir: &Path) -> Result<(), String> {
    ACTIVE
        .set(load_active(
            data_dir,
            startup_url(),
            cfg!(debug_assertions),
            if cfg!(debug_assertions) {
                local_url(std::env::var("UXDUCK_LOCAL_API_URL").ok().as_deref())?
            } else {
                LOCAL.into()
            },
        )?)
        .map_err(|_| "The API environment was initialized twice.".to_owned())
}

fn load_active(
    data_dir: &Path,
    startup: Option<String>,
    development: bool,
    local: String,
) -> Result<Active, String> {
    let path = data_dir.join(FILE);
    let selected = if development { read(&path)? } else { None };
    Ok(Active {
        api: target(selected, startup.as_deref(), &local),
        startup,
        local,
        path,
    })
}

pub fn api_url() -> Option<String> {
    ACTIVE
        .get()
        .map_or_else(startup_url, |active| active.api.clone())
}

/// Before server selection, debug executables defaulted to staging. Keep
/// those legacy tokens there when a new development startup defaults to Local.
pub fn legacy_api_url() -> Option<String> {
    startup_target(
        std::env::var("UXDUCK_API_URL").ok().as_deref(),
        cfg!(debug_assertions),
        STAGING,
    )
}

fn state(
    active: &Active,
    selected: Option<AccountEnvironment>,
    development: bool,
) -> AccountEnvironmentState {
    AccountEnvironmentState {
        environment: match active.api.as_deref() {
            Some(api) if api == active.local => Some(AccountEnvironment::Local),
            Some(STAGING) => Some(AccountEnvironment::Staging),
            Some(PRODUCTION) => Some(AccountEnvironment::Production),
            _ => None,
        },
        api_url: active.api.clone(),
        selected,
        can_change: development,
        restart_required: target(selected, active.startup.as_deref(), &active.local) != active.api,
    }
}

pub fn get() -> Result<AccountEnvironmentState, String> {
    let active = ACTIVE
        .get()
        .ok_or("The API environment is not initialized.")?;
    let development = cfg!(debug_assertions);
    let selected = if development {
        read(&active.path)?
    } else {
        None
    };
    Ok(state(active, selected, development))
}

pub fn set(selected: Option<AccountEnvironment>) -> Result<AccountEnvironmentState, String> {
    if !cfg!(debug_assertions) {
        return Err("Server selection is available in development daemons only.".into());
    }
    let active = ACTIVE
        .get()
        .ok_or("The API environment is not initialized.")?;
    let temporary = active.path.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec(&selected).map_err(|e| e.to_string())?,
    )
    .and_then(|()| std::fs::rename(&temporary, &active.path))
    .map_err(|e| format!("Cannot save {}: {e}", active.path.display()))?;
    Ok(state(active, selected, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_defaults_to_local_and_release_defaults_to_production() {
        assert_eq!(startup_target(None, true, LOCAL).as_deref(), Some(LOCAL));
        assert_eq!(
            startup_target(None, false, LOCAL).as_deref(),
            Some(PRODUCTION)
        );
        assert_eq!(
            startup_target(None, true, "http://127.0.0.1:47985/").as_deref(),
            Some("http://127.0.0.1:47985")
        );
        assert_eq!(
            startup_target(Some(STAGING), true, LOCAL).as_deref(),
            Some(STAGING)
        );
        assert_eq!(startup_target(Some("none"), true, LOCAL), None);
        assert_eq!(startup_target(Some(""), true, LOCAL), None);
        // Legacy tokens retain the pre-selector staging default.
        assert_eq!(
            startup_target(None, true, STAGING).as_deref(),
            Some(STAGING)
        );
    }

    #[test]
    fn saved_choices_still_override_the_new_local_startup_default() {
        let dir = tempfile::tempdir().unwrap();
        let startup = startup_target(None, true, LOCAL);
        let active = load_active(dir.path(), startup.clone(), true, LOCAL.into()).unwrap();
        assert_eq!(
            state(&active, None, true).environment,
            Some(AccountEnvironment::Local)
        );
        std::fs::write(dir.path().join(FILE), "\"staging\"").unwrap();
        let active = load_active(dir.path(), startup, true, LOCAL.into()).unwrap();
        assert_eq!(active.api.as_deref(), Some(STAGING));
    }

    #[test]
    fn release_configuration_ignores_development_choices_even_when_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "not json").unwrap();
        let active = load_active(dir.path(), Some(PRODUCTION.into()), false, LOCAL.into()).unwrap();
        assert_eq!(active.api.as_deref(), Some(PRODUCTION));
        assert!(!state(&active, None, false).can_change);
    }

    #[test]
    fn pending_choice_does_not_change_the_active_server() {
        let active = Active {
            api: Some(STAGING.into()),
            startup: Some(STAGING.into()),
            local: LOCAL.into(),
            path: PathBuf::new(),
        };
        let pending = state(&active, Some(AccountEnvironment::Production), true);
        assert_eq!(pending.api_url.as_deref(), Some(STAGING));
        assert!(pending.restart_required);
        assert_eq!(
            target(pending.selected, active.startup.as_deref(), LOCAL).as_deref(),
            Some(PRODUCTION)
        );
        assert!(!state(&active, None, true).restart_required);
    }

    #[test]
    fn default_restores_custom_or_disabled_startup_configuration() {
        assert_eq!(
            target(None, Some("http://127.0.0.1:45103"), LOCAL),
            Some("http://127.0.0.1:45103".into())
        );
        assert_eq!(target(None, None, LOCAL), None);
        assert_eq!(
            target(Some(AccountEnvironment::Production), None, LOCAL).as_deref(),
            Some(PRODUCTION)
        );
    }

    #[test]
    fn invalid_saved_choices_fail_with_an_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        assert_eq!(read(&path).unwrap(), None);
        std::fs::write(&path, "\"production\"").unwrap();
        assert_eq!(read(&path).unwrap(), Some(AccountEnvironment::Production));
        std::fs::write(&path, "\"elsewhere\"").unwrap();
        assert!(read(&path).unwrap_err().contains("Remove it to restore"));
    }

    #[test]
    fn local_is_an_explicit_persisted_target_independent_of_the_startup_server() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "\"local\"").unwrap();
        let active = load_active(dir.path(), Some(PRODUCTION.into()), true, LOCAL.into()).unwrap();
        let state = state(&active, Some(AccountEnvironment::Local), true);
        assert_eq!(state.environment, Some(AccountEnvironment::Local));
        assert_eq!(state.api_url.as_deref(), Some(LOCAL));
        assert!(!state.restart_required);
        assert_eq!(
            target(None, active.startup.as_deref(), LOCAL).as_deref(),
            Some(PRODUCTION)
        );
    }

    #[test]
    fn alternate_local_ports_are_allowed_but_remote_or_credentialed_urls_are_not() {
        assert_eq!(local_url(None).unwrap(), LOCAL);
        for url in [
            "http://127.0.0.1:47985",
            "http://[::1]:45103/",
            "https://localhost:45104",
        ] {
            assert_eq!(local_url(Some(url)).unwrap(), url.trim_end_matches('/'));
        }
        for url in [
            "https://api.uxduck.com",
            "http://192.168.1.2:45103",
            "file:///tmp/api",
            "http://user:secret@localhost:45103",
            "http://localhost:45103?token=secret",
            "http://localhost:45103#api",
        ] {
            assert!(local_url(Some(url)).is_err(), "{url}");
        }
    }
}
