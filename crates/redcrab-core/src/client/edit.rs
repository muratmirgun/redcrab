//! Changes to values: fields, elements and members of collections, whole
//! string and JSON values, and new keys.
//!
//! Each change is one Lua script on one key (or, for a string saved by a
//! user who may not run scripts, one SET with the same checks), so it is
//! atomic and checks what it relies on in the same step it acts: that the
//! key still exists and holds the expected type (a key deleted or replaced
//! since it was loaded is never recreated or overwritten by accident), that
//! a list element still is what was loaded, that a new name is free. One
//! key per script also means every change routes by its key's slot in a
//! cluster.
//!
//! What a script refuses comes back as an [`EditError`], which front ends
//! can tell apart with `err.downcast_ref::<EditError>()` (to offer to
//! overwrite, say).

use std::{
    collections::hash_map::RandomState,
    fmt,
    hash::{BuildHasher as _, Hasher as _},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail, ensure};
use redis::{Cmd, FromRedisValue, RedisError, cmd};

use super::{Client, Conn, Key};
use crate::{formats::json, runtime::spawn};

/// Why a change was refused, when it was the data rather than the server
/// that stood in the way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// The key is gone (deleted or expired since it was loaded).
    KeyMissing,
    /// A key with the new name exists already.
    KeyExists,
    /// The key holds another type now (the name Redis's `TYPE` gives).
    WrongType(String),
    /// The hash field, or the set or sorted set member, to change is gone.
    ElementMissing,
    /// The new hash field, set member or sorted set member exists already.
    /// Sending the change again with `overwrite` replaces it.
    ElementExists,
    /// A list element is no longer what was loaded: the list changed in
    /// the meantime. Nothing was written.
    Changed,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyMissing => f.write_str("the key no longer exists"),
            Self::KeyExists => f.write_str("a key with that name already exists"),
            Self::WrongType(ty) => write!(f, "the key now holds a value of type {ty}"),
            Self::ElementMissing => f.write_str("it no longer exists; reload to see the current value"),
            Self::ElementExists => f.write_str("it already exists"),
            Self::Changed => f.write_str(
                "the list changed since it was loaded, so nothing was written; reload and try again",
            ),
        }
    }
}

impl std::error::Error for EditError {}

/// Where [`Client::list_push`] adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListEnd {
    Head,
    Tail,
}

/// What a delete of elements did, read in the same step: how many it
/// removed, and how many the value holds after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Removed {
    pub count: u64,
    /// Elements left. At 0 a hash, list, set or sorted set is gone, as
    /// Redis deletes them once empty; a stream stays, empty.
    pub left: u64,
}

/// A whole value for [`Client::create_key`].
#[derive(Clone, Debug, PartialEq)]
pub enum NewValue {
    String(Vec<u8>),
    /// Field, value pairs.
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    /// Elements, head first.
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    /// Member, score pairs.
    ZSet(Vec<(Vec<u8>, f64)>),
    /// One entry: its ID (`*` for a new one) and field, value pairs.
    Stream {
        id: String,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
    },
    /// A JSON document, for the JSON module (`JSON.SET`).
    Json(String),
}

impl NewValue {
    /// The name Redis's `TYPE` reports for it.
    fn type_name(&self) -> &'static str {
        match self {
            Self::String(_) => "string",
            Self::Hash(_) => "hash",
            Self::List(_) => "list",
            Self::Set(_) => "set",
            Self::ZSet(_) => "zset",
            Self::Stream { .. } => "stream",
            Self::Json(_) => "json",
        }
    }
}

/// The most fields a stream entry may have here: the script passes them
/// to XADD in one call, and Lua's `unpack` stops at about 8,000 values.
pub const MAX_STREAM_FIELDS: usize = 3_000;

// Every script starts by checking the key's type. Lua sees TYPE's status
// reply as a table with an `ok` field.
macro_rules! expect_type {
    ($ty:literal) => {
        concat!(
            "local t = redis.call('TYPE', KEYS[1]).ok\n",
            "if t == 'none' then return redis.error_reply('REDCRAB_NOKEY no such key') end\n",
            "if t ~= '",
            $ty,
            "' then return redis.error_reply('REDCRAB_TYPE ' .. t) end\n",
        )
    };
}

/// Calls `command` with `ARGV[first..]` in chunks of 1,000 (an even
/// number, so pairs stay together), adding up the replies; `unpack` has a
/// limit.
const CHUNKED: &str = r"
local function chunked(command, first)
  local n = 0
  for i = first, #ARGV, 1000 do
    n = n + redis.call(command, KEYS[1], unpack(ARGV, i, math.min(i + 999, #ARGV)))
  end
  return n
end
";

/// Hash fields can expire on their own (Redis 7.4+), and HSET clears that.
/// These keep a field's TTL across a change; on older servers HPTTL is an
/// unknown command, and fields have no TTL to keep.
const FIELD_TTL: &str = r"
local function field_ttl(field)
  local r = redis.pcall('HPTTL', KEYS[1], 'FIELDS', 1, field)
  if type(r) == 'table' and r.err == nil and type(r[1]) == 'number' and r[1] > 0 then
    return r[1]
  end
  return nil
end
local function keep_ttl(field, ttl)
  if ttl then redis.call('HPEXPIRE', KEYS[1], ttl, 'FIELDS', 1, field) end
end
";

const SET_STRING: &str = concat!(
    expect_type!("string"),
    "redis.call('SET', KEYS[1], ARGV[1], 'KEEPTTL')\nreturn 1\n"
);

// JSON.SET on the root keeps the key's TTL.
const SET_JSON: &str = concat!(
    expect_type!("ReJSON-RL"),
    "redis.call('JSON.SET', KEYS[1], '$', ARGV[1])\nreturn 1\n"
);

// ARGV: field, value, overwrite. Returns 1 if the field is new.
const HASH_SET: &str = concat!(
    expect_type!("hash"),
    r"
local exists = redis.call('HEXISTS', KEYS[1], ARGV[1]) == 1
if exists and ARGV[3] ~= '1' then return redis.error_reply('REDCRAB_EXISTS already exists') end
local ttl = field_ttl(ARGV[1])
redis.call('HSET', KEYS[1], ARGV[1], ARGV[2])
keep_ttl(ARGV[1], ttl)
if exists then return 0 end
return 1
"
);

// The deletes return how many elements went and how many are left (see
// `Removed`), so a view can tell an emptied key from one that others
// added to meanwhile.

// ARGV: fields.
const HASH_DELETE: &str = concat!(
    expect_type!("hash"),
    "local n = chunked('HDEL', 1)\nreturn {n, redis.call('HLEN', KEYS[1])}\n"
);

// ARGV: from, to, value, has value, overwrite. The new field is written
// before the old one goes, so the key never empties (which would drop it
// and its TTL).
const HASH_RENAME: &str = concat!(
    expect_type!("hash"),
    r"
if redis.call('HEXISTS', KEYS[1], ARGV[1]) == 0 then return redis.error_reply('REDCRAB_GONE no such element') end
local value = ARGV[3]
if ARGV[4] ~= '1' then value = redis.call('HGET', KEYS[1], ARGV[1]) end
local renamed = ARGV[1] ~= ARGV[2]
if renamed and redis.call('HEXISTS', KEYS[1], ARGV[2]) == 1 and ARGV[5] ~= '1' then
  return redis.error_reply('REDCRAB_EXISTS already exists')
end
local ttl = field_ttl(ARGV[1])
redis.call('HSET', KEYS[1], ARGV[2], value)
keep_ttl(ARGV[2], ttl)
if renamed then redis.call('HDEL', KEYS[1], ARGV[1]) end
return 1
"
);

// ARGV: LPUSH or RPUSH, elements. Returns the new length.
const LIST_PUSH: &str = concat!(
    expect_type!("list"),
    r"
local n = 0
for i = 2, #ARGV, 1000 do
  n = redis.call(ARGV[1], KEYS[1], unpack(ARGV, i, math.min(i + 999, #ARGV)))
end
return n
"
);

// ARGV: index, expected element, new element.
const LIST_SET: &str = concat!(
    expect_type!("list"),
    r"
if redis.call('LINDEX', KEYS[1], ARGV[1]) ~= ARGV[2] then
  return redis.error_reply('REDCRAB_CHANGED ' .. ARGV[1])
end
redis.call('LSET', KEYS[1], ARGV[1], ARGV[3])
return 1
"
);

// ARGV: a tombstone, then index, expected element pairs. Every element is
// checked before any is touched; then each becomes the tombstone, and one
// LREM takes them all out.
const LIST_REMOVE_AT: &str = concat!(
    expect_type!("list"),
    r"
for i = 2, #ARGV, 2 do
  if redis.call('LINDEX', KEYS[1], ARGV[i]) ~= ARGV[i + 1] then
    return redis.error_reply('REDCRAB_CHANGED ' .. ARGV[i])
  end
end
for i = 2, #ARGV, 2 do
  redis.call('LSET', KEYS[1], ARGV[i], ARGV[1])
end
local n = redis.call('LREM', KEYS[1], 0, ARGV[1])
return {n, redis.call('LLEN', KEYS[1])}
"
);

// ARGV: element. Removes its first occurrence from the head.
const LIST_REMOVE: &str = concat!(
    expect_type!("list"),
    "return redis.call('LREM', KEYS[1], 1, ARGV[1])\n"
);

const SET_ADD: &str = concat!(expect_type!("set"), "return chunked('SADD', 1)\n");
const SET_REMOVE: &str = concat!(
    expect_type!("set"),
    "local n = chunked('SREM', 1)\nreturn {n, redis.call('SCARD', KEYS[1])}\n"
);

// ARGV: from, to, overwrite. Adds before removing, so the set never
// empties. Returns 1 if `to` is new, 0 if it was a member already.
const SET_REPLACE: &str = concat!(
    expect_type!("set"),
    r"
if redis.call('SISMEMBER', KEYS[1], ARGV[1]) == 0 then return redis.error_reply('REDCRAB_GONE no such element') end
if ARGV[1] == ARGV[2] then return 0 end
local exists = redis.call('SISMEMBER', KEYS[1], ARGV[2]) == 1
if exists and ARGV[3] ~= '1' then return redis.error_reply('REDCRAB_EXISTS already exists') end
redis.call('SADD', KEYS[1], ARGV[2])
redis.call('SREM', KEYS[1], ARGV[1])
if exists then return 0 end
return 1
"
);

// ARGV: score, member, overwrite. Returns 1 if the member is new.
const ZSET_ADD: &str = concat!(
    expect_type!("zset"),
    r"
local exists = redis.call('ZSCORE', KEYS[1], ARGV[2])
if exists and ARGV[3] ~= '1' then return redis.error_reply('REDCRAB_EXISTS already exists') end
redis.call('ZADD', KEYS[1], ARGV[1], ARGV[2])
if exists then return 0 end
return 1
"
);

// ARGV: from, to, score, has score, overwrite. ZSCORE gives the score as
// text with every digit, so a kept score is exactly the old one.
const ZSET_RENAME: &str = concat!(
    expect_type!("zset"),
    r"
local score = redis.call('ZSCORE', KEYS[1], ARGV[1])
if not score then return redis.error_reply('REDCRAB_GONE no such element') end
if ARGV[4] == '1' then score = ARGV[3] end
if ARGV[1] ~= ARGV[2] then
  if redis.call('ZSCORE', KEYS[1], ARGV[2]) and ARGV[5] ~= '1' then
    return redis.error_reply('REDCRAB_EXISTS already exists')
  end
  redis.call('ZADD', KEYS[1], score, ARGV[2])
  redis.call('ZREM', KEYS[1], ARGV[1])
else
  redis.call('ZADD', KEYS[1], score, ARGV[1])
end
return 1
"
);

const ZSET_REMOVE: &str = concat!(
    expect_type!("zset"),
    "local n = chunked('ZREM', 1)\nreturn {n, redis.call('ZCARD', KEYS[1])}\n"
);

// ARGV: ID, field, value, ... Returns the entry's ID.
const STREAM_ADD: &str = concat!(
    expect_type!("stream"),
    "return redis.call('XADD', KEYS[1], unpack(ARGV))\n"
);

// XDEL never deletes the stream, however few entries are left.
const STREAM_DELETE: &str = concat!(
    expect_type!("stream"),
    "local n = chunked('XDEL', 1)\nreturn {n, redis.call('XLEN', KEYS[1])}\n"
);

// ARGV: type, TTL in seconds or '', then the value's arguments. Refuses an
// existing key. The key does not exist when the writes start, so if one
// fails (say, out of memory halfway through a big hash) deleting it
// restores the state before: a half-created key cannot remain.
const CREATE: &str = r"
if redis.call('EXISTS', KEYS[1]) == 1 then return redis.error_reply('REDCRAB_EXISTS already exists') end
local function undo(r)
  if type(r) == 'table' and r.err then
    redis.call('DEL', KEYS[1])
    return r
  end
  return nil
end
local function chunked(command)
  for i = 3, #ARGV, 1000 do
    local failed = undo(redis.pcall(command, KEYS[1], unpack(ARGV, i, math.min(i + 999, #ARGV))))
    if failed then return failed end
  end
  return nil
end
local kind = ARGV[1]
local failed
if kind == 'string' then
  failed = undo(redis.pcall('SET', KEYS[1], ARGV[3]))
elseif kind == 'hash' then
  failed = chunked('HSET')
elseif kind == 'list' then
  failed = chunked('RPUSH')
elseif kind == 'set' then
  failed = chunked('SADD')
elseif kind == 'zset' then
  failed = chunked('ZADD')
elseif kind == 'stream' then
  failed = undo(redis.pcall('XADD', KEYS[1], unpack(ARGV, 3)))
elseif kind == 'json' then
  failed = undo(redis.pcall('JSON.SET', KEYS[1], '$', ARGV[3]))
else
  return redis.error_reply('ERR unknown type ' .. kind)
end
if failed then return failed end
if ARGV[2] ~= '' then
  failed = undo(redis.pcall('EXPIRE', KEYS[1], ARGV[2]))
  if failed then return failed end
end
return 1
";

/// `EVAL script 1 key` with the arguments added by the caller.
fn script(source: &str, key: &[u8]) -> Cmd {
    let mut eval = cmd("EVAL");
    eval.arg(source).arg(1).arg(key);
    eval
}

fn script_with(prelude: &str, body: &str, key: &[u8]) -> Cmd {
    let mut source = String::with_capacity(prelude.len() + body.len());
    source.push_str(prelude);
    source.push_str(body);
    script(&source, key)
}

/// Runs a script on the I/O runtime, turning its refusals into
/// [`EditError`]s.
async fn run<T: FromRedisValue>(eval: Cmd, con: &mut Conn) -> Result<T> {
    eval.query_async(con).await.map_err(script_error)
}

/// Whether the server would not run a script at all: the user's ACL lacks
/// @scripting (NOPERM), or EVAL was disabled or renamed away.
fn scripting_refused(err: &RedisError) -> bool {
    err.code() == Some("NOPERM")
        || (err.code() == Some("ERR")
            && err
                .detail()
                .is_some_and(|d| d.to_ascii_lowercase().starts_with("unknown command")))
}

/// [`SET_STRING`] without a script, in one command all the same: `XX`
/// refuses a key that is gone, and `GET` makes SET refuse a key that is
/// no longer a string (it answers the old value, nil if there was none).
async fn set_string_plain(key: &[u8], value: Vec<u8>, con: &mut Conn) -> Result<()> {
    let old: Result<Option<redis::Value>, RedisError> = cmd("SET")
        .arg(key)
        .arg(value)
        .arg("XX")
        .arg("KEEPTTL")
        .arg("GET")
        .query_async(con)
        .await;
    match old {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(EditError::KeyMissing.into()),
        Err(err) if err.code() == Some("WRONGTYPE") => {
            let ty: String = cmd("TYPE")
                .arg(key)
                .query_async(con)
                .await
                .unwrap_or_else(|_| "unknown".into());
            Err(EditError::WrongType(ty).into())
        }
        Err(err) => Err(err.into()),
    }
}

/// Runs a delete script, which answers `{removed, left}`.
async fn removed(eval: Cmd, con: &mut Conn) -> Result<Removed> {
    let (count, left) = run(eval, con).await?;
    Ok(Removed { count, left })
}

/// A script's refusal (`REDCRAB_<WHAT> <detail>`) as an [`EditError`];
/// any other error as it is.
pub(crate) fn script_error(err: RedisError) -> anyhow::Error {
    // Redis 7+ puts `ERR ` before an error reply that has no space in it;
    // the scripts always add a detail, but read either form.
    let (code, detail) = match (err.code(), err.detail()) {
        (Some("ERR"), Some(detail)) => {
            let mut words = detail.splitn(2, ' ');
            (words.next(), words.next())
        }
        other => other,
    };
    let edit = match code {
        Some("REDCRAB_NOKEY") => EditError::KeyMissing,
        Some("REDCRAB_EXISTS") => EditError::ElementExists,
        Some("REDCRAB_GONE") => EditError::ElementMissing,
        Some("REDCRAB_CHANGED") => EditError::Changed,
        Some("REDCRAB_TYPE") => EditError::WrongType(detail.unwrap_or("unknown").to_string()),
        _ => return err.into(),
    };
    edit.into()
}

fn flag(on: bool) -> &'static str {
    if on { "1" } else { "0" }
}

/// A score as Redis reads it back exactly: `inf`, `-inf`, or the shortest
/// decimal that round-trips.
pub(crate) fn score_arg(score: f64) -> Result<String> {
    ensure!(!score.is_nan(), "a score must be a number, not NaN");
    Ok(if score == f64::INFINITY {
        "+inf".into()
    } else if score == f64::NEG_INFINITY {
        "-inf".into()
    } else {
        // Both forms have the fewest digits that read back as the same
        // number; very big and very small numbers are shorter with an
        // exponent (`1e300` rather than 301 digits).
        let plain = score.to_string();
        if plain.len() <= 24 {
            plain
        } else {
            format!("{score:e}")
        }
    })
}

/// Checks the shape of a stream entry ID for XADD: `*`, `<ms>`,
/// `<ms>-<seq>` or `<ms>-*`. Whether it is past the stream's last entry is
/// for the server to say.
pub fn check_stream_id(id: &str) -> Result<()> {
    if id == "*" {
        return Ok(());
    }
    let (ms, seq) = id.split_once('-').unwrap_or((id, "0"));
    let number = |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit());
    ensure!(
        number(ms)
            && ms.parse::<u64>().is_ok()
            && (seq == "*" || (number(seq) && seq.parse::<u64>().is_ok())),
        "{id:?} is not a stream entry ID: use * for a new one, or <milliseconds>-<sequence> such as 1700000000000-0"
    );
    Ok(())
}

/// A value no list element is going to be: LSET puts it where elements
/// are to be removed, then one LREM takes them out.
fn tombstone() -> Vec<u8> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    hasher.write_u128(nanos);
    let a = hasher.finish();
    let b = RandomState::new().build_hasher().finish();
    let mut out = b"\0redcrab:removed:".to_vec();
    out.extend_from_slice(&a.to_be_bytes());
    out.extend_from_slice(&b.to_be_bytes());
    out
}

impl Client {
    /// Spawns `f` with a connection after the read-only check, the way
    /// every change starts.
    fn change<T, F>(
        &self,
        f: impl FnOnce(Conn) -> F + Send + 'static,
    ) -> impl Future<Output = Result<T>> + Send + 'static
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let writable = self.ensure_writable();
        let con = self.conn.clone();
        spawn(async move {
            writable?;
            f(con).await
        })
    }

    /// Replaces a string value, keeping its TTL. Fails with
    /// [`EditError::KeyMissing`] if the key is gone and
    /// [`EditError::WrongType`] if it is no longer a string, rather than
    /// creating or replacing it: an edit is of the value that was loaded.
    /// New keys come from [`Self::create_key`].
    ///
    /// Users the server does not let run scripts (an ACL without
    /// @scripting) get the same checks from `SET ... XX KEEPTTL GET`
    /// (Redis 6.2+).
    pub fn set_string(
        &self,
        key: Key,
        value: Vec<u8>,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        self.change(move |mut con| async move {
            let mut eval = script(SET_STRING, &key);
            eval.arg(&value);
            match eval.query_async::<()>(&mut con).await {
                Ok(()) => Ok(()),
                Err(err) if scripting_refused(&err) => {
                    set_string_plain(&key, value, &mut con).await
                }
                Err(err) => Err(script_error(err)),
            }
        })
    }

    /// Replaces a JSON document (a key of the JSON module's type), keeping
    /// its TTL. Fails if the key is gone or no longer a JSON document.
    pub fn set_json(
        &self,
        key: Key,
        json: String,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        self.change(move |mut con| async move {
            json::check(&json).map_err(|err| anyhow::anyhow!("not valid JSON: {err}"))?;
            let mut eval = script(SET_JSON, &key);
            eval.arg(json);
            run::<()>(eval, &mut con).await
        })
    }

    /// Sets a hash field. Refuses with [`EditError::ElementExists`] if the
    /// field exists, unless `overwrite`. A field's own TTL is kept. Returns
    /// whether the field is new.
    pub fn hash_set(
        &self,
        key: Key,
        field: Vec<u8>,
        value: Vec<u8>,
        overwrite: bool,
    ) -> impl Future<Output = Result<bool>> + Send + 'static {
        self.change(move |mut con| async move {
            let mut eval = script_with(FIELD_TTL, HASH_SET, &key);
            eval.arg(field).arg(value).arg(flag(overwrite));
            run(eval, &mut con).await
        })
    }

    /// Deletes hash fields; says how many there were and how many are
    /// left. Deleting the last one deletes the key, as Redis does.
    pub fn hash_delete(
        &self,
        key: Key,
        fields: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Removed>> + Send + 'static {
        self.change(move |mut con| async move {
            let mut eval = script_with(CHUNKED, HASH_DELETE, &key);
            eval.arg(fields);
            removed(eval, &mut con).await
        })
    }

    /// Renames a hash field, with `value` as its new value if given (else
    /// the old one), and its TTL. Refuses with
    /// [`EditError::ElementExists`] if `to` exists, unless `overwrite`, and
    /// with [`EditError::ElementMissing`] if `from` is gone. With `to ==
    /// from` it sets the value of a field that must still exist, unlike
    /// [`Self::hash_set`].
    pub fn hash_rename_field(
        &self,
        key: Key,
        from: Vec<u8>,
        to: Vec<u8>,
        value: Option<Vec<u8>>,
        overwrite: bool,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        self.change(move |mut con| async move {
            let mut eval = script_with(FIELD_TTL, HASH_RENAME, &key);
            let has_value = value.is_some();
            eval.arg(from)
                .arg(to)
                .arg(value.unwrap_or_default())
                .arg(flag(has_value))
                .arg(flag(overwrite));
            run::<()>(eval, &mut con).await
        })
    }

    /// Pushes elements onto an existing list. At the head they keep the
    /// order given: `values[0]` becomes the first element. Returns the new
    /// length.
    pub fn list_push(
        &self,
        key: Key,
        mut values: Vec<Vec<u8>>,
        end: ListEnd,
    ) -> impl Future<Output = Result<u64>> + Send + 'static {
        self.change(move |mut con| async move {
            ensure!(!values.is_empty(), "nothing to push");
            let command = match end {
                ListEnd::Head => {
                    // LPUSH a b puts b first.
                    values.reverse();
                    "LPUSH"
                }
                ListEnd::Tail => "RPUSH",
            };
            let mut eval = script(LIST_PUSH, &key);
            eval.arg(command).arg(values);
            run(eval, &mut con).await
        })
    }

    /// Sets the element at `index` if it still is `expected`; otherwise
    /// refuses with [`EditError::Changed`] (also when the index is out of
    /// range now).
    pub fn list_set(
        &self,
        key: Key,
        index: i64,
        expected: Vec<u8>,
        value: Vec<u8>,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        self.change(move |mut con| async move {
            let mut eval = script(LIST_SET, &key);
            eval.arg(index).arg(expected).arg(value);
            run::<()>(eval, &mut con).await
        })
    }

    /// Removes the elements at these indexes, each only if it still is the
    /// element given; if any is not, nothing is removed and the change is
    /// refused with [`EditError::Changed`]. Says how many were removed and
    /// how many are left.
    pub fn list_remove_at(
        &self,
        key: Key,
        elements: Vec<(i64, Vec<u8>)>,
    ) -> impl Future<Output = Result<Removed>> + Send + 'static {
        self.change(move |mut con| async move {
            let mut eval = script(LIST_REMOVE_AT, &key);
            eval.arg(tombstone());
            for (index, expected) in elements {
                eval.arg(index).arg(expected);
            }
            removed(eval, &mut con).await
        })
    }

    /// Removes the first occurrence of `value`, from the head. Returns
    /// whether there was one.
    pub fn list_remove(
        &self,
        key: Key,
        value: Vec<u8>,
    ) -> impl Future<Output = Result<bool>> + Send + 'static {
        self.change(move |mut con| async move {
            let mut eval = script(LIST_REMOVE, &key);
            eval.arg(value);
            run(eval, &mut con).await
        })
    }

    /// Adds members to an existing set; returns how many were new.
    pub fn set_add(
        &self,
        key: Key,
        members: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<u64>> + Send + 'static {
        self.change(move |mut con| async move {
            ensure!(!members.is_empty(), "nothing to add");
            let mut eval = script_with(CHUNKED, SET_ADD, &key);
            eval.arg(members);
            run(eval, &mut con).await
        })
    }

    /// Removes set members; says how many there were and how many are
    /// left.
    pub fn set_remove(
        &self,
        key: Key,
        members: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Removed>> + Send + 'static {
        self.change(move |mut con| async move {
            let mut eval = script_with(CHUNKED, SET_REMOVE, &key);
            eval.arg(members);
            removed(eval, &mut con).await
        })
    }

    /// Replaces set member `from` with `to`. Refuses with
    /// [`EditError::ElementMissing`] if `from` is gone, and with
    /// [`EditError::ElementExists`] if `to` is a member already, unless
    /// `overwrite` (then `from` just goes). Returns whether `to` is new.
    pub fn set_replace(
        &self,
        key: Key,
        from: Vec<u8>,
        to: Vec<u8>,
        overwrite: bool,
    ) -> impl Future<Output = Result<bool>> + Send + 'static {
        self.change(move |mut con| async move {
            let mut eval = script(SET_REPLACE, &key);
            eval.arg(from).arg(to).arg(flag(overwrite));
            run(eval, &mut con).await
        })
    }

    /// Adds a member to an existing sorted set, or with `overwrite` sets
    /// the score of one that exists; without it an existing member is
    /// refused with [`EditError::ElementExists`]. Returns whether the
    /// member is new.
    pub fn zset_add(
        &self,
        key: Key,
        member: Vec<u8>,
        score: f64,
        overwrite: bool,
    ) -> impl Future<Output = Result<bool>> + Send + 'static {
        let score = score_arg(score);
        self.change(move |mut con| async move {
            let mut eval = script(ZSET_ADD, &key);
            eval.arg(score?).arg(member).arg(flag(overwrite));
            run(eval, &mut con).await
        })
    }

    /// Renames a sorted set member, keeping its score unless a new one is
    /// given; with `to == from` it just sets the score. Refuses with
    /// [`EditError::ElementMissing`] if `from` is gone, and with
    /// [`EditError::ElementExists`] if `to` is a member, unless
    /// `overwrite`.
    pub fn zset_rename_member(
        &self,
        key: Key,
        from: Vec<u8>,
        to: Vec<u8>,
        score: Option<f64>,
        overwrite: bool,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        let score = score.map(score_arg).transpose();
        self.change(move |mut con| async move {
            let score = score?;
            let mut eval = script(ZSET_RENAME, &key);
            eval.arg(from)
                .arg(to)
                .arg(score.clone().unwrap_or_default())
                .arg(flag(score.is_some()))
                .arg(flag(overwrite));
            run::<()>(eval, &mut con).await
        })
    }

    /// Removes sorted set members; says how many there were and how many
    /// are left.
    pub fn zset_remove(
        &self,
        key: Key,
        members: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Removed>> + Send + 'static {
        self.change(move |mut con| async move {
            let mut eval = script_with(CHUNKED, ZSET_REMOVE, &key);
            eval.arg(members);
            removed(eval, &mut con).await
        })
    }

    /// Adds an entry to an existing stream: `id` is `*` (or `<ms>-*`) for
    /// the server to pick, or an explicit ID past the last entry's.
    /// Returns the entry's ID.
    pub fn stream_add(
        &self,
        key: Key,
        id: String,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> impl Future<Output = Result<String>> + Send + 'static {
        self.change(move |mut con| async move {
            check_stream_id(&id)?;
            check_stream_fields(&fields)?;
            let mut eval = script(STREAM_ADD, &key);
            eval.arg(id);
            for (field, value) in fields {
                eval.arg(field).arg(value);
            }
            run(eval, &mut con).await
        })
    }

    /// Deletes stream entries by ID; says how many there were and how many
    /// are left. The stream stays even when no entry is left (with its
    /// TTL, last ID and consumer groups), as Redis keeps it.
    pub fn stream_delete(
        &self,
        key: Key,
        ids: Vec<String>,
    ) -> impl Future<Output = Result<Removed>> + Send + 'static {
        self.change(move |mut con| async move {
            let mut eval = script_with(CHUNKED, STREAM_DELETE, &key);
            eval.arg(ids);
            removed(eval, &mut con).await
        })
    }

    /// Creates a key with `value` and, if given, a TTL in seconds, all at
    /// once: either the whole key appears or nothing does. Refuses with
    /// [`EditError::KeyExists`] if the key exists, whatever its type.
    pub fn create_key(
        &self,
        key: Key,
        value: NewValue,
        ttl: Option<u64>,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        self.change(move |mut con| async move {
            check_new_value(&value)?;
            if let Some(ttl) = ttl {
                check_ttl(ttl)?;
            }
            let mut eval = script(CREATE, &key);
            eval.arg(value.type_name())
                .arg(ttl.map(|t| t.to_string()).unwrap_or_default());
            match value {
                NewValue::String(bytes) => {
                    eval.arg(bytes);
                }
                NewValue::Hash(pairs) => {
                    for (field, value) in pairs {
                        eval.arg(field).arg(value);
                    }
                }
                NewValue::List(items) | NewValue::Set(items) => {
                    eval.arg(items);
                }
                NewValue::ZSet(members) => {
                    for (member, score) in members {
                        eval.arg(score_arg(score)?).arg(member);
                    }
                }
                NewValue::Stream { id, fields } => {
                    eval.arg(id);
                    for (field, value) in fields {
                        eval.arg(field).arg(value);
                    }
                }
                NewValue::Json(json) => {
                    eval.arg(json);
                }
            }
            match run::<()>(eval, &mut con).await {
                Err(err) if err.downcast_ref() == Some(&EditError::ElementExists) => {
                    Err(EditError::KeyExists.into())
                }
                other => other,
            }
        })
    }
}

/// Values Redis would refuse, refused before anything is sent.
fn check_new_value(value: &NewValue) -> Result<()> {
    match value {
        NewValue::String(_) => {}
        NewValue::Hash(pairs) => ensure!(!pairs.is_empty(), "a hash needs at least one field"),
        NewValue::List(items) => ensure!(!items.is_empty(), "a list needs at least one element"),
        NewValue::Set(items) => ensure!(!items.is_empty(), "a set needs at least one member"),
        NewValue::ZSet(members) => {
            ensure!(
                !members.is_empty(),
                "a sorted set needs at least one member"
            );
            for (_, score) in members {
                score_arg(*score)?;
            }
        }
        NewValue::Stream { id, fields } => {
            check_stream_id(id)?;
            check_stream_fields(fields)?;
        }
        NewValue::Json(json) => {
            json::check(json).map_err(|err| anyhow::anyhow!("not valid JSON: {err}"))?;
        }
    }
    Ok(())
}

fn check_stream_fields(fields: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    ensure!(
        !fields.is_empty(),
        "a stream entry needs at least one field"
    );
    ensure!(
        fields.len() <= MAX_STREAM_FIELDS,
        "a stream entry can have at most {MAX_STREAM_FIELDS} fields here"
    );
    Ok(())
}

/// The longest TTL Redis takes in seconds without overflowing its
/// millisecond clock.
pub const MAX_TTL_SECS: u64 = (i64::MAX as u64 / 1000) - (100 * 365 * 24 * 3600);

pub(crate) fn check_ttl(seconds: u64) -> Result<()> {
    if seconds == 0 {
        bail!("a TTL of 0 would delete the key at once; delete it instead");
    }
    ensure!(
        seconds <= MAX_TTL_SECS,
        "a TTL of {seconds} seconds is out of range"
    );
    Ok(())
}
