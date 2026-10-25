//! Embeddings via Ollama -- optional.
//!
//! One embedding model per database (bge-m3, 1024 dimensions, recorded in the
//! `embed_model` column) -- mixing models makes cosine distance meaningless.
//!
//! Having no embeddings is a supported state, not a failure: with
//! `EMBEDDINGS=off`, Ollama unreachable or the model not pulled, recall ranks by
//! keyword only and writes store their row without a vector -- and every command
//! says which of the three it was. `embed --missing` fills the vectors in later.

pub mod missing;

use std::time::Duration;

use crate::config::{Config, DIM};
use crate::error::{Error, Result};

/// Ollama can take a while on a cold model load; the Python client allowed 120s.
const EMBED_TIMEOUT: Duration = Duration::from_secs(120);
/// The liveness probe must not stall a `status` call.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why this run has no embeddings. Printed by `status`, `recall` and `remember`,
/// so the reason is always named rather than implied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// `EMBEDDINGS=off`: the binary does not contact Ollama at all.
    Off,
    /// Nothing answered at the configured URL with a model list.
    Unreachable { url: String },
    /// Ollama answered, but the configured model is not pulled.
    NotPulled { url: String, model: String },
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unavailable::Off => write!(f, "EMBEDDINGS=off"),
            Unavailable::Unreachable { url } => write!(f, "Ollama unreachable at {url}"),
            Unavailable::NotPulled { url, model } => write!(
                f,
                "{model} is not pulled in Ollama at {url} (ollama pull {model})"
            ),
        }
    }
}

impl Unavailable {
    /// The short form for a section header. "Ollama down" is what the
    /// keyword-only header has always said, so that case keeps its wording.
    pub fn label(&self) -> String {
        match self {
            Unavailable::Off => "EMBEDDINGS=off".to_string(),
            Unavailable::Unreachable { .. } => "Ollama down".to_string(),
            Unavailable::NotPulled { model, .. } => format!("{model} not pulled"),
        }
    }
}

/// Where vectors come from. A trait so the degradation paths are tested with an
/// injected fake instead of a live Ollama.
pub trait Embedder {
    /// `Ok` when Ollama answers and the configured model is pulled.
    fn probe(&self) -> std::result::Result<(), Unavailable>;

    /// Embed one text, or fail. There is deliberately no fallback vector: a row
    /// stored with a wrong vector is mis-ranked by every later search, whereas a
    /// row stored without one is visibly unembedded and can be filled in.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

/// Whether this run has embeddings. With `EMBEDDINGS=off` the embedder is never
/// asked, so the binary does not contact Ollama at all.
pub fn availability(cfg: &Config, embedder: &dyn Embedder) -> std::result::Result<(), Unavailable> {
    if !cfg.embeddings {
        return Err(Unavailable::Off);
    }
    embedder.probe()
}

/// The real embedder: Ollama's HTTP API.
pub struct Ollama {
    url: String,
    model: String,
}

impl Ollama {
    pub fn new(cfg: &Config) -> Self {
        Ollama {
            url: cfg.ollama_url.clone(),
            model: cfg.embed_model.clone(),
        }
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into()
}

impl Embedder for Ollama {
    /// `/api/tags` rather than `/api/version`: an Ollama that answers but lacks
    /// the model fails every embed, and that is a different fix for the user.
    fn probe(&self) -> std::result::Result<(), Unavailable> {
        let tags: Option<serde_json::Value> = agent(PROBE_TIMEOUT)
            .get(&format!("{}/api/tags", self.url))
            .call()
            .ok()
            .and_then(|mut r| r.body_mut().read_json().ok());
        // Anything that is not a model list is not an Ollama we can use.
        match tags.and_then(|t| model_is_pulled(&t, &self.model)) {
            Some(true) => Ok(()),
            Some(false) => Err(Unavailable::NotPulled {
                url: self.url.clone(),
                model: self.model.clone(),
            }),
            None => Err(Unavailable::Unreachable {
                url: self.url.clone(),
            }),
        }
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let body = serde_json::json!({ "model": self.model, "input": text });
        let mut resp = agent(EMBED_TIMEOUT)
            .post(&format!("{}/api/embed", self.url))
            .send_json(&body)
            .map_err(|e| Error::failed(format!("ollama at {} is unreachable: {e}", self.url)))?;
        let json: serde_json::Value = resp
            .body_mut()
            .read_json()
            .map_err(|e| Error::failed(format!("ollama returned a body that is not JSON: {e}")))?;
        parse_embedding(&json, &self.model)
    }
}

/// Whether `/api/tags` lists `model`, or `None` when the body is not a model list.
///
/// Ollama names every model with a tag, and a bare name means `:latest` -- the
/// configured `bge-m3` is listed as `bge-m3:latest`.
fn model_is_pulled(tags: &serde_json::Value, model: &str) -> Option<bool> {
    let wanted = if model.contains(':') {
        model.to_string()
    } else {
        format!("{model}:latest")
    };
    let models = tags.get("models")?.as_array()?;
    Some(models.iter().any(|m| {
        ["name", "model"]
            .iter()
            .any(|k| m.get(k).and_then(|v| v.as_str()) == Some(wanted.as_str()))
    }))
}

/// Split out from the HTTP call so the shape checks are testable without Ollama.
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

/// A stand-in for Ollama that counts every call, so a test can assert that
/// `EMBEDDINGS=off` never reaches it.
#[cfg(test)]
pub(crate) mod fake {
    use std::cell::Cell;

    use super::{Embedder, Unavailable};
    use crate::config::DIM;
    use crate::error::Result;

    pub struct Fake {
        probe: std::result::Result<(), Unavailable>,
        /// Probes and embeds alike.
        pub calls: Cell<usize>,
    }

    impl Fake {
        pub fn up() -> Self {
            Fake {
                probe: Ok(()),
                calls: Cell::new(0),
            }
        }

        pub fn down(why: Unavailable) -> Self {
            Fake {
                probe: Err(why),
                calls: Cell::new(0),
            }
        }

        /// A deterministic, non-zero vector per text: equal texts get equal
        /// vectors, different texts different ones.
        pub fn vector(text: &str) -> Vec<f32> {
            // FNV-1a seeds a small LCG; meaning is irrelevant, determinism is not.
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in text.bytes() {
                h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
            }
            (0..DIM)
                .map(|_| {
                    h = h
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    0.01 + ((h >> 40) % 1000) as f32 / 1000.0
                })
                .collect()
        }
    }

    impl Embedder for Fake {
        fn probe(&self) -> std::result::Result<(), Unavailable> {
            self.calls.set(self.calls.get() + 1);
            self.probe.clone()
        }

        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            self.calls.set(self.calls.get() + 1);
            Ok(Fake::vector(text))
        }
    }
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

    fn cfg(embeddings: bool) -> Config {
        let mut cfg = crate::config::Config::from_env(&crate::config::Env {
            vars: Default::default(),
            home: std::path::PathBuf::from("/home/tester"),
            windows: false,
            dotenv: crate::config::DotEnv::absent(crate::config::tests::EXE.into()),
        })
        .unwrap();
        cfg.embeddings = embeddings;
        cfg
    }

    /// `EMBEDDINGS=off` means the binary never contacts Ollama -- not even to ask
    /// whether it is there.
    #[test]
    fn embeddings_off_never_asks_the_embedder() {
        let fake = fake::Fake::up();
        assert_eq!(availability(&cfg(false), &fake), Err(Unavailable::Off));
        assert_eq!(fake.calls.get(), 0);
    }

    #[test]
    fn embeddings_on_is_whatever_the_probe_finds() {
        let fake = fake::Fake::up();
        assert_eq!(availability(&cfg(true), &fake), Ok(()));
        assert_eq!(fake.calls.get(), 1);
        let why = Unavailable::Unreachable {
            url: "http://127.0.0.1:9".into(),
        };
        let fake = fake::Fake::down(why.clone());
        assert_eq!(availability(&cfg(true), &fake), Err(why));
    }

    /// What Ollama 0.32 answers on `/api/tags`, trimmed.
    fn tags(names: &[&str]) -> serde_json::Value {
        json!({ "models": names.iter().map(|n| json!({ "name": n, "model": n })).collect::<Vec<_>>() })
    }

    /// A bare model name means its `:latest` tag, as it does to `ollama pull`.
    #[test]
    fn a_pulled_model_is_found_by_its_bare_name_or_its_full_tag() {
        let t = tags(&["nomic-embed-text:latest", "bge-m3:latest"]);
        assert_eq!(model_is_pulled(&t, "bge-m3"), Some(true));
        assert_eq!(model_is_pulled(&t, "bge-m3:latest"), Some(true));
        assert_eq!(model_is_pulled(&t, "bge-m3:567m"), Some(false));
        assert_eq!(
            model_is_pulled(&tags(&["bge-m3:567m"]), "bge-m3"),
            Some(false)
        );
        assert_eq!(model_is_pulled(&tags(&[]), "bge-m3"), Some(false));
    }

    #[test]
    fn a_body_that_is_not_a_model_list_is_not_read_as_model_missing() {
        assert_eq!(model_is_pulled(&json!({"error": "nope"}), "bge-m3"), None);
        assert_eq!(model_is_pulled(&json!([1, 2]), "bge-m3"), None);
    }

    /// The real client against a port that refuses: no hang, and the reason
    /// names the URL rather than a bare "error".
    #[test]
    fn a_dead_ollama_url_is_unreachable() {
        let mut c = cfg(true);
        c.ollama_url = "http://127.0.0.1:9".into();
        assert_eq!(
            Ollama::new(&c).probe(),
            Err(Unavailable::Unreachable {
                url: "http://127.0.0.1:9".into()
            })
        );
    }

    /// Each reason is printed to an agent, so each must say what to fix.
    #[test]
    fn every_reason_names_its_cause() {
        let off = Unavailable::Off.to_string();
        assert!(off.contains("EMBEDDINGS=off"), "{off}");
        let down = Unavailable::Unreachable {
            url: "http://box:11434".into(),
        }
        .to_string();
        assert!(
            down.contains("Ollama unreachable") && down.contains("http://box:11434"),
            "{down}"
        );
        let pull = Unavailable::NotPulled {
            url: "http://box:11434".into(),
            model: "bge-m3".into(),
        }
        .to_string();
        assert!(
            pull.contains("bge-m3") && pull.contains("not pulled") && pull.contains("ollama pull"),
            "{pull}"
        );
    }

    /// The keyword-only header keeps its old wording for the one case it always
    /// described, and names the real cause otherwise.
    #[test]
    fn the_short_label_keeps_ollama_down_and_names_the_other_causes() {
        assert_eq!(
            Unavailable::Unreachable { url: "u".into() }.label(),
            "Ollama down"
        );
        assert_eq!(Unavailable::Off.label(), "EMBEDDINGS=off");
        assert_eq!(
            Unavailable::NotPulled {
                url: "u".into(),
                model: "bge-m3".into()
            }
            .label(),
            "bge-m3 not pulled"
        );
    }
}
