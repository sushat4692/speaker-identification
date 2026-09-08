//! Minimal local HTTP server exposing [`recluster`] over a pre-built
//! embedding cache — the piece that turns the 探索モード practicality test
//! into an actual live UI control.
//!
//! Deliberately hand-rolled over `std::net` rather than pulling in
//! axum/tokio: this is a single-endpoint, single-user, localhost-only dev
//! tool (paired with `tools/diarization-viewer.html`), not a service.
//! Re-clustering itself is what has to be fast (see the plan doc's
//! practicality numbers) — the HTTP plumbing around it just needs to be
//! correct.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use anyhow::{Context, Result};
use asr_core::diarize::fast::{recluster, EmbeddingCache};
use asr_core::diarize::merge_same_speaker_runs;

pub struct ServeConfig {
    pub cache: EmbeddingCache,
    pub max_gap_ms: u64,
    pub max_merged_ms: u64,
}

pub fn run(config: ServeConfig, port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("binding 127.0.0.1:{port} (already in use?)"))?;
    let config = Arc::new(config);

    println!(
        "listening on http://127.0.0.1:{port}  ({} cached windows, {} dims)",
        config.cache.windows.len(),
        config.cache.embedding_dim
    );
    println!("  GET /health");
    println!("  GET /recluster?threshold=0.3");

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        let config = Arc::clone(&config);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &config) {
                eprintln!("connection error: {e}");
            }
        });
    }

    Ok(())
}

fn handle_connection(mut stream: TcpStream, config: &ServeConfig) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if request_line.is_empty() {
        return Ok(()); // client disconnected before sending anything
    }

    // We don't need any request header (no auth, no body, no cookies) —
    // just consume them up to the blank line terminating the request.
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let params = parse_query(query);

    match path {
        "/recluster" => {
            let threshold: f64 = params
                .get("threshold")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.3);

            let start = std::time::Instant::now();
            let raw = recluster(&config.cache, threshold);
            let merged = merge_same_speaker_runs(&raw, config.max_gap_ms, config.max_merged_ms);
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

            let distinct_speakers = merged
                .iter()
                .map(|s| s.speaker)
                .collect::<std::collections::HashSet<_>>()
                .len();

            let body = serde_json::json!({
                "threshold": threshold,
                "elapsed_ms": elapsed_ms,
                "num_speakers": distinct_speakers,
                "segments": merged,
            });
            write_json(&mut stream, 200, "OK", &serde_json::to_vec(&body)?)?;
        }
        "/health" => {
            let body = serde_json::json!({
                "ok": true,
                "windows": config.cache.windows.len(),
                "embedding_dim": config.cache.embedding_dim,
            });
            write_json(&mut stream, 200, "OK", &serde_json::to_vec(&body)?)?;
        }
        _ => {
            write_json(&mut stream, 404, "Not Found", br#"{"error":"not found"}"#)?;
        }
    }

    Ok(())
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect()
}

/// Minimal `application/x-www-form-urlencoded` decode: enough for the
/// simple `key=value` query strings this endpoint actually receives.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
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

/// Every response carries `Access-Control-Allow-Origin: *` — the HTML
/// viewer is typically opened from a different origin (a `file://` page or
/// a separate `python -m http.server` port) than this API server, and a
/// plain cross-origin `GET` with no custom headers is a CORS "simple
/// request": the browser sends it directly (no preflight `OPTIONS`) and
/// just checks this response header before letting page JS read the body.
fn write_json(stream: &mut TcpStream, status: u16, status_text: &str, body: &[u8]) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: application/json\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}
