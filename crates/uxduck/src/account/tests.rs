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
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for TestAccount {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn setup(old_status: StatusCode, delayed: bool) -> TestAccount {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let handler_started = started.clone();
    let handler_release = release.clone();
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
                                "user": { "email": "old@example.com", "name": "" },
                                "productUpdates": false,
                            })),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "user": { "email": "new@example.com", "name": "" },
                            "productUpdates": false,
                        })),
                    )
                }
            }),
        )
        .route(
            "/v1/auth/sign-in/email-otp",
            post(|| async {
                (
                    [("set-auth-token", "new-session")],
                    Json(json!({ "isNew": false })),
                )
            }),
        )
        .route("/v1/me/sessions/current", put(|| async { Json(json!({})) }))
        .route("/v1/auth/sign-out", post(|| async { Json(json!({})) }));
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
        last: Mutex::new(AccountState::default()),
        shared_dir: dir.path().join("shared"),
    });
    TestAccount {
        account,
        started,
        release,
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
    })
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
