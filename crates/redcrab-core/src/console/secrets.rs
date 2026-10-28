//! Which console lines carry a secret, so that the history never keeps
//! them. Erring on the side of caution costs one history entry; the other
//! way leaves a password in a file.

use super::words;

/// Whether a command line (name first) may carry a password or another
/// secret: AUTH, HELLO ... AUTH, ACL SETUSER, CONFIG SET of a credential
/// (requirepass, masterauth, ...), MIGRATE ... AUTH / AUTH2, SENTINEL SET
/// auth-pass, a script (EVAL, EVAL_RO, SCRIPT LOAD, FUNCTION LOAD) that
/// mentions any of these, MODULE LOAD / LOADEX with arguments for the
/// module, and any command with an argument that looks like a credential
/// (see [`credential_like`]). Names and keywords match in any case, as the
/// server reads them.
pub fn is_sensitive<A: AsRef<[u8]>>(argv: &[A]) -> bool {
    let argv: Vec<&[u8]> = argv.iter().map(AsRef::as_ref).collect();
    sensitive(&argv)
}

/// [`is_sensitive`] for a line as typed. Lines that do not parse (an
/// unclosed quote, ...) are read as far as they go, so a half-typed
/// `AUTH "pass` is caught too.
pub fn is_sensitive_line(line: &str) -> bool {
    let words = words::split(line);
    let argv: Vec<&[u8]> = words.words.iter().map(|w| &w.value[..]).collect();
    sensitive(&argv)
}

fn sensitive(argv: &[&[u8]]) -> bool {
    let Some((name, args)) = argv.split_first() else {
        return false;
    };
    let is = |arg: &[u8], word: &str| arg.eq_ignore_ascii_case(word.as_bytes());
    let sub = args.first().copied().unwrap_or_default();
    let command = if is(name, "AUTH") {
        true
    } else if is(name, "HELLO") || is(name, "MIGRATE") {
        // HELLO [protover [AUTH username password]]; MIGRATE ... [AUTH
        // password | AUTH2 username password]. Any AUTH, however incomplete.
        args.iter().any(|arg| is(arg, "AUTH") || is(arg, "AUTH2"))
    } else if is(name, "ACL") && is(sub, "SETUSER") {
        // Rules carry passwords (`>secret`) and hashes (`#...`).
        true
    } else if is(name, "ACL") && is(sub, "DRYRUN") {
        // ACL DRYRUN username command [arg ...]: whatever the command carries.
        args.len() > 2 && sensitive(&args[2..])
    } else if (is(name, "CONFIG") && is(sub, "SET"))
        || (is(name, "SENTINEL") && (is(sub, "SET") || is(sub, "CONFIG")))
    {
        // CONFIG SET requirepass ..., SENTINEL SET mymaster auth-pass ...,
        // SENTINEL CONFIG SET sentinel-pass ...: any parameter named like a
        // credential.
        args[1..].iter().any(|arg| credential_parameter(arg))
    } else if is(name, "EVAL") || is(name, "EVAL_RO") {
        // EVAL script numkeys ...: the body is one argument.
        args.first()
            .is_some_and(|body| script_with_credentials(body))
    } else if (is(name, "SCRIPT") || is(name, "FUNCTION")) && is(sub, "LOAD") {
        // SCRIPT LOAD script; FUNCTION LOAD [REPLACE] function-code.
        args[1..].iter().any(|body| script_with_credentials(body))
    } else if is(name, "MODULE") && (is(sub, "LOAD") || is(sub, "LOADEX")) {
        // MODULE LOAD path [arg ...]; MODULE LOADEX path [CONFIG name value
        // ...] [ARGS arg ...]. What a module takes is its own business, a
        // token as likely as anything, and its configs are named as it
        // likes: only the path alone is kept.
        args.len() > 2
    } else {
        false
    };
    command || args.iter().any(|arg| credential_like(arg))
}

/// A script body that may hold a secret. Its commands and their arguments
/// are all in this one argument (`redis.call('MIGRATE', ..., 'AUTH',
/// 'pw')`), where no word of its own is `AUTH`; so whatever would make a
/// command line or a parameter name sensitive counts anywhere in it: `auth`
/// (AUTH, MIGRATE ... AUTH2, HELLO ... AUTH, masterauth), `setuser` (ACL
/// SETUSER), `pass`, and so on. A server runs MIGRATE from a script, so the
/// password really goes out; AUTH and ACL SETUSER are refused there, but
/// the line carries the secret all the same. Dropping a harmless script
/// that merely mentions an author costs one history entry.
fn script_with_credentials(body: &[u8]) -> bool {
    credential_parameter(body)
        || contains(&body.to_ascii_lowercase(), b"setuser")
        || credential_like(body)
}

/// Configuration parameters that hold credentials: requirepass,
/// masterauth, masteruser, tls-key-file-pass, tls-client-key-file-pass,
/// auth-pass, auth-user, sentinel-pass, sentinel-user, Valkey's
/// primaryauth and primaryuser (its names for masterauth and masteruser),
/// and whatever a newer server names alike.
fn credential_parameter(name: &[u8]) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "pass",
        "auth",
        "secret",
        "masteruser",
        "primaryuser",
        "sentinel-user",
    ]
    .iter()
    .any(|part| contains(&name, part.as_bytes()))
}

/// An argument that is, or names, a credential in any command, including
/// module commands this version does not know: the keywords AUTH and
/// AUTH2, the short field names `pass` and `pw`, anything mentioning a
/// password or a secret (`password`, `passwd`, `pwd`, `requirepass`,
/// `auth-pass`, `client_secret`, `api-key`, ...), and URLs with credentials
/// (`redis://user:pw@host`). `pass` and `pw` only as whole words: as parts
/// they are in too many words (`compass`, `bypass`, `pwm`).
///
/// This also drops harmless lines such as `HGET user:1 password_hint`;
/// losing those from the history is the price of never keeping a real one.
pub(crate) fn credential_like(arg: &[u8]) -> bool {
    let lower = arg.to_ascii_lowercase();
    ["auth", "auth2", "pass", "pw"]
        .iter()
        .any(|word| lower == word.as_bytes())
        || [
            "password",
            "passwd",
            "pwd",
            "passphrase",
            "requirepass",
            "masterauth",
            "-pass",
            "_pass",
            "secret",
            "apikey",
            "api-key",
            "api_key",
            "private-key",
            "private_key",
        ]
        .iter()
        .any(|part| contains(&lower, part.as_bytes()))
        || url_with_credentials(&lower)
}

/// `scheme://user:password@host...`, or any other userinfo before an `@`.
///
/// Generated passwords often hold `/`, `?` or `#`, which redcrab's own URL
/// parser takes as they are: it reads the credentials up to the last `@`.
/// So an `@` past one of them counts too when a `:` comes before it
/// (`https://user:pa/ss@host`), and always in a Redis URL, where a lone
/// credential is a password (`redis://pa/ss@host`). Only an `@` in the path
/// of another URL, with no `:` before it, is let through
/// (`https://example.com/@user`).
fn url_with_credentials(arg: &[u8]) -> bool {
    let Some(at) = find(arg, b"://") else {
        return false;
    };
    let (scheme, rest) = (&arg[..at], &arg[at + 3..]);
    let Some(last) = rest.iter().rposition(|&b| b == b'@') else {
        return false;
    };
    let end = rest
        .iter()
        .position(|&b| matches!(b, b'/' | b'?' | b'#'))
        .unwrap_or(rest.len());
    let redis = contains(scheme, b"redis") || contains(scheme, b"valkey");
    redis || rest[..end].contains(&b'@') || rest[..last].contains(&b':')
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::format::{quote_arg, split_args};

    fn line(line: &str) -> bool {
        is_sensitive_line(line)
    }

    #[test]
    fn auth_is_never_kept() {
        for typed in [
            "AUTH s3cret",
            "auth default s3cret",
            "Auth",
            "AUTH \"pass with spaces\"",
            // Half typed or broken: still caught.
            "AUTH \"s3cr",
            "AUTH 's3cret",
            "  auth   x  ",
        ] {
            assert!(line(typed), "{typed}");
        }
    }

    #[test]
    fn hello_with_auth_is_never_kept() {
        assert!(line("HELLO 3 AUTH default s3cret"));
        assert!(line("hello 2 auth app apppass SETNAME console"));
        assert!(line("HELLO 3 SETNAME x AUTH u p"));
        // Incomplete forms too: the password may be the last word typed.
        assert!(line("HELLO 3 AUTH s3cret"));
        assert!(!line("HELLO 3"));
        assert!(!line("HELLO 3 SETNAME console"));
    }

    #[test]
    fn acl_setuser_is_never_kept() {
        assert!(line("ACL SETUSER alice on >s3cret ~* +@all"));
        assert!(line(
            "acl setuser alice #5e884898da28047151d0e56f8dc6292773603d0d"
        ));
        assert!(line("ACL SETUSER alice"));
        assert!(!line("ACL WHOAMI"));
        assert!(!line("ACL LIST"));
        assert!(!line("ACL GETUSER alice"));
        // A dry run of a command that carries a secret carries it too.
        assert!(line("ACL DRYRUN alice AUTH alice s3cret"));
        assert!(line("ACL DRYRUN alice CONFIG SET requirepass x"));
        assert!(!line("ACL DRYRUN alice GET k"));
        assert!(!line("ACL DRYRUN alice"));
    }

    #[test]
    fn config_set_of_credentials_is_never_kept() {
        for typed in [
            "CONFIG SET requirepass s3cret",
            "config set MasterAuth s3cret",
            "CONFIG SET masteruser replicator",
            "CONFIG SET tls-key-file-pass s3cret",
            "CONFIG SET tls-client-key-file-pass s3cret",
            // Valkey's names for masteruser and masterauth.
            "CONFIG SET primaryuser replicator",
            "config set PrimaryAuth s3cret",
            // Several parameters at once; the credential is not the first.
            "CONFIG SET maxmemory 1gb requirepass s3cret",
            "CONFIG SET maxmemory 1gb masterauth",
        ] {
            assert!(line(typed), "{typed}");
        }
        assert!(!line("CONFIG SET maxmemory 1gb"));
        assert!(!line("CONFIG GET maxmemory"));
        // Reading it back shows no secret of the user's.
        assert!(!line("CONFIG GET maxmemory-policy"));
    }

    #[test]
    fn migrate_with_auth_is_never_kept() {
        assert!(line("MIGRATE 10.0.0.2 6379 key 0 5000 AUTH s3cret"));
        assert!(line(
            r#"MIGRATE 10.0.0.2 6379 "" 0 5000 COPY AUTH2 app s3cret KEYS a b"#
        ));
        assert!(line("migrate h 1 k 0 1 auth2 u"));
        assert!(!line("MIGRATE 10.0.0.2 6379 key 0 5000 COPY REPLACE"));
        assert!(!line(r#"MIGRATE h 6379 "" 0 5000 KEYS a b"#));
    }

    #[test]
    fn scripts_with_credentials_are_never_kept() {
        for typed in [
            // The server runs MIGRATE from a script and sends the password.
            r#"EVAL "return redis.call('MIGRATE','10.0.0.2','6379','k','0','2000','AUTH','hunter2')" 0"#,
            r#"EVAL "redis.call('MIGRATE',ARGV[1],ARGV[2],'k','0','5000','AUTH2','app','hunter2')" 0"#,
            r#"EVAL "return redis.call('MIGRATE',ARGV[1],ARGV[2],'k','0','5000','AUTH',ARGV[3])" 0 10.0.0.2 6379 hunter2"#,
            // Refused in a script, but the line holds the secret anyway.
            r#"eval_ro "redis.call('ACL','SETUSER','bob','on','>hunter2')" 0"#,
            r#"EVAL "return redis.call('auth','hunter2')" 0"#,
            r#"EVAL "return redis.call('HELLO','3','AUTH','app','hunter2')" 0"#,
            r#"EVAL "return redis.call('CONFIG','SET','requirepass','hunter2')" 0"#,
            r#"EVAL "return redis.call('CONFIG','SET','masteruser','replicator')" 0"#,
            // Loaded for later.
            r#"SCRIPT LOAD "return redis.call('MIGRATE','h','1','k','0','1','AUTH','hunter2')""#,
            "FUNCTION LOAD \"#!lua name=lib\\nredis.register_function('f', function() return redis.call('MIGRATE','h','1','k','0','1','AUTH','hunter2') end)\"",
            r##"function load replace "#!lua name=lib\nredis.call('auth', 'pw')""##,
            // Through a dry run, and half typed.
            r#"ACL DRYRUN bob EVAL "return redis.call('AUTH','hunter2')" 0"#,
            r#"EVAL "return redis.call('AUTH','hunt"#,
        ] {
            assert!(line(typed), "{typed}");
        }
        for typed in [
            r#"EVAL "return redis.call('GET', KEYS[1])" 1 k"#,
            // Only the body is read as a script: keys and arguments are not.
            r#"EVAL "return redis.call('GET', KEYS[1])" 1 author:1"#,
            "EVALSHA 6b1bf486c81ceb7edf3c093f4c48582e38c0e791 0",
            r#"SCRIPT LOAD "return 1""#,
            "SCRIPT EXISTS 6b1bf486c81ceb7edf3c093f4c48582e38c0e791",
            "FUNCTION LOAD \"#!lua name=lib\\nredis.register_function('f', function() return 1 end)\"",
            "FUNCTION LIST",
            "EVAL",
        ] {
            assert!(!line(typed), "{typed}");
        }
    }

    #[test]
    fn module_arguments_are_never_kept() {
        for typed in [
            // The same parameter CONFIG SET drops, given at load time.
            "MODULE LOADEX /opt/mymod.so CONFIG mymod.auth-token abc123",
            "module loadex /opt/mymod.so config mymod.token abc123",
            "MODULE LOADEX /opt/mymod.so ARGS token hunter2",
            "MODULE LOAD /m.so hunter2",
        ] {
            assert!(line(typed), "{typed}");
        }
        assert!(line("CONFIG SET mymod.auth-token abc123"));
        for typed in [
            "MODULE LOAD /opt/mymod.so",
            "MODULE LOADEX /opt/mymod.so",
            "MODULE LIST",
            "MODULE UNLOAD mymod",
            "MODULE",
        ] {
            assert!(!line(typed), "{typed}");
        }
    }

    #[test]
    fn sentinel_credentials_are_never_kept() {
        assert!(line("SENTINEL SET mymaster auth-pass s3cret"));
        assert!(line("SENTINEL SET mymaster auth-user app"));
        assert!(line("SENTINEL CONFIG SET sentinel-pass s3cret"));
        assert!(line("SENTINEL CONFIG SET sentinel-user admin"));
        assert!(!line("SENTINEL SET mymaster quorum 2"));
        assert!(!line("SENTINEL GET-MASTER-ADDR-BY-NAME mymaster"));
    }

    #[test]
    fn credential_like_arguments_are_never_kept() {
        for typed in [
            // Unknown or module commands that take credentials.
            "X.CONNECT host AUTH s3cret",
            "MODULE LOADEX /m.so CONFIG password s3cret",
            "FOO.SET client_secret abc",
            "SET cfg redis://app:s3cret@cache:6379/0",
            "SET cfg rediss://:s3cret@cache",
            "SET token_api-key 123",
            "HSET svc api_key 123",
            "HSET user:1 passwd x",
            "HSET \"user:1\" \"Password\" x",
            // Short field names, as common as the long ones.
            "HSET user:1 pwd hunter2",
            "HSET user:1 user_pwd hunter2",
            "HSET user:1 PWD hunter2",
            "HSET user:1 pass hunter2",
            "HSET user:1 Pass hunter2",
            "HSET user:1 pw hunter2",
            "HMSET db host h user u pass hunter2",
        ] {
            assert!(line(typed), "{typed}");
        }
        for typed in [
            "GET user:1",
            "SET url https://example.com/path@v2",
            "SET url https://example.com",
            "SET url https://medium.com/@user",
            "SET url https://example.com/a?q=@x#@y",
            "HGETALL session:42",
            "SET author bob",
            "PING",
            "SCAN 0 MATCH pass* COUNT 100",
            // Only as whole words.
            "SET compass north",
            "SADD gates bypass",
            "HGET user:1 passenger",
            "SET pwm 1",
        ] {
            assert!(!line(typed), "{typed}");
        }
    }

    #[test]
    fn urls_with_any_password_are_never_kept() {
        for typed in [
            // Generated passwords hold `/`, `?` and `#` as they come.
            "SET app:redis_url redis://default:Ab/9xQ@cache:6379",
            "SET app:redis_url redis://default:Ab?9xQ@cache:6379",
            "SET app:redis_url rediss://:Ab#9xQ@cache",
            "SET app:redis_url REDIS://default:Ab/9xQ@cache:6379/2",
            "SET app:redis_url valkey://app:a/b?c@cache",
            // A lone credential in a Redis URL is a password.
            "SET app:redis_url redis://Ab/9xQ@cache",
            "SET app:redis_url redis://s3cret@cache",
            "SET svc https://user:pa/ss@example.com/path",
            "SET svc https://user@example.com/",
        ] {
            assert!(line(typed), "{typed}");
        }
    }

    #[test]
    fn binary_arguments_are_fine() {
        assert!(!is_sensitive(&[&b"SET"[..], b"\xff\x00", b"\x80"]));
        assert!(is_sensitive(&[&b"\x41UTH"[..], b"\xff"]));
        assert!(!is_sensitive::<&[u8]>(&[]));
    }

    /// `word` in any mix of ASCII upper and lower case, as the server
    /// accepts it.
    fn ascii_case(word: &'static str) -> impl Strategy<Value = String> {
        proptest::collection::vec(any::<bool>(), word.len()).prop_map(move |upper| {
            word.chars()
                .zip(upper)
                .map(|(c, upper)| if upper { c.to_ascii_uppercase() } else { c })
                .collect()
        })
    }

    proptest! {
        /// However the password is quoted and whatever it holds, AUTH lines
        /// are caught, as typed or as split arguments.
        #[test]
        fn any_auth_line_is_sensitive(
            name in ascii_case("auth"),
            user in proptest::option::of(proptest::collection::vec(any::<u8>(), 0..10)),
            password in proptest::collection::vec(any::<u8>(), 0..20),
        ) {
            let mut argv = vec![name.into_bytes()];
            argv.extend(user);
            argv.push(password);
            prop_assert!(is_sensitive(&argv));
            let typed: Vec<String> = argv.iter().map(|a| quote_arg(a)).collect();
            let typed = typed.join(" ");
            prop_assert_eq!(split_args(&typed), Ok(argv.clone()));
            prop_assert!(is_sensitive_line(&typed));
        }

        /// A script that logs in, in any case and however it is quoted, is
        /// caught, as typed or as split arguments.
        #[test]
        fn any_script_that_logs_in_is_sensitive(
            command in prop_oneof![ascii_case("eval"), ascii_case("eval_ro"), Just("SCRIPT LOAD".to_string())],
            keyword in prop_oneof![ascii_case("auth"), ascii_case("auth2"), ascii_case("setuser")],
            quote in proptest::sample::select(&["'", "\""][..]),
            before in "[ -~]{0,16}",
            password in "[ -~]{0,16}",
        ) {
            let body = format!("{before}redis.call({quote}{keyword}{quote}, {quote}{password}{quote})");
            let mut argv: Vec<Vec<u8>> = command.split(' ').map(|w| w.as_bytes().to_vec()).collect();
            argv.push(body.into_bytes());
            argv.push(b"0".to_vec());
            prop_assert!(is_sensitive(&argv));
            let typed: Vec<String> = argv.iter().map(|a| quote_arg(a)).collect();
            let typed = typed.join(" ");
            prop_assert!(is_sensitive_line(&typed), "{}", typed);
        }

        /// A URL with a password is caught whatever the password holds, as
        /// redcrab's own URL parser would read it.
        #[test]
        fn any_url_with_a_password_is_sensitive(
            scheme in proptest::sample::select(&["redis", "rediss", "valkey", "https", "amqp"][..])
                .prop_flat_map(ascii_case),
            user in "[a-z]{0,8}",
            password in "[!-~]{1,24}",
            host in "[a-z]{1,8}(:[0-9]{1,5})?(/[a-z0-9]{0,6})?",
        ) {
            let url = format!("{scheme}://{user}:{password}@{host}");
            prop_assert!(is_sensitive(&["SET", "k", url.as_str()]), "{}", url);
            let typed = format!("SET k {}", quote_arg(url.as_bytes()));
            prop_assert!(is_sensitive_line(&typed), "{}", typed);
            if scheme.to_ascii_lowercase().starts_with("redis") {
                // A lone credential there is a password.
                let lone = format!("{scheme}://{password}@{host}");
                prop_assert!(is_sensitive(&["SET", "k", lone.as_str()]), "{}", lone);
            }
        }

        #[test]
        fn any_config_set_requirepass_is_sensitive(
            before in proptest::collection::vec("[a-z-]{1,12} [a-z0-9]{1,6}", 0..3),
            param in proptest::sample::select(&[
                "requirepass",
                "masterauth",
                "masteruser",
                "primaryauth",
                "primaryuser",
                "tls-key-file-pass",
            ][..])
                .prop_flat_map(ascii_case),
            value in "[ -~]{0,16}",
        ) {
            let line = format!(
                "CONFIG SET {} {param} {}",
                before.join(" "),
                quote_arg(value.as_bytes())
            );
            prop_assert!(is_sensitive_line(&line), "{}", line);
        }
    }
}
