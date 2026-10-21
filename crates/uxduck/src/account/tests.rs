#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use tokio::sync::Notify;

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

async fn setup_scenarios(
    old_status: StatusCode,
    delayed: bool,
    hold_verify: bool,
    hold_sessions: bool,
) -> TestAccount {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let handler_started = started.clone();
    let handler_release = release.clone();
    let verify_started = started.clone();
    let verify_release = release.clone();
    let session_started = started.clone();
    let session_release = release.clone();
    let signed_out = Arc::new(Mutex::new(Vec::new()));
    let sign_out_log = signed_out.clone();
    let app = Router::new()
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
                                "user": { "id": "old-person", "email": "old@example.com", "name": "" },
                                "productUpdates": false,
                                "analyticsExcluded": true,
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
            "/v1/auth/sign-in/email-otp",
            post(move || {
                let started = verify_started.clone();
                let release = verify_release.clone();
                async move {
                if hold_verify { started.notify_one(); release.notified().await; }
                (
                    [("set-auth-token", "new-session")],
                    Json(json!({ "isNew": false })),
                )
                }
            }),
        )
        .route("/v1/me/sessions/current", put(|| async { Json(json!({})) }))
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
        }));
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
        generation: AtomicU64::new(0),
        last: Mutex::new(AccountState::default()),
        analytics_internal: Mutex::new(None),
        shared_dir: dir.path().join("shared"),
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

#[tokio::test]
async fn switching_people_clears_the_previous_cloud_session_before_returning() {
    let test = setup(StatusCode::OK, false).await;
    test.account.credentials.bind_operator(Some("ana")).unwrap();
    test.account
        .get(
            test.account.api.as_deref().unwrap(),
            test.account.client.as_ref().unwrap(),
        )
        .await
        .unwrap();
    let before = test.account.last.lock().unwrap().clone();
    assert_eq!(before.account_id.as_deref(), Some("old-person"));
    let switched = tokio::time::timeout(
        Duration::from_millis(200),
        test.account.act_for(
            AccountAction::SwitchOperator {
                operator_id: Some("ben".into()),
                restoring: false,
                device: None,
            },
            Some("ana"),
            0,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(switched, json!({}));
    assert!(test.account.token().is_none());
    assert_eq!(
        test.account.credentials.account_operator().as_deref(),
        Some("ben")
    );
    assert!(!test.account.last.lock().unwrap().signed_in);
    // Selecting the old name again does not resurrect a credential.
    test.account
        .act_for(
            AccountAction::SwitchOperator {
                operator_id: Some("ana".into()),
                restoring: false,
                device: None,
            },
            Some("ben"),
            0,
        )
        .await
        .unwrap();
    assert!(test.account.token().is_none());
}

#[tokio::test]
async fn a_different_operator_cannot_read_or_modify_the_active_account() {
    let test = setup(StatusCode::OK, false).await;
    test.account.credentials.bind_operator(Some("ana")).unwrap();
    let state = test
        .account
        .act_for(AccountAction::Get, Some("ben"), 0)
        .await
        .unwrap();
    assert_eq!(state, json!({"signedIn":false}));
    assert!(
        test.account
            .act_for(AccountAction::Sessions, Some("ben"), 0)
            .await
            .is_err()
    );
    assert!(
        test.account
            .act_for(
                AccountAction::Verify {
                    email: "new@example.com".into(),
                    code: "123456".into(),
                    product_updates: false
                },
                Some("ben"),
                0
            )
            .await
            .is_err()
    );
    assert_eq!(test.account.token().as_deref(), Some("old-session"));
}

#[tokio::test]
async fn a_verification_started_before_a_switch_cannot_restore_its_token() {
    let test = setup(StatusCode::OK, false).await;
    test.account.credentials.bind_operator(Some("ana")).unwrap();
    // The generation was captured when verification was queued. A switch
    // invalidates that generation before the response can install a token.
    test.account.generation.fetch_add(1, Ordering::SeqCst);
    let result = test
        .account
        .act_for(
            AccountAction::Verify {
                email: "new@example.com".into(),
                code: "123456".into(),
                product_updates: false,
            },
            Some("ana"),
            0,
        )
        .await;
    assert!(result.is_err());
    assert_eq!(test.account.token().as_deref(), Some("old-session"));
}

#[tokio::test]
async fn a_late_verification_response_cannot_sign_the_previous_person_back_in() {
    let test = setup_with_verification(StatusCode::OK, false, true).await;
    test.account.credentials.bind_operator(Some("ana")).unwrap();
    let account = test.account.clone();
    let pending = tokio::spawn(async move {
        account
            .act_for(
                AccountAction::Verify {
                    email: "new@example.com".into(),
                    code: "123456".into(),
                    product_updates: false,
                },
                Some("ana"),
                0,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), test.started.notified())
        .await
        .unwrap();
    test.account.generation.fetch_add(1, Ordering::SeqCst);
    test.account
        .act_for(
            AccountAction::SwitchOperator {
                operator_id: Some("ben".into()),
                restoring: false,
                device: None,
            },
            Some("ana"),
            1,
        )
        .await
        .unwrap();
    test.release.notify_one();
    assert!(pending.await.unwrap().is_err());
    assert!(test.account.token().is_none());
    assert_eq!(
        test.account.credentials.account_operator().as_deref(),
        Some("ben")
    );
}

#[tokio::test]
async fn restoring_a_legacy_profile_offline_keeps_its_existing_session() {
    let test = setup(StatusCode::OK, false).await;
    test.account.credentials.protect_legacy_workspace().unwrap();
    test.server.abort();
    test.account
        .act_for(
            AccountAction::SwitchOperator {
                operator_id: Some("ana".into()),
                restoring: true,
                device: None,
            },
            None,
            0,
        )
        .await
        .unwrap();
    assert_eq!(test.account.token().as_deref(), Some("old-session"));
    assert_eq!(
        test.account.credentials.account_operator().as_deref(),
        Some("ana")
    );
    assert!(test.account.credentials.workspace_needs_identity());
    // Actually changing people must not guess a new owner for legacy data.
    assert!(
        test.account
            .act_for(
                AccountAction::SwitchOperator {
                    operator_id: Some("ben".into()),
                    restoring: false,
                    device: None,
                },
                Some("ana"),
                0
            )
            .await
            .is_err()
    );
    assert_eq!(test.account.token().as_deref(), Some("old-session"));
}

#[tokio::test]
async fn a_delayed_session_sign_out_cannot_sign_out_the_next_person() {
    let test = setup_scenarios(StatusCode::OK, false, false, true).await;
    test.account.credentials.bind_operator(Some("ana")).unwrap();
    let account = test.account.clone();
    let pending = tokio::spawn(async move {
        account
            .act_for(
                AccountAction::SignOutSession {
                    id: "current-old".into(),
                    device: None,
                },
                Some("ana"),
                0,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), test.started.notified())
        .await
        .unwrap();
    test.account.generation.fetch_add(1, Ordering::SeqCst);
    test.account
        .act_for(
            AccountAction::SwitchOperator {
                operator_id: Some("ben".into()),
                restoring: false,
                device: None,
            },
            Some("ana"),
            1,
        )
        .await
        .unwrap();
    test.account
        .act_for(
            AccountAction::Verify {
                email: "new@example.com".into(),
                code: "123456".into(),
                product_updates: false,
            },
            Some("ben"),
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
