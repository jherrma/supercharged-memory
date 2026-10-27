//! A fake Ollama on an ephemeral port: `/api/tags` lists `bge-m3:latest` and
//! `other-model:latest`, `/api/version` answers, and `/api/embed` returns
//! [`crate::vectors::vector`] of the input for either model. It answers on
//! 127.0.0.1 only and forwards nothing; nothing in the suite ever contacts a
//! real Ollama.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use crate::vectors;

const MODELS: &[&str] = &["bge-m3", "other-model"];

/// Start the server on its own threads; its base URL.
pub fn start() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake Ollama");
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let _ = serve(stream);
            });
        }
    });
    url
}

/// A URL whose port refuses connections: one the system handed out and that
/// was closed again at once.
pub fn refusing_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a port to close");
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    url
}

/// One request, one response, then the connection closes.
fn serve(stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;

    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (status, reply) = match (method, path) {
        ("GET", "/api/tags") => (
            "200 OK",
            format!(
                "{{\"models\":[{}]}}",
                MODELS
                    .iter()
                    .map(|m| format!("{{\"name\":\"{m}:latest\"}}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        ("GET", "/api/version") => ("200 OK", "{\"version\":\"0.0.0-fake\"}".to_string()),
        ("POST", "/api/embed") => embed(&body),
        _ => ("404 Not Found", "{\"error\":\"not found\"}".to_string()),
    };
    let mut out = stream;
    write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    )?;
    out.flush()
}

fn embed(body: &[u8]) -> (&'static str, String) {
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else {
        return ("400 Bad Request", "{\"error\":\"not JSON\"}".into());
    };
    let model = request["model"].as_str().unwrap_or("");
    let model = model.strip_suffix(":latest").unwrap_or(model);
    if !MODELS.contains(&model) {
        return (
            "404 Not Found",
            format!("{{\"error\":\"model \\\"{model}\\\" not found, try pulling it first\"}}"),
        );
    }
    match request["input"].as_str() {
        Some(text) => (
            "200 OK",
            format!("{{\"embeddings\":[{}]}}", vectors::json(text)),
        ),
        None => (
            "400 Bad Request",
            "{\"error\":\"input must be a string\"}".into(),
        ),
    }
}
