//! What Radarr and Sonarr know about one catalog entry, for the dashboard's item inspector:
//! `GET /api/entries/{id}/arr`. Only the dashboard process reads them (`RADARR_URL`/`RADARR_API_KEY`,
//! `SONARR_URL`/`SONARR_API_KEY`); the daemon never contacts either. Read-only, one request per
//! click. A film is asked of Radarr by its TMDB id (`/api/v3/movie?tmdbId=`; Radarr ignores an
//! `imdbId` filter and answers with its whole library, 91 MB on the real one). Sonarr's series list
//! is small: the series is matched by the show's IMDb id, else its title, and the one episode is
//! picked out of that series' episodes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::catalog::Entry;

const TIMEOUT: Duration = Duration::from_secs(20);
/// How long a finished answer is served again without asking the service.
const FRESH: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct Service {
    url: String,
    key: String,
}

/// The two services' connection settings, read once when the dashboard starts.
#[derive(Clone, Default)]
pub struct ArrEnv {
    radarr: Option<Service>,
    sonarr: Option<Service>,
    /// Answers by entry id. A lookup runs on its own thread, so a slow or down service never holds
    /// the dashboard's one request thread; the page asks again while a slot is `Pending`.
    slots: Arc<Mutex<HashMap<String, Slot>>>,
}

enum Slot {
    Pending,
    Done(Value, Instant),
}

impl ArrEnv {
    pub fn from_env() -> Self {
        let pair = |url: &str, key: &str| {
            let url = std::env::var(url).ok().filter(|v| !v.is_empty())?;
            let key = std::env::var(key).ok().filter(|v| !v.is_empty())?;
            Some(Service { url, key })
        };
        Self {
            radarr: pair("RADARR_URL", "RADARR_API_KEY"),
            sonarr: pair("SONARR_URL", "SONARR_API_KEY"),
            slots: Arc::default(),
        }
    }

    #[cfg(test)]
    pub fn with(radarr: Option<(&str, &str)>, sonarr: Option<(&str, &str)>) -> Self {
        let make = |p: (&str, &str)| Service {
            url: p.0.to_string(),
            key: p.1.to_string(),
        };
        Self {
            radarr: radarr.map(make),
            sonarr: sonarr.map(make),
            slots: Arc::default(),
        }
    }
}

fn get(service: &Service, path: &str) -> Result<Value, String> {
    let url = format!("{}{path}", service.url.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let response = agent
        .get(&url)
        .set("X-Api-Key", &service.key)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("{path}: HTTP {code}"),
            ureq::Error::Transport(t) => format!("{path}: {t}"),
        })?;
    response
        .into_json()
        .map_err(|e| format!("{path}: not JSON: {e}"))
}

fn text(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn gb(bytes: Option<i64>) -> String {
    format!("{:.1} GB", bytes.unwrap_or(0) as f64 / 1e9)
}

fn file_fields(file: &Value) -> Vec<(&'static str, String)> {
    let info = &file["mediaInfo"];
    let join =
        |parts: Vec<Option<String>>| parts.into_iter().flatten().collect::<Vec<_>>().join(" ");
    let num = |v: &Value| v.as_f64().map(|n| n.to_string());
    [
        ("File", text(&file["relativePath"])),
        ("Quality", text(&file["quality"]["quality"]["name"])),
        (
            "Video",
            Some(join(vec![
                text(&info["videoCodec"]),
                text(&info["resolution"]),
            ])),
        ),
        (
            "Audio",
            Some(join(vec![
                text(&info["audioCodec"]),
                num(&info["audioChannels"]),
            ])),
        ),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.filter(|s| !s.is_empty()).map(|s| (k, s)))
    .collect()
}

fn poster(images: &Value) -> Value {
    images
        .as_array()
        .and_then(|list| list.iter().find(|i| i["coverType"] == "poster"))
        .and_then(|i| i["remoteUrl"].as_str())
        .map_or(Value::Null, |u| json!(u))
}

fn monitored(v: &Value) -> String {
    format!(
        "{}{}",
        v["status"].as_str().unwrap_or("unknown"),
        if v["monitored"] == true {
            " · monitored"
        } else {
            " · not monitored"
        }
    )
}

fn movie(service: &Service, _entry: &Entry, tmdb: Option<&str>) -> Result<Value, String> {
    let Some(tmdb) = tmdb else {
        return Ok(json!({ "found": false, "note": "the catalog holds no TMDB id for this item" }));
    };
    let listed = get(service, &format!("/api/v3/movie?tmdbId={tmdb}"))?;
    let Some(m) = listed.as_array().and_then(|l| l.first()) else {
        return Ok(json!({ "found": false }));
    };
    let mut fields = vec![
        ("Status", monitored(m)),
        ("Size", gb(m["sizeOnDisk"].as_i64())),
    ];
    fields.extend(file_fields(&m["movieFile"]));
    for (label, value) in [
        (
            "Added",
            text(&m["added"]).map(|d| d.chars().take(10).collect()),
        ),
        ("Certification", text(&m["certification"])),
        (
            "Genres",
            m["genres"].as_array().map(|g| {
                g.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            }),
        ),
    ] {
        if let Some(v) = value.filter(|s: &String| !s.is_empty()) {
            fields.push((label, v));
        }
    }
    Ok(
        json!({ "found": true, "fields": fields, "overview": text(&m["overview"]), "poster": poster(&m["images"]) }),
    )
}

fn episode(service: &Service, entry: &Entry, _tmdb: Option<&str>) -> Result<Value, String> {
    let Some(show) = entry.show.as_deref() else {
        return Ok(json!({ "found": false }));
    };
    let all = get(service, "/api/v3/series")?;
    let show_imdb = entry
        .show_id
        .as_deref()
        .and_then(|id| id.strip_prefix("imdb:"));
    let series = || all.as_array().into_iter().flatten();
    let found = series()
        .find(|s| show_imdb.is_some_and(|id| s["imdbId"] == id))
        .or_else(|| {
            series()
                .find(|s| s["title"].as_str().map(str::to_lowercase) == Some(show.to_lowercase()))
        });
    let Some(s) = found else {
        return Ok(json!({ "found": false }));
    };
    let episodes = get(
        service,
        &format!(
            "/api/v3/episode?seriesId={}&includeEpisodeFile=true",
            s["id"]
        ),
    )?;
    let ep = episodes.as_array().into_iter().flatten().find(|e| {
        Some(e["seasonNumber"].as_i64()) == Some(entry.season)
            && Some(e["episodeNumber"].as_i64()) == Some(entry.episode)
    });
    let stats = &s["statistics"];
    let mut fields = vec![
        ("Series", s["title"].as_str().unwrap_or(show).to_string()),
        ("Status", monitored(s)),
        (
            "On disk",
            format!(
                "{}/{} episodes · {}",
                stats["episodeFileCount"],
                stats["totalEpisodeCount"],
                gb(stats["sizeOnDisk"].as_i64())
            ),
        ),
    ];
    if let Some(network) = text(&s["network"]) {
        fields.push(("Network", network));
    }
    if let Some(ep) = ep {
        if let Some(aired) = text(&ep["airDate"]) {
            fields.push(("Aired", aired));
        }
        fields.extend(file_fields(&ep["episodeFile"]));
    }
    Ok(json!({
        "found": true,
        "fields": fields,
        "overview": ep.and_then(|e| text(&e["overview"])),
        "poster": poster(&s["images"]),
    }))
}

/// One service's lookup for an entry, given the entry's TMDB id when the catalog has one.
type Ask = fn(&Service, &Entry, Option<&str>) -> Result<Value, String>;

/// The inspector's Radarr/Sonarr block for one entry as `(status, body)`: 200 with the answer, or
/// 202 `{pending: true}` while the service is being asked (the page asks again). An entry no
/// service is set for answers at once, with no thread and no request.
pub(super) fn inspect(env: &ArrEnv, entry: &Entry, tmdb: Option<&str>) -> (u16, Value) {
    let asks_a_service = match entry.kind.as_str() {
        "movie" => env.radarr.is_some(),
        "episode" => env.sonarr.is_some(),
        _ => false,
    };
    if !asks_a_service {
        return (200, lookup(env, entry, tmdb));
    }
    let mut slots = env.slots.lock().expect("arr slots");
    match slots.get(&entry.entry_id) {
        Some(Slot::Done(body, at)) if at.elapsed() < FRESH => return (200, body.clone()),
        Some(Slot::Pending) => return (202, json!({ "pending": true })),
        _ => {}
    }
    slots.insert(entry.entry_id.clone(), Slot::Pending);
    drop(slots);
    let (env, entry, tmdb) = (env.clone(), entry.clone(), tmdb.map(str::to_string));
    std::thread::spawn(move || {
        let body = lookup(&env, &entry, tmdb.as_deref());
        env.slots
            .lock()
            .expect("arr slots")
            .insert(entry.entry_id.clone(), Slot::Done(body, Instant::now()));
    });
    (202, json!({ "pending": true }))
}

/// Asks the service and shapes its answer. `service` is the one asked, `configured` whether its
/// URL and key are set, `found` whether it lists this item; `error` carries a failed request so a
/// down service shows as that and not as "not found".
fn lookup(env: &ArrEnv, entry: &Entry, tmdb: Option<&str>) -> Value {
    let (name, service, ask): (&str, &Option<Service>, Ask) = match entry.kind.as_str() {
        "movie" => ("Radarr", &env.radarr, movie),
        "episode" => ("Sonarr", &env.sonarr, episode),
        _ => {
            return json!({ "entry_id": entry.entry_id, "service": null, "configured": false, "found": false, "error": null });
        }
    };
    let mut out = json!({ "entry_id": entry.entry_id, "service": name, "configured": service.is_some(), "found": false, "error": null });
    let Some(service) = service else { return out };
    let began = std::time::Instant::now();
    let result = ask(service, entry, tmdb);
    let ms = began.elapsed().as_millis() as u64;
    match result {
        Ok(body) => {
            tracing::info!(event = "ui.arr", service = name, entry = %entry.entry_id, found = body["found"] == true, ms);
            for (k, v) in body.as_object().into_iter().flatten() {
                out[k] = v.clone();
            }
        }
        Err(e) => {
            tracing::warn!(event = "ui.arr_failed", service = name, entry = %entry.entry_id, error = %e, ms);
            out["error"] = json!(e);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// A stand-in service: answers each request path with its canned body, 404 otherwise.
    fn stub(routes: Vec<(&'static str, Value)>) -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let key_ok = request
                    .headers()
                    .iter()
                    .any(|h| h.field.equiv("X-Api-Key") && h.value.as_str() == "k");
                let body = routes
                    .iter()
                    .find(|(p, _)| request.url().starts_with(p))
                    .map(|(_, b)| b.to_string());
                let response = match (key_ok, body) {
                    (true, Some(b)) => tiny_http::Response::from_string(b),
                    _ => tiny_http::Response::from_string("{}").with_status_code(404),
                };
                let _ = request.respond(response);
            }
        });
        url
    }

    /// `inspect`, asked again until it answers 200 as the page does.
    fn inspect(env: &ArrEnv, entry: &Entry, tmdb: Option<&str>) -> Value {
        for _ in 0..200 {
            let (status, body) = super::inspect(env, entry, tmdb);
            if status == 200 {
                return body;
            }
            assert_eq!(body, json!({ "pending": true }));
            thread::sleep(Duration::from_millis(25));
        }
        panic!("still pending after 5 s");
    }

    fn entry(kind: &str, id: &str) -> Entry {
        Entry::new(id, kind, "Title", crate::catalog::Source::Plex)
    }

    #[test]
    fn a_movie_is_matched_by_tmdb_id_and_a_missing_one_is_not_found() {
        let url = stub(vec![
            ("/api/v3/movie?tmdbId=12", json!([])),
            (
                "/api/v3/movie?tmdbId=11",
                json!([
                    {"imdbId": "tt1", "status": "released", "monitored": true, "sizeOnDisk": 4_500_000_000i64,
                     "genres": ["Drama"], "overview": "Plot.",
                     "movieFile": {"relativePath": "A (2001).mkv", "quality": {"quality": {"name": "Bluray-1080p"}},
                                   "mediaInfo": {"videoCodec": "x265", "resolution": "1920x1080", "audioCodec": "EAC3", "audioChannels": 5.1}},
                     "images": [{"coverType": "poster", "remoteUrl": "http://img/p.jpg"}]},
                ]),
            ),
        ]);
        let env = ArrEnv::with(Some((&url, "k")), None);
        let hit = inspect(&env, &entry("movie", "imdb:tt1"), Some("11"));
        assert_eq!(hit["service"], "Radarr");
        assert_eq!(hit["found"], true, "{hit}");
        assert_eq!(hit["poster"], "http://img/p.jpg");
        let fields: Vec<(String, String)> = serde_json::from_value(hit["fields"].clone()).unwrap();
        assert!(
            fields.contains(&("Size".into(), "4.5 GB".into())),
            "{fields:?}"
        );
        assert!(
            fields.contains(&("Video".into(), "x265 1920x1080".into())),
            "{fields:?}"
        );
        assert!(
            fields.contains(&("Audio".into(), "EAC3 5.1".into())),
            "{fields:?}"
        );
        let miss = inspect(&env, &entry("movie", "imdb:tt2"), Some("12"));
        assert_eq!(
            (miss["found"].clone(), miss["error"].clone()),
            (json!(false), Value::Null)
        );
        let no_id = inspect(&env, &entry("movie", "imdb:tt3"), None);
        assert_eq!(no_id["found"], false);
        assert!(no_id["note"].as_str().unwrap().contains("TMDB"), "{no_id}");
    }

    #[test]
    fn an_episode_is_matched_by_show_season_and_episode() {
        let url = stub(vec![
            (
                "/api/v3/series",
                json!([{"id": 6, "title": "Other", "imdbId": "tt9"}, {"id": 7, "title": "The Show Renamed", "imdbId": "tt5", "year": 1999, "status": "ended", "monitored": false,
                "network": "NBC", "statistics": {"episodeFileCount": 3, "totalEpisodeCount": 4, "sizeOnDisk": 2_000_000_000i64}}]),
            ),
            (
                "/api/v3/episode?seriesId=7",
                json!([
                {"seasonNumber": 1, "episodeNumber": 1, "airDate": "1999-01-01", "overview": "Wrong."},
                {"seasonNumber": 1, "episodeNumber": 2, "airDate": "1999-01-08", "overview": "Right.",
                 "episodeFile": {"relativePath": "S01E02.mkv"}}]),
            ),
        ]);
        let env = ArrEnv::with(None, Some((&url, "k")));
        let mut e = entry("episode", "plex:2");
        e.show = Some("the show".into());
        e.show_id = Some("imdb:tt5".into());
        e.season = Some(1);
        e.episode = Some(2);
        let got = inspect(&env, &e, None);
        assert_eq!(got["service"], "Sonarr");
        assert_eq!(got["overview"], "Right.", "{got}");
        let fields: Vec<(String, String)> = serde_json::from_value(got["fields"].clone()).unwrap();
        assert!(
            fields.contains(&("On disk".into(), "3/4 episodes · 2.0 GB".into())),
            "{fields:?}"
        );
        assert!(
            fields.contains(&("Aired".into(), "1999-01-08".into())),
            "{fields:?}"
        );
        assert!(
            fields.contains(&("File".into(), "S01E02.mkv".into())),
            "{fields:?}"
        );
    }

    #[test]
    fn an_unset_service_is_reported_and_a_failing_one_is_an_error_not_a_miss() {
        let none = inspect(&ArrEnv::default(), &entry("movie", "imdb:tt1"), Some("1"));
        assert_eq!(
            (none["configured"].clone(), none["found"].clone()),
            (json!(false), json!(false))
        );
        let url = stub(vec![]);
        let down = inspect(
            &ArrEnv::with(Some((&url, "k")), None),
            &entry("movie", "imdb:tt1"),
            Some("1"),
        );
        assert_eq!(down["configured"], true);
        assert!(
            down["error"].as_str().unwrap().contains("HTTP 404"),
            "{down}"
        );
        let bumper = inspect(&ArrEnv::default(), &entry("bumper", "b"), None);
        assert_eq!(bumper["service"], Value::Null);
    }

    #[test]
    fn a_slow_service_leaves_inspect_answering_pending_at_once() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        thread::spawn(move || {
            for request in server.incoming_requests() {
                thread::sleep(Duration::from_millis(600));
                let _ = request.respond(tiny_http::Response::from_string("[]"));
            }
        });
        let env = ArrEnv::with(Some((&url, "k")), None);
        let e = entry("movie", "imdb:tt7");
        let began = Instant::now();
        let (status, body) = super::inspect(&env, &e, Some("7"));
        assert_eq!((status, body), (200 + 2, json!({ "pending": true })));
        assert!(
            began.elapsed() < Duration::from_millis(300),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(
            super::inspect(&env, &e, Some("7")).0,
            202,
            "asked again while pending"
        );
        assert_eq!(inspect(&env, &e, Some("7"))["found"], false);
        assert_eq!(
            super::inspect(&env, &e, Some("7")).0,
            200,
            "served from the finished slot"
        );
    }
}
