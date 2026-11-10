//! `POST /mcp`: the dashboard as an MCP server, for an agent that would
//! otherwise build the page's URLs and JSON by hand.
//!
//! Every tool is one row of [`TOOLS`] and one dashboard route: the handler
//! turns the tool's arguments into that route's path, query and body, calls the
//! route over loopback HTTP on this process's own port, and hands back the
//! route's answer. The router ([`super::dispatch`]) and the state it owns stay
//! single-threaded and unaware of MCP; a tool is exactly what the page would
//! have sent.
//!
//! # Seam
//!
//! `serve()`'s accept loop is unchanged. A request for `/mcp` leaves that
//! thread for a short-lived one ([`Mcp::serve`]) which runs rmcp's
//! `StreamableHttpService` on a shared tokio runtime and writes the reply, so a
//! tool waiting out a simulation never holds the page. The service is
//! stateless (no sessions) and answers plain JSON.
//!
//! Host headers are not checked: the deployed dashboard is reached by the
//! Unraid host's name, and the page it serves has no check either.
//!
//! # Results
//!
//! An HTTP status of 400 or more sets `isError` on the tool result and the
//! status leads its text. A `GET` tool re-asks every [`POLL`] while the route
//! answers 202 (a simulation is running), until it answers something else or
//! `ETV_MCP_WAIT` seconds (default 900) pass; a `POST` is never sent twice.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use rmcp::ErrorData;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};

/// The path the endpoint answers on.
pub(super) const PATH: &str = "/mcp";

/// A request body larger than this is refused with 413 before it is parsed.
const MAX_BODY: u64 = 1024 * 1024;

/// How often a waiting `GET` tool asks again.
const POLL: Duration = Duration::from_secs(2);

/// `ETV_MCP_WAIT` is clamped to a day, so a mistyped value cannot overflow a deadline.
const MAX_WAIT_SECS: u64 = 24 * 60 * 60;

/// How many `/mcp` requests are answered at once; the next gets 503.
const MAX_IN_FLIGHT: usize = 16;

/// How long a tool waits for a pending route when `ETV_MCP_WAIT` is unset.
const DEFAULT_WAIT: Duration = Duration::from_secs(900);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Verb {
    Get,
    Post,
    Delete,
}

impl Verb {
    fn as_str(self) -> &'static str {
        match self {
            Verb::Get => "GET",
            Verb::Post => "POST",
            Verb::Delete => "DELETE",
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Str,
    Int,
    Object,
}

impl Kind {
    fn schema(self) -> &'static str {
        match self {
            Kind::Str => "string",
            Kind::Int => "integer",
            Kind::Object => "object",
        }
    }
}

/// Where an argument goes in the route's request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    /// Fills the `{name}` of the route's path.
    Path,
    Query,
    /// A field of the JSON body.
    Body,
}

#[derive(Clone, Copy)]
struct Arg {
    name: &'static str,
    kind: Kind,
    required: bool,
    at: At,
    about: &'static str,
}

pub(super) struct ToolSpec {
    pub(super) name: &'static str,
    about: &'static str,
    pub(super) verb: Verb,
    /// The route, `{name}` standing for the path argument of that name.
    pub(super) path: &'static str,
    args: &'static [Arg],
}

const CHANNEL: Arg = Arg {
    name: "channel",
    kind: Kind::Str,
    required: true,
    at: At::Path,
    about: "the channel's lineup key, as `lineup` lists it",
};

const DRAFT_PATH: Arg = Arg {
    name: "draft",
    kind: Kind::Str,
    required: true,
    at: At::Path,
    about: "a draft id from `draft_open`",
};

const DRAFT_QUERY: Arg = Arg {
    name: "draft",
    kind: Kind::Str,
    required: false,
    at: At::Query,
    about: "a draft id from `draft_open`: answer with that draft's profile edits applied",
};

pub(super) const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "lineup",
        about: "Every channel in the station's lineup, in station.yaml order.",
        verb: Verb::Get,
        path: "/api/lineup",
        args: &[],
    },
    ToolSpec {
        name: "channel_review",
        about: "A channel's schedule review: published and aired airings, first repeat, \
                airtime split, tune-in samples. Waits while a projection runs.",
        verb: Verb::Get,
        path: "/api/channels/{channel}/review",
        args: &[
            CHANNEL,
            Arg {
                name: "project",
                kind: Kind::Str,
                required: false,
                at: At::Query,
                about: "project past the published frontier: `1d`, `3d`, `7d` or `14d`",
            },
            DRAFT_QUERY,
        ],
    },
    ToolSpec {
        name: "airing_audit",
        about: "Why one real airing is on a channel: the stored audit record of the airing.",
        verb: Verb::Get,
        path: "/api/airings/{airing}/audit",
        args: &[Arg {
            name: "airing",
            kind: Kind::Str,
            required: true,
            at: At::Path,
            about: "`{channel number}.{start unix seconds}`, e.g. `34.1789000000`",
        }],
    },
    ToolSpec {
        name: "channel_pools",
        about: "Every pool of a channel with its sources, config, taste profile and ranking.",
        verb: Verb::Get,
        path: "/api/channels/{channel}/pools",
        args: &[CHANNEL, DRAFT_QUERY],
    },
    ToolSpec {
        name: "health",
        about: "What this dashboard process serves from, and whether the station config loads.",
        verb: Verb::Get,
        path: "/api/health",
        args: &[],
    },
    ToolSpec {
        name: "draft_open",
        about: "Open an in-memory draft of a channel's plugin-scored pools, to try profile edits.",
        verb: Verb::Post,
        path: "/api/channels/{channel}/drafts",
        args: &[
            CHANNEL,
            Arg {
                name: "account",
                kind: Kind::Int,
                required: false,
                at: At::Body,
                about: "a Plex account id; defaults to the channel's",
            },
        ],
    },
    ToolSpec {
        name: "draft_edit",
        about: "Add, update or remove one profile entry of a pool in a draft.",
        verb: Verb::Post,
        path: "/api/channels/{channel}/drafts/{draft}/edits",
        args: &[
            CHANNEL,
            DRAFT_PATH,
            Arg {
                name: "op",
                kind: Kind::Str,
                required: true,
                at: At::Body,
                about: "`add`, `update` or `remove`",
            },
            Arg {
                name: "pool",
                kind: Kind::Str,
                required: true,
                at: At::Body,
                about: "the pool's name",
            },
            Arg {
                name: "entry",
                kind: Kind::Object,
                required: false,
                at: At::Body,
                about: "for `add` a profile entry (one reference key and a `weight`); for \
                        `update` `{index, weight}`; for `remove` `{index}`",
            },
        ],
    },
    ToolSpec {
        name: "draft_yaml",
        about: "A pool's profile with the draft's edits, as YAML to paste into channel.yaml.",
        verb: Verb::Get,
        path: "/api/channels/{channel}/drafts/{draft}/yaml",
        args: &[
            CHANNEL,
            DRAFT_PATH,
            Arg {
                name: "pool",
                kind: Kind::Str,
                required: true,
                at: At::Query,
                about: "the pool's name",
            },
        ],
    },
    ToolSpec {
        name: "draft_discard",
        about: "Discard a draft.",
        verb: Verb::Delete,
        path: "/api/channels/{channel}/drafts/{draft}",
        args: &[CHANNEL, DRAFT_PATH],
    },
    ToolSpec {
        name: "channel_file",
        about: "The deployed channel.yaml text, the file `apply_channel` replaces.",
        verb: Verb::Get,
        path: "/api/channels/{channel}/file",
        args: &[CHANNEL],
    },
    ToolSpec {
        name: "apply_channel",
        about: "Replace a channel's deployed channel.yaml with `yaml`: validated with the \
                daemon's loader, backed up, written, and the daemon signalled to reload. \
                Writes the live deployment.",
        verb: Verb::Post,
        path: "/api/channels/{channel}/apply",
        args: &[
            CHANNEL,
            Arg {
                name: "yaml",
                kind: Kind::Str,
                required: true,
                at: At::Body,
                about: "the whole new channel.yaml",
            },
        ],
    },
    ToolSpec {
        name: "apply_status",
        about: "Whether the daemon's reload of an applied channel finished, and what it found.",
        verb: Verb::Get,
        path: "/api/channels/{channel}/apply/status",
        args: &[
            CHANNEL,
            Arg {
                name: "after",
                kind: Kind::Int,
                required: false,
                at: At::Query,
                about: "the reload sequence number `apply_channel` returned",
            },
        ],
    },
];

impl ToolSpec {
    /// Arguments that satisfy the schema: every required one, filled with a stand-in.
    #[cfg(test)]
    pub(super) fn sample_args(&self) -> JsonObject {
        self.args
            .iter()
            .filter(|a| a.required)
            .map(|a| {
                let v = match a.kind {
                    Kind::Str => json!("x"),
                    Kind::Int => json!(1),
                    Kind::Object => json!({}),
                };
                (a.name.to_string(), v)
            })
            .collect()
    }
}

/// `/api/health`'s `mcp` field: where the endpoint is and what it offers.
pub(super) fn health(mcp: Option<&Mcp>) -> Value {
    match mcp {
        None => Value::Null,
        Some(m) => json!({
            "path": PATH,
            "tools": TOOLS.iter().map(|t| t.name).collect::<Vec<_>>(),
            "wait_secs": m.backend.wait.total.as_secs(),
        }),
    }
}

fn tool_definition(spec: &ToolSpec) -> Tool {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for arg in spec.args {
        properties.insert(
            arg.name.to_string(),
            json!({ "type": arg.kind.schema(), "description": arg.about }),
        );
        if arg.required {
            required.push(arg.name);
        }
    }
    let schema = json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    });
    let Value::Object(schema) = schema else {
        unreachable!("a json! object literal")
    };
    Tool::new(spec.name, spec.about, Arc::new(schema))
}

/// What a tool's route is asked: path and query as one string, and the JSON
/// body of a `POST`.
pub(super) struct Built {
    pub(super) target: String,
    pub(super) body: Option<String>,
}

const UNRESERVED: &[u8] = b"-._~";

/// Percent-encode everything but RFC 3986's unreserved characters, so a
/// channel key holding a `/` stays one path segment and the router's own
/// decoding returns the key.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn scalar(arg: &Arg, value: &Value) -> Result<String, String> {
    match (arg.kind, value) {
        (Kind::Str, Value::String(s)) => Ok(s.clone()),
        (Kind::Int, Value::Number(n)) if n.is_i64() || n.is_u64() => Ok(n.to_string()),
        _ => Err(format!(
            "argument `{}` must be {}",
            arg.name,
            match arg.kind {
                Kind::Str => "a string",
                Kind::Int => "an integer",
                Kind::Object => "an object",
            }
        )),
    }
}

/// Turn a tool call's arguments into its route's request. An unknown, missing
/// or mistyped argument is an error naming it.
pub(super) fn build(spec: &ToolSpec, args: &JsonObject) -> Result<Built, String> {
    if let Some(unknown) = args
        .keys()
        .find(|k| !spec.args.iter().any(|a| a.name == k.as_str()))
    {
        return Err(format!("`{}` takes no argument `{unknown}`", spec.name));
    }
    for arg in spec.args.iter().filter(|a| a.required) {
        if !args.contains_key(arg.name) {
            return Err(format!("`{}` needs argument `{}`", spec.name, arg.name));
        }
    }
    let mut path = spec.path.to_string();
    let mut query = Vec::new();
    let mut body = serde_json::Map::new();
    for arg in spec.args {
        let Some(value) = args.get(arg.name) else {
            continue;
        };
        match arg.at {
            At::Path => {
                path = path.replace(&format!("{{{}}}", arg.name), &encode(&scalar(arg, value)?));
            }
            At::Query => query.push(format!("{}={}", arg.name, encode(&scalar(arg, value)?))),
            At::Body => {
                let value = match arg.kind {
                    Kind::Object if value.is_object() => value.clone(),
                    Kind::Object => {
                        return Err(format!("argument `{}` must be an object", arg.name));
                    }
                    _ => {
                        scalar(arg, value)?;
                        value.clone()
                    }
                };
                body.insert(arg.name.to_string(), value);
            }
        }
    }
    let target = if query.is_empty() {
        path
    } else {
        format!("{path}?{}", query.join("&"))
    };
    let body = (spec.verb == Verb::Post).then(|| Value::Object(body).to_string());
    Ok(Built { target, body })
}

/// One answer from the dashboard.
pub(super) struct Fetched {
    pub(super) status: u16,
    pub(super) body: String,
}

/// How long a `GET` tool waits for a route that answers 202, and how often it
/// asks again.
#[derive(Clone, Copy)]
pub(super) struct Wait {
    pub(super) total: Duration,
    pub(super) interval: Duration,
}

/// What a tool call came to.
#[derive(Debug)]
pub(super) struct Outcome {
    pub(super) text: String,
    pub(super) is_error: bool,
    pub(super) route: String,
    pub(super) status: u16,
    /// How many times a 202 was asked again.
    pub(super) polls: u32,
}

pub(super) type Fetch<'a> = &'a dyn Fn(Verb, &str, Option<&str>) -> Result<Fetched, String>;

/// Run one tool: build its request, fetch, and wait out a pending `GET`.
/// `Err` is a call that never reached a route answer: bad arguments, or the
/// loopback request failing.
pub(super) fn run(
    fetch: Fetch<'_>,
    wait: Wait,
    spec: &ToolSpec,
    args: &JsonObject,
) -> Result<Outcome, String> {
    let built = build(spec, args)?;
    let started = Instant::now();
    let mut polls = 0;
    let answer = loop {
        let answer = fetch(spec.verb, &built.target, built.body.as_deref())?;
        if spec.verb == Verb::Get && answer.status == 202 && started.elapsed() < wait.total {
            polls += 1;
            std::thread::sleep(wait.interval);
            continue;
        }
        break answer;
    };
    let pending = spec.verb == Verb::Get && answer.status == 202;
    let (text, is_error) = if pending {
        (
            format!(
                "HTTP 202: still pending after {}s ({polls} polls): {}",
                started.elapsed().as_secs(),
                answer.body
            ),
            true,
        )
    } else if answer.status >= 400 {
        (format!("HTTP {}: {}", answer.status, answer.body), true)
    } else {
        (answer.body, false)
    };
    Ok(Outcome {
        text,
        is_error,
        route: format!("{} {}", spec.verb.as_str(), built.target),
        status: answer.status,
        polls,
    })
}

#[derive(Clone)]
struct Backend {
    base: String,
    agent: ureq::Agent,
    wait: Wait,
}

impl Backend {
    fn fetch(&self, verb: Verb, target: &str, body: Option<&str>) -> Result<Fetched, String> {
        let request = self
            .agent
            .request(verb.as_str(), &format!("{}{target}", self.base));
        let result = match body {
            Some(body) => request
                .set("Content-Type", "application/json")
                .send_string(body),
            None => request.call(),
        };
        let response = match result {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(format!("{} {target}: {e}", verb.as_str())),
        };
        let status = response.status();
        let body = response
            .into_string()
            .map_err(|e| format!("{} {target}: reading the answer: {e}", verb.as_str()))?;
        Ok(Fetched { status, body })
    }
}

/// rmcp's side: the tools, answered through [`Backend`].
#[derive(Clone)]
struct Handler {
    backend: Arc<Backend>,
}

impl ServerHandler for Handler {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "etv-station",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "The ETV station's web dashboard. Each tool is one dashboard route. \
                 `apply_channel` writes the live channel.yaml; read it with `channel_file` first.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            TOOLS.iter().map(tool_definition).collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(spec) = TOOLS.iter().find(|t| t.name == request.name) else {
            return Err(ErrorData::invalid_params(
                format!("no tool named {:?}", request.name),
                None,
            ));
        };
        let args = request.arguments.unwrap_or_default();
        let backend = self.backend.clone();
        let began = Instant::now();
        let outcome = tokio::task::spawn_blocking(move || {
            run(
                &|verb, target, body| backend.fetch(verb, target, body),
                backend.wait,
                spec,
                &args,
            )
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("tool task failed: {e}"), None))?;
        let ms = began.elapsed().as_millis() as u64;
        let result = match outcome {
            Ok(o) => {
                tracing::info!(
                    event = "ui.mcp.call",
                    tool = spec.name,
                    route = %o.route,
                    status = o.status,
                    is_error = o.is_error,
                    polls = o.polls,
                    ms,
                    "tool called"
                );
                let content = vec![ContentBlock::text(o.text)];
                if o.is_error {
                    CallToolResult::error(content)
                } else {
                    CallToolResult::success(content)
                }
            }
            Err(reason) => {
                tracing::warn!(
                    event = "ui.mcp.error",
                    tool = spec.name,
                    reason = %reason,
                    ms,
                    "tool call failed"
                );
                CallToolResult::error(vec![ContentBlock::text(reason)])
            }
        };
        Ok(result.into())
    }
}

/// `ETV_MCP_WAIT` seconds, or [`DEFAULT_WAIT`] when unset or not a number.
pub(super) fn wait_from_env() -> Duration {
    std::env::var("ETV_MCP_WAIT")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.min(MAX_WAIT_SECS)))
        .unwrap_or(DEFAULT_WAIT)
}

/// The endpoint: rmcp's service, the runtime it runs on and the loopback
/// address its tools call. Cheap to clone; a clone goes to each request's
/// thread.
#[derive(Clone)]
pub(super) struct Mcp {
    runtime: Arc<tokio::runtime::Runtime>,
    service: StreamableHttpService<Handler, NeverSessionManager>,
    backend: Arc<Backend>,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

impl Mcp {
    /// An endpoint whose tools call the dashboard listening on `listen`.
    pub(super) fn new(listen: std::net::SocketAddr, wait: Duration) -> Result<Self, String> {
        let ip = match listen.ip() {
            ip if ip.is_unspecified() && ip.is_ipv6() => std::net::Ipv6Addr::LOCALHOST.into(),
            ip if ip.is_unspecified() => std::net::Ipv4Addr::LOCALHOST.into(),
            ip => ip,
        };
        let wait = Wait {
            total: wait,
            interval: POLL,
        };
        let backend = Arc::new(Backend {
            base: format!("http://{}", std::net::SocketAddr::new(ip, listen.port())),
            // A route may sit on a slow simulation for as long as a tool waits.
            agent: ureq::AgentBuilder::new()
                .timeout_read(wait.total + Duration::from_secs(60))
                .build(),
            wait,
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("mcp")
            .build()
            .map_err(|e| format!("mcp runtime: {e}"))?;
        let config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .disable_allowed_hosts();
        let handler_backend = backend.clone();
        let service = StreamableHttpService::new(
            move || {
                Ok(Handler {
                    backend: handler_backend.clone(),
                })
            },
            Arc::new(NeverSessionManager::default()),
            config,
        );
        Ok(Self {
            runtime: Arc::new(runtime),
            service,
            backend,
            in_flight: Arc::default(),
        })
    }

    /// Run a tool the way a `tools/call` does, without the JSON-RPC around it.
    #[cfg(test)]
    pub(super) fn call(&self, name: &str, args: &JsonObject) -> Result<Outcome, String> {
        let spec = TOOLS
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| format!("no tool named {name:?}"))?;
        run(
            &|verb, target, body| self.backend.fetch(verb, target, body),
            self.backend.wait,
            spec,
            args,
        )
    }

    /// Take a request for [`PATH`] off the accept loop: answered on its own
    /// thread, so the page never waits behind a tool.
    pub(super) fn serve(&self, request: tiny_http::Request) {
        use std::sync::atomic::Ordering;
        if self.in_flight.fetch_add(1, Ordering::AcqRel) >= MAX_IN_FLIGHT {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            tracing::warn!(
                event = "ui.mcp.error",
                reason = "too many requests in flight",
                "mcp refused"
            );
            let refusal =
                json!({ "error": format!("{MAX_IN_FLIGHT} MCP requests are already running") });
            super::respond_json(request, 503, &refusal);
            return;
        }
        let this = self.clone();
        std::thread::spawn(move || {
            this.answer(request);
            this.in_flight.fetch_sub(1, Ordering::AcqRel);
        });
    }

    fn answer(&self, mut request: tiny_http::Request) {
        let mut body = Vec::new();
        let read = std::io::Read::read_to_end(
            &mut std::io::Read::take(request.as_reader(), MAX_BODY + 1),
            &mut body,
        );
        if read.is_err() || body.len() as u64 > MAX_BODY {
            let refusal =
                json!({ "error": format!("an MCP request body is at most {MAX_BODY} bytes") });
            super::respond_json(request, 413, &refusal);
            return;
        }
        let mut builder = http::Request::builder()
            .method(request.method().as_str())
            .uri(PATH);
        for header in request.headers() {
            builder = builder.header(header.field.as_str().as_str(), header.value.as_str());
        }
        let Ok(forwarded) = builder.body(Full::new(Bytes::from(body))) else {
            let refusal = json!({ "error": "malformed request" });
            super::respond_json(request, 400, &refusal);
            return;
        };
        let response = self.runtime.block_on(self.service.handle(forwarded));
        let (parts, body) = response.into_parts();
        let bytes = match self.runtime.block_on(body.collect()) {
            Ok(collected) => collected.to_bytes(),
            Err(never) => match never {},
        };
        let mut reply =
            tiny_http::Response::from_data(bytes.to_vec()).with_status_code(parts.status.as_u16());
        for (name, value) in &parts.headers {
            if matches!(name.as_str(), "content-length" | "transfer-encoding") {
                continue;
            }
            if let Ok(header) = tiny_http::Header::from_bytes(name.as_str(), value.as_bytes()) {
                reply = reply.with_header(header);
            }
        }
        let _ = request.respond(reply);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn spec(name: &str) -> &'static ToolSpec {
        TOOLS.iter().find(|t| t.name == name).unwrap()
    }

    fn args(v: Value) -> JsonObject {
        let Value::Object(o) = v else { panic!() };
        o
    }

    const NO_WAIT: Wait = Wait {
        total: Duration::from_secs(60),
        interval: Duration::ZERO,
    };

    #[test]
    fn a_tool_becomes_its_routes_path_query_and_body() {
        let b = build(
            spec("channel_review"),
            &args(json!({"channel": "a/b c", "project": "14d", "draft": "d1"})),
        )
        .unwrap();
        assert_eq!(
            b.target,
            "/api/channels/a%2Fb%20c/review?project=14d&draft=d1"
        );
        assert_eq!(b.body, None);

        let b = build(
            spec("draft_edit"),
            &args(
                json!({"channel": "x", "draft": "d", "op": "add", "pool": "p",
                         "entry": {"weight": 2}}),
            ),
        )
        .unwrap();
        assert_eq!(b.target, "/api/channels/x/drafts/d/edits");
        let body: Value = serde_json::from_str(&b.body.unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"op": "add", "pool": "p", "entry": {"weight": 2}})
        );

        let b = build(spec("draft_open"), &args(json!({"channel": "x"}))).unwrap();
        assert_eq!(b.body.as_deref(), Some("{}"));
    }

    #[test]
    fn an_unknown_missing_or_mistyped_argument_is_named() {
        let err = |name, a| build(spec(name), &args(a)).err().unwrap();
        assert!(err("lineup", json!({"x": 1})).contains("no argument `x`"));
        assert!(err("channel_file", json!({})).contains("needs argument `channel`"));
        assert!(err("apply_status", json!({"channel": "c", "after": "3"})).contains("integer"));
        assert!(
            err(
                "draft_edit",
                json!({"channel": "c", "draft": "d", "op": "add", "pool": "p", "entry": "x"})
            )
            .contains("object")
        );
    }

    #[test]
    fn every_tool_lists_a_schema_naming_its_required_arguments() {
        for spec in TOOLS {
            let tool = tool_definition(spec);
            let required: Vec<&str> = spec
                .args
                .iter()
                .filter(|a| a.required)
                .map(|a| a.name)
                .collect();
            let listed = tool.input_schema.get("required").unwrap();
            assert_eq!(listed, &json!(required), "{}", spec.name);
        }
    }

    #[test]
    fn a_get_tool_asks_again_while_the_route_answers_202_then_returns_the_real_answer() {
        let calls = Cell::new(0);
        let fetch = |_: Verb, _: &str, _: Option<&str>| {
            calls.set(calls.get() + 1);
            Ok(if calls.get() < 3 {
                Fetched {
                    status: 202,
                    body: r#"{"pending":true}"#.into(),
                }
            } else {
                Fetched {
                    status: 200,
                    body: r#"{"review":1}"#.into(),
                }
            })
        };
        let o = run(
            &fetch,
            NO_WAIT,
            spec("channel_review"),
            &args(json!({"channel": "c"})),
        )
        .unwrap();
        assert_eq!((o.polls, o.status, o.is_error), (2, 200, false));
        assert_eq!(o.text, r#"{"review":1}"#);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn a_route_still_pending_at_the_deadline_is_returned_as_an_error() {
        let fetch = |_: Verb, _: &str, _: Option<&str>| {
            Ok(Fetched {
                status: 202,
                body: r#"{"pending":true}"#.into(),
            })
        };
        let wait = Wait {
            total: Duration::ZERO,
            interval: Duration::ZERO,
        };
        let o = run(
            &fetch,
            wait,
            spec("channel_review"),
            &args(json!({"channel": "c"})),
        )
        .unwrap();
        assert!(o.is_error);
        assert!(o.text.starts_with("HTTP 202: still pending"), "{}", o.text);
    }

    #[test]
    fn a_post_is_sent_once_whatever_it_answers() {
        let sent = RefCell::new(Vec::new());
        let fetch = |verb: Verb, target: &str, body: Option<&str>| {
            sent.borrow_mut()
                .push((verb, target.to_string(), body.map(str::to_string)));
            Ok(Fetched {
                status: 202,
                body: "{}".into(),
            })
        };
        let o = run(
            &fetch,
            NO_WAIT,
            spec("apply_channel"),
            &args(json!({"channel": "c", "yaml": "a: 1"})),
        )
        .unwrap();
        assert_eq!(sent.borrow().len(), 1);
        assert_eq!((o.status, o.is_error, o.polls), (202, false, 0));
    }

    #[test]
    fn a_status_of_400_or_more_is_an_error_leading_with_the_status() {
        let fetch = |_: Verb, _: &str, _: Option<&str>| {
            Ok(Fetched {
                status: 422,
                body: r#"{"error":"bad"}"#.into(),
            })
        };
        let o = run(
            &fetch,
            NO_WAIT,
            spec("apply_channel"),
            &args(json!({"channel": "c", "yaml": "x"})),
        )
        .unwrap();
        assert!(o.is_error);
        assert_eq!(o.text, r#"HTTP 422: {"error":"bad"}"#);
    }

    #[test]
    fn health_lists_the_path_the_tools_and_the_wait() {
        let mcp = Mcp::new("0.0.0.0:4747".parse().unwrap(), Duration::from_secs(5)).unwrap();
        let health = health(Some(&mcp));
        assert_eq!(health["path"], "/mcp");
        assert_eq!(health["wait_secs"], 5);
        assert_eq!(health["tools"].as_array().unwrap().len(), TOOLS.len());
    }
}

/// A dashboard answering on a real TCP port, for tests that drive the tools or
/// the `/mcp` glue the way a client does.
#[cfg(test)]
pub(super) mod testing {
    use super::*;

    pub(in crate::web_ui) struct Dashboard {
        pub(in crate::web_ui) base: String,
        pub(in crate::web_ui) mcp: Mcp,
        server: Arc<tiny_http::Server>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Dashboard {
        /// Serve the state `make` builds on its own thread, on a free loopback port.
        pub(in crate::web_ui) fn start(
            make: impl FnOnce() -> crate::web_ui::AppState + Send + 'static,
        ) -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind"));
            let listen = server.server_addr().to_ip().expect("ip");
            let mcp = Mcp::new(listen, Duration::from_secs(30)).expect("mcp");
            let (serving, handed) = (server.clone(), mcp.clone());
            let thread = std::thread::spawn(move || {
                let mut state = make();
                state.mcp = Some(handed);
                crate::web_ui::run(&serving, &mut state);
            });
            Self {
                base: format!("http://{listen}"),
                mcp,
                server,
                thread: Some(thread),
            }
        }

        /// A plain `GET` of the dashboard, the page's way: status and body.
        pub(in crate::web_ui) fn get(&self, target: &str) -> (u16, String) {
            let backend = &self.mcp.backend;
            let f = backend.fetch(Verb::Get, target, None).expect("get");
            (f.status, f.body)
        }
    }

    impl Drop for Dashboard {
        fn drop(&mut self) {
            self.server.unblock();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
}
