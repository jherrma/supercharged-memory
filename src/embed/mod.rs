//! Embeddings via Ollama.
//!
//! One embedding model per database (bge-m3, 1024 dimensions, recorded in the
//! `embed_model` column) -- mixing models makes cosine distance meaningless.
//!
//! Degradation is part of the contract: with Ollama down, recall falls back to
//! keyword-only so the database stays usable, and writes are refused, because a
//! row stored without an embedding cannot be found again.

use std::time::Duration;

use crate::config::{Config, DIM};
use crate::error::{Error, Result};

/// Ollama can take a while on a cold model load; the Python client allowed 120s.
#[allow(dead_code)]
const EMBED_TIMEOUT: Duration = Duration::from_secs(120);
/// The liveness probe must not stall a `status` call.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Is Ollama reachable? Used to choose between READY and DEGRADED, and to decide
/// whether a write may proceed.
pub fn ollama_up(cfg: &Config) -> bool {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(PROBE_TIMEOUT))
        .build()
        .into();
    agent
        .get(&format!("{}/api/version", cfg.ollama_url))
        .call()
        .is_ok()
}

/// Embed one text, or fail. There is deliberately no fallback vector: a row
/// stored with a wrong or absent embedding is invisible to every later search.
///
/// First caller is story 005 (`remember`); `status` only needs `ollama_up`.
#[allow(dead_code)]
pub fn embed(cfg: &Config, text: &str) -> Result<Vec<f32>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(EMBED_TIMEOUT))
        .build()
        .into();
    let body = serde_json::json!({ "model": cfg.embed_model, "input": text });
    let mut resp = agent
        .post(&format!("{}/api/embed", cfg.ollama_url))
        .send_json(&body)
        .map_err(|e| Error::failed(format!("ollama at {} is unreachable: {e}", cfg.ollama_url)))?;
    let json: serde_json::Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| Error::failed(format!("ollama returned a body that is not JSON: {e}")))?;
    parse_embedding(&json, &cfg.embed_model)
}

/// Split out from the HTTP call so the shape checks are testable without Ollama.
#[allow(dead_code)]
fn parse_embedding(json: &serde_json::Value, model: &str) -> Result<Vec<f32>> {
    let first = json
        .get("embeddings")
        .and_then(|e| e.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            Error::failed(format!(
                "ollama response has no embeddings array (model {model}); \
                 is {model} an embedding model?"
            ))
        })?;

    let vec: Vec<f32> = first
        .iter()
        .filter_map(|v| v.as_f64())
        .map(|v| v as f32)
        .collect();

    if vec.len() != first.len() {
        return Err(Error::failed(
            "ollama returned a non-numeric value inside the embedding".to_string(),
        ));
    }
    if vec.len() != DIM {
        // A refusal, not a failure: nothing was written, and the cause is a
        // configuration mistake the user must fix (wrong model for this database).
        return Err(Error::refused(format!(
            "embedding dim {} != expected {DIM} (model {model}); refusing to insert",
            vec.len()
        )));
    }
    Ok(vec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vec_of(n: usize) -> serde_json::Value {
        json!({ "embeddings": [vec![0.5f64; n]] })
    }

    #[test]
    fn a_well_formed_response_parses_to_dim_floats() {
        let v = parse_embedding(&vec_of(DIM), "bge-m3").unwrap();
        assert_eq!(v.len(), DIM);
        assert!((v[0] - 0.5).abs() < f32::EPSILON);
    }

    /// The guard that stops a second embedding model silently entering the corpus.
    #[test]
    fn a_wrong_dimension_is_refused_and_says_so() {
        let err = parse_embedding(&vec_of(768), "nomic-embed-text").unwrap_err();
        assert_eq!(err.exit_code(), crate::error::EXIT_REFUSED);
        let msg = err.to_string();
        assert!(msg.contains("768"), "{msg}");
        assert!(msg.contains("1024"), "{msg}");
        assert!(msg.contains("nomic-embed-text"), "{msg}");
    }

    #[test]
    fn a_response_without_embeddings_names_the_model() {
        let err = parse_embedding(&json!({"error": "model not found"}), "llama3").unwrap_err();
        assert!(err.to_string().contains("llama3"));
    }

    #[test]
    fn a_non_numeric_element_is_not_silently_dropped() {
        let bad = json!({ "embeddings": [[0.1, "oops", 0.3]] });
        assert!(parse_embedding(&bad, "bge-m3").is_err());
    }

    #[test]
    fn an_empty_embeddings_array_is_an_error_not_an_empty_vector() {
        assert!(parse_embedding(&json!({"embeddings": []}), "bge-m3").is_err());
    }
}
