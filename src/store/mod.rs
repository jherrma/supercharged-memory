//! The storage seam.
//!
//! Story 003 defines the trait and the Turso implementation; issue #3 adds a
//! MongoDB one behind the same trait. It is expressed in memory operations --
//! insert with supersede, retire, hybrid search, append an episodic event,
//! replace the topic index -- never in SQL. A signature that mentions SQL is at
//! the wrong altitude.
//!
//! Two invariants every implementation owes:
//!
//! - Multiprocess WAL on every open (Turso:
//!   `.experimental_multiprocess_wal(true)`, paired with the Windows IO backend).
//!   This is what lets several Claude sessions share one database.
//! - Never auto-create a database. A missing file almost always means a wrong
//!   path, and creating an empty one strands the real database.
