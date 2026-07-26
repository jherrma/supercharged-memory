#!/usr/bin/env python3
"""One-time seed bootstrap — empty by default. Add your own SEM/EPI entries below,
then run AFTER creating the tables from schema.sql, into an empty DB (it appends)."""
import sys
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
from remember import store_memory  # noqa: E402

# Example semantic entry (baseline needs confirm=True and the user's explicit go-ahead):
# dict(topic="example-topic", category="user", source="user-stated", model="unknown",
#      keywords="example, keyword",
#      text="Example memory text."),
SEM = [
]

# Example episodic entry:
# dict(topic="example-event", event_type="milestone", importance="notable",
#      source="conversation", model="unknown",
#      keywords="example, keyword",
#      text="Example event text."),
EPI = [
]

n = 0
for m in SEM:
    store_memory("semantic", m.pop("text"), confirm_baseline=m.pop("confirm", False), force=True, **m)
    print(f"semantic {m['topic']}"); n += 1
for m in EPI:
    store_memory("episodic", m.pop("text"), force=True, **m)
    print(f"episodic {m['topic']}"); n += 1
print(f"seeded {n} memories")
