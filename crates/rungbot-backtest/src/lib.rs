//! `rungbot-backtest` — the ladder replayed over history.
//!
//! Four backtests and a set of historical replays, all pure computation: price history
//! and a starting book go in, a report comes out. Reading caches and fetching candles is
//! the CLI's job (`rungbot backtest ...`).
//!
//! * [`invariants`] — scripted price paths through the live ladder, asserting its rules.
//! * [`window`] — the ladder *as configured* over a trailing window of real prices, with
//!   dollar bookkeeping, per-pair minimum notional and rollback of skipped trades.
//! * [`sweep`] — A/B variants of the ladder over the same window and book.
//! * [`monthly`] — several windows plus the sweep and per-coin band probes, rolled into
//!   one verdict and an expectation file a divergence check can steer against.
//! * [`replay`] — cycle studies: bull-confirmation replays, cycle-top anatomy, the bull
//!   sell policy over past tops, current market microstate.
//!
//! The report formats are a contract. The monthly verdict reads the window and sweep
//! reports back with patterns, so every line is produced with Python's exact number
//! formatting ([`py`]) and pinned by golden tests against the reference implementation.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod exact;
pub mod invariants;
pub mod monthly;
pub mod py;
pub mod replay;
pub mod sweep;
pub mod window;

use py::Py;

/// Hourly or daily prices per coin, `[(epoch_seconds, price)]`, in file order.
///
/// This is the shape of the window cache (`bt-hist-{days}d.json`): a JSON object of
/// `symbol -> [[ts, price], ...]`. Order matters — the first coin's timestamps are the
/// replay's clock, exactly as in the reference.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct History(pub Vec<(String, Vec<(i64, f64)>)>);

impl History {
    pub fn parse(text: &str) -> Result<History, String> {
        let v = py::loads(text)?;
        let mut out = Vec::new();
        for (k, pts) in v.items() {
            let sym = k
                .as_str()
                .ok_or("history keys must be symbols")?
                .to_string();
            let mut series = Vec::new();
            for p in pts.as_list() {
                let pair = p.as_list();
                if pair.len() < 2 {
                    return Err(format!("{sym}: each point must be [ts, price]"));
                }
                let t = pair[0].as_f64().ok_or("timestamp must be a number")?;
                let px = pair[1].as_f64().ok_or("price must be a number")?;
                series.push((t as i64, px));
            }
            out.push((sym, series));
        }
        if out.is_empty() {
            return Err("history has no coins".into());
        }
        Ok(History(out))
    }

    pub fn to_json(&self) -> String {
        Py::Dict(
            self.0
                .iter()
                .map(|(s, pts)| {
                    (
                        Py::from(s.as_str()),
                        Py::List(
                            pts.iter()
                                .map(|(t, p)| Py::List(vec![Py::Int(*t), Py::Float(*p)]))
                                .collect(),
                        ),
                    )
                })
                .collect(),
        )
        .json(None)
    }

    pub fn get(&self, sym: &str) -> Option<&Vec<(i64, f64)>> {
        self.0.iter().find(|(s, _)| s == sym).map(|(_, v)| v)
    }
}

/// An ordered `name -> number` map, as the book file stores it.
pub type Named = Vec<(String, f64)>;

pub fn named_get(m: &Named, k: &str) -> Option<f64> {
    m.iter().find(|(n, _)| n == k).map(|(_, v)| *v)
}

pub fn named_set(m: &mut Named, k: &str, v: f64) {
    match m.iter_mut().find(|(n, _)| n == k) {
        Some(slot) => slot.1 = v,
        None => m.push((k.to_string(), v)),
    }
}

/// The starting book: holdings per coin, free stable per venue, each pair's minimum
/// order notional, and how many coins share each venue's stable bag.
///
/// Same shape as `bt-book.json`, which the window run writes and the sweep reads, so
/// both replay the identical book.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Book {
    pub held: Named,
    pub stable: Named,
    pub min_notional: Named,
    pub counts: Vec<(String, i64)>,
}

fn named_from(v: Option<&Py>) -> Result<Named, String> {
    let mut out = Vec::new();
    if let Some(v) = v {
        for (k, x) in v.items() {
            let k = k.as_str().ok_or("book keys must be strings")?.to_string();
            let x = x
                .as_f64()
                .ok_or_else(|| format!("book value for {k} is not a number"))?;
            out.push((k, x));
        }
    }
    Ok(out)
}

impl Book {
    pub fn parse(text: &str) -> Result<Book, String> {
        let v = py::loads(text)?;
        let counts = named_from(v.get("counts"))?
            .into_iter()
            .map(|(k, c)| (k, c as i64))
            .collect();
        Ok(Book {
            held: named_from(v.get("held"))?,
            stable: named_from(v.get("stable"))?,
            min_notional: named_from(v.get("min_notional"))?,
            counts,
        })
    }

    pub fn to_py(&self) -> Py {
        let m = |n: &Named| {
            Py::Dict(
                n.iter()
                    .map(|(k, v)| (Py::from(k.as_str()), Py::Float(*v)))
                    .collect(),
            )
        };
        Py::Dict(vec![
            ("held".into(), m(&self.held)),
            ("stable".into(), m(&self.stable)),
            ("min_notional".into(), m(&self.min_notional)),
            (
                "counts".into(),
                Py::Dict(
                    self.counts
                        .iter()
                        .map(|(k, v)| (Py::from(k.as_str()), Py::Int(*v)))
                        .collect(),
                ),
            ),
        ])
    }

    pub fn to_json(&self) -> String {
        self.to_py().json(None)
    }
}

/// How a configured cost basis prints. A whole number was written as one.
pub fn entry_text(v: Option<f64>) -> String {
    match v {
        None => "None".into(),
        Some(x) if x.fract() == 0.0 && x.abs() < 1e16 => format!("{}", x as i64),
        Some(x) => py::repr(x),
    }
}
