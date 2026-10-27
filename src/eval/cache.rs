//! The query-embedding cache: pure derived data, kept beside the database so a
//! second run does not embed every case again. Delete it freely.
//!
//! One file per embedding model: a vector from another model measures nothing
//! about this database, and a cache that did not say which model made it would
//! serve one silently.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::DIM;
use crate::embed::Embedder;
use crate::error::Result;

pub struct QueryVectors {
    path: PathBuf,
    vectors: HashMap<String, Vec<f32>>,
    dirty: bool,
    /// What went wrong reading the file. The cache is derived, so none of it
    /// stops a run, but none of it is silent either.
    pub warnings: Vec<String>,
}

impl QueryVectors {
    /// The cache for `model` in `dir`. A missing file is an empty cache; an
    /// unreadable or malformed one is too, with a warning, and entries of the
    /// wrong width are dropped.
    pub fn open(dir: &Path, model: &str) -> Self {
        let file: String = model
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = dir.join(format!("qvec-{file}.json"));
        let mut warnings = Vec::new();
        let vectors = match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text).unwrap_or_else(|why| {
                warnings.push(format!(
                    "ignoring the query-embedding cache {}: {why}; it is derived data and \
                     is rebuilt",
                    path.display()
                ));
                HashMap::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                warnings.push(format!(
                    "cannot read the query-embedding cache {}: {e}",
                    path.display()
                ));
                HashMap::new()
            }
        };
        QueryVectors {
            path,
            vectors,
            dirty: false,
            warnings,
        }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The vector for `query`: cached, or embedded now and remembered.
    pub fn get(&mut self, query: &str, embedder: &dyn Embedder) -> Result<Vec<f32>> {
        if let Some(v) = self.vectors.get(query) {
            return Ok(v.clone());
        }
        let v = embedder.embed(query)?;
        self.vectors.insert(query.to_string(), v.clone());
        self.dirty = true;
        Ok(v)
    }

    /// Write the cache back when something was added. The directory is created
    /// only now. A failed write is a warning, never an error: the cache is
    /// derived.
    pub fn save(&self) -> Option<String> {
        if !self.dirty {
            return None;
        }
        let object: serde_json::Map<String, serde_json::Value> = self
            .vectors
            .iter()
            .map(|(q, v)| {
                // The shortest decimal that is this f32, so a reload gives the
                // same vector back.
                let numbers = v
                    .iter()
                    .map(|x| serde_json::Value::from(x.to_string().parse::<f64>().unwrap_or(0.0)))
                    .collect();
                (q.clone(), serde_json::Value::Array(numbers))
            })
            .collect();
        let text = serde_json::Value::Object(object).to_string();
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            // Beside the target, then renamed: a run that dies halfway leaves
            // the old cache, not half a new one.
            let tmp = self
                .path
                .with_extension(format!("tmp-{}", std::process::id()));
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, &self.path).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
        };
        write().err().map(|e| {
            format!(
                "could not write the query-embedding cache {}: {e}; the next run embeds the \
                 queries again",
                self.path.display()
            )
        })
    }
}

/// The cached vectors of a file: an object of query -> list of numbers, minus
/// any entry that is not exactly `DIM` numbers wide.
fn parse(text: &str) -> std::result::Result<HashMap<String, Vec<f32>>, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let object = value.as_object().ok_or("not a JSON object")?;
    Ok(object
        .iter()
        .filter_map(|(query, v)| {
            let numbers: Vec<f32> = v
                .as_array()?
                .iter()
                .filter_map(|n| n.as_f64().map(|x| x as f32))
                .collect();
            (numbers.len() == DIM && v.as_array()?.len() == DIM).then(|| (query.clone(), numbers))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::fake::Fake;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sm-evalcache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_vector_is_embedded_once_and_then_served_from_the_cache() {
        let d = dir("once");
        let fake = Fake::up();
        let mut c = QueryVectors::open(&d, "bge-m3");
        let v = c.get("how do I restore", &fake).unwrap();
        assert_eq!(v, Fake::vector("how do I restore"));
        assert_eq!(fake.calls.get(), 1);
        c.get("how do I restore", &fake).unwrap();
        assert_eq!(fake.calls.get(), 1, "second ask is cached");
    }

    #[test]
    fn saved_vectors_are_found_by_the_next_run_without_the_embedder() {
        let d = dir("roundtrip");
        let fake = Fake::up();
        let mut c = QueryVectors::open(&d, "bge-m3");
        c.get("first", &fake).unwrap();
        c.get("second", &fake).unwrap();
        assert_eq!(c.save(), None);
        assert!(c.path().exists());

        let again = Fake::up();
        let mut c = QueryVectors::open(&d, "bge-m3");
        assert!(c.warnings.is_empty());
        assert_eq!(c.get("first", &again).unwrap(), Fake::vector("first"));
        assert_eq!(c.get("second", &again).unwrap(), Fake::vector("second"));
        assert_eq!(again.calls.get(), 0, "both came from the file");
    }

    #[test]
    fn each_model_has_a_cache_of_its_own() {
        let d = dir("models");
        let fake = Fake::up();
        let mut a = QueryVectors::open(&d, "bge-m3");
        a.get("q", &fake).unwrap();
        a.save();
        let b = QueryVectors::open(&d, "other-model:latest");
        assert_ne!(a.path(), b.path());
        let mut b = b;
        let calls = fake.calls.get();
        b.get("q", &fake).unwrap();
        assert_eq!(
            fake.calls.get(),
            calls + 1,
            "another model's cache is not used"
        );
        let name = b.path().file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
            "{name}"
        );
    }

    #[test]
    fn nothing_is_written_when_nothing_was_added() {
        let d = dir("clean");
        let c = QueryVectors::open(&d, "bge-m3");
        assert_eq!(c.save(), None);
        assert!(!d.exists(), "not even the directory");
    }

    #[test]
    fn a_malformed_file_is_ignored_with_a_warning_and_replaced_on_save() {
        let d = dir("corrupt");
        std::fs::create_dir_all(&d).unwrap();
        let path = QueryVectors::open(&d, "bge-m3").path().to_path_buf();
        std::fs::write(&path, "{ not json").unwrap();
        let fake = Fake::up();
        let mut c = QueryVectors::open(&d, "bge-m3");
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        assert!(c.warnings[0].contains("qvec"), "{:?}", c.warnings);
        c.get("q", &fake).unwrap();
        assert_eq!(c.save(), None);
        assert!(QueryVectors::open(&d, "bge-m3").warnings.is_empty());
    }

    #[test]
    fn an_entry_of_the_wrong_width_is_dropped_and_embedded_again() {
        let d = dir("width");
        std::fs::create_dir_all(&d).unwrap();
        let path = QueryVectors::open(&d, "bge-m3").path().to_path_buf();
        std::fs::write(&path, r#"{"short": [0.1, 0.2]}"#).unwrap();
        let fake = Fake::up();
        let mut c = QueryVectors::open(&d, "bge-m3");
        assert_eq!(c.get("short", &fake).unwrap(), Fake::vector("short"));
        assert_eq!(fake.calls.get(), 1);
    }

    #[test]
    fn a_cache_that_cannot_be_written_is_a_warning() {
        let d = dir("unwritable");
        std::fs::create_dir_all(d.parent().unwrap()).unwrap();
        // A file where the directory should be.
        std::fs::write(&d, "in the way").unwrap();
        let fake = Fake::up();
        let mut c = QueryVectors::open(&d, "bge-m3");
        c.get("q", &fake).unwrap();
        let warning = c.save().expect("a warning");
        assert!(warning.contains("cache"), "{warning}");
    }
}
