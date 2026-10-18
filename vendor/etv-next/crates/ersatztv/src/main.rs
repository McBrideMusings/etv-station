mod channel_health;
mod channel_model;
mod channel_session;
mod scaffold;
mod xmltv;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::body::HttpBody as _;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::{Router, routing::get};
use clap::{Parser, Subcommand};
use ersatztv::error::LineupError;
use ersatztv_core::{HEARTBEAT_FILE_NAME, READY_FILE_TIMEOUT, empty_folder};
use tokio::signal;
use tokio::sync::Mutex;
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;

use crate::channel_model::ChannelModel;
use crate::channel_session::ChannelSession;

#[derive(Parser, Debug)]
#[command(version = ersatztv_core::VERSION, about, long_about = None, subcommand_negates_reqs = true)]
struct Args {
    /// Path to lineup.json (server mode)
    #[arg(required = true)]
    lineup_path: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Scaffold a new lineup at the provided lineup.json path
    AddLineup {
        lineup_path: PathBuf,
        #[arg(long)]
        channels: u32,
        #[arg(long)]
        force: bool,
    },
    /// Add a channel to an existing lineup
    AddChannel {
        lineup_path: PathBuf,
        #[arg(long)]
        number: String,
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
pub async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    if let Err(err) = run().await {
        log::error!("{err}");
        std::process::exit(1);
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install ctrl+c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

async fn run() -> Result<(), LineupError> {
    let args = Args::parse();

    match args.command {
        Some(Commands::AddLineup {
            lineup_path,
            channels,
            force,
        }) => scaffold::add_lineup(&lineup_path, channels, force).await,
        Some(Commands::AddChannel {
            lineup_path,
            number,
            force,
        }) => scaffold::add_channel(&lineup_path, &number, force).await,
        None => {
            let lineup_path =
                args.lineup_path
                    .ok_or(LineupError::LineupConfigFailure(String::from(
                        "lineup path is required",
                    )))?;

            // load lineup config
            let lineup_config = ersatztv::config::from_file(&lineup_path).await?;
            let output_folder = PathBuf::from(&lineup_config.output.folder);

            let mut channels: Vec<ChannelModel> = Vec::with_capacity(lineup_config.channels.len());
            for channel in lineup_config.channels {
                match ChannelModel::new(&lineup_path, &output_folder, channel).await {
                    Ok(channel_config) => {
                        channels.push(channel_config);
                    }
                    Err(err) => {
                        log::warn!("{err}")
                    }
                }
            }

            if channels.is_empty() {
                return Err(LineupError::NoChannelsLoaded);
            }

            log::debug!("loaded {} channel definitions", channels.len());

            empty_folder(&output_folder).await?;

            let state = Arc::new(LineupState {
                channels,
                active: Arc::new(Mutex::new(HashMap::new())),
                health: Arc::new(Mutex::new(crate::channel_health::HealthMap::default())),
                device_id: lineup_config.server.device_id.clone(),
            });

            // Per-run segment folders (etv-station-262) mean a channel's exit-time
            // cleanup only ever sees the run that JUST exited — a viewer usually
            // detaches AFTER that point, so a dead run whose viewer left later has
            // nothing tied to a spawn or an exit that would ever collect it. This
            // sweep is that collector: it periodically reaps every channel's run
            // folders down to the live run plus every run folder the channel's
            // current live.m3u8/live_sub.m3u8 still names (etv-station-262.1)
            // plus every run folder written to within SEGMENT_RETENTION,
            // catching exactly the run folders the exit-time reap in
            // `channel_session::spawn` left standing. The one-time
            // `empty_folder` above stays as it is — a cold start with no viewers
            // has nothing this sweep would improve on.
            //
            // Per channel, `channel_session::reap_channel_run_folders` holds
            // `state.active`'s lock across the whole decide-then-reap step
            // (etv-station-262.1) — the same lock `session_middleware` and the
            // ready-wait spawn path hold across `ChannelSession::spawn` — so a
            // worker spawning for that channel and this sweep reaping it can
            // never interleave: a run folder can only come into existence
            // before this sweep reads `active` (where the entry is then
            // visible, and `keep_newest` is `true`) or after the reap has
            // returned (there is nothing yet for it to have removed).
            {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(60));
                    loop {
                        interval.tick().await;
                        for channel in &state.channels {
                            let number = channel.number();
                            if let Err(err) = crate::channel_session::reap_channel_run_folders(
                                number,
                                channel.output_folder(),
                                &state.active,
                            )
                            .await
                            {
                                log::warn!(
                                    "failed to reap run folders for channel {number}: {err}"
                                );
                            }
                        }
                    }
                });
            }

            let addr = format!(
                "{}:{}",
                lineup_config.server.bind_address, lineup_config.server.port
            );

            let listener = tokio::net::TcpListener::bind(addr).await?;

            let mut app = Router::new()
                .route("/channel/{filename}", get(stream))
                .route("/channels.m3u", get(channel_playlist))
                .route("/xmltv.xml", get(crate::xmltv::xmltv_epg))
                .route("/health/channels.json", get(channel_health_report))
                .route("/build.json", get(build_stamp))
                // Plex has no M3U tuner type — of its four device kinds only
                // `hdhomerun` is both a live-TV protocol and publicly
                // implementable — so a server that wants Plex to carry its
                // channels answers as an HDHomeRun box. These are the three
                // addresses Plex asks for.
                .route("/discover.json", get(hdhr_discover))
                .route("/lineup.json", get(hdhr_lineup))
                .route("/lineup_status.json", get(hdhr_lineup_status))
                // Text routes only. Router::layer wraps the routes registered
                // above it, so /session (HLS segments) and /artwork
                // (JPEG/PNG) below stay uncompressed — both are already-
                // compressed media and gzipping them spends CPU per segment
                // per viewer for nothing.
                .layer(CompressionLayer::new())
                // Channel logos named as files in the lineup (#342): an image,
                // so registered after the compression layer like /artwork.
                .route("/logo/{filename}", get(channel_logo))
                .nest_service(
                    "/session",
                    ServiceBuilder::new()
                        .layer(axum::middleware::from_fn_with_state(
                            Arc::clone(&state),
                            session_middleware,
                        ))
                        .service(tower_http::services::ServeDir::new(&output_folder)),
                );

            // Station-cached Plex artwork (#187) — only mounted when the
            // lineup names a folder for it. `<icon src>` in xmltv.xml always
            // points here, never at Plex directly, which is what keeps
            // `X-Plex-Token` out of a file served over plain HTTP to every
            // guide reader.
            if let Some(artwork) = &lineup_config.artwork {
                app = app.nest_service(
                    "/artwork",
                    tower_http::services::ServeDir::new(&artwork.folder),
                );
            }

            let app = app
                .layer(axum::middleware::from_fn(fix_content_types))
                .layer(CorsLayer::permissive())
                // Outermost, so it sees the status every other layer settled
                // on rather than the one the handler first produced.
                .layer(axum::middleware::from_fn(access_log))
                .with_state(state);

            // into_make_service_with_connect_info is what puts the client's
            // address within reach of the access log. Without it a request
            // carries no way to tell one viewer from another.
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await?;

            Ok(())
        }
    }
}

async fn stream(
    Path(filename): Path<String>,
    State(state): State<Arc<LineupState>>,
    request: axum::extract::Request,
) -> Result<impl IntoResponse, LineupError> {
    let number = filename
        .strip_suffix(".m3u8")
        .ok_or(LineupError::ChannelNotFound(filename.clone()))?;

    let channel = state
        .channels
        .iter()
        .find(|c| c.number() == number)
        .ok_or(LineupError::ChannelNotFound(number.to_owned()))?;

    // A viewer tuning in to a channel that keeps dying gets told so, rather than
    // waiting out the 30s ready timeout and being handed a playlist that will
    // stutter. Checked before spawning: while the backoff is running there is
    // deliberately no worker to wait for.
    {
        let health = state.health.lock().await.get(number);
        if health.is_failed() && !health.may_spawn_at(std::time::Instant::now()) {
            return Err(LineupError::ChannelFailed(number.to_owned()));
        }
    }

    let mut ready_receiver = {
        let mut active = state.active.lock().await;

        if let Some(channel_session) = active.get(number) {
            channel_session.subscribe_ready()
        } else {
            let channel_session = ChannelSession::spawn(
                channel,
                Arc::clone(&state.active),
                Arc::clone(&state.health),
            )?;
            let ready_receiver = channel_session.subscribe_ready();
            active.insert(number.to_owned(), channel_session);
            ready_receiver
        }
    };

    let wait = ready_receiver.wait_for(|&r| r);
    match tokio::time::timeout(READY_FILE_TIMEOUT, wait).await {
        Ok(Ok(_)) => {}
        Ok(Err(_)) => return Err(LineupError::ChannelNotReady), // child died
        Err(_) => return Err(LineupError::ChannelNotReady),     // 30s deadline
    }

    // Refresh the heartbeat before handing the playlist over. The worker stamps
    // it at startup, but the wait above can take up to READY_FILE_TIMEOUT, so
    // without this the client receives a multivariant playlist whose heartbeat
    // is already most of a minute old and starts its session with a fraction of
    // the idle window. Touching it here means the clock starts when the viewer
    // actually gets something to play.
    let heartbeat_file = channel.output_folder().join(HEARTBEAT_FILE_NAME);
    if let Err(err) = tokio::fs::write(&heartbeat_file, b"").await {
        log::warn!("failed to refresh heartbeat for channel {number}: {err}");
    }

    let content = get_multi_variant(channel, request.headers()).await;

    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "application/vnd.apple.mpegurl",
        )],
        content,
    ))
}

/// The commit and time the image was built from. The Dockerfile bakes both
/// into the environment; outside the image they are unset and read `unknown`.
#[derive(serde::Serialize, PartialEq, Debug)]
struct BuildStamp {
    revision: String,
    created: String,
}

impl BuildStamp {
    fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        let read = |name| {
            get(name)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "unknown".to_owned())
        };
        Self {
            revision: read("ETV_STATION_REVISION"),
            created: read("ETV_STATION_CREATED"),
        }
    }
}

/// Which commit this server was built from, readable without `docker inspect`.
async fn build_stamp() -> impl IntoResponse {
    axum::Json(BuildStamp::from_env(|name| std::env::var(name).ok()))
}

#[derive(serde::Serialize)]
struct ChannelHealthReport {
    channels: Vec<ChannelHealthRow>,
}

#[derive(serde::Serialize)]
struct ChannelHealthRow {
    number: String,
    name: String,
    /// A worker is running for this channel right now.
    active: bool,
    consecutive_failures: u32,
    /// Past `FAILURE_THRESHOLD`: `/channel/{number}.m3u8` answers 503 while
    /// `retry_in_secs` is non-zero.
    failed: bool,
    /// Seconds until a respawn is allowed; `0` when it is allowed now.
    retry_in_secs: u64,
    last_failure: Option<LastFailureRow>,
}

#[derive(serde::Serialize)]
struct LastFailureRow {
    #[serde(with = "time::serde::rfc3339")]
    at: time::OffsetDateTime,
    cause: String,
}

/// Every channel's failure accounting, so a sweep can find a channel sitting
/// behind the 503 guard without tuning in to it or reading logs.
async fn channel_health_report(State(state): State<Arc<LineupState>>) -> impl IntoResponse {
    let now = std::time::Instant::now();
    let active: Vec<String> = state.active.lock().await.keys().cloned().collect();
    let health = state.health.lock().await;
    let channels = state
        .channels
        .iter()
        .map(|channel| {
            let h = health.get(channel.number());
            ChannelHealthRow {
                number: channel.number().to_owned(),
                name: channel.name().to_owned(),
                active: active.iter().any(|n| n == channel.number()),
                consecutive_failures: h.consecutive_failures,
                failed: h.is_failed(),
                retry_in_secs: h
                    .retry_after
                    .map(|at| at.saturating_duration_since(now))
                    // Round up: a guard with half a second left still refuses.
                    .map(|left| left.as_secs() + u64::from(left.subsec_nanos() > 0))
                    .unwrap_or(0),
                last_failure: h.last_failure.map(|f| LastFailureRow {
                    at: f.at,
                    cause: f.cause,
                }),
            }
        })
        .collect();
    axum::Json(ChannelHealthReport { channels })
}

struct LineupState {
    channels: Vec<ChannelModel>,
    active: Arc<Mutex<HashMap<String, ChannelSession>>>,
    health: Arc<Mutex<crate::channel_health::HealthMap>>,
    /// Reported verbatim as the HDHomeRun `DeviceID`. See `ServerConfig::device_id`.
    device_id: String,
}

/// How much to write per request in the access log.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AccessLog {
    /// Nothing.
    Off,
    /// Client address, request, status, duration.
    On,
    /// Adds the client's port and the size of the answer.
    ///
    /// The port ties a request to one TCP connection, so two players behind the
    /// same address stay apart and a reconnect is visible as a new number. The
    /// size separates "we answered" from "we answered with the whole file" —
    /// a short body and a complete one both log `200` without it.
    ///
    /// Between them these are what an out-of-process packet capture was
    /// previously needed to read off the wire. They cost a header lookup, which
    /// is why they are not on by default.
    Verbose,
}

/// How much the access log records, from `ETV_ACCESS_LOG`.
///
/// On by default. A channel serves roughly one playlist poll and one segment
/// per second per viewer, so the line rate tracks viewer count and stays small
/// — but it is still work done on the request path, and this is the switch to
/// turn it off if it ever stops being worth it.
///
/// `off` / `0` / `false` / `no` silences it; `verbose` / `debug` / `2` widens
/// it; anything else, including unset, is the normal line.
static ACCESS_LOG: LazyLock<AccessLog> = LazyLock::new(|| match std::env::var("ETV_ACCESS_LOG") {
    Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
        "0" | "off" | "false" | "no" => AccessLog::Off,
        "verbose" | "debug" | "2" => AccessLog::Verbose,
        _ => AccessLog::On,
    },
    Err(_) => AccessLog::On,
});

/// Record who asked for what, and what they got.
///
/// The server otherwise keeps nothing about its viewers: a request touches a
/// zero-byte `.heartbeat` file and vanishes. That is enough to know somebody is
/// watching and useless for working out why one player froze while another on
/// the same channel played on. A line per request gives the client address, the
/// exact file, the status, and how long it took — the four things needed to
/// tell "the player stopped asking" from "the player asked and we failed it".
async fn access_log(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let level = *ACCESS_LOG;
    if level == AccessLog::Off {
        return next.run(request).await;
    }

    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let agent = request
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-")
        .to_owned();

    let started = Instant::now();
    let response = next.run(request).await;

    if level == AccessLog::Verbose {
        // A file served off disk carries Content-Length; a playlist built in a
        // handler has not been serialized yet and carries none, so fall back to
        // what the body already knows about itself. `-` means neither could
        // say, which for a streamed body is the honest answer.
        let length = response
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .or_else(|| response.body().size_hint().exact().map(|n| n.to_string()))
            .unwrap_or_else(|| "-".to_owned());

        log::info!(
            "access: {}:{} {} {} {} len={} {}ms \"{}\"",
            peer.ip(),
            peer.port(),
            method,
            path,
            response.status().as_u16(),
            length,
            started.elapsed().as_millis(),
            agent
        );
    } else {
        log::info!(
            "access: {} {} {} {} {}ms \"{}\"",
            peer.ip(),
            method,
            path,
            response.status().as_u16(),
            started.elapsed().as_millis(),
            agent
        );
    }

    response
}

/// `/logo/{number}.{ext}` — the image file the lineup named as that channel's
/// logo. 404 for a channel whose logo is a URL, or that has none.
async fn channel_logo(
    Path(filename): Path<String>,
    State(state): State<Arc<LineupState>>,
) -> axum::response::Response {
    let number = filename.split('.').next().unwrap_or_default();
    let Some(file) = state
        .channels
        .iter()
        .find(|c| c.number() == number)
        .and_then(|c| c.logo_file())
    else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let content_type = match file
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => "image/png",
    };
    match tokio::fs::read(file).await {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, content_type)], bytes).into_response(),
        Err(e) => {
            log::warn!("channel {number}: logo {} unreadable: {e}", file.display());
            axum::http::StatusCode::NOT_FOUND.into_response()
        }
    }
}

async fn fix_content_types(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let is_m3u8 = request.uri().path().ends_with(".m3u8");
    let mut response = next.run(request).await;
    if is_m3u8 && let Ok(value) = "application/vnd.apple.mpegurl".parse() {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, value);
    }
    response
}

async fn get_multi_variant(channel: &ChannelModel, headers: &HeaderMap) -> String {
    let mut result = String::new();
    result.push_str("#EXTM3U\n");
    result.push_str("#EXT-X-VERSION:6\n");

    // Only channels converting subtitles to WebVTT have a separate track to
    // offer, and only when the item airing right now asks for one — not any
    // item anywhere in the playout folder, which announced a track over films
    // that had none and served the client an empty cue file. `has_subtitle_track()` alone is the channel's static config — true
    // for any Convert-mode channel regardless of whether any item ever names
    // a subtitle selection. A channel whose items never opt into subtitles
    // (or haven't yet) would otherwise advertise a rendition that never gets
    // cues, and an old VLC (3.0.4, what iPlayTV on tvOS bundles) tears down
    // video along with it the first time it reads an empty cue file — #286.
    // Reading the same playout folder the channel worker itself plays from
    // means both sides agree without any new cross-process signalling: the
    // worker never stops serving `live_sub.m3u8` when Convert is set, so
    // whatever this check allows, the worker already provides.
    let has_subtitle_track = channel.has_subtitle_track()
        && ersatztv_playout::playout::schedule_requests_subtitle(
            channel.playout_folder(),
            time::OffsetDateTime::now_utc(),
        )
        .await;

    if has_subtitle_track {
        result.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{}\",DEFAULT={},AUTOSELECT=YES,FORCED=NO,LANGUAGE=\"{}\",URI=\"{}/session/{}/live_sub.m3u8\"\n",
            channel.subtitle_language_name(),
            if channel.subtitle_default_on() {
                "YES"
            } else {
                "NO"
            },
            channel.subtitle_language_tag(),
            get_scheme_host(headers),
            channel.number()
        ));
        result.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={},SUBTITLES=\"subs\"\n",
            channel.bandwidth_bps()
        ));
    } else {
        result.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={}\n",
            channel.bandwidth_bps()
        ));
    }
    result.push_str(&format!(
        "{}/session/{}/live.m3u8",
        get_scheme_host(headers),
        channel.number()
    ));

    result
}

async fn channel_playlist(
    State(state): State<Arc<LineupState>>,
    request: axum::extract::Request,
) -> Result<impl IntoResponse, LineupError> {
    let mut content = String::new();
    let scheme_host = get_scheme_host(request.headers());
    let xmltv_url = format!("{}/xmltv.xml", scheme_host);
    content.push_str(&format!(
        "#EXTM3U url-tvg=\"{xmltv_url}\" x-tvg-url=\"{xmltv_url}\"\n"
    ));
    for channel in &state.channels {
        let logo = channel
            .logo()
            .map(|l| crate::xmltv::resolve_icon_url(l, &scheme_host))
            .map(|l| format!(" tvg-logo=\"{l}\""))
            .unwrap_or(String::new());

        let group = channel
            .group()
            .map(|g| format!(" group-title=\"{g}\""))
            .unwrap_or(String::new());

        // TODO: kodiprop when user agent starts with "kodi"
        content.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"{}\" tvg-name=\"{}\"{}{}, {}\n",
            channel.tvg_id(),
            channel.name(),
            logo,
            group,
            channel.name()
        ));
        content.push_str(&format!("{}\n", stream_url(&request, channel)));
    }

    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/x-mpegurl")],
        content,
    ))
}

/// Where a client fetches this channel's stream. One definition, because the
/// M3U and `lineup.json` handing out different addresses for the same channel
/// fails in whichever surface nobody happened to test.
fn stream_url(request: &axum::extract::Request, channel: &ChannelModel) -> String {
    format!(
        "{}/channel/{}.m3u8",
        get_scheme_host(request.headers()),
        channel.number()
    )
}

/// Who this tuner claims to be. Plex fetches this first and takes `DeviceID` as
/// the tuner's identity; the rest is the shape of an HDHomeRun's own reply.
async fn hdhr_discover(
    State(state): State<Arc<LineupState>>,
    request: axum::extract::Request,
) -> impl IntoResponse {
    let base = get_scheme_host(request.headers());
    axum::Json(serde_json::json!({
        "FriendlyName": "ErsatzTV-next",
        "Manufacturer": "ErsatzTV",
        "ModelNumber": "HDTC-2US",
        "FirmwareName": "hdhomeruntc_atsc",
        "FirmwareVersion": "20190621",
        "DeviceID": state.device_id,
        "DeviceAuth": "",
        "BaseURL": base,
        "LineupURL": format!("{base}/lineup.json"),
        // Concurrent tuners Plex believes it may use. Channels are transcoded
        // on demand rather than pulled off real hardware, so this is a
        // permission ceiling, not a count of anything physical.
        "TunerCount": 4,
    }))
}

/// The channel list, in HDHomeRun's shape. `GuideNumber` must be the same
/// channel number the XMLTV guide uses or Plex treats the tuner and the guide
/// as unrelated and shows no schedule.
async fn hdhr_lineup(
    State(state): State<Arc<LineupState>>,
    request: axum::extract::Request,
) -> impl IntoResponse {
    let entries: Vec<serde_json::Value> = state
        .channels
        .iter()
        .map(|channel| {
            serde_json::json!({
                "GuideNumber": channel.number(),
                "GuideName": channel.name(),
                "URL": stream_url(&request, channel),
            })
        })
        .collect();

    axum::Json(entries)
}

/// Channel scanning state. Nothing here scans — the lineup is the config — so
/// this reports a box that is idle and has already found its channels.
async fn hdhr_lineup_status() -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "ScanInProgress": 0,
        "ScanPossible": 1,
        "Source": "Cable",
        "SourceList": ["Cable"],
    }))
}

pub(crate) fn get_scheme_host(headers: &HeaderMap) -> String {
    let scheme = get_first_header_value(headers, "x-forwarded-proto")
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| "http".to_string());

    let host = get_first_header_value(headers, "x-forwarded-host")
        .or_else(|| get_first_header_value(headers, axum::http::header::HOST.as_str()))
        .unwrap_or("localhost");

    format!("{scheme}://{host}")
}

// headers set by chained proxies may contain multiple comma-separated values;
// the first value is the one set by the proxy closest to the client
fn get_first_header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// The channel's last written media playlist with `#EXT-X-ENDLIST` appended, or
/// `None` when there is nothing on disk to end.
///
/// Appended rather than synthesised: the segments listed are ones the player has
/// already been fetching, so ending that exact list lets it drain what it has
/// and stop cleanly. A synthetic empty playlist would instead make it discard
/// buffered content and cut off mid-programme.
async fn ended_playlist_response(
    channel_config: &ChannelModel,
    request_path: &str,
) -> Option<axum::response::Response> {
    let file_name = request_path.rsplit('/').next()?;
    let playlist_path = channel_config.output_folder().join(file_name);
    let body = tokio::fs::read_to_string(&playlist_path).await.ok()?;
    if body.contains("#EXT-X-ENDLIST") {
        return None;
    }
    let mut ended = body;
    if !ended.ends_with('\n') {
        ended.push('\n');
    }
    ended.push_str("#EXT-X-ENDLIST\n");
    Some(
        (
            [(
                axum::http::header::CONTENT_TYPE,
                "application/vnd.apple.mpegurl",
            )],
            ended,
        )
            .into_response(),
    )
}

async fn session_middleware(
    State(state): State<Arc<LineupState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // touch heartbeat file for channel
    let path = request.uri().path();
    if path.ends_with(".ts") || path.ends_with(".m3u8") {
        let split: Vec<&str> = request.uri().path().split('/').collect();
        let channel_number = split[1];
        if let Some(channel_config) = state.channels.iter().find(|c| c.number() == channel_number) {
            let mut active = state.active.lock().await;

            // A player that already holds the multi-variant playlist only ever
            // polls this path — it never requests /channel/{number}.m3u8 again,
            // which is the only other place a worker gets spawned. So when a
            // worker exits (stalled, or idled out just as a viewer returned),
            // nothing would bring the channel back while someone is still
            // watching. Respawn it here instead.
            let health = state.health.lock().await.get(channel_number);
            let spawn_allowed = health.may_spawn_at(std::time::Instant::now());

            if !active.contains_key(channel_number) && spawn_allowed {
                match ChannelSession::spawn(
                    channel_config,
                    Arc::clone(&state.active),
                    Arc::clone(&state.health),
                ) {
                    Ok(channel_session) => {
                        log::debug!("respawning channel {channel_number} for waiting viewer");
                        active.insert(channel_number.to_owned(), channel_session);
                    }
                    Err(err) => log::error!("failed to respawn channel {channel_number}: {err}"),
                }
            }

            // A channel that has failed every attempt is not coming back on this
            // poll, and the viewer holding the multivariant playlist has no way
            // to learn that: they only ever fetch this path, and a missing file
            // reads as an ordinary gap. End their stream instead, so the player
            // reports end-of-stream rather than spinning forever. Retries carry
            // on in the background — if the cause clears, a fresh tune-in works.
            if health.is_failed() && !spawn_allowed && path.ends_with(".m3u8") {
                drop(active);
                if let Some(response) = ended_playlist_response(channel_config, path).await {
                    log::debug!(
                        "channel {channel_number} is failed; serving ended playlist to viewer"
                    );
                    return response;
                }
                return LineupError::ChannelFailed(channel_number.to_owned()).into_response();
            }

            let heartbeat_file = channel_config.output_folder().join(HEARTBEAT_FILE_NAME);

            let mut exists = heartbeat_file.exists();
            if !exists {
                exists = tokio::fs::write(&heartbeat_file, b"").await.is_ok();
            }

            if exists {
                let _ = filetime::set_file_mtime(&heartbeat_file, filetime::FileTime::now());
            }
        }
    }

    next.run(request).await
}

#[cfg(test)]
mod get_multi_variant_tests {
    use ersatztv::config::ChannelConfig;
    use ersatztv_playout::playout::{Playout, PlayoutItem, PlayoutItemSource};
    use time::OffsetDateTime;

    use super::*;

    async fn convert_mode_channel(dir: &std::path::Path) -> ChannelModel {
        channel_with_subtitle(dir, r#"{"mode":"convert"}"#).await
    }

    async fn channel_with_subtitle(dir: &std::path::Path, subtitle_json: &str) -> ChannelModel {
        std::fs::create_dir(dir.join("playout")).unwrap();
        let cfg_path = dir.join("channel.json");
        std::fs::write(
            &cfg_path,
            format!(
                r#"{{"playout":{{"folder":"playout"}},"normalization":{{"subtitle":{subtitle_json}}}}}"#
            ),
        )
        .unwrap();
        let cfg = ChannelConfig {
            number: "1".into(),
            name: "ETV 1".into(),
            config: cfg_path.to_string_lossy().into_owned(),
            overlays: Vec::new(),
            tvg_id: None,
            logo: None,
            group: None,
        };
        ChannelModel::new(&cfg_path, dir, cfg).await.unwrap()
    }

    fn write_playout_chunk(dir: &std::path::Path, items: Vec<PlayoutItem>) {
        let playout = Playout::new(items);
        std::fs::write(
            dir.join("playout").join("chunk.json"),
            serde_json::to_string(&playout).unwrap(),
        )
        .unwrap();
    }

    fn item_wanting_subtitle(wants_subtitle: bool) -> PlayoutItem {
        let start = OffsetDateTime::now_utc();
        let finish = start + time::Duration::minutes(5);
        let mut item = PlayoutItem::scheduled(
            "item-1".into(),
            start,
            finish,
            PlayoutItemSource::local("/media/file.mkv".into(), None, None),
        );
        if wants_subtitle {
            item.tracks = Some(ersatztv_playout::playout::PlayoutItemTracks {
                audio: None,
                video: None,
                subtitle: Some(ersatztv_playout::playout::TrackSelection {
                    source: None,
                    stream_index: Some(2),
                }),
            });
        }
        item
    }

    // #286: a Convert-mode channel whose schedule never requests a subtitle
    // track must not advertise one — that's exactly the case where the
    // subtitle rendition is present but every cue file it would serve is
    // empty, which kills an old bundled-VLC player outright.
    #[tokio::test]
    async fn convert_mode_with_no_item_requesting_subtitles_does_not_advertise() {
        let dir = tempfile::tempdir().unwrap();
        let channel = convert_mode_channel(dir.path()).await;
        write_playout_chunk(dir.path(), vec![item_wanting_subtitle(false)]);

        let result = get_multi_variant(&channel, &HeaderMap::new()).await;

        assert!(!result.contains("TYPE=SUBTITLES"), "{result}");
        assert!(!result.contains("SUBTITLES=\"subs\""), "{result}");
    }

    #[tokio::test]
    async fn convert_mode_with_an_item_requesting_subtitles_advertises() {
        let dir = tempfile::tempdir().unwrap();
        let channel = convert_mode_channel(dir.path()).await;
        write_playout_chunk(
            dir.path(),
            vec![item_wanting_subtitle(false), item_wanting_subtitle(true)],
        );

        let result = get_multi_variant(&channel, &HeaderMap::new()).await;

        assert!(result.contains("TYPE=SUBTITLES"), "{result}");
        assert!(result.contains("SUBTITLES=\"subs\""), "{result}");
    }

    #[tokio::test]
    async fn the_rendition_is_not_the_default_unless_the_channel_asks() {
        let dir = tempfile::tempdir().unwrap();
        let channel = convert_mode_channel(dir.path()).await;
        write_playout_chunk(dir.path(), vec![item_wanting_subtitle(true)]);

        let result = get_multi_variant(&channel, &HeaderMap::new()).await;

        assert!(
            result.contains("DEFAULT=NO,AUTOSELECT=YES"),
            "a rendition left alone must stay off: {result}"
        );
    }

    #[tokio::test]
    async fn default_on_announces_the_rendition_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let channel =
            channel_with_subtitle(dir.path(), r#"{"mode":"convert","default_on":true}"#).await;
        write_playout_chunk(dir.path(), vec![item_wanting_subtitle(true)]);

        let result = get_multi_variant(&channel, &HeaderMap::new()).await;

        assert!(
            result.contains("DEFAULT=YES,AUTOSELECT=YES"),
            "HLS requires AUTOSELECT=YES alongside DEFAULT=YES: {result}"
        );
    }

    #[tokio::test]
    async fn convert_mode_with_no_playout_folder_contents_does_not_advertise() {
        let dir = tempfile::tempdir().unwrap();
        let channel = convert_mode_channel(dir.path()).await;

        let result = get_multi_variant(&channel, &HeaderMap::new()).await;

        assert!(!result.contains("TYPE=SUBTITLES"), "{result}");
    }

    #[test]
    fn build_stamp_reads_the_baked_environment() {
        let stamp = BuildStamp::from_env(|name| match name {
            "ETV_STATION_REVISION" => Some("a4d013b".to_owned()),
            "ETV_STATION_CREATED" => Some("2026-08-22T06:11:23Z".to_owned()),
            _ => None,
        });
        assert_eq!(stamp.revision, "a4d013b");
        assert_eq!(stamp.created, "2026-08-22T06:11:23Z");
    }

    #[test]
    fn build_stamp_outside_the_image_is_unknown() {
        let stamp = BuildStamp::from_env(|_| None);
        assert_eq!(stamp.revision, "unknown");
        assert_eq!(stamp.created, "unknown");
    }
}

// `tools/channel-health.sh` reads `.channels[]`, `.failed`, `.retry_in_secs`
// and `.last_failure.{at,cause}` off this endpoint; renaming one of them
// breaks the sweep without any compile error.
#[cfg(test)]
mod channel_health_report_tests {
    use ersatztv::config::ChannelConfig;

    use super::*;

    async fn lineup_with_channel(dir: &std::path::Path) -> Arc<LineupState> {
        std::fs::create_dir(dir.join("playout")).unwrap();
        let cfg_path = dir.join("channel.json");
        std::fs::write(&cfg_path, r#"{"playout":{"folder":"playout"}}"#).unwrap();
        let cfg = ChannelConfig {
            number: "1".into(),
            name: "ETV 1".into(),
            config: cfg_path.to_string_lossy().into_owned(),
            overlays: Vec::new(),
            tvg_id: None,
            logo: None,
            group: None,
        };
        let channel = ChannelModel::new(&cfg_path, dir, cfg).await.unwrap();
        Arc::new(LineupState {
            channels: vec![channel],
            active: Arc::new(Mutex::new(HashMap::new())),
            health: Arc::new(Mutex::new(crate::channel_health::HealthMap::default())),
            device_id: String::new(),
        })
    }

    async fn report(state: &Arc<LineupState>) -> serde_json::Value {
        let response = channel_health_report(State(state.clone()))
            .await
            .into_response();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn healthy_channel_reports_every_key_the_sweep_reads() {
        let dir = tempfile::tempdir().unwrap();
        let state = lineup_with_channel(dir.path()).await;

        let json = report(&state).await;

        let row = &json["channels"][0];
        assert_eq!(row["number"], "1");
        assert_eq!(row["name"], "ETV 1");
        assert_eq!(row["active"], false);
        assert_eq!(row["consecutive_failures"], 0);
        assert_eq!(row["failed"], false);
        assert_eq!(row["retry_in_secs"], 0);
        assert!(row["last_failure"].is_null());
    }

    #[tokio::test]
    async fn failed_channel_rounds_retry_up_and_names_its_last_failure() {
        let dir = tempfile::tempdir().unwrap();
        let state = lineup_with_channel(dir.path()).await;
        {
            let mut health = state.health.lock().await;
            for _ in 0..3 {
                health.record_exit(
                    "1",
                    true,
                    true,
                    Duration::from_secs(3),
                    Instant::now(),
                    "no segments".to_owned(),
                );
            }
        }

        let json = report(&state).await;

        // The third failure sets a 30s guard; microseconds have passed by the
        // time the handler reads it, so a floor would report 29.
        let row = &json["channels"][0];
        assert_eq!(row["consecutive_failures"], 3);
        assert_eq!(row["failed"], true);
        assert_eq!(row["retry_in_secs"], 30);
        assert_eq!(row["last_failure"]["cause"], "no segments");
        assert!(row["last_failure"]["at"].is_string());
    }
}
