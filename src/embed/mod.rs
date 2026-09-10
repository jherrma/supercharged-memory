//! Embeddings via Ollama.
//!
//! Story 003 fills this in: bge-m3 at `OLLAMA_URL`, asserting 1024 dimensions on
//! every response. One embedding model per database, recorded in the
//! `embed_model` column -- mixing models makes cosine distance meaningless.
//!
//! Degradation is part of the contract: with Ollama down, recall falls back to
//! keyword-only so the database stays usable, and writes are refused, because a
//! row stored without an embedding cannot be found again.
