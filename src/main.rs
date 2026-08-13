//! Rust Flux Starter - Backend Server
//!
//! Simple WebSocket proxy to Deepgram's Flux API using Axum.
//! Forwards all messages (JSON and binary) bidirectionally between client and Deepgram.
//!
//! Routes:
//!   GET  /api/session              - Issue JWT session token
//!   WS   /api/flux                 - WebSocket proxy to Deepgram Flux (auth required)
//!   GET  /api/metadata             - Project metadata from deepgram.toml
//!   GET  /health                   - Health check

// ============================================================================
// DEPENDENCIES
// ============================================================================

use axum::{
    extract::{State, WebSocketUpgrade},
    http::{header, Method, StatusCode},
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use axum::extract::ws::{Message, WebSocket};
use deepgram::{
    common::{
        flux_response::FluxResponse,
        options::{Encoding, Model, Options},
    },
    Deepgram,
};
use futures_util::{SinkExt, StreamExt};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::{env, sync::Arc, time::SystemTime};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;

// ============================================================================
// CONFIGURATION
// ============================================================================

/// Application configuration loaded from environment variables.
#[derive(Clone)]
struct Config {
    deepgram_api_key: String,
    port: u16,
    host: String,
    session_secret: String,
}

/// Loads configuration from environment variables, with sensible defaults.
fn load_config() -> Config {
    // Load .env file (optional, won't error if missing)
    let _ = dotenvy::dotenv();

    let api_key = env::var("DEEPGRAM_API_KEY").unwrap_or_else(|_| {
        eprintln!("ERROR: DEEPGRAM_API_KEY environment variable is required");
        eprintln!("Please copy sample.env to .env and add your API key");
        std::process::exit(1);
    });

    let port = env::var("PORT")
        .unwrap_or_else(|_| "8081".to_string())
        .parse::<u16>()
        .unwrap_or(8081);

    let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());

    let session_secret = env::var("SESSION_SECRET").unwrap_or_else(|_| {
        use rand::Rng;
        let bytes: [u8; 32] = rand::rng().random();
        hex::encode(bytes)
    });

    Config {
        deepgram_api_key: api_key,
        port,
        host,
        session_secret,
    }
}

// ============================================================================
// SESSION AUTH - JWT tokens for production security
// ============================================================================

const JWT_EXPIRY_SECS: u64 = 3600; // 1 hour

/// JWT claims structure.
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    iat: u64,
    exp: u64,
}

/// Generates a signed JWT for session authentication.
fn generate_token(secret: &str) -> Result<String, jsonwebtoken::errors::Error> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = Claims {
        iat: now,
        exp: now + JWT_EXPIRY_SECS,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
}

/// Validates a JWT and returns an error if invalid.
fn validate_token(token: &str, secret: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.required_spec_claims.clear();
    validation.validate_exp = true;

    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )?;

    Ok(token_data.claims)
}

/// Extracts and validates a JWT from WebSocket subprotocol header.
/// Returns the full protocol string (e.g., "access_token.<jwt>") if valid.
fn validate_ws_token(protocols: &str, secret: &str) -> Option<String> {
    for proto in protocols.split(',').map(|s| s.trim()) {
        if let Some(token) = proto.strip_prefix("access_token.") {
            if validate_token(token, secret).is_ok() {
                return Some(proto.to_string());
            }
        }
    }
    None
}

// ============================================================================
// METADATA
// ============================================================================

/// Represents the parsed deepgram.toml structure.
#[derive(Deserialize)]
struct DeepgramToml {
    meta: Option<toml::Value>,
}

// ============================================================================
// QUERY PARAMETERS
// ============================================================================

/// Query parameters forwarded from the client to Deepgram.
#[derive(Debug, Deserialize)]
struct FluxQueryParams {
    encoding: Option<String>,
    sample_rate: Option<String>,
    eot_threshold: Option<String>,
    eager_eot_threshold: Option<String>,
    eot_timeout_ms: Option<String>,
    // Note: keyterm is a multi-value param handled separately via get_all()
}

// ============================================================================
// HTTP HANDLERS
// ============================================================================

/// GET /api/session - Issues a signed JWT for session authentication.
async fn handle_session(State(config): State<Arc<Config>>) -> impl IntoResponse {
    match generate_token(&config.session_secret) {
        Ok(token) => {
            let body = serde_json::json!({ "token": token });
            (StatusCode::OK, Json(body))
        }
        Err(_) => {
            let body = serde_json::json!({
                "error": "INTERNAL_SERVER_ERROR",
                "message": "Failed to generate token"
            });
            (StatusCode::INTERNAL_SERVER_ERROR, Json(body))
        }
    }
}

/// GET /api/metadata - Returns project metadata from deepgram.toml.
async fn handle_metadata() -> impl IntoResponse {
    let content = match std::fs::read_to_string("deepgram.toml") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error reading deepgram.toml: {}", e);
            let body = serde_json::json!({
                "error": "INTERNAL_SERVER_ERROR",
                "message": "Failed to read metadata from deepgram.toml"
            });
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(body));
        }
    };

    let config: DeepgramToml = match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error parsing deepgram.toml: {}", e);
            let body = serde_json::json!({
                "error": "INTERNAL_SERVER_ERROR",
                "message": "Failed to read metadata from deepgram.toml"
            });
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(body));
        }
    };

    match config.meta {
        Some(meta) => {
            // Convert TOML value to JSON value via serde
            let body: serde_json::Value =
                serde_json::to_value(&meta).unwrap_or_else(|_| serde_json::json!({}));
            (StatusCode::OK, Json(body))
        }
        None => {
            let body = serde_json::json!({
                "error": "INTERNAL_SERVER_ERROR",
                "message": "Missing [meta] section in deepgram.toml"
            });
            (StatusCode::INTERNAL_SERVER_ERROR, Json(body))
        }
    }
}

/// GET /health - Health check endpoint.
async fn handle_health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

// ============================================================================
// WEBSOCKET PROXY
// ============================================================================

/// WS /api/flux - WebSocket proxy to Deepgram Flux (auth via subprotocol).
async fn handle_flux(
    State(config): State<Arc<Config>>,
    headers: axum::http::HeaderMap,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Extract Sec-WebSocket-Protocol header for JWT validation
    let protocols = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let valid_proto = match validate_ws_token(protocols, &config.session_secret) {
        Some(proto) => proto,
        None => {
            eprintln!("WebSocket auth failed: invalid or missing token");
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    };

    // Parse standard query parameters
    let query_string = raw_query.unwrap_or_default();
    let params: FluxQueryParams = serde_urlencoded::from_str(&query_string).unwrap_or(FluxQueryParams {
        encoding: None,
        sample_rate: None,
        eot_threshold: None,
        eager_eot_threshold: None,
        eot_timeout_ms: None,
    });

    // Parse multi-value keyterm parameters using get_all() pattern
    let keyterms: Vec<String> = url::form_urlencoded::parse(query_string.as_bytes())
        .filter(|(key, _)| key == "keyterm")
        .map(|(_, value)| value.to_string())
        .collect();

    let encoding = params.encoding.unwrap_or_else(|| "linear16".to_string());
    let sample_rate = params.sample_rate.unwrap_or_else(|| "16000".to_string());
    let model = "flux-general-en".to_string();

    // Accept the validated subprotocol so the client sees it echoed back
    ws.protocols([valid_proto.clone()])
        .on_upgrade(move |socket| {
            proxy_flux(
                socket,
                config,
                model,
                encoding,
                sample_rate,
                params.eot_threshold,
                params.eager_eot_threshold,
                params.eot_timeout_ms,
                keyterms,
            )
        })
        .into_response()
}

/// Proxies WebSocket messages bidirectionally between the client and Deepgram's Flux API.
///
/// The Deepgram side is driven by the official `deepgram` crate's Flux
/// [`FluxHandle`](deepgram::listen::flux). Audio frames from the browser are
/// forwarded via `send_data`, a `CloseStream` control message maps onto
/// `close_stream`, and typed [`FluxResponse`] events coming back from Deepgram
/// are re-serialized to JSON (preserving the `type`-tagged wire shape) and
/// forwarded to the browser unchanged.
///
/// NOTE: Deepgram's Flux module interface in the SDK is currently marked as
/// "changing", so the exact builder/handle surface used here may shift in a
/// future release.
async fn proxy_flux(
    client_ws: WebSocket,
    config: Arc<Config>,
    model: String,
    encoding: String,
    sample_rate: String,
    eot_threshold: Option<String>,
    eager_eot_threshold: Option<String>,
    eot_timeout_ms: Option<String>,
    keyterms: Vec<String>,
) {
    println!("Client connected to /api/flux");

    println!(
        "Connecting to Deepgram Flux: model={}, encoding={}, sample_rate={}",
        model, encoding, sample_rate
    );

    // Build Flux options from the forwarded query parameters.
    let mut builder = Options::builder().model(Model::from(model));
    if let Some(v) = eot_threshold.as_deref().and_then(|s| s.parse::<f64>().ok()) {
        builder = builder.eot_threshold(v);
    }
    if let Some(v) = eager_eot_threshold
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
    {
        builder = builder.eager_eot_threshold(v);
    }
    if let Some(v) = eot_timeout_ms.as_deref().and_then(|s| s.parse::<u32>().ok()) {
        builder = builder.eot_timeout_ms(v);
    }
    if !keyterms.is_empty() {
        builder = builder.keyterms(keyterms.iter().map(|s| s.as_str()));
    }
    let options = builder.build();

    let sample_rate_num: u32 = sample_rate.parse().unwrap_or(16000);

    // Establish the Deepgram Flux websocket via the SDK.
    let dg = match Deepgram::new(&config.deepgram_api_key) {
        Ok(dg) => dg,
        Err(e) => {
            eprintln!("Failed to initialize Deepgram client: {}", e);
            close_client(client_ws, 1011, "Deepgram connection failed").await;
            return;
        }
    };

    let mut dg_handle = match dg
        .transcription()
        .flux_request_with_options(options)
        .encoding(map_encoding(&encoding))
        .sample_rate(sample_rate_num)
        .handle()
        .await
    {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("Deepgram connection failed: {}", e);
            close_client(client_ws, 1011, "Deepgram connection failed").await;
            return;
        }
    };

    println!("Connected to Deepgram Flux API");

    let (mut client_sender, mut client_receiver) = client_ws.split();
    let mut client_msg_count: u64 = 0;
    let mut dg_msg_count: u64 = 0;

    // Single-task proxy loop: the SDK handle is not splittable, so both
    // directions share one task via `tokio::select!`.
    loop {
        tokio::select! {
            client_msg = client_receiver.next() => {
                match client_msg {
                    Some(Ok(Message::Binary(data))) => {
                        client_msg_count += 1;
                        if client_msg_count % 100 == 0 {
                            println!(
                                "-> Client message #{} (binary: true, size: {})",
                                client_msg_count,
                                data.len()
                            );
                        }
                        if dg_handle.send_data(data.to_vec()).await.is_err() {
                            eprintln!("Error forwarding to Deepgram");
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        client_msg_count += 1;
                        println!(
                            "-> Client message #{} (binary: false, size: {})",
                            client_msg_count,
                            text.len()
                        );
                        // The only client->Deepgram control message the Flux
                        // frontend sends is CloseStream; map it onto the handle.
                        let text = text.to_string();
                        let kind = serde_json::from_str::<serde_json::Value>(&text)
                            .ok()
                            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from));
                        if matches!(kind.as_deref(), Some("CloseStream")) {
                            let _ = dg_handle.close_stream().await;
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        println!("Client disconnected normally");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        eprintln!("Client read error: {}", e);
                        break;
                    }
                }
            }
            dg_msg = dg_handle.receive() => {
                match dg_msg {
                    Some(Ok(response)) => {
                        dg_msg_count += 1;
                        match serde_json::to_string(&response) {
                            Ok(json) => {
                                if dg_msg_count % 10 == 0 {
                                    println!(
                                        "<- Deepgram message #{} (size: {})",
                                        dg_msg_count,
                                        json.len()
                                    );
                                }
                                if client_sender.send(Message::Text(json.into())).await.is_err() {
                                    eprintln!("Error forwarding to client");
                                    break;
                                }
                            }
                            Err(e) => eprintln!("Error serializing Deepgram response: {}", e),
                        }
                        // Stop forwarding once the server reports a fatal error.
                        if matches!(response, FluxResponse::FatalError { .. }) {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("Deepgram read error: {}", e);
                        break;
                    }
                    None => {
                        println!("Deepgram connection closed");
                        break;
                    }
                }
            }
        }
    }

    let _ = dg_handle.close_stream().await;
    let _ = client_sender
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code: 1000,
            reason: "Session ended".into(),
        })))
        .await;

    println!("WebSocket proxy session ended");
}

/// Sends a close frame to the (un-split) client WebSocket, ignoring errors.
async fn close_client(client_ws: WebSocket, code: u16, reason: &str) {
    let (mut sender, _) = client_ws.split();
    let _ = sender
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

/// Maps an encoding query-parameter string to the SDK's [`Encoding`] enum,
/// falling back to a custom encoding for any value the SDK does not model.
fn map_encoding(value: &str) -> Encoding {
    match value {
        "linear16" => Encoding::Linear16,
        "linear32" => Encoding::Linear32,
        "flac" => Encoding::Flac,
        "mulaw" => Encoding::Mulaw,
        "amr-nb" => Encoding::AmrNb,
        "amr-wb" => Encoding::AmrWb,
        "opus" => Encoding::Opus,
        "speex" => Encoding::Speex,
        "g729" => Encoding::G729,
        other => Encoding::CustomEncoding(other.to_string()),
    }
}

// ============================================================================
// MAIN
// ============================================================================

#[tokio::main]
async fn main() {
    let config = load_config();
    let addr = format!("{}:{}", config.host, config.port);
    let config = Arc::new(config);

    // Configure CORS
    let cors = CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE]);

    // Build router
    let app = Router::new()
        .route("/api/session", get(handle_session))
        .route("/api/flux", get(handle_flux))
        .route("/api/metadata", get(handle_metadata))
        .route("/health", get(handle_health))
        .layer(cors)
        .with_state(config.clone());

    println!("{}", "=".repeat(70));
    println!(
        "Backend API Server running at http://localhost:{}",
        config.port
    );
    println!();
    println!("GET  /api/session");
    println!("WS   /api/flux (auth required)");
    println!("GET  /api/metadata");
    println!("GET  /health");
    println!("{}", "=".repeat(70));

    // Start server with graceful shutdown
    let listener = TcpListener::bind(&addr).await.unwrap();

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    println!("Shutdown complete");
}

/// Listens for SIGINT/SIGTERM for graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => println!("\nSIGINT received: starting graceful shutdown..."),
        _ = terminate => println!("\nSIGTERM received: starting graceful shutdown..."),
    }
}
