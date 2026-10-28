use anyhow::Result;
use redis::{Value, cmd, pipe};

use super::{Client, Key};
use crate::{
    runtime::spawn,
    value::{
        Cursor, KeyType, ValueData, ValuePage, as_f64, flatten, into_bytes, pairs, scan_reply,
        stream_entries,
    },
};

/// Strings longer than this are previewed, not loaded whole.
const STRING_PREVIEW_BYTES: usize = 1 << 20;

impl Client {
    /// Loads up to `count` (at least one) elements of `key` starting at
    /// `cursor`.
    pub fn load(
        &self,
        key: Key,
        ty: KeyType,
        cursor: Cursor,
        count: usize,
    ) -> impl Future<Output = Result<ValuePage>> + Send + 'static {
        // A page of nothing would never get anywhere, and the range end
        // below must not underflow.
        let count = count.max(1);
        let mut con = self.conn.clone();
        spawn(async move {
            let key = &*key;
            let page = match ty {
                KeyType::String => {
                    let (len, head): (u64, Vec<u8>) = pipe()
                        .cmd("STRLEN")
                        .arg(key)
                        .cmd("GETRANGE")
                        .arg(key)
                        .arg(0)
                        .arg(STRING_PREVIEW_BYTES - 1)
                        .query_async(&mut con)
                        .await?;
                    ValuePage {
                        data: ValueData::String(head),
                        total: len,
                        next: None,
                    }
                }
                KeyType::Hash | KeyType::Set => {
                    let (len_cmd, scan_cmd) = if ty == KeyType::Hash {
                        ("HLEN", "HSCAN")
                    } else {
                        ("SCARD", "SSCAN")
                    };
                    let at = match cursor {
                        Cursor::Scan(c) => c,
                        _ => 0,
                    };
                    let (total, reply): (u64, Value) = pipe()
                        .cmd(len_cmd)
                        .arg(key)
                        .cmd(scan_cmd)
                        .arg(key)
                        .arg(at)
                        .arg("COUNT")
                        .arg(count)
                        .query_async(&mut con)
                        .await?;
                    let (next, items) = scan_reply(reply);
                    let data = if ty == KeyType::Hash {
                        ValueData::Hash(pairs(items))
                    } else {
                        ValueData::Set(flatten(items).into_iter().map(into_bytes).collect())
                    };
                    ValuePage {
                        data,
                        total,
                        next: (next != 0).then_some(Cursor::Scan(next)),
                    }
                }
                KeyType::List | KeyType::ZSet => {
                    let offset = match cursor {
                        Cursor::Offset(o) => o,
                        _ => 0,
                    };
                    let stop = offset + count as u64 - 1;
                    let mut p = pipe();
                    if ty == KeyType::List {
                        p.cmd("LLEN")
                            .arg(key)
                            .cmd("LRANGE")
                            .arg(key)
                            .arg(offset)
                            .arg(stop);
                    } else {
                        p.cmd("ZCARD")
                            .arg(key)
                            .cmd("ZRANGE")
                            .arg(key)
                            .arg(offset)
                            .arg(stop)
                            .arg("WITHSCORES");
                    }
                    let (total, reply): (u64, Value) = p.query_async(&mut con).await?;
                    let items = flatten(reply);
                    let data = if ty == KeyType::List {
                        ValueData::List(items.into_iter().map(into_bytes).collect())
                    } else {
                        let mut items = items.into_iter();
                        let mut members = Vec::with_capacity(items.len() / 2);
                        while let (Some(m), Some(s)) = (items.next(), items.next()) {
                            members.push((into_bytes(m), as_f64(&s)));
                        }
                        ValueData::ZSet(members)
                    };
                    let end = offset + data.len() as u64;
                    ValuePage {
                        data,
                        total,
                        next: (end < total).then_some(Cursor::Offset(end)),
                    }
                }
                KeyType::Stream => {
                    let start = match &cursor {
                        Cursor::StreamAfter(id) => format!("({id}"),
                        _ => "-".into(),
                    };
                    // One entry more than the page tells whether there is
                    // another page, so a stream whose length is a multiple
                    // of `count` does not end with an empty one.
                    let (total, reply): (u64, Value) = pipe()
                        .cmd("XLEN")
                        .arg(key)
                        .cmd("XRANGE")
                        .arg(key)
                        .arg(start)
                        .arg("+")
                        .arg("COUNT")
                        .arg(count + 1)
                        .query_async(&mut con)
                        .await?;
                    let mut entries = stream_entries(reply);
                    let more = entries.len() > count;
                    entries.truncate(count);
                    let next = more
                        .then(|| entries.last().map(|e| Cursor::StreamAfter(e.id.clone())))
                        .flatten();
                    ValuePage {
                        data: ValueData::Stream(entries),
                        total,
                        next,
                    }
                }
                KeyType::Json => {
                    let json: String = cmd("JSON.GET").arg(key).query_async(&mut con).await?;
                    ValuePage {
                        total: json.len() as u64,
                        data: ValueData::Json(json),
                        next: None,
                    }
                }
                KeyType::Other(name) => ValuePage {
                    data: ValueData::Unsupported(name),
                    total: 0,
                    next: None,
                },
                KeyType::None => ValuePage {
                    data: ValueData::Missing,
                    total: 0,
                    next: None,
                },
            };
            Ok(page)
        })
    }
}
