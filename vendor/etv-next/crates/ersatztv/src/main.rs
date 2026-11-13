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
use ersatztv_core::{HEARTBEAT_FILE_NAME, READY_FILE_TIMEOUT, empty_output_root};
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

            empty_output_root(&output_folder).await?;

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
            // `empty_output_root` above stays as it is — a cold start with no viewers
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

            // Station-level pre-warm: respawn any channel whose worker is gone,
            // so the first tune is immediate. Staying up with nobody watching
            // is the worker's idle grace, which the lineup's writer raises.
            if lineup_config.server.prewarm {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(20));
                    loop {
                        interval.tick().await;
                        crate::channel_session::prewarm_sweep(
                            &state.channels,
                            &state.active,
                            &state.health,
                        )
                        .await;
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
                // The continuous MPEG-TS stream lineup.json hands to Plex.
                // Video, so after the compression layer like /session.
                .route("/stream/{filename}", get(ts_stream))
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

    let channel = tune_in(&state, number).await?;

    let content = get_multi_variant(channel, request.headers()).await;

    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "application/vnd.apple.mpegurl",
        )],
        content,
    ))
}

/// Start the channel's worker if none is running, wait until it is ready, and
/// refresh its heartbeat — the step every way of tuning in shares.
async fn tune_in<'a>(
    state: &'a LineupState,
    number: &str,
) -> Result<&'a ChannelModel, LineupError> {
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

    Ok(channel)
}

/// How long the TS stream waits for the worker to list a new segment before it
/// ends the response. Three 4 s segments: past that the worker has stopped or
/// died, and the client's reconnect (`-reconnect_streamed`) tunes in afresh.
const TS_STREAM_STALL: Duration = Duration::from_secs(12);

/// How often the TS stream re-reads the media playlist. The worker signals
/// through files, never IPC (see AGENTS.md); an HLS player follows the same
/// file the same way.
const TS_STREAM_POLL: Duration = Duration::from_millis(500);

/// How long one segment write may wait on a client that stopped reading. Well
/// under the worker's 90 s heartbeat timeout, so a stalled client ends its own
/// stream rather than leaving the heartbeat unrefreshed until the worker idles out.
const TS_STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// One channel as a single, continuous MPEG-TS byte stream: the segments the
/// worker lists in `live.m3u8`, written back to back as each one appears.
///
/// This is what an HDHomeRun serves, and what Plex's tuner code expects: Plex
/// opens a tuner URL with mpegts demuxer options (`-scan_all_pmts`), which an
/// HLS playlist URL rejects outright (`Option scan_all_pmts not found`).
/// Within one worker run plain concatenation is a valid stream, because the
/// worker offsets every item's timestamps to continue the last
/// (`-output_ts_offset`). A new run restarts timestamps at zero, so the
/// response ends there and the client's reconnect tunes in to the new run.
async fn ts_stream(
    Path(filename): Path<String>,
    State(state): State<Arc<LineupState>>,
) -> Result<impl IntoResponse, LineupError> {
    let number = filename
        .strip_suffix(".ts")
        .ok_or(LineupError::ChannelNotFound(filename.clone()))?
        .to_owned();

    let channel = tune_in(&state, &number).await?;
    let output_folder = channel.output_folder().to_owned();
    log::info!("ts stream started: channel={number}");

    let (mut writer, reader) = tokio::io::duplex(1 << 20);
    let active = Arc::clone(&state.active);
    tokio::spawn(async move {
        let started = Instant::now();
        let worker_alive = || {
            let (active, number) = (Arc::clone(&active), number.clone());
            async move { active.lock().await.contains_key(&number) }
        };
        let (segments, bytes, reason) =
            write_ts_stream(&output_folder, &mut writer, worker_alive).await;
        log::info!(
            "ts stream ended: channel={number} segments={segments} bytes={bytes} secs={} reason={reason}",
            started.elapsed().as_secs()
        );
    });

    Ok((
        [(axum::http::header::CONTENT_TYPE, "video/mp2t")],
        axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(reader)),
    ))
}

/// Follow `live.m3u8` in `output_folder` and copy each newly listed segment
/// into `out`, starting with the newest two so a client gets data at once,
/// and keep the worker's heartbeat fresh while `worker_alive` says it runs.
/// Returns segments and bytes written and why it stopped.
async fn write_ts_stream<Alive>(
    output_folder: &std::path::Path,
    out: &mut (impl tokio::io::AsyncWrite + Unpin),
    worker_alive: impl Fn() -> Alive,
) -> (u64, u64, &'static str)
where
    Alive: std::future::Future<Output = bool>,
{
    use tokio::io::AsyncWriteExt as _;

    let playlist = output_folder.join("live.m3u8");
    let heartbeat = output_folder.join(HEARTBEAT_FILE_NAME);
    let mut last_sent: Option<String> = None;
    let mut last_progress = Instant::now();
    let (mut segments, mut bytes) = (0u64, 0u64);

    loop {
        if !worker_alive().await {
            return (segments, bytes, "worker exited");
        }
        // A TS client never touches /session, so nothing else keeps the
        // worker from idling out under it.
        let _ = filetime::set_file_mtime(&heartbeat, filetime::FileTime::now());

        let listed = tokio::fs::read_to_string(&playlist)
            .await
            .map(|body| segment_uris(&body))
            .unwrap_or_default();

        let owed = match unsent_segments(&listed, last_sent.as_deref()) {
            Owed::Segments(owed) => owed,
            Owed::NewRun => return (segments, bytes, "worker restarted"),
            Owed::LostPlace => return (segments, bytes, "last segment left the playlist"),
        };
        for uri in owed {
            let Ok(data) = tokio::fs::read(output_folder.join(uri)).await else {
                // Sending the next one would leave an unmarked hole.
                return (segments, bytes, "listed segment missing on disk");
            };
            match tokio::time::timeout(TS_STREAM_WRITE_TIMEOUT, out.write_all(&data)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return (segments, bytes, "client closed"),
                Err(_) => return (segments, bytes, "client stopped reading"),
            }
            segments += 1;
            bytes += data.len() as u64;
            last_sent = Some(uri.to_owned());
            last_progress = Instant::now();
        }

        if last_progress.elapsed() > TS_STREAM_STALL {
            return (segments, bytes, "no new segment");
        }
        tokio::time::sleep(TS_STREAM_POLL).await;
    }
}

/// The segment URIs a media playlist lists, in order.
fn segment_uris(playlist: &str) -> Vec<String> {
    playlist
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// What a TS stream should do next with the playlist it just read.
#[derive(Debug, PartialEq)]
enum Owed<'a> {
    /// Write these, in order (possibly none yet).
    Segments(&'a [String]),
    /// The next segment belongs to a new worker run, whose timestamps restart.
    NewRun,
    /// The last segment sent is no longer listed, so where to continue is unknown.
    LostPlace,
}

/// The worker run a segment URI belongs to: its `r<id>-NNNN` directory.
fn segment_run(uri: &str) -> &str {
    uri.split_once('/').map_or("", |(run, _)| run)
}

/// What a stream that last sent `last_sent` owes from `listed`. A new stream
/// starts two from the end of the newest run.
fn unsent_segments<'a>(listed: &'a [String], last_sent: Option<&str>) -> Owed<'a> {
    let Some(sent) = last_sent else {
        let newest_run = listed.last().map_or("", |uri| segment_run(uri));
        let run_start = listed
            .iter()
            .position(|uri| segment_run(uri) == newest_run)
            .unwrap_or(0);
        let from = run_start.max(listed.len().saturating_sub(2));
        return Owed::Segments(&listed[from..]);
    };
    let Some(index) = listed.iter().position(|uri| uri == sent) else {
        return Owed::LostPlace;
    };
    let owed = &listed[index + 1..];
    match owed.first() {
        Some(next) if segment_run(next) != segment_run(sent) => Owed::NewRun,
        _ => Owed::Segments(owed),
    }
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
    // offer, and only when the item airing right now asks for one; an item
    // elsewhere in the playout folder says nothing about the cue file this
    // client is about to fetch. `has_subtitle_track()` alone is the channel's
    // static config — true for any Convert-mode channel regardless of whether any item ever names
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

/// Where an IPTV player fetches this channel's HLS stream (`channels.m3u`).
/// HDHomeRun clients get `tuner_url` instead; each surface has one definition
/// so no caller builds a stream address by hand.
fn stream_url(request: &axum::extract::Request, channel: &ChannelModel) -> String {
    format!(
        "{}/channel/{}.m3u8",
        get_scheme_host(request.headers()),
        channel.number()
    )
}

/// Where an HDHomeRun client (Plex) fetches this channel: the continuous
/// MPEG-TS stream, because Plex cannot open an HLS URL as a tuner.
fn tuner_url(request: &axum::extract::Request, channel: &ChannelModel) -> String {
    format!(
        "{}/stream/{}.ts",
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
                "URL": tuner_url(&request, channel),
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

    // a reverse proxy that strips a path prefix before forwarding (Caddy
    // `handle_path /iptv/*`) reports it here; without it every absolute URL
    // handed out would point outside the prefix
    let prefix = get_first_header_value(headers, "x-forwarded-prefix")
        .filter(|p| p.starts_with('/'))
        .map(|p| p.trim_end_matches('/'))
        .unwrap_or("");

    format!("{scheme}://{host}{prefix}")
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

#[cfg(test)]
mod forwarded_prefix_tests {
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

    async fn channels_m3u(prefix: Option<&str>) -> String {
        let dir = tempfile::tempdir().unwrap();
        let state = lineup_with_channel(dir.path()).await;
        let mut req = axum::http::Request::builder()
            .header("host", "internal:8409")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "ersatz.example.com");
        if let Some(p) = prefix {
            req = req.header("x-forwarded-prefix", p);
        }
        let request = req.body(axum::body::Body::empty()).unwrap();
        let response = channel_playlist(State(state), request)
            .await
            .unwrap()
            .into_response();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn prefix_header_lands_in_stream_and_guide_urls() {
        let m3u = channels_m3u(Some("/iptv/")).await;
        assert!(m3u.contains("url-tvg=\"https://ersatz.example.com/iptv/xmltv.xml\""));
        assert!(m3u.contains("\nhttps://ersatz.example.com/iptv/channel/1.m3u8\n"));
    }

    #[tokio::test]
    async fn without_the_header_urls_are_unchanged() {
        let m3u = channels_m3u(None).await;
        assert!(m3u.contains("url-tvg=\"https://ersatz.example.com/xmltv.xml\""));
        assert!(m3u.contains("\nhttps://ersatz.example.com/channel/1.m3u8\n"));
    }

    #[tokio::test]
    async fn a_prefix_not_starting_with_slash_is_ignored() {
        let m3u = channels_m3u(Some("iptv")).await;
        assert!(m3u.contains("\nhttps://ersatz.example.com/channel/1.m3u8\n"));
    }
}

#[cfg(test)]
mod ts_stream_tests {
    use tokio::io::AsyncReadExt as _;

    use super::*;

    fn uris(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn a_new_stream_starts_two_from_the_live_edge_of_the_newest_run() {
        let listed = uris(&["r1/a.ts", "r1/b.ts", "r1/c.ts"]);
        assert_eq!(
            unsent_segments(&listed, None),
            Owed::Segments(&uris(&["r1/b.ts", "r1/c.ts"]))
        );
        let restarted = uris(&["r1/a.ts", "r1/b.ts", "r2/a.ts"]);
        assert_eq!(
            unsent_segments(&restarted, None),
            Owed::Segments(&uris(&["r2/a.ts"]))
        );
        let one = uris(&["r1/a.ts"]);
        assert_eq!(unsent_segments(&one, None), Owed::Segments(&one));
        assert_eq!(unsent_segments(&[], None), Owed::Segments(&[]));
    }

    #[test]
    fn a_running_stream_gets_only_what_follows_its_last_segment() {
        let listed = uris(&["r1/a.ts", "r1/b.ts", "r1/c.ts"]);
        assert_eq!(
            unsent_segments(&listed, Some("r1/a.ts")),
            Owed::Segments(&uris(&["r1/b.ts", "r1/c.ts"]))
        );
        assert_eq!(
            unsent_segments(&listed, Some("r1/c.ts")),
            Owed::Segments(&[])
        );
    }

    #[test]
    fn a_new_run_or_a_lost_place_ends_the_stream() {
        let listed = uris(&["r1/a.ts", "r2/a.ts"]);
        assert_eq!(unsent_segments(&listed, Some("r1/a.ts")), Owed::NewRun);
        assert_eq!(unsent_segments(&listed, Some("r0/z.ts")), Owed::LostPlace);
    }

    #[test]
    fn segment_uris_skips_tags_and_blank_lines() {
        let playlist = "#EXTM3U\n#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\nr1/live1.ts\n\n#EXTINF:4.0,\nr1/live2.ts\n";
        assert_eq!(
            segment_uris(playlist),
            uris(&["r1/live1.ts", "r1/live2.ts"])
        );
    }

    /// A folder with segments `r1/live{1..=n}.ts` holding four bytes each.
    fn segment_folder(n: u32) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("r1")).unwrap();
        for i in 1..=n {
            let body = [b'A' + (i as u8 - 1); 4];
            std::fs::write(dir.path().join(format!("r1/live{i}.ts")), body).unwrap();
        }
        std::fs::write(dir.path().join(HEARTBEAT_FILE_NAME), b"").unwrap();
        dir
    }

    fn write_playlist(folder: &std::path::Path, uris: &[&str]) {
        let body: String = uris
            .iter()
            .map(|u| format!("#EXTINF:4.0,\n{u}\n"))
            .collect();
        std::fs::write(folder.join("live.m3u8"), body).unwrap();
    }

    #[tokio::test]
    async fn the_stream_is_the_segments_back_to_back_as_they_appear() {
        let dir = segment_folder(5);
        let folder = dir.path().to_owned();
        write_playlist(&folder, &["r1/live1.ts", "r1/live2.ts", "r1/live3.ts"]);
        let old = filetime::FileTime::from_unix_time(1_000_000, 0);
        filetime::set_file_mtime(folder.join(HEARTBEAT_FILE_NAME), old).unwrap();

        let (mut writer, mut reader) = tokio::io::duplex(64);
        let task_folder = folder.clone();
        let task = tokio::spawn(async move {
            write_ts_stream(&task_folder, &mut writer, || async { true }).await
        });

        let mut first = [0u8; 8];
        reader.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"BBBBCCCC");
        let touched = std::fs::metadata(folder.join(HEARTBEAT_FILE_NAME)).unwrap();
        assert!(filetime::FileTime::from_last_modification_time(&touched) > old);

        write_playlist(
            &folder,
            &["r1/live1.ts", "r1/live2.ts", "r1/live3.ts", "r1/live4.ts"],
        );
        let mut next = [0u8; 4];
        reader.read_exact(&mut next).await.unwrap();
        assert_eq!(&next, b"DDDD");

        drop(reader);
        write_playlist(&folder, &["r1/live3.ts", "r1/live4.ts", "r1/live5.ts"]);
        assert_eq!(task.await.unwrap(), (3, 12, "client closed"));
    }

    #[tokio::test]
    async fn the_stream_ends_where_a_new_worker_run_begins() {
        let dir = segment_folder(2);
        let folder = dir.path().to_owned();
        write_playlist(&folder, &["r1/live1.ts", "r1/live2.ts"]);

        let (mut writer, mut reader) = tokio::io::duplex(64);
        let task_folder = folder.clone();
        let task = tokio::spawn(async move {
            write_ts_stream(&task_folder, &mut writer, || async { true }).await
        });
        let mut first = [0u8; 8];
        reader.read_exact(&mut first).await.unwrap();

        std::fs::create_dir(folder.join("r2")).unwrap();
        std::fs::write(folder.join("r2/live1.ts"), b"ZZZZ").unwrap();
        write_playlist(&folder, &["r1/live1.ts", "r1/live2.ts", "r2/live1.ts"]);
        assert_eq!(task.await.unwrap(), (2, 8, "worker restarted"));
    }

    #[tokio::test]
    async fn the_stream_ends_when_the_worker_exits_or_a_segment_is_missing() {
        let dir = segment_folder(1);
        write_playlist(dir.path(), &["r1/live1.ts"]);
        let (mut writer, _reader) = tokio::io::duplex(64);
        let ended = write_ts_stream(dir.path(), &mut writer, || async { false }).await;
        assert_eq!(ended, (0, 0, "worker exited"));

        write_playlist(dir.path(), &["r1/live1.ts", "r1/gone.ts"]);
        let ended = write_ts_stream(dir.path(), &mut writer, || async { true }).await;
        assert_eq!(ended, (1, 4, "listed segment missing on disk"));
    }

    #[tokio::test]
    async fn lineup_json_hands_plex_the_ts_stream_and_the_m3u_keeps_hls() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("playout")).unwrap();
        let cfg_path = dir.path().join("channel.json");
        std::fs::write(&cfg_path, r#"{"playout":{"folder":"playout"}}"#).unwrap();
        let cfg = ersatztv::config::ChannelConfig {
            number: "7".into(),
            name: "ETV 7".into(),
            config: cfg_path.to_string_lossy().into_owned(),
            overlays: Vec::new(),
            tvg_id: None,
            logo: None,
            group: None,
        };
        let channel = ChannelModel::new(&cfg_path, dir.path(), cfg).await.unwrap();
        let state = Arc::new(LineupState {
            channels: vec![channel],
            active: Arc::new(Mutex::new(HashMap::new())),
            health: Arc::new(Mutex::new(crate::channel_health::HealthMap::default())),
            device_id: String::new(),
        });
        let request = || {
            axum::http::Request::builder()
                .header("host", "tuner:8409")
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let body = |response: axum::response::Response| async move {
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        };

        let lineup = body(
            hdhr_lineup(State(Arc::clone(&state)), request())
                .await
                .into_response(),
        )
        .await;
        let entries: serde_json::Value = serde_json::from_str(&lineup).unwrap();
        assert_eq!(entries[0]["URL"], "http://tuner:8409/stream/7.ts");

        let m3u = body(
            channel_playlist(State(state), request())
                .await
                .unwrap()
                .into_response(),
        )
        .await;
        assert!(m3u.contains("\nhttp://tuner:8409/channel/7.m3u8\n"));
    }
}
