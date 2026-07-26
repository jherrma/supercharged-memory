#!/usr/bin/env python3
"""Manage AI coworkers: named personas with scoped memory + trust-gated
autonomy. WRITES ONLY — reads (list active coworkers, load a coworker's
current state: profile + current appraisal + feedback since) are ad-hoc SQL
via the turso MCP, not this script (see README.md - Coworkers).
"""
import argparse, sys
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
import memlib as M  # noqa: E402

TRUST_LEVELS = ("supervised", "trusted", "autonomous")


def add(name, expertise, personality):
    M.require_db()
    existing = M.scalar(f"SELECT id FROM coworkers WHERE name={M.q(name)};")
    if existing:
        sys.exit(f"refused: coworker '{name}' already exists (id {existing}). "
                 "Use --reactivate if retired, or pick a different name.")
    M.exec_sql("INSERT INTO coworkers (name, expertise, personality) VALUES "
              f"({M.q(name)}, {M.q(expertise)}, {M.q(personality)});")
    print(f"added coworker '{name}'")


def appraise(name, trust, text):
    if trust not in TRUST_LEVELS:
        sys.exit(f"refused: --trust must be one of {TRUST_LEVELS}.")
    if len(text) > M.MAX_TEXT:
        sys.exit(f"refused: appraisal is {len(text)} chars; hard cap {M.MAX_TEXT}.")
    M.require_db()
    cid = M.resolve_coworker(name)
    prior_id = M.scalar(f"SELECT id FROM appraisals WHERE coworker_id={cid} AND superseded_by IS NULL;")
    # period_start: prior appraisal's created_at, or the coworker's own
    # created_at if this is the first-ever appraisal. Two scalar() calls,
    # not one multi-column query — scalar() is the already-proven single-value
    # extraction path; parsing multi-column tursodb output isn't verified anywhere.
    period_start = (M.scalar(f"SELECT created_at FROM appraisals WHERE id={prior_id};")
                    if prior_id else M.scalar(f"SELECT created_at FROM coworkers WHERE id={cid};"))

    insert = ("INSERT INTO appraisals (coworker_id, period_start, trust_level, memory_text) "
             f"VALUES ({cid}, {M.q(period_start)}, {M.q(trust)}, {M.q(text)});")
    # UPDATE doesn't touch last_insert_rowid(), so it still correctly refers
    # to the appraisal INSERT above even after this UPDATE runs.
    supersede = (f" UPDATE appraisals SET superseded_by=last_insert_rowid() WHERE id={prior_id};"
                if prior_id else "")
    update_trust = (f" UPDATE coworkers SET trust_level={M.q(trust)}, "
                    f"updated_at=datetime('now') WHERE id={cid};")
    M.exec_sql("BEGIN; " + insert + supersede + update_trust + " COMMIT;")
    tag = f"superseded appraisal {prior_id}" if prior_id else "first appraisal"
    print(f"appraised '{name}': trust={trust} ({tag})")


def set_trust(name, trust):
    if trust not in TRUST_LEVELS:
        sys.exit(f"refused: trust level must be one of {TRUST_LEVELS}.")
    M.require_db()
    cid = M.resolve_coworker(name)
    M.exec_sql(f"UPDATE coworkers SET trust_level={M.q(trust)}, "
              f"updated_at=datetime('now') WHERE id={cid};")
    print(f"'{name}' trust_level -> {trust}")


def set_active(name, active):
    M.require_db()
    cid = M.resolve_coworker(name)
    M.exec_sql(f"UPDATE coworkers SET active={1 if active else 0}, "
              f"updated_at=datetime('now') WHERE id={cid};")
    print(f"'{name}' {'reactivated' if active else 'retired'}")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--add", action="store_true")
    p.add_argument("--name")
    p.add_argument("--expertise")
    p.add_argument("--personality")
    p.add_argument("--appraise", metavar="NAME")
    p.add_argument("--trust", choices=TRUST_LEVELS)
    p.add_argument("--text")
    p.add_argument("--set-trust", dest="set_trust", nargs=2, metavar=("NAME", "LEVEL"))
    p.add_argument("--retire", metavar="NAME")
    p.add_argument("--reactivate", metavar="NAME")
    a = p.parse_args()

    if a.add:
        if not (a.name and a.expertise and a.personality):
            sys.exit("--add requires --name --expertise --personality")
        add(a.name, a.expertise, a.personality)
    elif a.appraise:
        if not (a.trust and a.text):
            sys.exit("--appraise requires --trust and --text")
        appraise(a.appraise, a.trust, a.text)
    elif a.set_trust:
        set_trust(a.set_trust[0], a.set_trust[1])
    elif a.retire:
        set_active(a.retire, False)
    elif a.reactivate:
        set_active(a.reactivate, True)
    else:
        sys.exit("provide one of --add / --appraise / --set-trust / --retire / --reactivate")


if __name__ == "__main__":
    main()
