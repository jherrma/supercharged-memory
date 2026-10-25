# The `instructions` table

## What broke

**Schema.** Version 0.1.0 keeps instructions — directives the user wants followed
on demand, such as "how I want release notes written" — in a table of their own,
`instructions`, with an index on `retired_at`. A database created before then (by
the Python scripts' `schema.sql`, or restored from a dump taken before then) does
not have the table.

Nothing else changed: the memory tables, their rows and their embeddings are left
as they are.

## How to resolve

Run `update` and relay what it prints; once the user agrees, `update -y`. It backs
up the database first, then creates the table and its index in one transaction,
and records this version in the `.env` only once that has succeeded.

Whether the table is there is checked in the database itself, not read from the
`.env`: a database restored from an older dump, or a `.env` pointing at another
database, is migrated all the same.

## Verification

Run `update` again: it must list no pending schema migration. If it still lists
`instructions`, the `-y` run printed the reason it stopped; the backup it took is
the way back (`restore --out <a new file>`).
