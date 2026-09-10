// Package store is the storage seam.
//
// Story 003 defines the interface and the Turso implementation; issue #3 adds a
// MongoDB one behind the same interface. The interface is expressed in memory
// operations — insert with supersede, retire, hybrid search, append an episodic
// event, replace the topic index — never in SQL. A method signature that mentions
// SQL is at the wrong altitude.
//
// Two invariants every implementation owes:
//
//   - multiprocess WAL on every open (Turso: ExperimentalFeatures
//     "multiprocess_wal", paired with vfs "experimental_win_iocp" on Windows).
//     This is what lets several Claude sessions share one database.
//   - never auto-create a database. A missing file almost always means a wrong
//     path, and creating an empty one strands the real database.
package store
