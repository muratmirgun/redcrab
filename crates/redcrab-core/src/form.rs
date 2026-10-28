//! The connection form as plain values, so every front end edits
//! connections the same way: [`ConnectionForm::from_config`] fills a form
//! from saved settings, and [`ConnectionForm::to_config`] turns what was
//! typed into settings, or into the first problem with the field it is
//! about (see [`ConnectionConfig::validate`]).
//!
//! Text fields are kept as typed; empty ones take defaults (host
//! `127.0.0.1`, port 6379, SSH port 22, database 0). What a choice makes
//! irrelevant is ignored rather than checked (a socket path once the type is
//! Cluster, a cluster's database), since a form hides those fields. Secrets
//! are never filled in from the credential store: an empty password field
//! means "use the saved one".

use std::path::PathBuf;

use crate::config::{
    ConfigField, ConnectionConfig, Environment, HostKeyPolicy, InvalidConfig, SshAuth, SshConfig,
    TlsConfig, Topology, split_addresses,
};

/// The kinds of [`Topology`], for a selector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TopologyKind {
    #[default]
    Standalone,
    Cluster,
    Sentinel,
}

impl TopologyKind {
    pub const ALL: [Self; 3] = [Self::Standalone, Self::Cluster, Self::Sentinel];

    pub fn label(self) -> &'static str {
        match self {
            Self::Standalone => "Standalone",
            Self::Cluster => "Cluster",
            Self::Sentinel => "Sentinel",
        }
    }

    pub fn of(topology: &Topology) -> Self {
        match topology {
            Topology::Standalone => Self::Standalone,
            Topology::Cluster { .. } => Self::Cluster,
            Topology::Sentinel { .. } => Self::Sentinel,
        }
    }
}

/// The kinds of [`SshAuth`], for a selector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SshMethod {
    #[default]
    Password,
    KeyFile,
    Agent,
}

impl SshMethod {
    pub const ALL: [Self; 3] = [Self::Password, Self::KeyFile, Self::Agent];

    pub fn label(self) -> &'static str {
        match self {
            Self::Password => "Password",
            Self::KeyFile => "Key file",
            Self::Agent => "SSH agent",
        }
    }
}

/// Everything a connection form shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionForm {
    pub name: String,
    /// The name the form was filled with ([`Self::from_config`]); empty for
    /// a new connection. Another name is a new one, which must pass
    /// [`ConnectionConfig::validate_new_name`]; the saved one is kept as
    /// it is, so connections older versions saved keep working.
    pub saved_name: String,
    pub kind: TopologyKind,
    pub host: String,
    pub port: String,
    pub db: String,
    pub username: String,
    pub password: String,
    pub separator: String,
    /// Standalone only: connect to `unix_path` instead of host and port.
    pub unix: bool,
    pub unix_path: String,
    /// Cluster: more seed nodes, `host:port` separated by commas or spaces.
    pub seeds: String,
    pub sentinel_master: String,
    /// `host:port` separated by commas or spaces.
    pub sentinels: String,
    pub sentinel_username: String,
    pub sentinel_password: String,
    pub sentinel_tls: bool,
    pub tls: bool,
    pub tls_insecure: bool,
    pub tls_ca: String,
    pub tls_cert: String,
    pub tls_key: String,
    pub read_only: bool,
    pub environment: Environment,
    /// Reach the server through an SSH tunnel.
    pub ssh: bool,
    pub ssh_host: String,
    pub ssh_port: String,
    pub ssh_user: String,
    pub ssh_method: SshMethod,
    pub ssh_password: String,
    pub ssh_key: String,
    pub ssh_passphrase: String,
    pub ssh_host_key: HostKeyPolicy,
}

impl Default for ConnectionForm {
    /// A new connection: the defaults of [`ConnectionConfig`], unnamed.
    fn default() -> Self {
        Self::from_config(&ConnectionConfig {
            name: String::new(),
            ..ConnectionConfig::default()
        })
    }
}

fn path(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

fn text(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}

/// A number field: `default` when empty, else what it parses to.
fn number<T: std::str::FromStr>(
    text: &str,
    default: T,
    field: ConfigField,
    what: &str,
) -> Result<T, InvalidConfig> {
    match text.trim() {
        "" => Ok(default),
        n => n.parse().map_err(|_| InvalidConfig {
            field,
            message: format!("{n:?} is not a valid {what}"),
        }),
    }
}

impl ConnectionForm {
    /// The form for saved (or URL-given) settings.
    pub fn from_config(config: &ConnectionConfig) -> Self {
        let tls = config.tls.clone().unwrap_or_default();
        let ssh = config.ssh.clone().unwrap_or_default();
        let (seeds, master, sentinels, sentinel_username, sentinel_tls) = match &config.topology {
            Topology::Standalone => Default::default(),
            Topology::Cluster { seeds } => {
                (seeds.join(", "), String::new(), String::new(), None, false)
            }
            Topology::Sentinel {
                master,
                sentinels,
                sentinel_username,
                sentinel_tls,
            } => (
                String::new(),
                master.clone(),
                sentinels.join(", "),
                sentinel_username.clone(),
                *sentinel_tls,
            ),
        };
        let (ssh_method, ssh_key) = match &ssh.auth {
            SshAuth::Password => (SshMethod::Password, String::new()),
            SshAuth::PrivateKey { path } => (SshMethod::KeyFile, path.display().to_string()),
            SshAuth::Agent => (SshMethod::Agent, String::new()),
        };
        Self {
            name: config.name.clone(),
            saved_name: config.name.clone(),
            kind: TopologyKind::of(&config.topology),
            host: config.host.clone(),
            port: config.port.to_string(),
            db: config.db.to_string(),
            username: text(&config.username),
            password: text(&config.password),
            separator: config.separator.to_string(),
            unix: config.unix_socket.is_some(),
            unix_path: path(&config.unix_socket),
            seeds,
            sentinel_master: master,
            sentinels,
            sentinel_username: text(&sentinel_username),
            sentinel_password: text(&config.sentinel_password),
            sentinel_tls,
            tls: config.tls.is_some(),
            tls_insecure: tls.insecure,
            tls_ca: path(&tls.ca_file),
            tls_cert: path(&tls.client_cert_file),
            tls_key: path(&tls.client_key_file),
            read_only: config.read_only,
            environment: config.environment,
            ssh: config.ssh.is_some(),
            ssh_host: ssh.host.clone(),
            ssh_port: ssh.port.to_string(),
            ssh_user: ssh.user.clone(),
            ssh_method,
            ssh_password: text(&ssh.password),
            ssh_key,
            ssh_passphrase: text(&ssh.passphrase),
            ssh_host_key: ssh.host_key,
        }
    }

    /// Turns TLS on or off. A Sentinel connection's sentinels follow, so
    /// that TLS for the master does not leave the sentinel password and the
    /// master's address in clear text by default; they can still be set
    /// apart afterwards, for sentinels that only speak plain TCP.
    pub fn set_tls(&mut self, on: bool) {
        self.tls = on;
        self.sentinel_tls = on;
    }

    /// Picks the topology. Switching to Sentinel makes the sentinels use
    /// TLS when the connection does (see [`Self::set_tls`]).
    pub fn set_kind(&mut self, kind: TopologyKind) {
        if kind == TopologyKind::Sentinel && self.kind != TopologyKind::Sentinel {
            self.sentinel_tls = self.tls;
        }
        self.kind = kind;
    }

    /// A warning to show while a Sentinel connection uses TLS for the
    /// master but not for the sentinels.
    pub fn sentinel_warning(&self) -> Option<&'static str> {
        (self.kind == TopologyKind::Sentinel && self.tls && !self.sentinel_tls).then_some(
            "The sentinels are asked without TLS: their password and the master's address they answer with can be read, or changed, on the way.",
        )
    }

    /// The settings the form describes, checked with
    /// [`ConnectionConfig::validate`]. Fields left empty take their
    /// defaults; an unnamed connection is named after where it goes.
    pub fn to_config(&self) -> Result<ConnectionConfig, InvalidConfig> {
        let defaults = ConnectionConfig::default();
        let some = |s: &str| {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        };
        // Secrets are taken as typed: spaces may be part of them.
        let secret = |s: &str| (!s.is_empty()).then(|| s.to_string());
        let file = |s: &str| some(s).map(PathBuf::from);
        let topology = match self.kind {
            TopologyKind::Standalone => Topology::Standalone,
            TopologyKind::Cluster => Topology::Cluster {
                seeds: split_addresses(&self.seeds),
            },
            TopologyKind::Sentinel => Topology::Sentinel {
                master: self.sentinel_master.trim().to_string(),
                sentinels: split_addresses(&self.sentinels),
                sentinel_username: some(&self.sentinel_username),
                sentinel_tls: self.sentinel_tls,
            },
        };
        let sentinel = self.kind == TopologyKind::Sentinel;
        let unix = self.kind == TopologyKind::Standalone && self.unix;
        // A form hides the fields a choice does not use, so what is left in
        // them must not stand in the way: the port of a socket or Sentinel
        // connection, and the database of a cluster (which has only 0).
        let port = match number(&self.port, defaults.port, ConfigField::Port, "port") {
            Err(_) if sentinel || unix => defaults.port,
            port => port?,
        };
        let db = match self.kind {
            TopologyKind::Cluster => 0,
            _ => number(&self.db, 0, ConfigField::Db, "database index")?,
        };
        let ssh = if self.ssh {
            Some(SshConfig {
                host: self.ssh_host.trim().to_string(),
                port: number(&self.ssh_port, 22, ConfigField::SshPort, "SSH port")?,
                user: self.ssh_user.trim().to_string(),
                auth: match self.ssh_method {
                    SshMethod::Password => SshAuth::Password,
                    SshMethod::KeyFile => SshAuth::PrivateKey {
                        path: PathBuf::from(self.ssh_key.trim()),
                    },
                    SshMethod::Agent => SshAuth::Agent,
                },
                host_key: self.ssh_host_key,
                password: (self.ssh_method == SshMethod::Password)
                    .then(|| secret(&self.ssh_password))
                    .flatten(),
                passphrase: (self.ssh_method == SshMethod::KeyFile)
                    .then(|| secret(&self.ssh_passphrase))
                    .flatten(),
            })
        } else {
            None
        };
        let mut config = ConnectionConfig {
            name: self.name.trim().to_string(),
            host: some(&self.host).unwrap_or(defaults.host),
            port,
            username: some(&self.username),
            password: secret(&self.password),
            db,
            separator: self
                .separator
                .trim()
                .chars()
                .next()
                .unwrap_or(defaults.separator),
            tls: self.tls.then(|| TlsConfig {
                insecure: self.tls_insecure,
                ca_file: file(&self.tls_ca),
                client_cert_file: file(&self.tls_cert),
                client_key_file: file(&self.tls_key),
            }),
            read_only: self.read_only,
            environment: self.environment,
            unix_socket: unix.then(|| PathBuf::from(self.unix_path.trim())),
            topology,
            ssh,
            sentinel_password: sentinel.then(|| secret(&self.sentinel_password)).flatten(),
        };
        if config.name.is_empty() {
            config.name = config.suggested_name();
        }
        config.validate()?;
        if config.name != self.saved_name {
            config.validate_new_name()?;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::config::UNIX_SOCKETS;

    fn round_trip(config: ConnectionConfig) {
        let form = ConnectionForm::from_config(&config);
        assert_eq!(form.to_config(), Ok(config.clone()), "{form:#?}");
    }

    #[test]
    fn settings_survive_the_form() {
        round_trip(ConnectionConfig::default());
        round_trip(ConnectionConfig {
            name: "prod".into(),
            host: "cache.example.com".into(),
            port: 6380,
            db: 3,
            username: Some("app".into()),
            password: Some(" spaced secret ".into()),
            separator: '/',
            tls: Some(TlsConfig {
                insecure: true,
                ca_file: Some("/ca.pem".into()),
                client_cert_file: Some("/c.pem".into()),
                client_key_file: Some("/k.pem".into()),
            }),
            read_only: true,
            environment: Environment::Production,
            ..ConnectionConfig::default()
        });
        if UNIX_SOCKETS {
            round_trip(ConnectionConfig {
                name: "sock".into(),
                unix_socket: Some("/tmp/redis.sock".into()),
                db: 2,
                ..ConnectionConfig::default()
            });
        }
        round_trip(ConnectionConfig {
            name: "cluster".into(),
            port: 7000,
            topology: Topology::Cluster {
                seeds: vec!["10.0.0.2:7001".into(), "[::1]:7002".into()],
            },
            ..ConnectionConfig::default()
        });
        round_trip(ConnectionConfig {
            name: "watched".into(),
            db: 4,
            topology: Topology::Sentinel {
                master: "mymaster".into(),
                sentinels: vec!["a:26379".into(), "b:26379".into()],
                sentinel_username: Some("watcher".into()),
                sentinel_tls: true,
            },
            sentinel_password: Some("sentinel-pw".into()),
            ..ConnectionConfig::default()
        });
        for (auth, password, passphrase) in [
            (SshAuth::Password, Some("ssh-pw"), None),
            (
                SshAuth::PrivateKey {
                    path: "~/.ssh/id_ed25519".into(),
                },
                None,
                Some("phrase"),
            ),
            (SshAuth::PrivateKey { path: "/k".into() }, None, None),
            (SshAuth::Agent, None, None),
        ] {
            round_trip(ConnectionConfig {
                name: "tunneled".into(),
                host: "10.0.0.5".into(),
                ssh: Some(SshConfig {
                    host: "bastion".into(),
                    port: 2222,
                    user: "ops".into(),
                    auth,
                    host_key: HostKeyPolicy::AcceptNew,
                    password: password.map(str::to_string),
                    passphrase: passphrase.map(str::to_string),
                }),
                ..ConnectionConfig::default()
            });
        }
    }

    #[test]
    fn empty_fields_take_defaults() {
        let form = ConnectionForm {
            host: " ".into(),
            port: String::new(),
            db: String::new(),
            separator: String::new(),
            ..ConnectionForm::default()
        };
        let config = form.to_config().unwrap();
        assert_eq!(
            (
                config.host.as_str(),
                config.port,
                config.db,
                config.separator
            ),
            ("127.0.0.1", 6379, 0, ':')
        );
        assert_eq!(config.name, "127.0.0.1", "named after where it goes");
        let config = ConnectionForm {
            ssh: true,
            ssh_host: "bastion".into(),
            ssh_port: String::new(),
            ssh_user: "ops".into(),
            ssh_method: SshMethod::Agent,
            ..ConnectionForm::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(config.ssh.unwrap().port, 22);
    }

    #[test]
    fn problems_name_their_field() {
        let field = |form: ConnectionForm| form.to_config().unwrap_err().field;
        let base = ConnectionForm::default;
        assert_eq!(
            field(ConnectionForm {
                port: "x".into(),
                ..base()
            }),
            ConfigField::Port
        );
        assert_eq!(
            field(ConnectionForm {
                port: "70000".into(),
                ..base()
            }),
            ConfigField::Port
        );
        assert_eq!(
            field(ConnectionForm {
                db: "-1".into(),
                ..base()
            }),
            ConfigField::Db
        );
        assert_eq!(
            field(ConnectionForm {
                ssh: true,
                ssh_port: "22a".into(),
                ..base()
            }),
            ConfigField::SshPort
        );
        assert_eq!(
            field(ConnectionForm {
                kind: TopologyKind::Sentinel,
                sentinel_master: "m".into(),
                sentinels: "s:26379".into(),
                db: "x".into(),
                ..base()
            }),
            ConfigField::Db
        );
        assert_eq!(
            field(ConnectionForm {
                unix: true,
                ..base()
            }),
            ConfigField::UnixSocket
        );
        assert_eq!(
            field(ConnectionForm {
                kind: TopologyKind::Sentinel,
                ..base()
            }),
            ConfigField::SentinelMaster
        );
        let err = ConnectionForm {
            port: "x".into(),
            ..base()
        }
        .to_config()
        .unwrap_err();
        assert_eq!(err.message, "\"x\" is not a valid port");
    }

    #[test]
    fn switches_only_apply_where_they_can() {
        // A socket path typed before switching to Cluster is not used.
        let config = ConnectionForm {
            kind: TopologyKind::Cluster,
            unix: true,
            unix_path: "/tmp/r.sock".into(),
            sentinel_password: "x".into(),
            ..ConnectionForm::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(config.unix_socket, None);
        assert_eq!(config.sentinel_password, None);
        // Nor is a database or port typed into a field the choice hides.
        let config = ConnectionForm {
            kind: TopologyKind::Cluster,
            db: "3".into(),
            ..ConnectionForm::default()
        }
        .to_config()
        .unwrap();
        assert_eq!(config.db, 0, "a cluster has only database 0");
        let socket = ConnectionForm {
            unix: true,
            unix_path: "/tmp/r.sock".into(),
            ..ConnectionForm::default()
        };
        let sentinel = ConnectionForm {
            kind: TopologyKind::Sentinel,
            sentinel_master: "m".into(),
            sentinels: "s:26379".into(),
            ..ConnectionForm::default()
        };
        for form in [Some(sentinel), UNIX_SOCKETS.then_some(socket)]
            .into_iter()
            .flatten()
        {
            let config = ConnectionForm {
                port: "6379x".into(),
                ..form
            }
            .to_config()
            .unwrap();
            assert_eq!(config.port, 6379);
        }
        // Over TCP the port matters, so a bad one is still a problem.
        let err = ConnectionForm {
            port: "6379x".into(),
            ..ConnectionForm::default()
        }
        .to_config()
        .unwrap_err();
        assert_eq!(err.field, ConfigField::Port);
        // Only the secret the chosen SSH login uses is kept.
        let config = ConnectionForm {
            ssh: true,
            ssh_host: "b".into(),
            ssh_user: "u".into(),
            ssh_method: SshMethod::Agent,
            ssh_password: "pw".into(),
            ssh_passphrase: "phrase".into(),
            ..ConnectionForm::default()
        }
        .to_config()
        .unwrap();
        let ssh = config.ssh.unwrap();
        assert_eq!((ssh.password, ssh.passphrase), (None, None));
    }

    #[test]
    fn names_with_hash_are_kept_but_not_given() {
        // Saved by an older version, which allowed any name.
        let old = ConnectionConfig {
            name: "cache #2".into(),
            ..ConnectionConfig::default()
        };
        let form = ConnectionForm::from_config(&old);
        assert_eq!(form.to_config(), Ok(old));
        // A new name with # is refused, for a new connection or a rename.
        let renamed = ConnectionForm {
            name: "cache #3".into(),
            ..form
        };
        assert_eq!(renamed.to_config().unwrap_err().field, ConfigField::Name);
        let new = ConnectionForm {
            name: "prod#ssh".into(),
            ..ConnectionForm::default()
        };
        assert_eq!(new.to_config().unwrap_err().field, ConfigField::Name);
        // Renaming away from it is fine.
        let renamed = ConnectionForm {
            name: "cache 2".into(),
            ..ConnectionForm::from_config(&ConnectionConfig {
                name: "cache #2".into(),
                ..ConnectionConfig::default()
            })
        };
        assert_eq!(renamed.to_config().unwrap().name, "cache 2");
    }

    #[test]
    fn sentinels_use_tls_when_the_connection_does() {
        let sentinel = |form: &ConnectionForm| match form.to_config().unwrap().topology {
            Topology::Sentinel { sentinel_tls, .. } => sentinel_tls,
            other => panic!("{other:?}"),
        };
        let mut form = ConnectionForm {
            sentinel_master: "m".into(),
            sentinels: "s:26379".into(),
            ..ConnectionForm::default()
        };
        // TLS turned on before or after choosing Sentinel.
        form.set_tls(true);
        form.set_kind(TopologyKind::Sentinel);
        assert!(sentinel(&form));
        assert_eq!(form.sentinel_warning(), None);
        let mut form = ConnectionForm {
            kind: TopologyKind::Sentinel,
            ..form
        };
        form.set_tls(false);
        assert!(!sentinel(&form));
        form.set_tls(true);
        assert!(sentinel(&form));
        // A saved Standalone connection with TLS, switched to Sentinel.
        let mut form = ConnectionForm::from_config(&ConnectionConfig {
            tls: Some(TlsConfig::default()),
            ..ConnectionConfig::default()
        });
        form.sentinel_master = "m".into();
        form.sentinels = "s:26379".into();
        form.set_kind(TopologyKind::Sentinel);
        assert!(sentinel(&form));

        // Sentinels that only speak plain TCP can still be set apart,
        // with a warning; choosing Sentinel again keeps that.
        form.sentinel_tls = false;
        assert!(!sentinel(&form));
        assert!(form.sentinel_warning().is_some());
        form.set_kind(TopologyKind::Sentinel);
        assert!(!form.sentinel_tls);
        form.set_kind(TopologyKind::Standalone);
        assert_eq!(form.sentinel_warning(), None);
    }

    proptest! {
        #[test]
        fn any_input_gives_valid_settings_or_a_problem(
            host in "\\PC{0,12}",
            port in "\\PC{0,6}",
            db in "[0-9a-z-]{0,3}",
            seeds in "[a-z0-9:., \\[\\]]{0,20}",
            kind in 0usize..3,
            unix in any::<bool>(),
            ssh in any::<bool>(),
            path in "\\PC{0,10}",
        ) {
            let form = ConnectionForm {
                host,
                port,
                db,
                seeds: seeds.clone(),
                sentinels: seeds,
                sentinel_master: "m".into(),
                kind: TopologyKind::ALL[kind],
                unix,
                unix_path: path.clone(),
                ssh,
                ssh_host: "b".into(),
                ssh_user: "u".into(),
                ssh_key: path,
                ssh_method: SshMethod::KeyFile,
                ..ConnectionForm::default()
            };
            match form.to_config() {
                Ok(config) => {
                    prop_assert_eq!(config.validate(), Ok(()));
                    // What came out goes back in unchanged.
                    prop_assert_eq!(ConnectionForm::from_config(&config).to_config(), Ok(config));
                }
                Err(err) => prop_assert!(!err.message.is_empty()),
            }
        }
    }
}
