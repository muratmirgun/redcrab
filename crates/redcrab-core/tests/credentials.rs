//! The credential store (the macOS Keychain, Windows Credential Manager or
//! the Secret Service) with the secrets of a Sentinel connection through an
//! SSH tunnel. Runs under `REDCRAB_CREDENTIAL_SERVICE` when that names a
//! test service, else under `redcrab-test`; never under the app's own
//! `redcrab`. Every entry it creates is deleted again.

#[macro_use]
mod common;

use std::{sync::mpsc, thread, time::Duration};

use redcrab_core::{
    ConnectionConfig, SshAuth, SshConfig, Topology,
    store::{Credentials, Secret},
};

fn test_service() -> Credentials {
    let service = std::env::var("REDCRAB_CREDENTIAL_SERVICE")
        .ok()
        .filter(|s| !s.is_empty() && s != "redcrab")
        .unwrap_or_else(|| "redcrab-test".into());
    assert_ne!(service, "redcrab");
    Credentials::new(service)
}

/// Removes every secret of the connection, however the test ends.
struct Forget(Credentials, String);

impl Drop for Forget {
    fn drop(&mut self) {
        if let Err(err) = self.0.delete_all(&self.1) {
            eprintln!(
                "could not remove the test credentials of {}: {err:#}",
                self.1
            );
        }
    }
}

/// Runs `f` on a thread of its own and fails the test if the credential
/// store blocks (e.g. on a locked keychain's prompt) instead of hanging.
fn bounded<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect("the credential store did not answer within 30 s (locked?)")
}

#[test]
fn every_secret_round_trips_in_an_entry_of_its_own() {
    let Some(_t) = it!() else { return };
    bounded(|| {
        let store = test_service();
        let name = format!("it-credentials-{}", std::process::id());
        let _forget = Forget(store.clone(), name.clone());
        let typed = ConnectionConfig {
            name: name.clone(),
            password: Some("redis-pw".into()),
            sentinel_password: Some("sentinel-pw".into()),
            topology: Topology::Sentinel {
                master: "m".into(),
                sentinels: vec!["127.0.0.1:26379".into()],
                sentinel_username: None,
                sentinel_tls: false,
            },
            ssh: Some(SshConfig {
                host: "bastion".into(),
                user: "ops".into(),
                auth: SshAuth::Password,
                password: Some("ssh-pw".into()),
                // Not used with password login, so not saved.
                passphrase: Some("unused".into()),
                ..SshConfig::default()
            }),
            ..ConnectionConfig::default()
        };
        store.save_from(&typed).unwrap();
        for (secret, expected) in [
            (Secret::Redis, Some("redis-pw")),
            (Secret::Sentinel, Some("sentinel-pw")),
            (Secret::SshPassword, Some("ssh-pw")),
            (Secret::SshPassphrase, None),
        ] {
            assert_eq!(
                store.load(&name, secret).unwrap().as_deref(),
                expected,
                "{secret:?}"
            );
        }

        // A form shown again has no secrets; connecting fills them in.
        let mut saved = typed.without_secrets();
        assert!(store.fill(&mut saved).is_empty());
        assert_eq!(saved.password.as_deref(), Some("redis-pw"));
        assert_eq!(saved.sentinel_password.as_deref(), Some("sentinel-pw"));
        assert_eq!(
            saved.ssh.as_ref().unwrap().password.as_deref(),
            Some("ssh-pw")
        );
        assert_eq!(saved.ssh.as_ref().unwrap().passphrase, None);

        // What was typed wins over what is saved.
        let mut retyped = ConnectionConfig {
            password: Some("new".into()),
            ..typed.without_secrets()
        };
        store.fill(&mut retyped);
        assert_eq!(retyped.password.as_deref(), Some("new"));

        // A key file's passphrase has an entry of its own.
        let with_key = ConnectionConfig {
            ssh: Some(SshConfig {
                auth: SshAuth::PrivateKey {
                    path: "/keys/id".into(),
                },
                passphrase: Some("phrase".into()),
                ..typed.ssh.clone().unwrap()
            }),
            ..typed.without_secrets()
        };
        store.save_from(&with_key).unwrap();
        assert_eq!(
            store.load(&name, Secret::SshPassphrase).unwrap().as_deref(),
            Some("phrase")
        );
        assert_eq!(
            store.load(&name, Secret::SshPassword).unwrap().as_deref(),
            Some("ssh-pw"),
            "the SSH password is a separate entry"
        );

        store.delete_all(&name).unwrap();
        for secret in Secret::ALL {
            assert_eq!(store.load(&name, secret).unwrap(), None, "{secret:?}");
        }
        // Deleting what is not there is fine.
        store.delete_all(&name).unwrap();
    });
}
