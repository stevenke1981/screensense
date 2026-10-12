"""Inspect/seed only an explicitly owned installer test database, never user data."""
import json
import pathlib
import sqlite3
import sys
import time

root = pathlib.Path(sys.argv[1]).resolve(strict=True)
if not root.name.startswith("installer-fixture-"):
    raise SystemExit("Expected an owned installer fixture")
db = root / "gui-check" / "data" / "screensense.db"
mode = sys.argv[2]
conn = sqlite3.connect(db.as_uri() + ("?mode=rw" if mode == "seed" else "?mode=ro"), uri=True)
if mode == "seed":
    now = time.time()
    conn.execute("INSERT INTO frames(ts,app,title,text,image,reason) VALUES(?,?,?,?,NULL,?)",
                 (now, "fixture.exe", "Owned install fixture", "Synthetic retained installer history", "fixture"))
    insight = {"id": 0, "ts": now, "trigger": "fixture", "model": "synthetic", "doing": "Synthetic installer analysis", "state": "", "suggestions": [], "notable": ""}
    conn.execute('INSERT INTO insights(ts,"trigger",model,json) VALUES(?,?,?,?)',
                 (now, "fixture", "synthetic", json.dumps(insight)))
    conn.commit()
elif mode != "read":
    raise SystemExit("Unsupported fixture operation")
result = {"frames": conn.execute("SELECT COUNT(*) FROM frames").fetchone()[0],
          "insights": conn.execute("SELECT COUNT(*) FROM insights").fetchone()[0],
          "controls": dict(conn.execute("SELECT key,value FROM meta WHERE key LIKE 'control.%'"))}
conn.close()
print(json.dumps(result, sort_keys=True))
