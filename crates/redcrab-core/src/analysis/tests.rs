use super::*;

fn analysis() -> Analysis {
    let mut a = Analysis::new(Some(b':'), 1);
    a.add(b"user:1:profile", KeyType::Hash, None, 300);
    a.add(b"user:2:profile", KeyType::Hash, Some(5_000), 200);
    a.add(b"user:2:cart", KeyType::List, None, 1_000);
    a.add(b"session:abc", KeyType::String, Some(60_000), 50);
    a.add(b"counter", KeyType::String, None, 10);
    a
}

fn prefix_names(rows: &[PrefixRow]) -> Vec<Option<String>> {
    rows.iter()
        .map(|row| {
            row.prefix
                .as_ref()
                .map(|p| String::from_utf8_lossy(p).into_owned())
        })
        .collect()
}

#[test]
fn totals_count_keys_bytes_and_those_without_a_ttl() {
    let a = analysis();
    assert_eq!(
        a.measured,
        Totals {
            keys: 5,
            bytes: 1_560
        }
    );
    assert_eq!(
        a.without_ttl,
        Totals {
            keys: 3,
            bytes: 1_310
        }
    );
    assert_eq!(
        a.by_type(),
        [
            (
                KeyType::List,
                Totals {
                    keys: 1,
                    bytes: 1_000
                }
            ),
            (
                KeyType::Hash,
                Totals {
                    keys: 2,
                    bytes: 500
                }
            ),
            (KeyType::String, Totals { keys: 2, bytes: 60 }),
        ]
    );
}

#[test]
fn prefixes_nest_and_sort_by_memory() {
    let a = analysis();
    let top = a.prefixes(b"");
    // `counter` has no prefix: it counts in the totals only.
    assert_eq!(
        prefix_names(&top),
        [Some("user".into()), Some("session".into())]
    );
    assert_eq!(
        top[0].totals,
        Totals {
            keys: 3,
            bytes: 1_500
        }
    );
    assert!(top[0].nested);
    // `session:abc` is a key under `session`, not a prefix.
    assert!(!top[1].nested);
    let users = a.prefixes(b"user");
    assert_eq!(
        prefix_names(&users),
        [Some("user:2".into()), Some("user:1".into())]
    );
    assert_eq!(
        users[0].totals,
        Totals {
            keys: 2,
            bytes: 1_200
        }
    );
    assert!(!users[0].nested);
    assert!(a.prefixes(b"user:9").is_empty());
    assert!(a.prefixes(b"nothing").is_empty());
}

#[test]
fn the_tree_stops_at_its_depth_and_fanout() {
    let mut a = Analysis::new(Some(b':'), 1);
    a.add(b"a:b:c:d:e", KeyType::String, None, 1);
    assert_eq!(prefix_names(&a.prefixes(b"a:b")), [Some("a:b:c".into())]);
    // Three levels deep and no further.
    assert!(!a.prefixes(b"a:b")[0].nested);
    for id in 0..(PREFIX_FANOUT + 10) {
        a.add(format!("user:{id}:x").as_bytes(), KeyType::String, None, 2);
    }
    let users = a.prefixes(b"user");
    assert_eq!(users.len(), PREFIX_FANOUT + 1);
    let others = users.last().unwrap();
    assert_eq!(others.prefix, None);
    assert_eq!(
        others.totals,
        Totals {
            keys: 10,
            bytes: 20
        }
    );
    // The sum under `user` still counts every key.
    assert_eq!(
        a.prefixes(b"")[1].totals.keys + a.prefixes(b"")[0].totals.keys,
        (PREFIX_FANOUT + 11) as u64
    );
}

#[test]
fn the_biggest_keys_are_kept_biggest_first() {
    let mut a = Analysis::new(None, 1);
    for n in 0..(BIGGEST as u64 * 3) {
        a.add(format!("k{n}").as_bytes(), KeyType::String, None, n);
    }
    let biggest = a.biggest();
    assert_eq!(biggest.len(), BIGGEST);
    assert_eq!(biggest[0].bytes, BIGGEST as u64 * 3 - 1);
    assert!(biggest.windows(2).all(|w| w[0].bytes >= w[1].bytes));
    // No separator: no prefix tree.
    assert!(a.prefixes(b"").is_empty());
}

#[test]
fn samples_scale_to_the_whole() {
    let a = Analysis::new(Some(b':'), 10);
    assert!(a.is_sampled());
    let totals = Totals {
        keys: 3,
        bytes: 120,
    };
    assert_eq!(
        totals.scaled(a.every()),
        Totals {
            keys: 30,
            bytes: 1_200
        }
    );
    assert!(!Analysis::new(None, 0).is_sampled());
}
