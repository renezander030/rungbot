"""Ladder state: the high-water rungs and ledgers that make a rung fire exactly once.

The write is atomic on purpose. A kill mid-write would otherwise leave half-written JSON
that parses as empty on the next run — and an empty state re-fires every active rung.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

VERSION = 1


def default_path() -> Path:
    """`$RUNGBOT_STATE`, else XDG state dir, else ~/.local/state/rungbot/state.json."""
    if os.environ.get("RUNGBOT_STATE"):
        return Path(os.environ["RUNGBOT_STATE"]).expanduser()
    base = os.environ.get("XDG_STATE_HOME") or str(Path.home() / ".local" / "state")
    return Path(base).expanduser() / "rungbot" / "state.json"


def load(path: Path) -> dict:
    """Prior state, or {} on a cold start or an unreadable file."""
    if not path.exists():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as e:
        print(f"WARN: unreadable state at {path} ({e}); starting cold", file=sys.stderr)
        return {}
    if not isinstance(data, dict):
        return {}
    return data.get("coins", {}) if "coins" in data else data


def save(path: Path, state: dict) -> None:
    """Atomic write. Failure warns and continues — a report is still worth printing."""
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps({"version": VERSION, "coins": state}, indent=2),
                       encoding="utf-8")
        os.replace(tmp, path)
    except OSError as e:
        print(f"WARN: could not write state to {path}: {e}", file=sys.stderr)
