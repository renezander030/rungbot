//! Zone watch: the three ways resting deploy zones stop doing their job.
//!
//! The deploy layer places GTC limit buys and then nothing re-anchors them, nothing says
//! when a coin's RUN gate flips, and cash freed by a venue-side cancel never re-ladders.
//! This watcher names all three:
//!
//! * **RUN flip** — a coin's 3-of-4 strength gate changed since last seen. Several alts
//!   running at once is the alt-season tripwire: the case for keeping alt dip ladders was
//!   measured on alts that were *not* running.
//! * **Stale rung** — rung 1 unfilled for `stale_days` while spot ran `stale_extra_pp`
//!   further above it than when it was placed: the wait is not being paid. Once per order.
//! * **Idle cash** — free stable on a venue above the minimum with no tranche coming for
//!   it. Once per venue until the amount moves by a quarter.
//!
//! Every mail leads with the verdict: how many items need a person. A RUN flip on its own
//! needs nobody and is filed as no-action, with what it means for the coins held.

use super::json::{obj, Json};
use super::pyfmt::{comma, comma_g, fixed, g6, ljust, sum as pysum};
use super::textwrap::wrap;
use super::{upper, utc_minute, Hints};

#[derive(Debug, Clone, PartialEq)]
pub struct ZoneConfig {
    pub stale_days: f64,
    pub stale_extra_pp: f64,
    /// Free stable below this is not idle cash (the deploy minimum).
    pub idle_min_usd: f64,
    /// Alts running at once that make the RUN flips a decision.
    pub alt_run_tripwire: i64,
    /// The alts the tripwire counts, in display order.
    pub alts: Vec<String>,
    /// A book snapshot older than this says nothing about holdings.
    pub book_stale_s: f64,
}

impl Default for ZoneConfig {
    fn default() -> Self {
        ZoneConfig {
            stale_days: 14.0,
            stale_extra_pp: 5.0,
            idle_min_usd: 25.0,
            alt_run_tripwire: 3,
            alts: Vec::new(),
            book_stale_s: 24.0 * 3600.0,
        }
    }
}

/// The capital split used to hold back the pending top-up of the Gate venue on the
/// Revolut X venue (see [`onramp_reserve`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReserveConfig {
    /// `(symbol, weight)`.
    pub alloc: Vec<(String, f64)>,
    /// `(symbol, venue)` for every routed coin.
    pub routing: Vec<(String, String)>,
}

/// Top-up gaps below this are not worth a transfer, so nothing is held back for them.
pub const DUE_MIN_USD: f64 = 50.0;

impl ReserveConfig {
    /// Gate's share of the working book (Revolut X + Gate); Binance is excluded.
    pub fn gate_share(&self) -> f64 {
        let (mut revx, mut gate) = (0.0, 0.0);
        for (sym, venue) in &self.routing {
            let w = self
                .alloc
                .iter()
                .find(|(s, _)| s == sym)
                .map(|(_, w)| *w)
                .unwrap_or(0.0);
            match venue.as_str() {
                "revx" => revx += w,
                "gate" => gate += w,
                _ => {}
            }
        }
        let working = revx + gate;
        if working != 0.0 {
            gate / working
        } else {
            0.0
        }
    }

    /// Stable that must stay free on Revolut X for the pending Gate top-up; 0 below
    /// [`DUE_MIN_USD`].
    pub fn onramp_reserve(&self, rx_stable: f64, gate_stable: f64) -> f64 {
        let gap = (rx_stable + gate_stable) * self.gate_share() - gate_stable;
        if gap >= DUE_MIN_USD {
            gap
        } else {
            0.0
        }
    }
}

pub const SIGNAL_NAMES: [(&str, &str); 5] = [
    ("above_sma30", "above 30d SMA"),
    ("ret30_strong", "+25% in 30d"),
    ("fresh_30d_high", "fresh 30d high"),
    ("higher_lows", "higher lows"),
    ("insufficient_history", "not enough history"),
];

fn signal_name(k: &str) -> &str {
    SIGNAL_NAMES
        .iter()
        .find(|(key, _)| *key == k)
        .map(|(_, v)| *v)
        .unwrap_or(k)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flip {
    pub sym: String,
    pub on: bool,
    pub score: usize,
    pub hits: Vec<String>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stale {
    pub sym: String,
    pub client_id: String,
    pub price: f64,
    pub quote: f64,
    pub age_d: f64,
    pub dist: f64,
    pub placed: f64,
    pub spot: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Idle {
    pub venue: String,
    pub free: f64,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Events {
    pub run: Vec<Flip>,
    pub stale: Vec<Stale>,
    pub idle: Vec<Idle>,
}

impl Events {
    pub fn any(&self) -> bool {
        !(self.run.is_empty() && self.stale.is_empty() && self.idle.is_empty())
    }
}

/// Everything one run found.
#[derive(Debug, Clone, PartialEq)]
pub struct Collected {
    pub market: String,
    /// `{symbol: running}` in the regime's order: the next run's baseline.
    pub running: Json,
    pub alts_running: usize,
    pub events: Events,
    /// Whether the bull sell policy governs sells: `None` when the book could not say.
    pub bull_sells: Option<bool>,
}

/// The first `-N%` / `-N.N%` in an order note: the depth below spot it was placed at.
pub fn depth_from_note(note: &str) -> Option<f64> {
    let b = note.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'-' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                let mut k = j;
                if k + 1 < b.len() && b[k] == b'.' && b[k + 1].is_ascii_digit() {
                    k += 1;
                    while k < b.len() && b[k].is_ascii_digit() {
                        k += 1;
                    }
                }
                if k < b.len() && b[k] == b'%' {
                    return note[i + 1..k].parse().ok();
                }
            }
        }
        i += 1;
    }
    None
}

/// `(hold, bull_sells)` from the executor's persisted book: base units per coin, and
/// whether the bull sell policy governs sells. Stale or unreadable says nothing.
pub fn book_state(book: Option<&Json>, now: f64, stale_s: f64) -> (Json, Option<bool>) {
    let Some(st) = book.filter(|b| b.is_obj()) else {
        return (Json::obj(), None);
    };
    let bal = st.get("_bal").filter(|b| b.truthy());
    let mut hold = bal
        .and_then(|b| b.get("hold"))
        .filter(|h| h.truthy())
        .cloned()
        .unwrap_or_else(Json::obj);
    let ts = bal.and_then(|b| b.float_or0("ts")).unwrap_or(0.0);
    if now - ts > stale_s {
        hold = Json::obj();
    }
    let on = st
        .get("_sellpolicy")
        .filter(|s| s.truthy())
        .and_then(|s| s.get("on"))
        .filter(|v| !v.is_null());
    (hold, on.map(Json::truthy))
}

/// One line on what a flip means for the coins held: held or not, and how far the
/// resting buy rungs now sit below spot. Empty when nothing is known.
pub fn flip_note(sym: &str, rows: &[&Json], hold: &Json, price: Option<f64>) -> String {
    let held = hold
        .get(sym)
        .filter(|h| !h.is_null())
        .and_then(Json::to_float);
    let have = match held {
        None => String::new(),
        Some(h) if h > 0.0 => format!("You hold {} {sym}.", comma_g(h, 6)),
        Some(_) => format!("You hold no {sym}."),
    };
    if let Some(p) = price.filter(|p| *p != 0.0 && !rows.is_empty()) {
        let mut d: Vec<f64> = rows
            .iter()
            .map(|o| (p - o.get("price").and_then(Json::to_float).unwrap_or(0.0)) / p * 100.0)
            .collect();
        d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let span = if d.len() == 1 {
            format!("{}%", fixed(d[0], 1))
        } else {
            format!("{}% to {}%", fixed(d[0], 1), fixed(d[d.len() - 1], 1))
        };
        let rungs = format!(
            "{} resting buy rung{} {span} below spot ${}",
            rows.len(),
            if rows.len() > 1 { "s" } else { "" },
            g6(p)
        );
        return if have.is_empty() {
            format!("Your {rungs}.")
        } else {
            format!("{have} Your {rungs}: a coin that keeps running fills none of them.")
        };
    }
    have
}

fn quote_of(venue: &str) -> Option<&'static str> {
    match venue {
        "binance" => Some("USDC"),
        "gate" => Some("USDT"),
        "revx" => Some("USD"),
        _ => None,
    }
}

fn stable_sum(bal: &Json, assets: &[&str], only_listed: bool) -> Result<f64, String> {
    let mut terms = Vec::new();
    let one = |v: &Json, k: &str| -> Result<f64, String> {
        match v.get(k) {
            None => Ok(0.0),
            Some(x) => x
                .to_float()
                .ok_or_else(|| format!("could not convert {} to float", x.py_str())),
        }
    };
    if only_listed {
        for a in assets {
            let v = bal.get(a).cloned().unwrap_or_else(Json::obj);
            terms.push(one(&v, "free")? + one(&v, "locked")?);
        }
    } else {
        for (a, v) in bal.entries() {
            if assets.contains(&a.as_str()) {
                terms.push(one(v, "free")? + one(v, "locked")?);
            }
        }
    }
    Ok(pysum(terms))
}

/// Free quote stable per venue, net of the Revolut X hold-back for the Gate top-up.
/// `balances` is `(venue, balances or the read error)` in venue order.
pub fn free_stable(
    balances: &[(String, Result<Json, String>)],
    reserve: &ReserveConfig,
) -> Vec<(String, Option<f64>, String)> {
    let mut out: Vec<(String, Option<f64>, String)> = Vec::new();
    for (venue, b) in balances {
        if let Err(e) = b {
            out.push((venue.clone(), None, format!("balances unavailable: {e}")));
        }
    }
    for (venue, b) in balances {
        let Ok(b) = b else { continue };
        let Some(q) = quote_of(venue) else { continue };
        let free = b
            .get(q)
            .filter(|x| x.is_obj())
            .and_then(|x| x.float_or0("free"))
            .unwrap_or(0.0);
        out.push((venue.clone(), Some(free), String::new()));
    }
    let bal = |v: &str| {
        balances
            .iter()
            .find(|(n, b)| n == v && b.is_ok())
            .and_then(|(_, b)| b.as_ref().ok())
    };
    if let (Some(rx), Some(gt)) = (bal("revx"), bal("gate")) {
        let slot = out.iter().position(|(v, _, _)| v == "revx");
        let res = stable_sum(rx, &["USD", "USDC"], false)
            .and_then(|r| Ok((r, stable_sum(gt, &["USDT", "USDC"], true)?)))
            .map(|(r, g)| reserve.onramp_reserve(r, g));
        if let Some(i) = slot {
            match res {
                Ok(r) if r > 0.0 => {
                    if let Some(f) = out[i].1 {
                        out[i].1 = Some((f - r).max(0.0));
                        out[i].2 = format!("after ${} Gate reserve", fixed(r, 2));
                    }
                }
                Ok(_) => {}
                Err(e) => out[i].2 = format!("reserve unknown: {e}"),
            }
        }
    }
    out
}

/// Inputs to one run, as read by the caller.
pub struct Input<'a> {
    pub now: f64,
    /// The previous zone state (`{"run": null, "stale": {}, "idle": {}}` when missing).
    pub state: &'a Json,
    /// The cached regime reading.
    pub reg: &'a Json,
    /// Open `deploy_buy` orders, in journal order.
    pub deploy_rows: &'a [&'a Json],
    pub balances: &'a [(String, Result<Json, String>)],
    /// The executor's book, if readable.
    pub book: Option<&'a Json>,
    pub reserve: &'a ReserveConfig,
    pub cfg: &'a ZoneConfig,
}

/// A fresh zone state.
pub fn empty_state() -> Json {
    obj(vec![
        ("run", Json::Null),
        ("stale", Json::obj()),
        ("idle", Json::obj()),
    ])
}

/// Find this run's events. `price(venue, pair)` reads a spot price.
pub fn collect<F>(inp: &Input<'_>, mut price: F) -> Collected
where
    F: FnMut(&str, &str) -> Result<f64, String>,
{
    let coins = inp.reg.get("coins").map(Json::entries).unwrap_or_default();
    let running = Json::Obj(
        coins
            .iter()
            .map(|(s, c)| (s.clone(), c.get("running").is_some_and(Json::truthy).into()))
            .collect(),
    );
    let mut events = Events::default();

    let mut flips: Vec<(String, bool, usize, Vec<String>)> = Vec::new();
    if let Some(prev) = inp.state.get("run").filter(|r| r.is_obj()) {
        let mut syms: Vec<&(String, Json)> = running.entries().iter().collect();
        syms.sort_by(|a, b| a.0.cmp(&b.0));
        for (s, on) in syms {
            let same = prev.get(s).is_some_and(|p| on.py_eq(p));
            if !same {
                let sig = coins
                    .iter()
                    .find(|(k, _)| k == s)
                    .and_then(|(_, c)| c.get("signals"))
                    .map(Json::entries)
                    .unwrap_or_default();
                let hits = sig
                    .iter()
                    .filter(|(_, v)| *v == Json::Bool(true))
                    .map(|(k, _)| k.clone())
                    .collect();
                let score = sig.iter().filter(|(_, v)| v.truthy()).count();
                flips.push((s.clone(), on.truthy(), score, hits));
            }
        }
    }
    let alts_running = inp
        .cfg
        .alts
        .iter()
        .filter(|a| running.get(a).is_some_and(Json::truthy))
        .count();

    let mut by_coin: Vec<(String, Vec<&Json>)> = Vec::new();
    for o in inp.deploy_rows {
        if !o.get("rung").is_some_and(Json::truthy) || !o.get("order_id").is_some_and(Json::truthy)
        {
            continue;
        }
        let sym = o.get("sym").map(Json::py_str).unwrap_or_default();
        match by_coin.iter_mut().find(|(s, _)| *s == sym) {
            Some((_, rows)) => rows.push(o),
            None => by_coin.push((sym, vec![o])),
        }
    }
    let str_of = |o: &Json, k: &str| o.get(k).map(Json::py_str).unwrap_or_default();

    let (hold, bull_sells) = book_state(inp.book, inp.now, inp.cfg.book_stale_s);
    for (sym, on, score, hits) in flips {
        let rows: Vec<&Json> = by_coin
            .iter()
            .find(|(s, _)| *s == sym)
            .map(|(_, r)| r.clone())
            .unwrap_or_default();
        let mut px = None;
        if let Some(r0) = rows.first() {
            px = match price(&str_of(r0, "exch"), &str_of(r0, "pair")) {
                Ok(p) => Some(p),
                Err(_) => coins
                    .iter()
                    .find(|(k, _)| *k == sym)
                    .and_then(|(_, c)| c.get("px"))
                    .and_then(Json::num),
            };
        }
        let note = flip_note(&sym, &rows, &hold, px);
        events.run.push(Flip {
            sym,
            on,
            score,
            hits,
            note,
        });
    }

    let seen_stale = inp.state.get("stale");
    let mut sorted: Vec<&(String, Vec<&Json>)> = by_coin.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (sym, rows) in sorted {
        let rung = |o: &Json| o.get("rung").and_then(Json::to_float).unwrap_or(9.0);
        let mut r1 = rows[0];
        for o in &rows[1..] {
            if rung(o) < rung(r1) {
                r1 = o;
            }
        }
        let ts = r1.get("ts").and_then(Json::to_float).unwrap_or(inp.now);
        let age_d = (inp.now - ts) / 86400.0;
        let cid = str_of(r1, "client_id");
        if age_d < inp.cfg.stale_days || seen_stale.is_some_and(|s| s.contains_key(&cid)) {
            continue;
        }
        let Ok(spot) = price(&str_of(r1, "exch"), &str_of(r1, "pair")) else {
            continue;
        };
        let p1 = r1.get("price").and_then(Json::to_float).unwrap_or(0.0);
        let dist = (spot - p1) / spot * 100.0;
        let placed = r1
            .get("note")
            .and_then(Json::as_str)
            .and_then(depth_from_note)
            .filter(|d| *d != 0.0)
            .unwrap_or(0.0);
        if dist >= placed + inp.cfg.stale_extra_pp {
            events.stale.push(Stale {
                sym: sym.clone(),
                client_id: cid,
                price: p1,
                quote: r1.get("quote").and_then(Json::to_float).unwrap_or(0.0),
                age_d,
                dist,
                placed,
                spot,
            });
        }
    }

    let idle_seen = inp.state.get("idle");
    for (venue, free, why) in free_stable(inp.balances, inp.reserve) {
        let Some(free) = free.filter(|f| *f >= inp.cfg.idle_min_usd) else {
            continue;
        };
        let last = idle_seen.and_then(|i| i.float_or0(&venue)).unwrap_or(0.0);
        if last != 0.0 && (free - last).abs() / last < 0.25 {
            continue;
        }
        events.idle.push(Idle { venue, free, why });
    }

    Collected {
        market: inp
            .reg
            .get("market")
            .map(Json::py_str)
            .unwrap_or_else(|| "?".into()),
        running,
        alts_running,
        events,
        bull_sells,
    }
}

fn wrap74(text: &str, width: usize) -> Vec<String> {
    let w = wrap(text, width);
    if w.is_empty() {
        vec![String::new()]
    } else {
        w
    }
}

fn sell_rule(bull_sells: Option<bool>) -> &'static str {
    match bull_sells {
        Some(true) => {
            "Sells are on the bull policy right now (multiples of your cost, plus the trail), \
             and it does not read this gate at all."
        }
        Some(false) => {
            "Sells are on the dip ladder right now, so a running coin's upper rungs trail \
             instead of firing at fixed thresholds. The bot does that by itself."
        }
        None => "",
    }
}

struct Todo {
    tag: String,
    head: String,
    why: String,
    opts: Vec<String>,
}

fn todo_items(c: &Collected, cfg: &ZoneConfig, hints: &Hints) -> Vec<Todo> {
    let d = &hints.deploy_cmd;
    let mut items = Vec::new();
    if !c.events.run.is_empty() && c.alts_running as i64 >= cfg.alt_run_tripwire {
        let names: Vec<&str> = cfg
            .alts
            .iter()
            .filter(|a| c.running.get(a).is_some_and(Json::truthy))
            .map(String::as_str)
            .collect();
        items.push(Todo {
            tag: "alt season".into(),
            head: format!(
                "ALT SEASON: {} of {} alts running at once ({}).",
                c.alts_running,
                cfg.alts.len(),
                names.join(", ")
            ),
            why: "The case for keeping the alt dip ladders (alts come back to the rungs after a \
                  BTC turn) was measured on alts that were NOT running, so it does not cover \
                  this. Nothing moves until you say so."
                .into(),
            opts: vec![
                "  keep the alt ladders as they are   do nothing".into(),
                format!("  give alts a market leg             {d} --market <pct> <venue> <SYM>"),
            ],
        });
    }
    for s in &c.events.stale {
        items.push(Todo {
            tag: format!("stale rung {}", s.sym),
            head: format!(
                "STALE RUNG: {} rung 1 at ${} (${}) has sat unfilled {}d.",
                s.sym,
                g6(s.price),
                fixed(s.quote, 0),
                fixed(s.age_d, 0)
            ),
            why: format!(
                "Placed {}% below spot, it is now {}% below spot ${}: the wait is not being \
                 paid. All three answers are fine, pick one.",
                g6(s.placed),
                fixed(s.dist, 1),
                g6(s.spot)
            ),
            opts: vec![
                format!(
                    "  re-anchor to today's spot   {d} --tranche 0 <venue> --only {}",
                    s.sym
                ),
                "  market leg (BTC only)".into(),
                format!("                              {d} --market <pct> <venue> BTC"),
                "  hold                        do nothing, you will not be told about this".into(),
                "                              order again".into(),
            ],
        });
    }
    for i in &c.events.idle {
        items.push(Todo {
            tag: format!("idle cash {}", i.venue),
            head: format!(
                "IDLE CASH: ${} free on {}{}, and no tranche is coming for it.",
                comma(i.free, 2),
                i.venue,
                if i.why.is_empty() {
                    String::new()
                } else {
                    format!(" {}", i.why)
                }
            ),
            why: format!(
                "{d} sweeps idle cash back into zones every 30-min cycle, so this means the \
                 sweep is NOT running: halt file, live trading off, sweep marked stuck (pair \
                 gone?), or placement errors. This one is a bot problem, not a market call."
            ),
            opts: vec![
                "  check the bot log first".into(),
                format!("  by hand   {d} --tranche <usd> {}", i.venue),
            ],
        });
    }
    items
}

/// `(subject, text)`.
pub fn build_message(
    c: &Collected,
    force: bool,
    now: f64,
    cfg: &ZoneConfig,
    hints: &Hints,
) -> (String, String) {
    let todo = todo_items(c, cfg, hints);
    let n = todo.len();
    let verdict = match n {
        0 => "NOTHING TO DO".to_string(),
        1 => "1 THING NEEDS YOU".into(),
        n => format!("{n} THINGS NEED YOU"),
    };
    let mut l = vec![
        format!("ZONE WATCH: {verdict}"),
        format!(
            "market {}, {}/{} alts running (alt-season tripwire at {})",
            upper(&c.market),
            c.alts_running,
            cfg.alts.len(),
            cfg.alt_run_tripwire
        ),
        String::new(),
    ];
    if !todo.is_empty() {
        l.push("YOUR CALL".into());
        for (i, t) in todo.iter().enumerate() {
            for (j, ln) in wrap74(&t.head, 71).iter().enumerate() {
                l.push(if j == 0 {
                    format!("  {}. {ln}", i + 1)
                } else {
                    format!("     {ln}")
                });
            }
            l.extend(wrap74(&t.why, 71).iter().map(|ln| format!("     {ln}")));
            l.push(String::new());
            l.extend(t.opts.iter().map(|o| format!("     {o}")));
            l.push(String::new());
        }
    }
    let ev = &c.events;
    if !ev.run.is_empty() {
        l.push(if todo.is_empty() {
            "FOR THE RECORD, NO ACTION".into()
        } else {
            "ALSO, NO ACTION".into()
        });
        l.push("  RUN gate flips (a strength reading, not an order):".into());
        for f in &ev.run {
            let names: Vec<&str> = f.hits.iter().map(|h| signal_name(h)).collect();
            let names = if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            };
            l.push(format!(
                "    {} {}  ({}/4: {names})",
                ljust(&f.sym, 5),
                if f.on {
                    "now RUNNING"
                } else {
                    "stopped running"
                },
                f.score
            ));
            if !f.note.is_empty() {
                l.extend(
                    wrap74(&f.note, 64)
                        .iter()
                        .map(|ln| format!("          {ln}")),
                );
            }
        }
        let rule = sell_rule(c.bull_sells);
        l.push(String::new());
        l.push("  A flip buys nothing, sells nothing and moves no rung.".into());
        if !rule.is_empty() {
            l.extend(wrap74(rule, 74).iter().map(|ln| format!("  {ln}")));
        }
        l.push(format!(
            "  You get this mail because {} alts running at once IS your call. Right now: {}/{}.",
            cfg.alt_run_tripwire,
            c.alts_running,
            cfg.alts.len()
        ));
        l.push(String::new());
    }
    if force && !ev.any() {
        l.push("No flips, no stale rungs, no idle cash.".into());
        l.push(String::new());
    }
    l.push("Read-only: nothing was placed, cancelled or repriced.".into());
    l.push(format!("Checked: {}", utc_minute(now)));

    let (head, tags) = if !todo.is_empty() {
        let tags: Vec<&str> = todo.iter().map(|t| t.tag.as_str()).collect();
        (
            if n == 1 {
                "1 needs you".to_string()
            } else {
                format!("{n} need you")
            },
            tags.join(", "),
        )
    } else {
        let flips: Vec<String> = ev
            .run
            .iter()
            .map(|f| format!("{} {}", f.sym, if f.on { "running" } else { "stopped" }))
            .collect();
        let mut tags = flips.iter().take(2).cloned().collect::<Vec<_>>().join(", ");
        if tags.is_empty() && force {
            tags = "quiet".into();
        }
        if flips.len() > 2 {
            tags = format!("{} run flips", flips.len());
        }
        ("nothing to do".to_string(), tags)
    };
    let subject = if tags.is_empty() {
        format!("ZONE WATCH: {head}")
    } else {
        format!("ZONE WATCH: {head} ({tags})")
    };
    (subject, l.join("\n"))
}

/// What to print and write after collecting.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Nothing happened: print the line, and (unless dry) write the new baseline.
    Quiet { line: String, state: Json },
    /// Something to send.
    Send { subject: String, text: String },
}

/// Decide the run's outcome. `state` is the loaded state (or [`empty_state`]).
pub fn plan(
    state: &Json,
    c: &Collected,
    force: bool,
    now: f64,
    cfg: &ZoneConfig,
    hints: &Hints,
) -> Plan {
    if !c.events.any() && !force {
        let first = state.get("run").is_none_or(Json::is_null);
        let mut st = state.clone();
        st.set("run", c.running.clone());
        return Plan::Quiet {
            line: format!(
                "zone-watch: quiet{}",
                if first { " (baseline recorded)" } else { "" }
            ),
            state: st,
        };
    }
    let (subject, text) = build_message(c, force, now, cfg, hints);
    Plan::Send { subject, text }
}

/// The state after a delivered mail: the new RUN baseline, each stale order marked
/// (never mailed again) and each idle amount remembered.
pub fn after_delivery(state: &Json, c: &Collected, now: f64) -> Json {
    let mut st = state.clone();
    st.set("run", c.running.clone());
    if !c.events.stale.is_empty() {
        if let Some(s) = st.setdefault_obj("stale") {
            for e in &c.events.stale {
                s.set(&e.client_id, now.into());
            }
        }
    }
    if !c.events.idle.is_empty() {
        if let Some(s) = st.setdefault_obj("idle") {
            for e in &c.events.idle {
                s.set(&e.venue, e.free.into());
            }
        }
    }
    st
}

/// Render every shape of the mail from fixtures, touching nothing: read what you will
/// be sent before you are sent it live.
pub fn preview(now: f64, cfg: &ZoneConfig, hints: &Hints) -> String {
    // Fixtures, not the configured book: six placeholder alts, so every shape renders.
    let cfg = &ZoneConfig {
        alts: ["AAA", "BBB", "CCC", "DDD", "EEE", "FFF"]
            .map(String::from)
            .to_vec(),
        ..cfg.clone()
    };
    let (x, y, z) = (
        cfg.alts[0].clone(),
        cfg.alts[5].clone(),
        cfg.alts[4].clone(),
    );
    let run3 = |note: String| Flip {
        sym: z.clone(),
        on: true,
        score: 3,
        hits: vec![
            "above_sma30".into(),
            "fresh_30d_high".into(),
            "higher_lows".into(),
        ],
        note,
    };
    let rows = [
        obj(vec![("price", 0.0236.into())]),
        obj(vec![("price", 0.02163.into())]),
        obj(vec![("price", 0.02037.into())]),
    ];
    let row_refs: Vec<&Json> = rows.iter().collect();
    let hold = obj(vec![(z.as_str(), 0.0.into())]);
    let running =
        |on: &[&str]| Json::Obj(on.iter().map(|s| (s.to_string(), true.into())).collect());
    let mk = |running: Json, alts_running: usize, events: Events| Collected {
        market: "bull".into(),
        running,
        alts_running,
        events,
        bull_sells: Some(true),
    };
    let mut fa = running(&[&x, &y, &z]);
    fa.set(&x, false.into());
    let cases = vec![
        (
            "A: run flip only",
            mk(
                fa,
                2,
                Events {
                    run: vec![run3(flip_note(&z, &row_refs, &hold, Some(0.026)))],
                    ..Default::default()
                },
            ),
        ),
        (
            "B: run flip that trips alt season",
            mk(
                running(&[&x, &y, &z]),
                3,
                Events {
                    run: vec![run3(format!("You hold no {z}."))],
                    ..Default::default()
                },
            ),
        ),
        (
            "C: stale rung + idle cash",
            mk(
                running(&[&y]),
                1,
                Events {
                    stale: vec![Stale {
                        sym: y.clone(),
                        client_id: "x".into(),
                        price: 0.00147,
                        quote: 251.0,
                        age_d: 21.4,
                        dist: 11.3,
                        placed: 2.0,
                        spot: 0.001658,
                    }],
                    idle: vec![Idle {
                        venue: "gate".into(),
                        free: 812.4,
                        why: String::new(),
                    }],
                    ..Default::default()
                },
            ),
        ),
        (
            "D: --force with nothing at all",
            mk(Json::obj(), 0, Events::default()),
        ),
    ];
    let bar = "=".repeat(78);
    let mut out = String::new();
    for (title, c) in cases {
        let (subject, text) = build_message(&c, true, now, cfg, hints);
        out.push_str(&format!(
            "\n{bar}\n{title}\n{bar}\nSubject: {subject}\n\n{text}\n"
        ));
    }
    let (_, _, _, h, mi, _) = crate::time::civil(now);
    out.push_str(&format!("\n(fixtures only, rendered {h:02}:{mi:02})\n"));
    out
}
