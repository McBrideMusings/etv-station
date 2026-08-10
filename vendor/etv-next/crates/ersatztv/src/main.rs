mod channel_health;
mod channel_model;
mod channel_session;
mod scaffold;
mod xmltv;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use axum::body::HttpBody as _;
use axum::extract::{ConnectInfo, Path, State};
use axum::response::IntoResponse;
use axum::{Router, routing::get};
use clap::{Parser, Subcommand};
use ersatztv::error::LineupError;
use ersatztv_core::{HEARTBEAT_FILE_NAME, READY_FILE_TIMEOUT, empty_folder};
use tokio::signal;
use tokio::sync::Mutex;
use tower::ServiceBuilder;
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
                match ChannelModel::new(&lineup_path, &output_folder, channel) {
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

            let addr = format!(
                "{}:{}",
                lineup_config.server.bind_address, lineup_config.server.port
            );

            let listener = tokio::net::TcpListener::bind(addr).await?;

            let app = Router::new()
                .route("/channel/{filename}", get(stream))
                .route("/channels.m3u", get(channel_playlist))
                .route("/xmltv.xml", get(crate::xmltv::xmltv_epg))
                // Plex has no M3U tuner type — of its four device kinds only
                // `hdhomerun` is both a live-TV protocol and publicly
                // implementable — so a server that wants Plex to carry its
                // channels answers as an HDHomeRun box. These are the three
                // addresses Plex asks for.
                .route("/discover.json", get(hdhr_discover))
                .route("/lineup.json", get(hdhr_lineup))
                .route("/lineup_status.json", get(hdhr_lineup_status))
                .nest_service(
                    "/session",
                    ServiceBuilder::new()
                        .layer(axum::middleware::from_fn_with_state(
                            Arc::clone(&state),
                            session_middleware,
                        ))
                        .service(tower_http::services::ServeDir::new(&output_folder)),
                )
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

    let content = get_multi_variant(channel, request);

    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "application/vnd.apple.mpegurl",
        )],
        content,
    ))
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

fn get_multi_variant(channel: &ChannelModel, request: axum::extract::Request) -> String {
    let mut result = String::new();
    result.push_str("#EXTM3U\n");
    result.push_str("#EXT-X-VERSION:6\n");

    // Only channels converting subtitles to WebVTT have a separate track to
    // offer. A burned-in channel has the words inside the video picture, so
    // announcing a rendition here would point players at a subtitle playlist
    // listing .vtt files that were never written.
    if channel.has_subtitle_track() {
        result.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{}\",DEFAULT=NO,AUTOSELECT=YES,FORCED=NO,LANGUAGE=\"{}\",URI=\"{}/session/{}/live_sub.m3u8\"\n",
            channel.subtitle_language_name(),
            channel.subtitle_language_tag(),
            get_scheme_host(&request),
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
        get_scheme_host(&request),
        channel.number()
    ));

    result
}

async fn channel_playlist(
    State(state): State<Arc<LineupState>>,
    request: axum::extract::Request,
) -> Result<impl IntoResponse, LineupError> {
    let mut content = String::new();
    let xmltv_url = format!("{}/xmltv.xml", get_scheme_host(&request));
    content.push_str(&format!(
        "#EXTM3U url-tvg=\"{xmltv_url}\" x-tvg-url=\"{xmltv_url}\"\n"
    ));
    for channel in &state.channels {
        let logo = channel
            .logo()
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
        get_scheme_host(request),
        channel.number()
    )
}

/// Who this tuner claims to be. Plex fetches this first and takes `DeviceID` as
/// the tuner's identity; the rest is the shape of an HDHomeRun's own reply.
async fn hdhr_discover(
    State(state): State<Arc<LineupState>>,
    request: axum::extract::Request,
) -> impl IntoResponse {
    let base = get_scheme_host(&request);
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

fn get_scheme_host(request: &axum::extract::Request) -> String {
    // TODO: need scheme, host from reverse proxy
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");

    format!("http://{host}")
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
