use super::super::tests::{Fixture, MemoryStore, Reply, saved_token};
use super::*;
use serde_json::json;
use std::{collections::HashMap, sync::atomic::Ordering};
use tokio::sync::mpsc;

fn auth(base: &str, store: Arc<MemoryStore>) -> AuthManager {
    AuthManager::new(
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        AuthEndpoints {
            authorize: format!("{base}/authorize"),
            token: format!("{base}/token"),
        },
        store,
    )
}

#[test]
fn pkce_and_callback_bind_method_path_host_and_unique_state() {
    assert_eq!(
        challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let base = Url::parse("http://127.0.0.1:40000/callback").unwrap();
    let valid =
        "GET /callback?code=fixture-code&state=expected HTTP/1.1\r\nHost: 127.0.0.1:40000\r\n\r\n";
    assert!(
        matches!(callback_code(valid, &base, "127.0.0.1:40000", "expected"), Ok(Callback::Code(code)) if code == "fixture-code")
    );
    for invalid in [
        valid.replace("state=expected", "state=wrong"),
        valid.replace("state=expected", "state=expected&state=expected"),
        valid.replace("GET ", "POST "),
        valid.replace("/callback?", "/other?"),
        valid.replace("Host: 127.0.0.1:40000", "Host: example.com"),
    ] {
        assert!(callback_code(&invalid, &base, "127.0.0.1:40000", "expected").is_err());
    }
}

#[tokio::test]
async fn loopback_login_exchanges_bound_code_and_only_saves_own_store() {
    let mut fixture = Fixture::start(vec![Reply::json(json!({"access_token": "fixture-new-access", "refresh_token": "fixture-new-refresh", "expires_in": 3600, "token_type": "Bearer"}))]).await;
    let store = Arc::new(MemoryStore::default());
    saved_token(&store, false);
    let manager = auth(&fixture.base, store.clone());
    let (events, mut receiver) = mpsc::unbounded_channel();
    let driver = async {
        let Some(GrokLoginEvent::OpenBrowser(url)) = receiver.recv().await else {
            panic!("missing browser event");
        };
        let url = Url::parse(&url).unwrap();
        let parameters: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(parameters["client_id"], CLIENT_ID);
        assert_eq!(parameters["code_challenge_method"], "S256");
        assert!(!parameters["scope"].contains("conversation"));
        let redirect = Url::parse(&parameters["redirect_uri"]).unwrap();
        assert_eq!(redirect.host_str(), Some("127.0.0.1"));
        let mut socket = TcpStream::connect(("127.0.0.1", redirect.port().unwrap()))
            .await
            .unwrap();
        let request = format!(
            "GET /callback?code=fixture-code&state={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            parameters["state"],
            redirect.port().unwrap()
        );
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut result = String::new();
        socket.read_to_string(&mut result).await.unwrap();
        assert!(result.starts_with("HTTP/1.1 200"));
        let token_request = fixture.requests.recv().await.unwrap();
        let form: HashMap<String, String> = url::form_urlencoded::parse(&token_request.body)
            .into_owned()
            .collect();
        assert_eq!(token_request.path, "/token");
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "fixture-code");
        assert_eq!(form["redirect_uri"], parameters["redirect_uri"]);
        assert_eq!(
            challenge(&form["code_verifier"]),
            parameters["code_challenge"]
        );
        assert!(!form.contains_key("client_secret"));
        // Keep the event receiver alive through the final save.
        while receiver.recv().await.is_some() {}
    };
    let (result, ()) = tokio::join!(manager.sign_in(events), driver);
    result.unwrap();
    fixture.task.await.unwrap();
    let saved = store.load().unwrap().unwrap();
    assert_eq!(saved.access_token, "fixture-new-access");
    assert_eq!(saved.refresh_token.as_deref(), Some("fixture-new-refresh"));
    assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn concurrent_requests_refresh_once_and_rotate_the_saved_token() {
    let mut fixture = Fixture::start(vec![Reply::json(json!({"access_token": "fixture-new", "refresh_token": "fixture-rotated", "expires_in": 3600}))]).await;
    let store = Arc::new(MemoryStore::default());
    saved_token(&store, true);
    let manager = auth(&fixture.base, store.clone());
    let (first, second) = tokio::join!(manager.access_token(), manager.access_token());
    assert_eq!(first.unwrap().as_str(), "fixture-new");
    assert_eq!(second.unwrap().as_str(), "fixture-new");
    fixture.task.await.unwrap();
    let request = fixture.requests.recv().await.unwrap();
    let form: HashMap<String, String> = url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect();
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "fixture-refresh-old");
    assert_eq!(store.saves.load(Ordering::SeqCst), 2); // initial + one refresh
    assert_eq!(
        store.load().unwrap().unwrap().refresh_token.as_deref(),
        Some("fixture-rotated")
    );
}

#[tokio::test]
async fn refresh_errors_preserve_credentials_and_do_not_echo_provider_secrets() {
    for reply in [
        Reply {
            status: "400 Bad Request",
            content_type: "application/json",
            body:
                br#"{"error":"invalid_grant","error_description":"fixture-secret-must-not-escape"}"#
                    .to_vec(),
        },
        Reply {
            status: "200 OK",
            content_type: "application/json",
            body: b"invalid token JSON fixture-secret-must-not-escape".to_vec(),
        },
    ] {
        let fixture = Fixture::start(vec![reply]).await;
        let store = Arc::new(MemoryStore::default());
        saved_token(&store, true);
        let manager = auth(&fixture.base, store.clone());
        let error = manager.access_token().await.err().unwrap();
        assert!(!error.contains("fixture-secret"));
        assert_eq!(
            store.load().unwrap().unwrap().access_token,
            "fixture-access-old"
        );
        assert_eq!(store.saves.load(Ordering::SeqCst), 1);
        fixture.task.await.unwrap();
    }
}

#[tokio::test]
async fn dropping_login_closes_loopback_and_signout_invalidates_late_save() {
    let store = Arc::new(MemoryStore::default());
    saved_token(&store, false);
    let manager = Arc::new(auth("http://127.0.0.1:1", store.clone()));
    let (events, mut receiver) = mpsc::unbounded_channel();
    let task_manager = manager.clone();
    let task = tokio::spawn(async move { task_manager.sign_in(events).await });
    let Some(GrokLoginEvent::OpenBrowser(url)) = receiver.recv().await else {
        panic!("missing browser event");
    };
    let url = Url::parse(&url).unwrap();
    let redirect = url
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    let redirect = Url::parse(&redirect).unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(
        TcpStream::connect(("127.0.0.1", redirect.port().unwrap()))
            .await
            .is_err()
    );
    assert_eq!(
        store.load().unwrap().unwrap().access_token,
        "fixture-access-old"
    );
    let old_epoch = manager.epoch();
    manager.sign_out().unwrap();
    let late = Credentials {
        access_token: "late".into(),
        refresh_token: None,
        expires_at: None,
    };
    assert!(manager.save_if_current(&late, old_epoch, false).is_err());
    assert!(!manager.is_signed_in().unwrap());
    assert_eq!(store.deletes.load(Ordering::SeqCst), 1);
}
