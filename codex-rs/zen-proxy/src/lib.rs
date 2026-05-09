//! `codex-zen-proxy` — A translating proxy for OpenCode Zen.
//!
//! Accepts OAI Responses API requests from codex-rs and routes them to the
//! appropriate Zen upstream endpoint:
//!
//! - **GPT models** → passthrough to `{upstream}/responses`
//! - **Claude models** → translate to Anthropic Messages API at `{upstream}/messages`,
//!   then translate the Anthropic SSE stream back to OAI Responses SSE
//!
//! Inherits the same security model as `codex-responses-api-proxy`: API key is
//! read from stdin, stored in `mlock(2)`-protected memory, and injected into
//! upstream requests.

use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::AUTHORIZATION;
use reqwest::header::HOST;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use serde::Serialize;
use tiny_http::Header;
use tiny_http::Method;
use tiny_http::Request;
use tiny_http::Response;
use tiny_http::Server;
use tiny_http::StatusCode;

mod read_api_key;
mod routing;
mod translate_request;
mod translate_sse;

use read_api_key::read_auth_header_from_stdin;
use routing::ModelFamily;

/// CLI arguments for the zen proxy.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "codex-zen-proxy",
    about = "Translating proxy: OAI Responses API ↔ OpenCode Zen (GPT passthrough + Claude translation)"
)]
pub struct Args {
    /// Port to listen on. If not set, an ephemeral port is used.
    #[arg(long)]
    pub port: Option<u16>,

    /// Path to a JSON file to write startup info (single line). Includes {"port": <u16>}.
    #[arg(long, value_name = "FILE")]
    pub server_info: Option<PathBuf>,

    /// Enable HTTP shutdown endpoint at GET /shutdown.
    #[arg(long)]
    pub http_shutdown: bool,

    /// Base URL of the Zen API (default: https://opencode.ai/zen/v1).
    /// GPT requests go to `{base}/responses`, Claude requests to `{base}/messages`.
    #[arg(long, default_value = "https://opencode.ai/zen/v1")]
    pub upstream_base: String,
}

#[derive(Serialize)]
struct ServerInfo {
    port: u16,
    pid: u32,
}

struct ProxyConfig {
    /// Base URL (no trailing slash), e.g. "https://opencode.ai/zen/v1"
    upstream_base: String,
    /// Pre-parsed host header value for upstream requests
    host_header: HeaderValue,
}

/// Entry point.
pub fn run_main(args: Args) -> Result<()> {
    let auth_header = read_auth_header_from_stdin()?;

    let upstream_base = args.upstream_base.trim_end_matches('/').to_string();
    let parsed = Url::parse(&upstream_base).context("parsing --upstream-base")?;
    let host = match (parsed.host_str(), parsed.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => return Err(anyhow!("upstream base URL must include a host")),
    };
    let host_header =
        HeaderValue::from_str(&host).context("constructing Host header from upstream URL")?;

    let config = Arc::new(ProxyConfig {
        upstream_base,
        host_header,
    });

    let (listener, bound_addr) = bind_listener(args.port)?;
    if let Some(path) = args.server_info.as_ref() {
        write_server_info(path, bound_addr.port())?;
    }
    let server = Server::from_listener(listener, None)
        .map_err(|err| anyhow!("creating HTTP server: {err}"))?;
    let client = Arc::new(
        Client::builder()
            .timeout(None::<Duration>)
            .build()
            .context("building reqwest client")?,
    );

    eprintln!(
        "codex-zen-proxy listening on {bound_addr} → {}",
        config.upstream_base
    );

    let http_shutdown = args.http_shutdown;
    for request in server.incoming_requests() {
        let client = client.clone();
        let config = config.clone();
        std::thread::spawn(move || {
            let method = request.method().clone();
            let url = request.url().to_string();

            if http_shutdown && method == Method::Get && url == "/shutdown" {
                let _ = request.respond(Response::new_empty(StatusCode(200)));
                std::process::exit(0);
            }

            if method == Method::Get && url == "/health" {
                let body = serde_json::json!({
                    "status": "ok",
                    "proxy": "codex-zen-proxy",
                    "upstream": config.upstream_base,
                });
                let data = serde_json::to_vec(&body).unwrap_or_default();
                let resp = Response::from_data(data)
                    .with_status_code(StatusCode(200))
                    .with_header(
                        Header::from_bytes(b"content-type", b"application/json")
                            .unwrap_or_else(|_| unreachable!()),
                    );
                let _ = request.respond(resp);
                return;
            }

            if let Err(e) = handle_request(&client, auth_header, &config, request) {
                eprintln!("zen-proxy error: {e}");
            }
        });
    }

    Err(anyhow!("server stopped unexpectedly"))
}

fn bind_listener(port: Option<u16>) -> Result<(TcpListener, SocketAddr)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port.unwrap_or(0)));
    let listener = TcpListener::bind(addr).with_context(|| format!("failed to bind {addr}"))?;
    let bound = listener.local_addr().context("failed to read local_addr")?;
    Ok((listener, bound))
}

fn write_server_info(path: &Path, port: u16) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let info = ServerInfo {
        port,
        pid: std::process::id(),
    };
    let mut data = serde_json::to_string(&info)?;
    data.push('\n');
    let mut f = File::create(path)?;
    f.write_all(data.as_bytes())?;
    Ok(())
}

fn handle_request(
    client: &Client,
    auth_header: &'static str,
    config: &ProxyConfig,
    mut req: Request,
) -> Result<()> {
    // Only allow POST /v1/responses
    let method = req.method().clone();
    let url_path = req.url().to_string();
    if method != Method::Post || url_path != "/v1/responses" {
        let _ = req.respond(Response::new_empty(StatusCode(403)));
        return Ok(());
    }

    // Extract forwarding headers before consuming the request body.
    let fwd_headers = build_upstream_headers(auth_header, config, &req);

    // Read request body (consumes the reader — must happen after header extraction).
    let mut body_bytes = Vec::new();
    req.as_reader().read_to_end(&mut body_bytes)?;

    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).context("parsing request JSON")?;
    let model = body["model"].as_str().unwrap_or("").to_string();
    let is_stream = body["stream"].as_bool().unwrap_or(false);

    let family = routing::classify_model(&model);
    eprintln!("→ model={model} family={family:?} stream={is_stream}");

    match family {
        ModelFamily::Gpt => handle_gpt_passthrough(client, config, req, fwd_headers, &body_bytes),
        ModelFamily::Claude => {
            handle_claude_translate(client, auth_header, config, req, &body, &model, is_stream)
        }
        ModelFamily::Unknown => {
            let err_body = serde_json::json!({
                "error": {
                    "message": format!("codex-zen-proxy: no route for model '{model}'"),
                    "type": "proxy_not_implemented",
                }
            });
            let data = serde_json::to_vec(&err_body).unwrap_or_default();
            let resp = Response::from_data(data)
                .with_status_code(StatusCode(501))
                .with_header(
                    Header::from_bytes(b"content-type", b"application/json")
                        .unwrap_or_else(|_| unreachable!()),
                );
            let _ = req.respond(resp);
            Ok(())
        }
    }
}

/// GPT: passthrough to `{upstream}/responses` — identical to codex-responses-api-proxy.
fn handle_gpt_passthrough(
    client: &Client,
    config: &ProxyConfig,
    req: Request,
    headers: HeaderMap,
    body_bytes: &[u8],
) -> Result<()> {
    let upstream_url = format!("{}/responses", config.upstream_base);

    let upstream_resp = client
        .post(&upstream_url)
        .headers(headers)
        .body(body_bytes.to_vec())
        .send()
        .context("forwarding GPT request to upstream")?;

    relay_response(req, upstream_resp)
}

/// Claude: translate OAI Responses → Anthropic Messages, then translate SSE back.
fn handle_claude_translate(
    client: &Client,
    auth_header: &'static str,
    config: &ProxyConfig,
    req: Request,
    body: &serde_json::Value,
    model: &str,
    is_stream: bool,
) -> Result<()> {
    let upstream_url = format!("{}/messages", config.upstream_base);
    let anthropic_body = translate_request::oai_to_anthropic(body);

    // Claude uses x-api-key rather than Bearer for the Anthropic format.
    // Extract just the key from "Bearer <key>".
    let api_key = auth_header.strip_prefix("Bearer ").unwrap_or(auth_header);

    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-api-key"),
        HeaderValue::from_str(api_key).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static("2023-06-01"),
    );
    headers.insert(HOST, config.host_header.clone());
    if is_stream {
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("text/event-stream"),
        );
    }

    let upstream_resp = client
        .post(&upstream_url)
        .headers(headers)
        .json(&anthropic_body)
        .send()
        .context("forwarding Claude request to upstream")?;

    if upstream_resp.status().as_u16() != 200 {
        return relay_response(req, upstream_resp);
    }

    if !is_stream {
        // Non-streaming: translate the single JSON response.
        let ant_body: serde_json::Value =
            upstream_resp.json().context("reading Claude response")?;
        let oai_resp = translate_request::anthropic_response_to_oai(&ant_body, model);
        let data = serde_json::to_vec(&oai_resp).unwrap_or_default();
        let resp = Response::from_data(data)
            .with_status_code(StatusCode(200))
            .with_header(
                Header::from_bytes(b"content-type", b"application/json")
                    .unwrap_or_else(|_| unreachable!()),
            );
        let _ = req.respond(resp);
        return Ok(());
    }

    // Streaming: translate Anthropic SSE → OAI Responses SSE.
    let translator = translate_sse::AnthropicToOaiStream::new(model.to_string(), upstream_resp);
    let resp = Response::new(
        StatusCode(200),
        vec![
            Header::from_bytes(b"content-type", b"text/event-stream")
                .unwrap_or_else(|_| unreachable!()),
            Header::from_bytes(b"cache-control", b"no-cache").unwrap_or_else(|_| unreachable!()),
            Header::from_bytes(b"x-accel-buffering", b"no").unwrap_or_else(|_| unreachable!()),
        ],
        translator,
        None,
        None,
    );
    let _ = req.respond(resp);
    Ok(())
}

/// Extract forwarding headers from an incoming request (all except auth/host).
fn build_upstream_headers(
    auth_header: &'static str,
    config: &ProxyConfig,
    req: &Request,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for header in req.headers() {
        let name_lower = header.field.as_str().to_ascii_lowercase();
        if name_lower == "authorization" || name_lower == "host" {
            continue;
        }
        let Ok(header_name) = HeaderName::from_bytes(name_lower.as_bytes()) else {
            continue;
        };
        if let Ok(value) = HeaderValue::from_bytes(header.value.as_bytes()) {
            headers.append(header_name, value);
        }
    }
    let mut auth_value = HeaderValue::from_static(auth_header);
    auth_value.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth_value);
    headers.insert(HOST, config.host_header.clone());
    headers
}

/// Relay a reqwest response back through tiny_http (passthrough).
fn relay_response(req: Request, upstream_resp: reqwest::blocking::Response) -> Result<()> {
    let status = upstream_resp.status();
    let mut response_headers = Vec::new();
    for (name, value) in upstream_resp.headers().iter() {
        if matches!(
            name.as_str(),
            "content-length" | "transfer-encoding" | "connection" | "trailer" | "upgrade"
        ) {
            continue;
        }
        if let Ok(h) = Header::from_bytes(name.as_str().as_bytes(), value.as_bytes()) {
            response_headers.push(h);
        }
    }

    let content_length = upstream_resp.content_length().and_then(|len| {
        if len <= usize::MAX as u64 {
            Some(len as usize)
        } else {
            None
        }
    });

    let response = Response::new(
        StatusCode(status.as_u16()),
        response_headers,
        Box::new(upstream_resp) as Box<dyn Read + Send>,
        content_length,
        None,
    );
    let _ = req.respond(response);
    Ok(())
}
