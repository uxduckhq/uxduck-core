#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use tokio::sync::Notify;

use uxduck_contracts::dto::operator::TermsContext;
use uxduck_contracts::version::TERMS_VERSION;

use super::*;

struct TestAccount {
    account: Arc<Account>,
    started: Arc<Notify>,
    release: Arc<Notify>,
    signed_out: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for TestAccount {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn setup(old_status: StatusCode, delayed: bool) -> TestAccount {
    setup_with_verification(old_status, delayed, false).await
}

async fn setup_with_verification(
    old_status: StatusCode,
    delayed: bool,
    hold_verify: bool,
) -> TestAccount {
    setup_scenarios(old_status, delayed, hold_verify, false).await
}

fn test_api(
    old_status: StatusCode,
    delayed: bool,
    hold_verify: bool,
    hold_sessions: bool,
    started: &Arc<Notify>,
    release: &Arc<Notify>,
    signed_out: &Arc<Mutex<Vec<String>>>,
) -> Router {
    let handler_started = started.clone();
    let handler_release = release.clone();
    let verify_started = started.clone();
    let verify_release = release.clone();
    let session_started = started.clone();
    let session_release = release.clone();
    let sign_out_log = signed_out.clone();
    Router::new()
        .route(
            "/v1/me",
            get(move |headers: HeaderMap| {
                let started = handler_started.clone();
                let release = handler_release.clone();
                async move {
                    if headers["authorization"] == "Bearer old-session" {
                        started.notify_one();
                        if delayed {
                            release.notified().await;
                        }
                        return (
                            old_status,
                            Json(json!({
                                "user": { "id": "old-person", "email": "old@example.com", "name": "", "sourceAnswered": false },
                                "productUpdates": false,
                                "analyticsExcluded": true,
                                "hostedModels": ["gpt-6-sol", "moonshotai/kimi-k3.1-pro"],
                            })),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "user": { "id": "new-person", "email": "new@example.com", "name": "" },
                            "productUpdates": false,
                        })),
                    )
                }
            }),
        )
        .route(
            "/v1/auth/email-code/request",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["intent"], "continue");
                Json(json!({ "challengeId": format!("challenge-{}", body["email"].as_str().unwrap().split('@').next().unwrap()) }))
            }),
        )
        .route(
            "/v1/auth/email-code/verify",
            post(move |Json(body): Json<Value>| {
                let started = verify_started.clone();
                let release = verify_release.clone();
                async move {
                assert_eq!(body["challengeId"], "challenge-new");
                if let Some(terms) = body.get("terms") {
                    assert_eq!(terms, &json!({ "version": TERMS_VERSION, "client": "desktop" }));
                }
                if hold_verify { started.notify_one(); release.notified().await; }
                (
                    [("set-auth-token", "new-session")],
                    Json(json!({ "isNew": false })),
                )
                }
            }),
        )
        .route("/v1/me/sessions/current", put(|| async { Json(json!({})) }))
        .route(
            "/v1/me/terms",
            put(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers["authorization"], "Bearer new-session");
                assert_eq!(
                    body,
                    json!({ "version": TERMS_VERSION, "context": "real-vehicle", "client": "desktop" })
                );
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/v1/me/source",
            put(|| async { (StatusCode::CONFLICT, Json(json!({ "error": "already answered" }))) }),
        )
        .route("/v1/me/sessions", get(move |headers: HeaderMap| {
            let started = session_started.clone();
            let release = session_release.clone();
            async move {
                let old = headers.get("authorization").and_then(|h| h.to_str().ok()) == Some("Bearer old-session");
                if old && hold_sessions { started.notify_one(); release.notified().await; }
                Json(json!({"sessions":[{"id":if old {"current-old"} else {"current-new"},"current":true}]}))
            }
        }))
        .route("/v1/auth/sign-out", post(move |headers: HeaderMap| {
            let log = sign_out_log.clone();
            async move {
                log.lock().unwrap().push(headers.get("authorization").unwrap().to_str().unwrap().to_owned());
                Json(json!({}))
            }
        }))
}

async fn setup_scenarios(
    old_status: StatusCode,
    delayed: bool,
    hold_verify: bool,
    hold_sessions: bool,
) -> TestAccount {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let signed_out = Arc::new(Mutex::new(Vec::new()));
    let app = test_api(
        old_status,
        delayed,
        hold_verify,
        hold_sessions,
        &started,
        &release,
        &signed_out,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let mut credentials = FileCredentials::new(dir.path());
    credentials.set(TOKEN, "old-session").unwrap();
    let account = Arc::new(Account {
        api: Some(api),
        client: crate::online::client(Duration::from_secs(5)),
        credentials,
        session_change: Mutex::new(()),
        // As though a code was asked for: each test verifies new@example.com.
        challenge: Mutex::new(Some(("new@example.com".into(), "challenge-new".into()))),
        generation: AtomicU64::new(0),
        last: Mutex::new(AccountState::default()),
        analytics_internal: Mutex::new(None),
        hosted_models: Mutex::new(None),
        shared_dir: dir.path().join("shared"),
        data_dir: dir.path().to_owned(),
    });
    TestAccount {
        account,
        started,
        release,
        signed_out,
        server,
        _dir: dir,
    }
}

fn pending_get(account: &Arc<Account>) -> tokio::task::JoinHandle<AccountState> {
    let account = account.clone();
    tokio::spawn(async move {
        account
            .get(
                account.api.as_deref().unwrap(),
                account.client.as_ref().unwrap(),
            )
            .await
            .unwrap()
    })
}

#[tokio::test]
async fn internal_account_status_survives_offline_reads_and_clears_on_sign_out() {
    let test = setup(StatusCode::OK, false).await;
    let state = pending_get(&test.account).await.unwrap();
    assert!(state.analytics_excluded);
    assert_eq!(*test.account.analytics_internal.lock().unwrap(), Some(true));
    test.server.abort();
    let offline = pending_get(&test.account).await.unwrap();
    assert!(offline.offline && offline.analytics_excluded);
    assert_eq!(*test.account.analytics_internal.lock().unwrap(), Some(true));
    test.account
        .act(AccountAction::SignOut { device: None })
        .await
        .unwrap();
    assert_eq!(
        *test.account.analytics_internal.lock().unwrap(),
        Some(false)
    );
}

/// Whether the account said where it heard of UXDuck (S251) comes with it;
/// an answer, or one given elsewhere first, keeps it answered while offline.
#[tokio::test]
async fn the_signup_source_question_is_asked_until_answered_anywhere() {
    let test = setup(StatusCode::OK, false).await;
    assert_eq!(
        pending_get(&test.account).await.unwrap().source_answered,
        Some(false)
    );
    test.account
        .act(AccountAction::SetSource {
            source: uxduck_contracts::dto::account::SignupSource::Youtube,
            other: None,
        })
        .await
        .unwrap();
    test.server.abort();
    let offline = pending_get(&test.account).await.unwrap();
    assert!(offline.offline);
    assert_eq!(offline.source_answered, Some(true));
}

/// What the hosted model offers comes with the account (S29), and is kept
/// while offline; an API that does not say leaves the configured list.
#[tokio::test]
async fn the_hosted_models_come_from_the_account_and_survive_offline_reads() {
    let test = setup(StatusCode::OK, false).await;
    pending_get(&test.account).await.unwrap();
    let offered = Some(vec![
        "gpt-6-sol".to_owned(),
        "moonshotai/kimi-k3.1-pro".to_owned(),
    ]);
    assert_eq!(test.account.hosted_models(), offered);
    test.server.abort();
    assert!(pending_get(&test.account).await.unwrap().offline);
    assert_eq!(test.account.hosted_models(), offered);
}

#[tokio::test]
async fn an_old_unauthorized_response_cannot_erase_a_new_sign_in() {
    let test = setup(StatusCode::UNAUTHORIZED, true).await;
    let pending = pending_get(&test.account);
    tokio::time::timeout(Duration::from_secs(5), test.started.notified())
        .await
        .unwrap();
    let signed = test
        .account
        .act(AccountAction::Verify {
            email: "new@example.com".into(),
            code: "123456".into(),
            product_updates: false,
            terms_version: None,
        })
        .await
        .unwrap();
    assert_eq!(signed["email"], "new@example.com");
    test.release.notify_one();
    let state = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(state.signed_in);
    assert_eq!(state.email.as_deref(), Some("new@example.com"));
    assert_eq!(test.account.token().as_deref(), Some("new-session"));
}

#[tokio::test]
async fn an_old_success_cannot_restore_a_signed_out_account() {
    let test = setup(StatusCode::OK, true).await;
    let pending = pending_get(&test.account);
    tokio::time::timeout(Duration::from_secs(5), test.started.notified())
        .await
        .unwrap();
    test.account
        .act(AccountAction::SignOut { device: None })
        .await
        .unwrap();
    test.release.notify_one();
    let state = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(!state.signed_in);
    assert!(test.account.token().is_none());
    assert!(!test.account.last.lock().unwrap().signed_in);
}

#[tokio::test]
async fn a_server_failure_keeps_the_token_but_a_real_rejection_removes_it() {
    let down = setup(StatusCode::INTERNAL_SERVER_ERROR, false).await;
    let offline = pending_get(&down.account).await.unwrap();
    assert!(offline.signed_in && offline.offline);
    assert_eq!(down.account.token().as_deref(), Some("old-session"));

    let revoked = setup(StatusCode::UNAUTHORIZED, false).await;
    assert!(!pending_get(&revoked.account).await.unwrap().signed_in);
    assert!(revoked.account.token().is_none());
}

/// One account per computer (ADR-0033, 2026-10-31): a sign-in an older
/// UXDuck tied to one operator keeps working after the upgrade, for every
/// operator, with no code asked for.
#[tokio::test]
async fn a_sign_in_tied_to_an_operator_before_the_upgrade_serves_every_operator() {
    let test = setup(StatusCode::OK, false).await;
    let path = test.account.data_dir.join("credentials.json");
    let mut stored: serde_json::Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let server = stored
        .keys()
        .find_map(|key| key.strip_prefix("account.token:"))
        .unwrap()
        .to_owned();
    stored.insert(format!("account.operator:{server}"), json!("ana"));
    std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

    let state = test.account.act(AccountAction::Get).await.unwrap();
    assert_eq!(state["signedIn"], true);
    assert_eq!(state["accountId"], "old-person");
    assert!(state.get("operatorId").is_none());
    let sessions = test.account.act(AccountAction::Sessions).await.unwrap();
    assert_eq!(sessions["sessions"][0]["id"], "current-old");
    assert_eq!(test.account.token().as_deref(), Some("old-session"));
    assert!(test.signed_out.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_verification_queued_before_a_sign_out_cannot_restore_its_token() {
    let test = setup(StatusCode::OK, false).await;
    // The generation was captured when verification was queued. A sign-out
    // queued after it moves the generation on before the response can
    // install a token.
    test.account.generation.fetch_add(1, Ordering::SeqCst);
    let result = test
        .account
        .act_at(
            AccountAction::Verify {
                email: "new@example.com".into(),
                code: "123456".into(),
                product_updates: false,
                terms_version: None,
            },
            0,
        )
        .await;
    assert!(result.is_err());
    assert_eq!(test.account.token().as_deref(), Some("old-session"));
}

#[tokio::test]
async fn a_late_verification_response_cannot_sign_back_in_after_a_sign_out() {
    let test = setup_with_verification(StatusCode::OK, false, true).await;
    let account = test.account.clone();
    let pending = tokio::spawn(async move {
        account
            .act_at(
                AccountAction::Verify {
                    email: "new@example.com".into(),
                    code: "123456".into(),
                    product_updates: false,
                    terms_version: None,
                },
                0,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), test.started.notified())
        .await
        .unwrap();
    test.account.generation.fetch_add(1, Ordering::SeqCst);
    test.account
        .act_at(AccountAction::SignOut { device: None }, 1)
        .await
        .unwrap();
    assert!(test.account.token().is_none());
    test.release.notify_one();
    assert!(pending.await.unwrap().is_err());
    assert!(test.account.token().is_none());
    assert!(!test.account.last.lock().unwrap().signed_in);
}

#[tokio::test]
async fn a_legacy_sign_in_keeps_its_session_while_offline() {
    let test = setup(StatusCode::OK, false).await;
    test.account.credentials.protect_legacy_workspace().unwrap();
    test.server.abort();
    let state = test.account.act(AccountAction::Get).await.unwrap();
    assert_eq!(state["signedIn"], true);
    assert_eq!(state["offline"], true);
    assert_eq!(test.account.token().as_deref(), Some("old-session"));
}

#[tokio::test]
async fn a_delayed_session_sign_out_cannot_sign_out_the_next_sign_in() {
    let test = setup_scenarios(StatusCode::OK, false, false, true).await;
    let account = test.account.clone();
    let pending = tokio::spawn(async move {
        account
            .act_at(
                AccountAction::SignOutSession {
                    id: "current-old".into(),
                    device: None,
                },
                0,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), test.started.notified())
        .await
        .unwrap();
    // A verification queued meanwhile moves the generation on.
    test.account.generation.fetch_add(1, Ordering::SeqCst);
    test.account
        .act_at(
            AccountAction::SignIn {
                email: "new@example.com".into(),
            },
            1,
        )
        .await
        .unwrap();
    test.account
        .act_at(
            AccountAction::Verify {
                email: "new@example.com".into(),
                code: "123456".into(),
                product_updates: false,
                terms_version: None,
            },
            1,
        )
        .await
        .unwrap();
    test.release.notify_one();
    assert!(pending.await.unwrap().is_err());
    assert_eq!(test.account.token().as_deref(), Some("new-session"));
    assert!(
        test.signed_out
            .lock()
            .unwrap()
            .iter()
            .all(|token| token == "Bearer old-session")
    );
}

#[tokio::test]
async fn a_code_signs_in_only_the_email_it_was_asked_for_once() {
    let test = setup(StatusCode::OK, false).await;
    let verify = |email: &str| AccountAction::Verify {
        email: email.into(),
        code: "123456".into(),
        product_updates: false,
        terms_version: None,
    };
    *test.account.challenge.lock().unwrap() = None;
    let missing = test
        .account
        .act(verify("new@example.com"))
        .await
        .unwrap_err();
    assert_eq!(missing.message, "Ask for a new code, then try again.");
    test.account
        .act(AccountAction::SignIn {
            email: "new@example.com".into(),
        })
        .await
        .unwrap();
    assert!(test.account.act(verify("other@example.com")).await.is_err());
    test.account.act(verify("new@example.com")).await.unwrap();
    assert!(test.account.challenge.lock().unwrap().is_none());
}

#[tokio::test]
async fn the_account_records_the_terms_agreed_to_at_sign_in_and_later() {
    let test = setup(StatusCode::OK, false).await;
    test.account
        .act(AccountAction::Verify {
            email: "new@example.com".into(),
            code: "123456".into(),
            product_updates: false,
            terms_version: Some(TERMS_VERSION.into()),
        })
        .await
        .unwrap();
    test.account
        .act(AccountAction::AcceptTerms {
            version: TERMS_VERSION.into(),
            context: TermsContext::RealVehicle,
        })
        .await
        .unwrap();
    assert_eq!(
        test.account.last.lock().unwrap().terms_version.as_deref(),
        Some(TERMS_VERSION)
    );
}

#[test]
fn verifying_says_which_terms_the_form_showed_only_when_it_showed_them() {
    assert_eq!(
        verify_body("challenge", "123456", None),
        json!({ "challengeId": "challenge", "code": "123456" })
    );
    assert_eq!(
        verify_body("challenge", "123456", Some("2026-11-03"))["terms"],
        json!({ "version": "2026-11-03", "client": "desktop" })
    );
}
