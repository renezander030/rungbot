"""Configuration: the watchlist and the ladder's tunables, loaded from YAML.

Nothing about a particular portfolio lives in the code. A coin, its venue, its pair and
its optional cost basis are all config. That is the whole point of this module: the
strategy is general, the book is yours.

YAML is parsed with PyYAML when it is installed, and otherwise by a tiny built-in
reader that understands the flat two-level subset this file format actually uses. That
keeps `rungbot plan` working on a bare interpreter with zero dependencies.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field, replace
from pathlib import Path

from .ladder import Bands

VENUES = ("binance", "gate", "coingecko")


class ConfigError(ValueError):
    """Raised for a config a human needs to fix, with a message that says how."""


@dataclass(frozen=True)
class Coin:
    symbol: str
    venue: str
    pair: str
    name: str = ""
    entry: float | None = None          # cost basis in quote currency; None = sells off
    bands: Bands | None = None          # per-coin override of the global bands

    def bands_or(self, default: Bands) -> Bands:
        return self.bands or default


@dataclass(frozen=True)
class Settings:
    """Every knob the ladder reads. Defaults are the notify-only, conservative ones."""

    bands: Bands = field(default_factory=Bands)
    min_trade_pct: float = 1.0          # don't emit a trade smaller than this
    min_core_pct: float = 20.0          # never sell below this % of the position
    window_hours: float = 24.0          # directional lock after an action
    buy_floor_pct: float = 50.0         # stop dip-buying past this 24h drop
    target_pct: float = 10.0            # take-profit target shown in the report
    breaker_pct: float = 40.0           # circuit breaker threshold (0 = disabled)
    breaker_days: float = 7.0
    trail: str = "off"                  # off | on  (per-coin "auto" needs a regime feed)
    trail_giveback_pct: float = 5.0


@dataclass(frozen=True)
class Config:
    coins: tuple[Coin, ...]
    settings: Settings = field(default_factory=Settings)

    def coin(self, symbol: str) -> Coin | None:
        return next((c for c in self.coins if c.symbol == symbol), None)


# --------------------------------------------------------------------- yaml loading
def _load_yaml(text: str) -> dict:
    try:
        import yaml  # type: ignore
    except ImportError:
        return _mini_yaml(text)
    data = yaml.safe_load(text)
    if not isinstance(data, dict):
        raise ConfigError("config must be a YAML mapping at the top level")
    return data


def _scalar(raw: str):
    """YAML scalar -> python. Only the types this config format can contain."""
    v = raw.strip()
    if not v or v in ("~", "null"):
        return None
    if v[0] in "\"'" and v[-1] == v[0] and len(v) > 1:
        return v[1:-1]
    # YAML 1.1 booleans, matching PyYAML exactly: `off` IS False, not the string "off".
    # Anything that reads a boolean-ish setting must therefore accept a bool (see
    # _settings_from), or the two readers would disagree on the same file.
    low = v.lower()
    if low in ("true", "yes", "on", "y"):
        return True
    if low in ("false", "no", "off", "n"):
        return False
    try:
        return int(v)
    except ValueError:
        pass
    try:
        return float(v)
    except ValueError:
        return v


def _mini_yaml(text: str) -> dict:
    """Dependency-free reader for the nested-mapping subset used by rungbot configs.

    Supports arbitrarily nested `key:` blocks with two-space indentation and scalar
    leaves. It deliberately does NOT support lists, anchors or multi-line strings: if a
    config needs those, PyYAML is one `pip install` away and takes over automatically.
    """
    root: dict = {}
    stack: list[tuple[int, dict]] = [(-1, root)]
    for lineno, raw in enumerate(text.splitlines(), 1):
        line = raw.split("#", 1)[0].rstrip() if not raw.lstrip().startswith("#") else ""
        if not line.strip():
            continue
        if line.lstrip().startswith("- "):
            raise ConfigError(
                f"line {lineno}: YAML lists are not supported by the built-in reader; "
                f"`pip install pyyaml` to use the full syntax"
            )
        indent = len(line) - len(line.lstrip())
        if ":" not in line:
            raise ConfigError(f"line {lineno}: expected `key: value`, got {line.strip()!r}")
        key, _, rest = line.strip().partition(":")
        while stack and indent <= stack[-1][0]:
            stack.pop()
        if not stack:
            raise ConfigError(f"line {lineno}: bad indentation")
        parent = stack[-1][1]
        if rest.strip() == "":
            child: dict = {}
            parent[key.strip()] = child
            stack.append((indent, child))
        else:
            parent[key.strip()] = _scalar(rest)
    return root


# --------------------------------------------------------------------- construction
def _bands_from(raw, default: Bands | None = None) -> Bands | None:
    if not raw:
        return default
    if not isinstance(raw, dict):
        raise ConfigError("`bands` must be a mapping with first_pct and step_pct")
    base = default or Bands()
    try:
        return Bands(
            first_pct=float(raw.get("first_pct", base.first_pct)),
            step_pct=float(raw.get("step_pct", base.step_pct)),
        )
    except (TypeError, ValueError) as e:
        raise ConfigError(f"bad `bands`: {e}") from e


def _trail(value) -> str:
    """Normalise a trail setting to "off"/"on".

    YAML 1.1 turns a bare `off` into the boolean False, so `trail: off` arrives here as
    a bool from PyYAML and from our own reader alike. Accept both spellings rather than
    making the user quote it.
    """
    if isinstance(value, bool):
        return "on" if value else "off"
    trail = str(value).strip().lower()
    if trail not in ("off", "on"):
        raise ConfigError(f"`ladder.trail` must be off or on, got {value!r}")
    return trail


def _settings_from(raw: dict, bands: Bands) -> Settings:
    s = Settings(bands=bands)
    out = {}
    for f in ("min_trade_pct", "min_core_pct", "window_hours", "buy_floor_pct",
              "target_pct", "breaker_pct", "breaker_days", "trail_giveback_pct"):
        if f in raw:
            try:
                out[f] = float(raw[f])
            except (TypeError, ValueError) as e:
                raise ConfigError(f"`ladder.{f}` must be a number: {e}") from e
    if "trail" in raw:
        out["trail"] = _trail(raw["trail"])
    merged = replace(s, **out)
    if not 0 <= merged.min_core_pct < 100:
        raise ConfigError("`ladder.min_core_pct` must be >= 0 and < 100")
    return merged


def _coins_from(raw, default_bands: Bands) -> tuple[Coin, ...]:
    if not isinstance(raw, dict) or not raw:
        raise ConfigError("config needs a `coins:` mapping with at least one coin")
    coins = []
    for sym, spec in raw.items():
        sym = str(sym).upper()
        if not isinstance(spec, dict):
            raise ConfigError(f"coin {sym}: expected a mapping with venue and pair")
        venue = str(spec.get("venue", "")).lower()
        if venue not in VENUES:
            raise ConfigError(
                f"coin {sym}: venue must be one of {', '.join(VENUES)}, got {venue!r}"
            )
        pair = str(spec.get("pair", "")).strip()
        if not pair:
            raise ConfigError(f"coin {sym}: `pair` is required (the venue's own symbol)")
        entry = spec.get("entry")
        if entry is not None:
            try:
                entry = float(entry)
            except (TypeError, ValueError) as e:
                raise ConfigError(f"coin {sym}: `entry` must be a number: {e}") from e
            if entry <= 0:
                raise ConfigError(f"coin {sym}: `entry` must be > 0")
        coins.append(Coin(
            symbol=sym,
            venue=venue,
            pair=pair,
            name=str(spec.get("name", sym)),
            entry=entry,
            bands=_bands_from(spec.get("bands"), None) if spec.get("bands") else None,
        ))
    return tuple(coins)


def load(path: str | Path) -> Config:
    """Read a config file and return a validated Config, or raise ConfigError."""
    p = Path(path)
    if not p.exists():
        raise ConfigError(f"config not found: {p}")
    try:
        text = p.read_text(encoding="utf-8")
    except OSError as e:
        raise ConfigError(f"cannot read {p}: {e}") from e
    return from_mapping(_load_yaml(text))


def from_mapping(data: dict) -> Config:
    bands = _bands_from(data.get("bands"), Bands()) or Bands()
    settings = _settings_from(data.get("ladder") or {}, bands)
    coins = _coins_from(data.get("coins"), bands)
    return apply_env(Config(coins=coins, settings=settings))


def apply_env(cfg: Config) -> Config:
    """Environment overrides, for CI and for one-off experiments.

    `RUNGBOT_FIRST_PCT`, `RUNGBOT_STEP_PCT`, `RUNGBOT_MIN_TRADE_PCT`, ... map onto the
    matching Settings field. Unknown or unparseable values are a hard error rather than
    a silent fallback, so a typo in a cron file cannot quietly change the strategy.
    """
    s = cfg.settings
    bands = s.bands
    for env_key, attr in (("RUNGBOT_FIRST_PCT", "first_pct"), ("RUNGBOT_STEP_PCT", "step_pct")):
        if env_key in os.environ:
            try:
                bands = replace(bands, **{attr: float(os.environ[env_key])})
            except ValueError as e:
                raise ConfigError(f"{env_key}: {e}") from e
    out = {"bands": bands}
    for f in ("min_trade_pct", "min_core_pct", "window_hours", "buy_floor_pct",
              "target_pct", "breaker_pct", "breaker_days", "trail_giveback_pct"):
        key = f"RUNGBOT_{f.upper()}"
        if key in os.environ:
            try:
                out[f] = float(os.environ[key])
            except ValueError as e:
                raise ConfigError(f"{key}: {e}") from e
    if "RUNGBOT_TRAIL" in os.environ:
        out["trail"] = _trail(os.environ["RUNGBOT_TRAIL"])
    return Config(coins=cfg.coins, settings=replace(s, **out))
