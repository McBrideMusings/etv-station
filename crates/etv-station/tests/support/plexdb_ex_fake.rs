//! A fake plex-db-ex explorer for tests, shared by the lib's unit tests and the
//! integration tests through `#[path]`: the read routes etv-station calls
//! (plex-db-ex `docs/api.md`), answered from an in-memory [`Store`] by a
//! std `TcpListener` on 127.0.0.1, one thread per connection. Lists page the
//! way the real API does (`limit` default 100, at most 1000, `after`,
//! `has_more`/`next`); an unknown title, account or keyword is a 404.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

#[derive(Debug, Clone, Default)]
pub struct Item {
    pub kind: String,
    /// `(namespace, key, value)`.
    pub enrichment: Vec<(String, String, String)>,
    /// `None`: never analyzed (the route answers `null`).
    pub audio: Option<Value>,
    /// `(rating_key, section_id)`.
    pub plex: Vec<(String, String)>,
    /// `(ns, value)`.
    pub external_ids: Vec<(String, String)>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Account {
    pub taste: Value,
    pub watched: Vec<String>,
}

/// What the fake answers from.
#[derive(Debug, Clone, Default)]
pub struct Store {
    pub items: BTreeMap<String, Item>,
    pub keywords: BTreeMap<String, Vec<String>>,
    pub accounts: BTreeMap<i64, Account>,
    pub pooled_taste: Option<Value>,
    /// `(kind, tag) -> [{tag, shared, df}]`.
    pub neighbours: BTreeMap<(String, String), Value>,
    /// The store holds no play history: every taste and watched read is a
    /// 500, so a test can prove a plugin never asked for one.
    pub no_plays: bool,
}

impl Store {
    pub fn item(&mut self, id: &str, kind: &str) -> &mut Item {
        let item = self.items.entry(id.to_string()).or_default();
        item.kind = kind.to_string();
        item
    }
    pub fn fact(&mut self, id: &str, namespace: &str, key: &str, value: &str) {
        let item = self.items.entry(id.to_string()).or_default();
        if item.kind.is_empty() {
            item.kind = "movie".into();
        }
        item.enrichment
            .push((namespace.into(), key.into(), value.into()));
    }
    pub fn audio(&mut self, id: &str, facts: Value) {
        self.items.entry(id.to_string()).or_default().audio = Some(facts);
    }
    pub fn audio_null(&mut self, id: &str) {
        self.items.entry(id.to_string()).or_default();
    }
    pub fn keyword(&mut self, keyword: &str, surfaces: &[&str]) {
        self.keywords.insert(
            keyword.to_string(),
            surfaces.iter().map(|s| s.to_string()).collect(),
        );
    }
    pub fn account(&mut self, id: i64) -> &mut Account {
        self.accounts.entry(id).or_insert_with(|| Account {
            taste: json!({"attributes": [], "shows_without_seasons": []}),
            watched: Vec::new(),
        })
    }

    /// A store read from a hand-written SQLite fixture in plex-db-ex's table
    /// shape (`items`, `enrichment`, `plays`, optional `keyword_forms`), with
    /// each account's and the pooled taste vector rolled up from `plays` by
    /// plex-db-ex's ADR-0011 rule: a unit is a film or an episode's show, `r`
    /// is its plays (over its median season length for a show), a unit under
    /// 0.5 is skipped, and each of its distinct attributes gains `sqrt(r) / n`.
    pub fn from_sqlite(path: &std::path::Path) -> Store {
        let conn = rusqlite::Connection::open(path).unwrap();
        let mut store = Store::default();
        let mut show_of: BTreeMap<String, String> = BTreeMap::new();
        let mut season_eps: BTreeMap<(String, i64), i64> = BTreeMap::new();
        let mut stmt = conn
            .prepare("SELECT item_id, type, show_item_id, season FROM items")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (id, kind, show, season) = row.unwrap();
            store.item(&id, &kind);
            if let Some(show) = show {
                if kind == "episode"
                    && let Some(season) = season
                {
                    *season_eps.entry((show.clone(), season)).or_default() += 1;
                }
                show_of.insert(id, show);
            }
        }
        let mut stmt = conn
            .prepare("SELECT item_id, namespace, key, value FROM enrichment ORDER BY rowid")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (id, ns, key, value) = row.unwrap();
            store.fact(&id, &ns, &key, &value);
        }
        let has_forms: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE name = 'keyword_forms'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if has_forms {
            let mut stmt = conn
                .prepare("SELECT keyword, surface FROM keyword_forms ORDER BY surface")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap();
            for row in rows {
                let (keyword, surface) = row.unwrap();
                store.keywords.entry(keyword).or_default().push(surface);
            }
        }
        let has_network: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE name = 'tag_network_edge'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if has_network {
            let mut stmt = conn
                .prepare(
                    "SELECT e.kind, e.a, e.b, e.shared, na.df, nb.df FROM tag_network_edge e \
                     JOIN tag_network na ON na.kind = e.kind AND na.value = e.a \
                     JOIN tag_network nb ON nb.kind = e.kind AND nb.value = e.b",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })
                .unwrap();
            // (kind, tag) -> [(shared, neighbour, neighbour df)]
            type Near = BTreeMap<(String, String), Vec<(i64, String, i64)>>;
            let mut near: Near = BTreeMap::new();
            for row in rows {
                let (kind, a, b, shared, df_a, df_b) = row.unwrap();
                near.entry((kind.clone(), a.clone()))
                    .or_default()
                    .push((shared, b.clone(), df_b));
                near.entry((kind, b)).or_default().push((shared, a, df_a));
            }
            for (key, mut list) in near {
                // Strongest first, then by tag — plex-db-ex's order.
                list.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
                let rows: Vec<Value> = list
                    .into_iter()
                    .map(|(shared, tag, df)| json!({"tag": tag, "shared": shared, "df": df}))
                    .collect();
                store.neighbours.insert(key, json!(rows));
            }
        }
        let has_plays: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE name = 'plays'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if !has_plays {
            store.no_plays = true;
            return store;
        }
        // (account, unit) -> plays
        let mut plays: BTreeMap<(i64, String), i64> = BTreeMap::new();
        let mut stmt = conn
            .prepare("SELECT plex_account_id, item_id FROM plays")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        for row in rows {
            let (account, item) = row.unwrap();
            if !store.items.contains_key(&item) {
                continue;
            }
            let unit = show_of.get(&item).cloned().unwrap_or(item);
            *plays.entry((account, unit)).or_default() += 1;
        }
        let mut medians: BTreeMap<String, i64> = BTreeMap::new();
        let mut by_show: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for ((show, _), eps) in season_eps {
            by_show.entry(show).or_default().push(eps);
        }
        for (show, mut lengths) in by_show {
            lengths.sort();
            medians.insert(show, lengths[lengths.len() / 2]);
        }
        let rollup = |units: &BTreeMap<String, i64>, store: &Store| -> Value {
            let mut totals: BTreeMap<(String, String, String), f64> = BTreeMap::new();
            let mut without: Vec<String> = Vec::new();
            for (unit, n) in units {
                let r = match medians.get(unit) {
                    Some(len) => *n as f64 / *len as f64,
                    None => {
                        if store.items.get(unit).is_some_and(|i| i.kind == "show") {
                            without.push(unit.clone());
                        }
                        *n as f64
                    }
                };
                if r < 0.5 {
                    continue;
                }
                let Some(item) = store.items.get(unit) else {
                    continue;
                };
                let attrs: std::collections::BTreeSet<&(String, String, String)> =
                    item.enrichment.iter().collect();
                if attrs.is_empty() {
                    continue;
                }
                let share = r.sqrt() / attrs.len() as f64;
                for a in attrs {
                    *totals.entry(a.clone()).or_insert(0.0) += share;
                }
            }
            json!({
                "attributes": totals.into_iter().map(|((namespace, key, value), weight)|
                    json!({"namespace": namespace, "key": key, "value": value, "weight": weight}))
                    .collect::<Vec<_>>(),
                "shows_without_seasons": without,
            })
        };
        let mut pooled: BTreeMap<String, i64> = BTreeMap::new();
        let mut per_account: BTreeMap<i64, BTreeMap<String, i64>> = BTreeMap::new();
        for ((account, unit), n) in plays {
            *pooled.entry(unit.clone()).or_default() += n;
            per_account.entry(account).or_default().insert(unit, n);
        }
        store.pooled_taste = Some(rollup(&pooled, &store));
        for (account, units) in per_account {
            let taste = rollup(&units, &store);
            let a = store.account(account);
            a.taste = taste;
            a.watched = units.into_keys().collect();
        }
        store
    }
}

/// A running fake; it stops answering when dropped (the listener thread is
/// left parked on its socket, which the test process reaps).
pub struct Server {
    pub url: String,
    /// Requests answered, by path without the query.
    pub hits: Arc<Mutex<Vec<String>>>,
    pub store: Arc<Mutex<Store>>,
}

pub fn serve(store: Store) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let store = Arc::new(Mutex::new(store));
    let hits = Arc::new(Mutex::new(Vec::new()));
    let (s, h) = (Arc::clone(&store), Arc::clone(&hits));
    let live = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let (s, h, live) = (Arc::clone(&s), Arc::clone(&h), Arc::clone(&live));
            std::thread::spawn(move || {
                live.fetch_add(1, Ordering::SeqCst);
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let target = parts.next().unwrap_or("").to_string();
                    let mut length = 0usize;
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) == 0 {
                            return;
                        }
                        let header = header.trim_end();
                        if header.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = header.split_once(':')
                            && k.eq_ignore_ascii_case("content-length")
                        {
                            length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; length];
                    let _ = reader.read_exact(&mut body);
                    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                    h.lock().unwrap().push(path.to_string());
                    let (status, reply) = if method == "GET" {
                        answer(&s.lock().unwrap(), path, query)
                    } else {
                        (405, json!({"error": "method"}))
                    };
                    let text = reply.to_string();
                    let reason = match status {
                        200 => "OK",
                        404 => "Not Found",
                        _ => "Error",
                    };
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        text.len()
                    );
                    if conn.write_all(head.as_bytes()).is_err()
                        || conn.write_all(text.as_bytes()).is_err()
                    {
                        break;
                    }
                }
                live.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    Server { url, hits, store }
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn params(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn page(rows: Vec<(String, Value)>, q: &BTreeMap<String, String>) -> (u16, Value) {
    let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100);
    if !(1..=1000).contains(&limit) {
        return (
            400,
            json!({"error": "limit must be a whole number from 1 to 1000"}),
        );
    }
    let after = q.get("after").filter(|a| !a.is_empty());
    let rest: Vec<(String, Value)> = rows
        .into_iter()
        .filter(|(k, _)| after.is_none_or(|a| k.as_str() > a.as_str()))
        .collect();
    let has_more = rest.len() > limit;
    let taken: Vec<(String, Value)> = rest.into_iter().take(limit).collect();
    let next = if has_more {
        json!(taken.last().map(|(k, _)| k.clone()))
    } else {
        Value::Null
    };
    (
        200,
        json!({"data": taken.into_iter().map(|(_, v)| v).collect::<Vec<_>>(), "has_more": has_more, "next": next}),
    )
}

fn facts(item: &Item, namespace: Option<&String>) -> Value {
    let mut rows: Vec<&(String, String, String)> = item
        .enrichment
        .iter()
        .filter(|(ns, _, _)| namespace.is_none_or(|n| n == ns))
        .collect();
    rows.sort();
    json!(
        rows.into_iter()
            .map(|(ns, k, v)| json!({"namespace": ns, "key": k, "value": v, "fetched_at": "2026-01-01T00:00:00+00:00"}))
            .collect::<Vec<_>>()
    )
}

fn item_row(id: &str, item: &Item, q: &BTreeMap<String, String>) -> Value {
    let mut row = json!({"item_id": id, "type": item.kind, "title": item.title});
    let include = q.get("include").cloned().unwrap_or_default();
    for field in include.split(',').filter(|f| !f.is_empty()) {
        row[field] = match field {
            "enrichment" => facts(item, q.get("namespace")),
            "audio_facts" => item.audio.clone().unwrap_or(Value::Null),
            "plex" => json!(
                item.plex
                    .iter()
                    .map(|(rk, sec)| json!({"rating_key": rk, "section_id": sec}))
                    .collect::<Vec<_>>()
            ),
            "external_ids" => json!(
                item.external_ids
                    .iter()
                    .map(|(ns, v)| json!({"ns": ns, "value": v}))
                    .collect::<Vec<_>>()
            ),
            _ => Value::Null,
        };
    }
    row
}

fn not_found(what: &str) -> (u16, Value) {
    (404, json!({"error": format!("{what} is not in the store")}))
}

fn answer(store: &Store, path: &str, query: &str) -> (u16, Value) {
    let q = params(query);
    let segs: Vec<String> = path
        .trim_start_matches('/')
        .split('/')
        .map(decode)
        .collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    if store.no_plays
        && matches!(
            segs.as_slice(),
            ["taste"] | ["watched"] | ["accounts", _, _]
        )
    {
        return (500, json!({"error": "the store holds no plays"}));
    }
    match segs.as_slice() {
        ["items"] => {
            let rows = store
                .items
                .iter()
                .filter(|(_, it)| q.get("type").is_none_or(|t| *t == it.kind))
                .map(|(id, it)| (id.clone(), item_row(id, it, &q)))
                .collect();
            page(rows, &q)
        }
        ["items", id] => match store.items.get(*id) {
            Some(it) => (200, item_row(id, it, &q)),
            None => not_found(id),
        },
        ["items", id, "enrichment"] => match store.items.get(*id) {
            Some(it) => (200, facts(it, q.get("namespace"))),
            None => not_found(id),
        },
        ["items", id, "audio-facts"] => match store.items.get(*id) {
            Some(it) => (200, it.audio.clone().unwrap_or(Value::Null)),
            None => not_found(id),
        },
        ["accounts"] => (
            200,
            json!(
                store
                    .accounts
                    .keys()
                    .map(|id| json!({"account_id": id}))
                    .collect::<Vec<_>>()
            ),
        ),
        ["accounts", id, "taste"] => {
            match id.parse().ok().and_then(|id: i64| store.accounts.get(&id)) {
                Some(a) => (200, a.taste.clone()),
                None => not_found(id),
            }
        }
        ["accounts", id, "watched"] => {
            match id.parse().ok().and_then(|id: i64| store.accounts.get(&id)) {
                Some(a) => {
                    let mut w = a.watched.clone();
                    w.sort();
                    w.dedup();
                    page(w.into_iter().map(|u| (u.clone(), json!(u))).collect(), &q)
                }
                None => not_found(id),
            }
        }
        ["taste"] => (
            200,
            store
                .pooled_taste
                .clone()
                .unwrap_or(json!({"attributes": [], "shows_without_seasons": []})),
        ),
        ["watched"] => {
            let mut w: Vec<String> = store
                .accounts
                .values()
                .flat_map(|a| a.watched.clone())
                .collect();
            w.sort();
            w.dedup();
            page(w.into_iter().map(|u| (u.clone(), json!(u))).collect(), &q)
        }
        ["keywords"] => {
            let surface = q.get("surface").map(|s| {
                s.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase()
            });
            let rows = store
                .keywords
                .iter()
                .filter(|(k, surfaces)| {
                    surface
                        .as_ref()
                        .is_none_or(|s| *k == s || surfaces.iter().any(|x| x.to_lowercase() == *s))
                })
                .map(|(k, s)| {
                    (
                        k.clone(),
                        json!({"keyword": k, "name": null, "surfaces": s}),
                    )
                })
                .collect();
            page(rows, &q)
        }
        ["keywords", kw] => match store.keywords.get(*kw) {
            Some(s) => (200, json!({"keyword": kw, "name": null, "surfaces": s})),
            None => not_found(kw),
        },
        ["tags", tag, "neighbours"] => {
            let kind = q.get("kind").cloned().unwrap_or_default();
            match store.neighbours.get(&(kind, tag.to_string())) {
                Some(n) => (200, n.clone()),
                None => not_found(tag),
            }
        }
        _ => (404, json!({"error": "no route"})),
    }
}
