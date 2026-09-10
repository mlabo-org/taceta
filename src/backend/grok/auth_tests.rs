use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};

use super::*;

fn write_auth(path: &Path, records: &str, mode: u32) {
    fs::write(path, records).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn write_helper(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn record(token: &str, expires_at: &str) -> String {
    format!(
        r#"{{"https://auth.x.ai::current-client":{{"key":"{token}","auth_mode":"oidc","create_time":"2026-08-01T00:00:00Z","user_id":"user-1","expires_at":"{expires_at}","oidc_issuer":"https://auth.x.ai"}}}}"#
    )
}

#[test]
fn reads_one_private_current_session_and_reloads_changed_file() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("auth.json");
    write_auth(
        &path,
        &record("first-secret", "2099-01-01T00:00:00Z"),
        0o600,
    );
    let store = CredentialStore::new(path.clone()).unwrap();
    let first = store.load().unwrap();
    assert_eq!(first.token(), "first-secret");

    write_auth(
        &path,
        &record("second-secret-longer", "2099-01-01T00:00:00Z"),
        0o600,
    );
    let second = store.load().unwrap();
    assert_eq!(second.token(), "second-secret-longer");
}

#[test]
fn rejects_group_readable_auth_file() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("auth.json");
    write_auth(&path, &record("secret", "2099-01-01T00:00:00Z"), 0o640);
    let store = CredentialStore::new(path).unwrap();
    assert!(matches!(
        store.load(),
        Err(CredentialError::UnsafeAuthPermissions)
    ));
}

#[test]
fn rejects_expired_and_ambiguous_session_credentials() {
    assert!(matches!(
        parse_auth_map(&record("secret", "2020-01-01T00:00:00Z")),
        Err(CredentialError::ExpiredSessionCredential)
    ));
    let ambiguous = r#"{
        "https://auth.x.ai::client-a":{"key":"a","auth_mode":"oidc","create_time":"2026-01-01T00:00:00Z","user_id":"user-a","expires_at":"2099-01-01T00:00:00Z"},
        "https://auth.x.ai::client-b":{"key":"b","auth_mode":"external","create_time":"2026-01-01T00:00:00Z","user_id":"user-b","expires_at":"2099-01-01T00:00:00Z","oidc_issuer":"https://auth.x.ai"}
    }"#;
    assert!(matches!(
        parse_auth_map(ambiguous),
        Err(CredentialError::AmbiguousSessionCredential)
    ));
}

#[test]
fn expired_credential_waits_for_official_file_replacement() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("auth.json");
    write_auth(&path, &record("expired", "2020-01-01T00:00:00Z"), 0o600);
    let store = CredentialStore::new(path.clone()).unwrap();

    let replacement_path = temporary.path().join("auth.json.renewed");
    let renew = thread::spawn(move || {
        thread::sleep(StdDuration::from_millis(25));
        write_auth(
            &replacement_path,
            &record("renewed", "2099-01-01T00:00:00Z"),
            0o600,
        );
        fs::rename(replacement_path, path).unwrap();
    });

    let credential = store
        .load_with_renewal_grace(StdDuration::from_secs(1))
        .unwrap();
    renew.join().unwrap();
    assert_eq!(credential.token(), "renewed");
}

#[test]
fn expired_credential_adopts_official_helper_replacement() {
    let temporary = tempfile::tempdir().unwrap();
    let auth_home = temporary.path();
    let path = auth_home.join("auth.json");
    let helper = auth_home.join("bin/grok");
    let invocation = auth_home.join("invocation");
    fs::create_dir(auth_home.join("bin")).unwrap();
    write_auth(&path, &record("expired", "2020-01-01T00:00:00Z"), 0o600);
    write_helper(
        &helper,
        &format!(
            "helper_dir=$(dirname \"$0\")\nauth_path=\"$helper_dir/../auth.json\"\nprintf '%s' \"$1\" > \"{}\"\nprintf '%s' '{}' > \"$auth_path.tmp\"\nchmod 600 \"$auth_path.tmp\"\nmv \"$auth_path.tmp\" \"$auth_path\"",
            invocation.display(),
            record("renewed", "2099-01-01T00:00:00Z")
        ),
    );

    let credential = CredentialStore::new(path)
        .unwrap()
        .load_with_renewal_grace(StdDuration::from_secs(1))
        .unwrap();
    assert_eq!(credential.token(), "renewed");
    assert_eq!(fs::read_to_string(invocation).unwrap(), "models");
}

#[test]
fn failed_official_helper_preserves_expired_error() {
    let temporary = tempfile::tempdir().unwrap();
    let auth_home = temporary.path();
    let path = auth_home.join("auth.json");
    let helper = auth_home.join("bin/grok");
    fs::create_dir(auth_home.join("bin")).unwrap();
    write_auth(&path, &record("expired", "2020-01-01T00:00:00Z"), 0o600);
    write_helper(&helper, "exit 7");

    assert!(matches!(
        CredentialStore::new(path)
            .unwrap()
            .load_with_renewal_grace(StdDuration::from_secs(1)),
        Err(CredentialError::ExpiredSessionCredential)
    ));
}

#[test]
fn concurrent_renewal_waiters_share_one_successful_helper_run() {
    let temporary = tempfile::tempdir().unwrap();
    let auth_home = temporary.path();
    let path = auth_home.join("auth.json");
    let helper = auth_home.join("bin/grok");
    let count = auth_home.join("refresh-count");
    fs::create_dir(auth_home.join("bin")).unwrap();
    write_auth(&path, &record("expired", "2020-01-01T00:00:00Z"), 0o600);
    write_helper(
        &helper,
        &format!(
            "sleep 0.1\ncount=0\nif [ -f '{}' ]; then count=$(cat '{}'); fi\ncount=$((count + 1))\nprintf '%s' \"$count\" > '{}'\nprintf '%s' '{}' > '{}.tmp'\nchmod 600 '{}.tmp'\nmv '{}.tmp' '{}'",
            count.display(),
            count.display(),
            count.display(),
            record("renewed", "2099-01-01T00:00:00Z"),
            path.display(),
            path.display(),
            path.display(),
            path.display(),
        ),
    );

    let store = Arc::new(CredentialStore::new(path).unwrap());
    let barrier = Arc::new(Barrier::new(4));
    let mut workers = Vec::new();
    for _ in 0..4 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            // Use the provider's renewal budget: an unrelated parallel
            // filesystem/helper test must not consume a one-second mock
            // deadline before this helper can publish its replacement.
            store.load_with_renewal_grace(StdDuration::from_secs(60))
        }));
    }
    let results = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    for result in results {
        assert_eq!(result.unwrap().token(), "renewed");
    }
    assert_eq!(fs::read_to_string(count).unwrap(), "1");
}

#[test]
fn concurrent_renewal_waiters_share_one_failed_helper_run() {
    let temporary = tempfile::tempdir().unwrap();
    let auth_home = temporary.path();
    let path = auth_home.join("auth.json");
    let helper = auth_home.join("bin/grok");
    let count = auth_home.join("refresh-count");
    fs::create_dir(auth_home.join("bin")).unwrap();
    write_auth(&path, &record("expired", "2020-01-01T00:00:00Z"), 0o600);
    write_helper(
        &helper,
        &format!(
            "sleep 0.1\ncount=0\nif [ -f '{}' ]; then count=$(cat '{}'); fi\ncount=$((count + 1))\nprintf '%s' \"$count\" > '{}'\nexit 7",
            count.display(),
            count.display(),
            count.display(),
        ),
    );

    let store = Arc::new(CredentialStore::new(path).unwrap());
    let barrier = Arc::new(Barrier::new(4));
    let mut workers = Vec::new();
    for _ in 0..4 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            store.load_with_renewal_grace(StdDuration::from_secs(1))
        }));
    }
    for worker in workers {
        assert!(matches!(
            worker.join().unwrap(),
            Err(CredentialError::ExpiredSessionCredential)
        ));
    }
    assert_eq!(fs::read_to_string(count).unwrap(), "1");
}

#[test]
fn official_helper_is_not_attempted_for_non_expiry_errors() {
    let current_record = record("current", "2099-01-01T00:00:00Z");
    let cases = [
        ("malformed", Some("not-json"), None),
        ("current", Some(current_record.as_str()), Some("current")),
    ];

    for (name, contents, expected_token) in cases {
        let temporary = tempfile::tempdir().unwrap();
        let auth_home = temporary.path();
        let path = auth_home.join("auth.json");
        let helper = auth_home.join("bin/grok");
        let marker = auth_home.join("attempted");
        fs::create_dir(auth_home.join("bin")).unwrap();
        write_helper(&helper, &format!("touch \"{}\"", marker.display()));
        if let Some(contents) = contents {
            write_auth(&path, contents, 0o600);
        }

        let result = CredentialStore::new(path)
            .unwrap()
            .load_with_renewal_grace(StdDuration::from_millis(1));
        match expected_token {
            Some(token) => assert_eq!(result.unwrap().token(), token, "{name}"),
            None => assert!(result.is_err(), "{name}"),
        }
        assert!(!marker.exists(), "{name}");
    }
}

#[test]
fn ensure_launches_official_oauth_once_and_reloads_credential() {
    let temporary = tempfile::tempdir().unwrap();
    let auth_home = temporary.path();
    let path = auth_home.join("auth.json");
    let helper = auth_home.join("bin/grok");
    let invocation = auth_home.join("invocation");
    fs::create_dir(auth_home.join("bin")).unwrap();
    write_helper(
        &helper,
        &format!(
            "if [ \"$1\" = models ]; then exit 7; fi\nhelper_dir=$(dirname \"$0\")\nauth_path=\"$helper_dir/../auth.json\"\nprintf '%s %s' \"$1\" \"$2\" > \"{}\"\nprintf '%s' '{}' > \"$auth_path.tmp\"\nchmod 600 \"$auth_path.tmp\"\nmv \"$auth_path.tmp\" \"$auth_path\"",
            invocation.display(),
            record("renewed", "2099-01-01T00:00:00Z")
        ),
    );

    let credential = CredentialStore::new(path)
        .unwrap()
        .ensure_with_official_login()
        .unwrap();
    assert_eq!(credential.token(), "renewed");
    assert_eq!(fs::read_to_string(invocation).unwrap(), "login --oauth");
}

#[test]
fn missing_credential_uses_silent_refresh_before_browser_login() {
    let temporary = tempfile::tempdir().unwrap();
    let auth_home = temporary.path();
    let path = auth_home.join("auth.json");
    let helper = auth_home.join("bin/grok");
    let invocation = auth_home.join("invocation");
    fs::create_dir(auth_home.join("bin")).unwrap();
    write_helper(
        &helper,
        &format!(
            "helper_dir=$(dirname \"$0\")\nauth_path=\"$helper_dir/../auth.json\"\nprintf '%s' \"$1\" > \"{}\"\nprintf '%s' '{}' > \"$auth_path.tmp\"\nchmod 600 \"$auth_path.tmp\"\nmv \"$auth_path.tmp\" \"$auth_path\"",
            invocation.display(),
            record("renewed", "2099-01-01T00:00:00Z")
        ),
    );

    let credential = CredentialStore::new(path)
        .unwrap()
        .ensure_with_official_login()
        .unwrap();
    assert_eq!(credential.token(), "renewed");
    assert_eq!(fs::read_to_string(invocation).unwrap(), "models");
}

#[test]
fn ensure_does_not_launch_login_for_unsafe_or_malformed_auth() {
    let cases = [("malformed", "not-json", 0o600), ("unsafe", "{}", 0o640)];
    for (name, contents, mode) in cases {
        let temporary = tempfile::tempdir().unwrap();
        let auth_home = temporary.path();
        let path = auth_home.join("auth.json");
        let helper = auth_home.join("bin/grok");
        let marker = auth_home.join("attempted");
        fs::create_dir(auth_home.join("bin")).unwrap();
        write_auth(&path, contents, mode);
        write_helper(&helper, &format!("touch \"{}\"", marker.display()));

        assert!(
            CredentialStore::new(path)
                .unwrap()
                .ensure_with_official_login()
                .is_err(),
            "{name}"
        );
        assert!(!marker.exists(), "{name}");
    }
}

#[test]
fn official_helper_uses_only_tacetas_isolated_credential_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let auth_home = temporary.path().join("taceta");
    let auth_path = auth_home.join("auth.json");
    let helper = temporary.path().join("grok");
    write_helper(
        &helper,
        &format!(
            "if [ \"$1\" = models ]; then exit 7; fi\n\
             printf '%s\\n%s\\n%s\\n' \"$GROK_HOME\" \"$GROK_AUTH_PATH\" \"${{GROK_AUTH+present}}\" > \"$GROK_HOME/invocation\"\n\
             printf '%s' '{}' > \"$GROK_AUTH_PATH\"\n\
             chmod 600 \"$GROK_AUTH_PATH\"",
            record("isolated", "2099-01-01T00:00:00Z"),
        ),
    );
    let store = CredentialStore::with_official_cli(auth_path.clone(), helper).unwrap();
    let credential = store.ensure_with_official_login().unwrap();
    assert_eq!(credential.token(), "isolated");
    assert_eq!(credential.user_id(), "user-1");
    assert_eq!(
        fs::read_to_string(auth_home.join("invocation")).unwrap(),
        format!("{}\n{}\n\n", auth_home.display(), auth_path.display()),
    );
    assert_eq!(
        fs::metadata(&auth_home).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn normal_credential_access_never_starts_interactive_login() {
    let temporary = tempfile::tempdir().unwrap();
    let auth_path = temporary.path().join("auth.json");
    let helper = temporary.path().join("grok");
    let invocation = temporary.path().join("invocation");
    write_helper(
        &helper,
        "printf '%s' \"$1\" >> \"$GROK_HOME/invocation\"\nexit 7",
    );
    let store = CredentialStore::with_official_cli(auth_path, helper).unwrap();
    assert!(matches!(
        store.load_with_renewal_grace(StdDuration::from_millis(300)),
        Err(CredentialError::ReadAuth(error)) if error.kind() == std::io::ErrorKind::NotFound
    ));
    assert_eq!(fs::read_to_string(invocation).unwrap(), "models");
}

#[tokio::test]
async fn signout_stops_pending_login_and_reacquires_without_the_old_cache() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("auth.json");
    let helper = temporary.path().join("grok");
    let started = temporary.path().join("started");
    write_helper(
        &helper,
        "if [ \"$1\" = models ]; then exit 7; fi\nprintf started > \"$GROK_HOME/started\"\nwhile :; do sleep 0.05; done",
    );
    let manager = Arc::new(AuthManager::with_store(
        CredentialStore::with_official_cli(path.clone(), helper.clone()).unwrap(),
    ));
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let worker = Arc::clone(&manager);
    let pending = tokio::spawn(async move { worker.sign_in(events).await });
    tokio::time::timeout(StdDuration::from_secs(3), async {
        while !started.exists() {
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let epoch = manager.epoch();
    manager.sign_out().unwrap();
    assert!(manager.epoch() > epoch);
    assert!(pending.await.unwrap().is_err());
    assert!(!manager.is_signed_in().unwrap());
    assert!(!path.exists());

    for token in ["first-login", "fresh-login"] {
        write_helper(
            &helper,
            &format!(
                "if [ \"$1\" = models ]; then exit 7; fi\nprintf '%s' '{}' > \"$GROK_AUTH_PATH\"\nchmod 600 \"$GROK_AUTH_PATH\"",
                record(token, "2099-01-01T00:00:00Z"),
            ),
        );
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        manager.sign_in(events).await.unwrap();
        assert!(manager.is_signed_in().unwrap());
        assert_eq!(manager.session_credential().await.unwrap().token(), token);
        manager.sign_out().unwrap();
        assert!(!manager.is_signed_in().unwrap());
        assert!(!path.exists());
    }
}

#[tokio::test]
async fn dropping_login_cancels_its_official_helper() {
    let temporary = tempfile::tempdir().unwrap();
    let helper = temporary.path().join("grok");
    let started = temporary.path().join("started");
    write_helper(
        &helper,
        "if [ \"$1\" = models ]; then exit 7; fi\nprintf started > \"$GROK_HOME/started\"\nwhile :; do sleep 0.05; done",
    );
    let manager = Arc::new(AuthManager::with_store(
        CredentialStore::with_official_cli(temporary.path().join("auth.json"), helper).unwrap(),
    ));
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let worker = Arc::clone(&manager);
    let pending = tokio::spawn(async move { worker.sign_in(events).await });
    tokio::time::timeout(StdDuration::from_secs(3), async {
        while !started.exists() {
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let epoch = manager.epoch();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(manager.epoch() > epoch);
    assert!(manager.store().unwrap().helper.lock().unwrap().is_none());
    assert!(!manager.is_signed_in().unwrap());
}
