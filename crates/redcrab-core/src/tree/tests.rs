use std::collections::{BTreeMap, BTreeSet};

use proptest::{collection::vec, prelude::*};

use super::{arena::NAME_CHUNK, *};

fn labels(tree: &mut KeyTree) -> Vec<String> {
    tree.rows()
        .iter()
        .map(|row| {
            let name = String::from_utf8_lossy(tree.name(row.node));
            let mark = match row.kind {
                RowKind::Folder => format!("{name}/ ({})", tree.key_count(row.node)),
                RowKind::Key => name.into_owned(),
            };
            format!("{}{mark}", "  ".repeat(row.depth as usize))
        })
        .collect()
}

#[test]
fn builds_folders() {
    let mut tree = KeyTree::new(Some(b':'));
    for key in [
        "user:2:name",
        "user:1:name",
        "user:1:age",
        "config",
        "user",
        "user:1:name",
    ] {
        tree.insert(key.as_bytes());
    }
    assert_eq!(tree.len(), 5);
    assert_eq!(labels(&mut tree), ["user", "user/ (4)", "config"]);

    let user = tree.find(b"user").unwrap();
    tree.set_expanded(user, true);
    let one = tree.find(b"user:1").unwrap();
    tree.set_expanded(one, true);
    assert_eq!(
        labels(&mut tree),
        [
            "user",
            "user/ (4)",
            "  1/ (2)",
            "    age",
            "    name",
            "  2/ (1)",
            "config"
        ]
    );
    assert_eq!(&*tree.key(tree.find(b"user:1:age").unwrap()), b"user:1:age");
}

#[test]
fn removes_keys() {
    let mut tree = KeyTree::new(Some(b':'));
    tree.insert(b"a:b");
    tree.insert(b"a:c");
    assert!(tree.remove(b"a:b"));
    assert!(!tree.remove(b"a:b"));
    assert!(!tree.remove(b"a"), "a folder is not a key");
    assert!(!tree.remove(b"nope"));
    assert_eq!(tree.len(), 1);
    assert!(tree.remove(b"a:c"));
    assert!(tree.rows().is_empty());
    assert!(tree.is_empty());
}

#[test]
fn keeps_odd_keys_intact() {
    let mut tree = KeyTree::new(Some(b':'));
    let keys = [
        &b"::"[..],
        b"a::b",
        b"trailing:",
        b"\xff\x00:x",
        b"",
        b":lead",
    ];
    for key in keys {
        assert!(tree.insert(key));
        assert_eq!(&*tree.key(tree.find(key).unwrap()), key);
    }
    assert_eq!(tree.len(), keys.len());
    for key in keys {
        assert!(tree.contains(key), "{key:?}");
    }
    assert!(!tree.contains(b":"));
}

#[test]
fn merges_new_keys_into_sorted_folders() {
    let mut tree = KeyTree::new(None);
    for key in ["d", "b"] {
        tree.insert(key.as_bytes());
    }
    assert_eq!(labels(&mut tree), ["b", "d"]);
    for key in ["c", "a", "e"] {
        tree.insert(key.as_bytes());
    }
    assert_eq!(labels(&mut tree), ["a", "b", "c", "d", "e"]);
}

#[test]
fn orders_names_that_share_a_long_prefix() {
    // The sort decides most comparisons on the first eight bytes; these all
    // need the rest of the name, or hinge on zero padding.
    let names: [&[u8]; 10] = [
        b"abcdefgh2",
        b"abcdefgh",
        b"abcdefgh10",
        b"abcdefgh1",
        b"abcdefgh\x00",
        b"a",
        b"a\x00",
        b"a\x00\x00",
        b"\xff\xff\xff\xff\xff\xff\xff\xff\x01",
        b"",
    ];
    let mut tree = KeyTree::new(None);
    for name in names {
        tree.insert(name);
    }
    let mut sorted = names.to_vec();
    sorted.sort();
    let rows: Vec<Vec<u8>> = tree
        .rows()
        .iter()
        .map(|row| tree.key(row.node).to_vec())
        .collect();
    assert_eq!(rows, sorted);
}

#[test]
fn flat_trees_ignore_separators() {
    let mut tree = KeyTree::new(None);
    tree.insert(b"a:b");
    tree.insert(b"a");
    assert_eq!(labels(&mut tree), ["a", "a:b"]);
    assert_eq!(&*tree.key(tree.find(b"a:b").unwrap()), b"a:b");
}

#[test]
fn a_key_whose_folder_empties_moves_back_among_the_keys() {
    let mut tree = KeyTree::new(Some(b':'));
    for key in ["m", "m:x", "a", "z:y"] {
        tree.insert(key.as_bytes());
    }
    assert_eq!(labels(&mut tree), ["m", "m/ (2)", "z/ (1)", "a"]);
    tree.remove(b"m:x");
    assert_eq!(labels(&mut tree), ["z/ (1)", "a", "m"]);
    tree.insert(b"m:x");
    assert_eq!(labels(&mut tree), ["m", "m/ (2)", "z/ (1)", "a"]);
    // A folder whose keys all went away disappears; one that comes back is
    // sorted in again.
    tree.remove(b"z:y");
    assert_eq!(labels(&mut tree), ["m", "m/ (2)", "a"]);
    tree.insert(b"z:y:deeper");
    assert_eq!(labels(&mut tree), ["m", "m/ (2)", "z/ (1)", "a"]);
}

#[test]
fn expands_and_collapses_everything() {
    let mut tree = KeyTree::new(Some(b':'));
    for key in ["a:b:c", "a:d", "e"] {
        tree.insert(key.as_bytes());
    }
    tree.expand_all();
    assert_eq!(
        labels(&mut tree),
        ["a/ (2)", "  b/ (1)", "    c", "  d", "e"]
    );
    tree.collapse_all();
    assert_eq!(labels(&mut tree), ["a/ (2)", "e"]);
    let a = tree.find(b"a").unwrap();
    tree.toggle(a);
    assert!(tree.is_expanded(a));
    assert_eq!(labels(&mut tree), ["a/ (2)", "  b/ (1)", "  d", "e"]);
}

#[test]
fn removes_everything_below_a_folder() {
    let mut tree = KeyTree::new(Some(b':'));
    for key in [
        "user", "user:1", "user:1:a", "user:2:b", "user2:x", "user:", "z",
    ] {
        tree.insert(key.as_bytes());
    }
    tree.expand_all();
    let user = tree.find(b"user").unwrap();
    // `user` itself, `user2:x` (a sibling sharing the first bytes) and `z`
    // stay; `user:` (an empty last segment) is below the folder.
    assert_eq!(tree.remove_below(user), 4);
    assert_eq!(tree.len(), 3);
    assert_eq!(labels(&mut tree), ["user2/ (1)", "  x", "user", "z"]);
    for gone in ["user:1", "user:1:a", "user:2:b", "user:"] {
        assert!(!tree.contains(gone.as_bytes()), "{gone}");
    }
    assert!(tree.contains(b"user"));
    assert!(tree.is_key(user) && !tree.is_folder(user));
    assert_eq!(tree.remove_below(user), 0, "nothing is left below");

    // Keys inserted again show up again, in order.
    tree.insert(b"user:2:b");
    tree.insert(b"user:0");
    tree.expand_all();
    assert_eq!(
        labels(&mut tree),
        [
            "user",
            "user/ (3)",
            "  2/ (1)",
            "    b",
            "  0",
            "user2/ (1)",
            "  x",
            "z"
        ]
    );
}

#[test]
fn removing_below_a_folder_without_a_key_hides_it() {
    let mut tree = KeyTree::new(Some(b':'));
    for key in ["a:b:c", "a:d", "e"] {
        tree.insert(key.as_bytes());
    }
    tree.rows();
    let a = tree.find(b"a").unwrap();
    let b = tree.find(b"a:b").unwrap();
    assert_eq!(tree.remove_below(b), 1);
    tree.expand_all();
    assert_eq!(labels(&mut tree), ["a/ (1)", "  d", "e"]);
    assert_eq!(tree.remove_below(a), 1);
    assert_eq!(labels(&mut tree), ["e"]);
    assert_eq!(tree.len(), 1);
    // A flat tree has no folders: only the key itself is below nothing.
    let mut flat = KeyTree::new(None);
    flat.insert(b"a:b");
    let node = flat.find(b"a:b").unwrap();
    assert_eq!(flat.remove_below(node), 0);
    assert!(flat.contains(b"a:b"));
}

#[test]
fn opens_and_closes_a_subtree() {
    let mut tree = KeyTree::new(Some(b':'));
    for key in ["a:b:c:d", "a:e:f", "g:h"] {
        tree.insert(key.as_bytes());
    }
    let a = tree.find(b"a").unwrap();
    tree.set_expanded_below(a, true);
    assert_eq!(
        labels(&mut tree),
        [
            "a/ (2)",
            "  b/ (1)",
            "    c/ (1)",
            "      d",
            "  e/ (1)",
            "    f",
            "g/ (1)"
        ]
    );
    let b = tree.find(b"a:b").unwrap();
    tree.set_expanded_below(b, false);
    assert_eq!(
        labels(&mut tree),
        ["a/ (2)", "  b/ (1)", "  e/ (1)", "    f", "g/ (1)"]
    );
    // Closing below closes the inner folders too, so opening `b` again
    // shows one level.
    tree.toggle(b);
    assert_eq!(
        labels(&mut tree),
        [
            "a/ (2)",
            "  b/ (1)",
            "    c/ (1)",
            "  e/ (1)",
            "    f",
            "g/ (1)"
        ]
    );
    assert!(tree.is_under(tree.find(b"a:b:c:d").unwrap(), a));
    assert!(tree.is_under(a, a));
    assert!(!tree.is_under(a, b));
    assert!(!tree.is_under(tree.find(b"g:h").unwrap(), a));
}

#[test]
fn reuses_the_row_buffer() {
    let mut tree = KeyTree::new(Some(b':'));
    tree.insert(b"a");
    let mut rows = vec![
        Row {
            node: 7,
            depth: 3,
            kind: RowKind::Folder,
        };
        10
    ];
    tree.rows_into(&mut rows);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, RowKind::Key);
}

#[test]
fn deep_keys_do_not_overflow_the_stack() {
    let mut tree = KeyTree::new(Some(b':'));
    let key = vec![b':'; 200_000];
    assert!(tree.insert(&key));
    assert!(tree.insert(b"x"));
    tree.expand_all();
    let rows = tree.rows();
    // 200,000 folders down to the key, plus `x`.
    assert_eq!(rows.len(), 200_002);
    assert_eq!(rows.iter().map(|r| r.depth).max(), Some(u16::MAX));
    let leaf = rows.iter().find(|r| r.kind == RowKind::Key).unwrap();
    assert_eq!(&*tree.key(leaf.node), &key[..]);
    assert_eq!(tree.key_count(tree.find(b"").unwrap()), 1);
}

#[test]
fn names_longer_than_a_chunk_get_their_own() {
    let mut tree = KeyTree::new(Some(b':'));
    let big = vec![b'x'; NAME_CHUNK + 17];
    let exact = vec![b'y'; NAME_CHUNK];
    let mut keys: Vec<Vec<u8>> = vec![b"a".to_vec(), big.clone(), b"".to_vec()];
    keys.push([&b"f:"[..], &exact].concat());
    keys.push(b"after".to_vec());
    keys.push([&big[..], b":child"].concat());
    for key in &keys {
        assert!(tree.insert(key));
    }
    for key in &keys {
        let node = tree.find(key).unwrap();
        assert_eq!(&*tree.key(node), &key[..]);
        assert!(tree.contains(key));
    }
    assert_eq!(tree.name(tree.find(&big).unwrap()).len(), big.len());
}

#[test]
fn spans_many_chunks() {
    // Enough keys for several node chunks and name chunks.
    let mut tree = KeyTree::new(Some(b':'));
    let key = |i: u32| format!("k:{}:{:0>40}", i % 7, i).into_bytes();
    for i in 0..60_000 {
        assert!(tree.insert(&key(i)));
    }
    assert!(!tree.insert(&key(123)));
    assert_eq!(tree.len(), 60_000);
    for i in (0..60_000).step_by(997) {
        let node = tree.find(&key(i)).unwrap();
        assert_eq!(&*tree.key(node), &key(i)[..]);
    }
    assert_eq!(tree.key_count(tree.find(b"k:3").unwrap()), 60_000 / 7);
    let heap = tree.heap_bytes();
    assert!(heap > 60_000 * 12 && heap < 60_000 * 200, "{heap}");
}

/// The folder of the node at `key`.
fn folder_of(tree: &KeyTree, key: &[u8]) -> FolderId {
    tree.nodes[tree.find(key).unwrap()].folder
}

/// A pseudo-random permutation of `0..n` (xorshift, fixed seed), the way
/// SCAN returns keys: in no particular order.
fn shuffled(n: u32) -> Vec<u32> {
    let mut order: Vec<u32> = (0..n).collect();
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for i in (1..order.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        order.swap(i, (state % (i as u64 + 1)) as usize);
    }
    order
}

#[test]
fn merges_names_with_long_shared_prefixes_across_flattens() {
    // Zero-padded ids share their first eight bytes, so every comparison
    // is decided past the head, in full sorts and in merges alike.
    let mut tree = KeyTree::new(Some(b':'));
    let name = |i: u32| format!("item_{i:012}");
    let order = shuffled(3000);
    for batch in order.chunks(700) {
        for &i in batch {
            tree.insert(name(i).as_bytes());
        }
        tree.rows();
    }
    let rows: Vec<String> = tree
        .rows()
        .iter()
        .map(|row| String::from_utf8_lossy(&tree.key(row.node)).into_owned())
        .collect();
    let expected: Vec<String> = (0..3000).map(name).collect();
    assert_eq!(rows, expected);
}

#[test]
fn removing_a_subfolders_last_key_needs_no_resort() {
    let mut tree = KeyTree::new(Some(b':'));
    for i in 0..200 {
        tree.insert(format!("s:{i:03}:v").as_bytes());
    }
    tree.insert(b"s:key");
    let s = tree.find(b"s").unwrap();
    tree.set_expanded(s, true);
    tree.rows();
    let f = folder_of(&tree, b"s");
    assert_eq!(
        tree.folders[f].sorted_len as usize,
        tree.folders[f].children.len()
    );

    // `s:100` goes from a folder to nothing at all: no row moves.
    assert!(tree.remove(b"s:100:v"));
    assert!(!tree.folders[f].stale);
    let labels_now = labels(&mut tree);
    assert!(
        !labels_now.iter().any(|l| l.contains("100")),
        "{labels_now:?}"
    );
    assert_eq!(labels_now.len(), 1 + 199 + 1);
    assert_eq!(labels_now.last().map(String::as_str), Some("  key"));

    // While a scan adds new subfolders, the sorted ones stay put.
    tree.insert(b"s:500:v");
    tree.insert(b"s:050x:v");
    assert!(!tree.folders[f].stale);

    // Showing again as the other kind (a key now) has to move it.
    assert!(tree.insert(b"s:100"));
    assert!(tree.folders[f].stale);
    let rows = labels(&mut tree);
    assert!(!tree.folders[f].stale);
    assert_eq!(rows[0], "s/ (203)");
    let keys: Vec<&str> = rows
        .iter()
        .filter(|l| !l.ends_with(')'))
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["  100", "  key"]);
    // The folders around it are still in order, the new ones merged in.
    let folders: Vec<&str> = rows[1..]
        .iter()
        .filter(|l| l.ends_with(')'))
        .map(String::as_str)
        .collect();
    let mut sorted = folders.clone();
    sorted.sort();
    assert_eq!(folders, sorted);
    assert!(folders.contains(&"  050x/ (1)") && folders.contains(&"  500/ (1)"));
}

#[test]
fn a_key_that_becomes_a_folder_moves_among_the_folders() {
    let mut tree = KeyTree::new(Some(b':'));
    for key in ["b", "d", "a:x", "c"] {
        tree.insert(key.as_bytes());
    }
    assert_eq!(labels(&mut tree), ["a/ (1)", "b", "c", "d"]);
    // `c` gains a key below it, and a new folder arrives in the same batch.
    tree.insert(b"c:y");
    tree.insert(b"e:z");
    assert!(tree.folders[ROOT_FOLDER].stale);
    assert_eq!(
        labels(&mut tree),
        ["a/ (1)", "c", "c/ (2)", "e/ (1)", "b", "d"]
    );
    // And back.
    tree.remove(b"c:y");
    assert_eq!(labels(&mut tree), ["a/ (1)", "e/ (1)", "b", "c", "d"]);
}

#[test]
fn reflattening_while_keys_stream_in_reuses_one_sort_buffer() {
    let mut tree = KeyTree::new(None);
    let order = shuffled(20_000);
    let (first, rest) = order.split_at(10_000);
    for &i in first {
        tree.insert(format!("k{i}").as_bytes());
    }
    tree.rows();
    let capacity = tree.scratch.capacity();
    assert!(capacity >= 10_000, "{capacity}");
    // Later flattens only sort what is new, in the buffer the first one made.
    for batch in rest.chunks(500) {
        for &i in batch {
            tree.insert(format!("k{i}").as_bytes());
        }
        tree.rows();
        assert_eq!(tree.scratch.capacity(), capacity);
    }
    let rows = tree.rows();
    assert_eq!(rows.len(), 20_000);
    let names: Vec<Vec<u8>> = rows.iter().map(|r| tree.key(r.node).to_vec()).collect();
    assert!(names.is_sorted());
}

#[test]
fn index_entries_rehash_without_names() {
    let hash = 0x0123_4567_89ab_cdef_u64;
    let entry = entry(hash, 42);
    assert_eq!(entry as NodeId, 42);
    assert_eq!(table_hash(entry), table_hash(hash));
    assert_eq!(table_hash(hash), 0x0123_4567_0123_4567);
    // Enough keys for the index to grow many times; every one is found.
    let mut tree = KeyTree::new(Some(b':'));
    for i in shuffled(100_000) {
        assert!(tree.insert(format!("g:{}:{i}", i % 13).as_bytes()));
    }
    for i in (0..100_000).step_by(311) {
        let key = format!("g:{}:{i}", i % 13);
        assert!(tree.contains(key.as_bytes()), "{key}");
        assert!(!tree.insert(key.as_bytes()));
    }
    assert!(!tree.contains(b"g:0:100000"));
}

#[test]
fn reports_parents_and_kinds() {
    let mut tree = KeyTree::new(Some(b':'));
    tree.insert(b"a:b:c");
    tree.insert(b"a");
    let a = tree.find(b"a").unwrap();
    let b = tree.find(b"a:b").unwrap();
    let c = tree.find(b"a:b:c").unwrap();
    assert_eq!(tree.parent(a), None);
    assert_eq!(tree.parent(b), Some(a));
    assert_eq!(tree.parent(c), Some(b));
    assert!(tree.is_key(a) && tree.is_folder(a));
    assert!(!tree.is_key(b) && tree.is_folder(b));
    assert!(tree.is_key(c) && !tree.is_folder(c));
}

/// The rows a fully expanded tree must show for `keys`, built independently
/// from a sorted map: (depth, kind, full key or folder prefix, key count).
fn expected_rows(keys: &BTreeSet<Vec<u8>>, sep: Option<u8>) -> Vec<(u16, RowKind, Vec<u8>, u32)> {
    #[derive(Default)]
    struct Trie {
        is_key: bool,
        children: BTreeMap<Vec<u8>, Trie>,
    }
    impl Trie {
        fn count(&self) -> u32 {
            self.is_key as u32 + self.children.values().map(Trie::count).sum::<u32>()
        }
    }
    fn walk(
        trie: &Trie,
        prefix: &[u8],
        sep: Option<u8>,
        depth: u16,
        out: &mut Vec<(u16, RowKind, Vec<u8>, u32)>,
    ) {
        // Folders first, then keys; the map already iterates in byte order.
        let (folders, leaves): (Vec<_>, Vec<_>) = trie
            .children
            .iter()
            .partition(|(_, child)| !child.children.is_empty());
        for (name, child) in folders.into_iter().chain(leaves) {
            let mut path = prefix.to_vec();
            if depth > 0 {
                path.extend(sep);
            }
            path.extend_from_slice(name);
            if child.is_key {
                out.push((depth, RowKind::Key, path.clone(), 1));
            }
            if !child.children.is_empty() {
                out.push((depth, RowKind::Folder, path.clone(), child.count()));
                walk(child, &path, sep, depth + 1, out);
            }
        }
    }
    let mut root = Trie::default();
    for key in keys {
        let segments: Vec<&[u8]> = match sep {
            Some(sep) => key.split(|b| *b == sep).collect(),
            None => vec![key],
        };
        let mut node = &mut root;
        for segment in segments {
            node = node.children.entry(segment.to_vec()).or_default();
        }
        node.is_key = true;
    }
    let mut out = Vec::new();
    walk(&root, b"", sep, 0, &mut out);
    out
}

fn actual_rows(tree: &mut KeyTree) -> Vec<(u16, RowKind, Vec<u8>, u32)> {
    tree.expand_all();
    tree.rows()
        .into_iter()
        .map(|row| {
            let count = match row.kind {
                RowKind::Key => 1,
                RowKind::Folder => tree.key_count(row.node),
            };
            (row.depth, row.kind, tree.key(row.node).to_vec(), count)
        })
        .collect()
}

#[derive(Clone, Debug)]
enum Op {
    Insert(Vec<u8>),
    Remove(Vec<u8>),
    /// Opens or closes the folder at a key, if there is one.
    Toggle(Vec<u8>),
    /// Removes every key below the node at a key, as a folder delete does.
    RemoveBelow(Vec<u8>),
    /// Opens (or closes) the node at a key and everything below it.
    ExpandBelow(Vec<u8>, bool),
    /// Flattens as the folders are, so that only some get sorted.
    Flatten,
    /// Flatten with every folder open and compare with the model, which also
    /// exercises the incremental sort between mutations.
    Check,
}

/// Short keys over a tiny alphabet, so that inserts, removes and shared
/// prefixes collide often: empty segments, separators at either end, and
/// bytes that are not UTF-8 all come up.
fn key() -> impl Strategy<Value = Vec<u8>> {
    vec(
        prop_oneof![
            4 => Just(b':'),
            3 => Just(b'a'),
            2 => Just(b'b'),
            1 => Just(0xff),
            1 => Just(0x00),
        ],
        0..14,
    )
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => key().prop_map(Op::Insert),
        3 => key().prop_map(Op::Remove),
        1 => key().prop_map(Op::Toggle),
        1 => key().prop_map(Op::RemoveBelow),
        1 => (key(), any::<bool>()).prop_map(|(key, open)| Op::ExpandBelow(key, open)),
        1 => Just(Op::Flatten),
        1 => Just(Op::Check),
    ]
}

fn run_model(ops: Vec<Op>, sep: Option<u8>) -> Result<(), TestCaseError> {
    let mut tree = KeyTree::new(sep);
    let mut model = BTreeSet::new();
    for op in ops {
        match op {
            Op::Insert(key) => prop_assert_eq!(tree.insert(&key), model.insert(key)),
            Op::Remove(key) => prop_assert_eq!(tree.remove(&key), model.remove(&key)),
            Op::Toggle(key) => {
                if let Some(node) = tree.find(&key) {
                    tree.toggle(node);
                }
            }
            Op::RemoveBelow(prefix) => {
                // The keys below a node are those that start with its key
                // and a separator; a flat tree has nothing below a node.
                let removed = tree.find(&prefix).map_or(0, |node| tree.remove_below(node));
                let below: Vec<Vec<u8>> = match sep {
                    Some(sep) => {
                        let mut start = prefix.clone();
                        start.push(sep);
                        model
                            .iter()
                            .filter(|key| key.starts_with(&start))
                            .cloned()
                            .collect()
                    }
                    None => Vec::new(),
                };
                prop_assert_eq!(removed as usize, below.len());
                for key in &below {
                    model.remove(key);
                    prop_assert!(!tree.contains(key));
                }
            }
            Op::ExpandBelow(key, open) => {
                if let Some(node) = tree.find(&key) {
                    tree.set_expanded_below(node, open);
                }
            }
            Op::Flatten => {
                tree.rows();
            }
            Op::Check => prop_assert_eq!(actual_rows(&mut tree), expected_rows(&model, sep)),
        }
        prop_assert_eq!(tree.len(), model.len());
    }
    prop_assert_eq!(actual_rows(&mut tree), expected_rows(&model, sep));
    for key in &model {
        prop_assert!(tree.contains(key));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn matches_a_sorted_set_model(ops in vec(op(), 0..120)) {
        run_model(ops, Some(b':'))?;
    }

    #[test]
    fn flat_tree_matches_a_sorted_set_model(ops in vec(op(), 0..80)) {
        run_model(ops, None)?;
    }

    /// Segments that mostly share their first eight bytes, so the sort
    /// compares past the heads (and on zero padding) all the time.
    #[test]
    fn long_shared_prefixes_match_a_sorted_set_model(
        ops in vec(op().prop_map(|op| match op {
            Op::Insert(key) => Op::Insert(shared_prefix(key)),
            Op::Remove(key) => Op::Remove(shared_prefix(key)),
            Op::Toggle(key) => Op::Toggle(shared_prefix(key)),
            Op::RemoveBelow(key) => Op::RemoveBelow(shared_prefix(key)),
            Op::ExpandBelow(key, open) => Op::ExpandBelow(shared_prefix(key), open),
            other => other,
        }), 0..100)
    ) {
        run_model(ops, Some(b':'))?;
    }
}

/// `key` with each segment after the first given a common 7-byte start.
fn shared_prefix(key: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, segment) in key.split(|b| *b == b':').enumerate() {
        if i > 0 {
            out.push(b':');
            out.extend_from_slice(b"prefix_");
        }
        out.extend_from_slice(segment);
    }
    out
}
