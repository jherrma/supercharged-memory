//! The storage seam.
//!
//! The trait is deliberately MINIMAL: it holds exactly the operations a landed
//! command needs, and grows as stories 004-014 land. A trait designed against
//! imagined callers is how a backend interface ends up shaped like one database's
//! query language, which is the mistake issue #3 (MongoDB) has to avoid.
//!
//! Two invariants every implementation owes:
//!
//! - Multiprocess WAL on every open. This is what lets several Claude sessions
//!   share one database.
//! - Never auto-create a database. A missing file almost always means a wrong
//!   path, and creating an empty one strands the real database.

pub mod candidates;
pub mod turso;

use crate::error::Result;

pub trait Store {
    /// Every memory in the database: all semantic rows including superseded and
    /// retired ones, plus all episodic rows. This is the `n` in `READY n`, and it
    /// deliberately counts history rather than current truth -- it answers "is
    /// this the database I think it is", not "how much is live".
    fn total_memories(&self) -> Result<u64>;
}
