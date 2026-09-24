//! The monthly-capital deploy layer: fresh stable on a venue becomes resting limit-buy
//! zones below spot ("let price come to us") instead of a market buy the day it lands.
//!
//! Detection runs inside every live `rungbot-exec run` ([`Layer::check`]):
//!
//! ```text
//! total(venue)  = free stable + stable locked in resting orders (venue-reported)
//! expected      = last baseline + journal-explained fills since then
//! new capital   = total - expected   >= deploy_min_usd  -> a tranche deploys
//! ```
//!
//! A cold start records the baseline without deploying. Withdrawals and noise re-baseline
//! silently; positive dust is carried until it sums to a tranche. The baseline is saved
//! **before** anything is journaled or placed, so a crash can only under-deploy.
//!
//! A tranche cancels the venue's unfilled zones and folds their unspent budget in; the
//! price levels still resting are held ([`rungbot_core::deploy::plan_levels`]). Around it:
//!
//! * the Revolut X onramp: EUR → USDC → USD, keeping the pending transfer to Gate in USDC
//!   and ring-fencing it from the ladder, with an in-flight guard while it travels; when
//!   Gate's balances cannot be read the reserve is unknown and the layer fails closed
//!   (staged USDC stays USDC, no free Revolut X cash is laddered that run);
//! * Gate's USDC (the transfer's bridge asset) auto-converted to USDT;
//! * the idle sweep: cash a venue freed by cancelling a zone is re-laddered;
//! * the withhold: the onramp venue re-laddered lighter so the transfer can leave;
//! * the bull sweep: a zone resting too long in a young bull is bought at market (a
//!   cancel whose final status is unreadable adds nothing, and the buy never exceeds
//!   the venue's free cash minus the reserve);
//! * resume: journaled rungs that never reached the venue are placed (or adopted when
//!   they did), a rung short of free balance by pennies is trimmed once.
//!
//! State lives in the deploy state file (JSON, tmp-and-rename); rows in the order
//! journal carry `kind: deploy_buy`. The manual commands are in [`cli`].

pub mod audit;
pub mod churn;
pub mod cli;
pub mod fillodds;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rungbot_core::deploy::{self as plan, Pinned, PlanKnobs, ZoneOverride};
use rungbot_core::watch::pyfmt::{round as py_round, sum as py_sum};
use serde_json::{json, Map, Value};

use crate::journal::{Journal, Order};
use crate::pyfmt::{fixed, g};
use crate::reconcile::{price_eq, settle_cancel, Settled, VenueSource};
use crate::run::config::RunConfig;
use crate::run::funding;
use crate::run::market::Market;
use crate::run::RunResult;
use crate::venue::{venue_id_matches, Balance, Venue};

/// Placement retries before a rung is given up on.
pub const MAX_RETRIES: i64 = 5;
/// Manual-command venues, in the order the layer walks them.
pub const VENUES: [&str; 3] = ["binance", "gate", "revx"];
/// A sweep or withhold that could not ladder anything waits this long.
pub const SWEEP_RETRY_S: f64 = 86_400.0;
/// Fee headroom when staging USDC (the taker fee is charged on top of the quote size).
pub const ONRAMP_HEADROOM_USD: f64 = 2.0;
/// A top-up that never shows up on Gate stops being "in flight" after this.
pub const INFLIGHT_MAX_S: f64 = 48.0 * 3600.0;
/// Venue credit time against the run's clock.
pub const INFLIGHT_SLACK_S: f64 = 600.0;
/// Our cancel notes (see [`crate::journal::OUR_CANCEL_NOTES`]).
pub const ROLL_NOTE: &str = "rolled into new tranche";
pub const CANCEL_NOTE: &str = "manual --cancel";
pub const BULL_NOTE: &str = "bull sweep -> market";

/// Where the layer reads the market regime and candles.
pub trait RegimeFeed {
    /// `regime-state.json`'s shape: `{market, coins: {SYM: {running, ...}}}`.
    fn regime(&self) -> Result<Value, String>;
    /// `regime-history.json`'s shape: `{labels: [...], ...}`.
    fn label_history(&self) -> Result<Value, String>;
    /// Daily closes for a venue pair (Revolut X pairs read their proxy venue).
    fn closes(&self, exch: &str, pair: &str, n: usize) -> Result<Vec<f64>, String>;
}

/// The live feed: the run's cached regime files and the public candles.
pub struct LiveRegime<'a> {
    pub cfg: &'a RunConfig,
    pub market: &'a dyn Market,
    pub now: f64,
}

impl RegimeFeed for LiveRegime<'_> {
    fn regime(&self) -> Result<Value, String> {
        Ok(crate::run::regime::get_regime(
            self.cfg,
            self.market,
            self.now,
        ))
    }
    fn label_history(&self) -> Result<Value, String> {
        Ok(crate::run::regime::label_history(
            self.cfg,
            self.market,
            self.now,
        ))
    }
    fn closes(&self, exch: &str, pair: &str, n: usize) -> Result<Vec<f64>, String> {
        crate::run::regime::closes_for_route(self.cfg, self.market, exch, pair, n)
    }
}

/// One planned rung, rounded to the venue.
#[derive(Debug, Clone, PartialEq)]
pub struct Rung {
    /// 1-based position in the profile.
    pub rung: i64,
    pub price: f64,
    pub qty: f64,
    pub usd: f64,
    pub note: String,
    pub spot: f64,
    pub held: bool,
    pub since: Option<f64>,
}

/// A venue's zone plan: coins in allocation order, skip reasons, and whether the
/// allocation named none of the venue's coins (equal split).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VenuePlan {
    pub plans: Vec<(String, Vec<Rung>)>,
    pub skips: Vec<String>,
    pub fallback: bool,
}

/// A coin set that restricts a re-ladder; empty means every coin.
pub type Only = Option<BTreeSet<String>>;

fn in_only(only: &Only, sym: &str) -> bool {
    only.as_ref()
        .is_none_or(|o| o.is_empty() || o.contains(sym))
}

/// `float(x or 0)` for a JSON value.
pub(crate) fn num(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
}

fn truthy_f(x: Option<f64>) -> bool {
    x.is_some_and(|v| v != 0.0)
}

fn bal(m: &BTreeMap<String, Balance>, cur: &str) -> Balance {
    m.get(cur).copied().unwrap_or_default()
}

/// Read the deploy state. A missing file is a fresh start; one that does not parse
/// stops the layer (a lost baseline would re-deploy everything).
pub fn load_state(path: &Path) -> Result<Map<String, Value>, String> {
    crate::store::read_state(path)
}

/// Write the deploy state (tmp and rename).
pub fn save_state(path: &Path, st: &Map<String, Value>) -> Result<(), String> {
    crate::store::write_atomic(path, &crate::pyfmt::dumps(st, Some(2)))
}

/// Is `err` a venue saying the rung costs a few cents more than the free balance?
pub fn short_by_pennies(err: &str) -> bool {
    let t = err.to_lowercase();
    if t.contains("balance_not_enough") || t.contains("not enough balance") {
        return true;
    }
    t.contains("insufficient") && (t.contains("balance") || t.contains("funds"))
}

/// The layer, bound to one config, venue set, journal and clock.
pub struct Layer<'a> {
    pub cfg: &'a RunConfig,
    pub venues: &'a dyn VenueSource,
    pub regime: &'a dyn RegimeFeed,
    pub clock: &'a dyn Fn() -> f64,
    pub sleep: &'a dyn Fn(f64),
    /// Writes the journal through: every journal change is saved before the next venue
    /// call, so a crash never loses a row whose order may already rest.
    pub persist: &'a dyn Fn(&Journal) -> Result<(), String>,
    /// Where warnings the reference printed on stderr go.
    pub stderr: &'a dyn Fn(&str),
    pub j: &'a mut Journal,
    /// This call's results, in order.
    pub results: Vec<RunResult>,
}

enum Lvl {
    Done,
    Warn,
    Err,
}

impl<'a> Layer<'a> {
    fn push(&mut self, sym: Option<&str>, side: Option<&str>, committed: bool, l: Lvl, t: String) {
        let mut r = RunResult {
            sym: sym.map(str::to_string),
            side: side.map(str::to_string),
            mode: Some("live".into()),
            hk: true,
            deploy: true,
            committed,
            ..Default::default()
        };
        match l {
            Lvl::Done => r.done = Some(t),
            Lvl::Warn => r.warn = Some(t),
            Lvl::Err => r.err = Some(t),
        }
        self.results.push(r);
    }
    fn done(&mut self, t: String) {
        self.push(None, None, false, Lvl::Done, t)
    }
    fn warn(&mut self, t: String) {
        self.push(None, None, false, Lvl::Warn, t)
    }

    fn save(&self) -> Result<(), String> {
        (self.persist)(self.j)
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    /// The client for `exch`, or the error the reference's `clients[exch]` raised.
    fn client(&self, exch: &str) -> Result<&'a dyn Venue, String> {
        let v: &'a dyn VenueSource = self.venues;
        v.venue(exch).map_err(|_| format!("'{exch}'"))
    }

    fn alloc_weight(&self, sym: &str) -> f64 {
        self.cfg
            .deploy_alloc
            .iter()
            .find(|(s, _)| s.to_uppercase() == sym)
            .map_or(0.0, |(_, w)| *w)
    }

    fn overrides(&self) -> BTreeMap<String, ZoneOverride> {
        self.cfg
            .deploy_zones
            .iter()
            .map(|(k, z)| {
                (
                    k.to_uppercase(),
                    ZoneOverride {
                        depths: z.depths.clone(),
                        weights: z.weights.clone(),
                    },
                )
            })
            .collect()
    }

    /// Quote asset per venue: the routed one (the last route wins), Revolut X USD and
    /// Binance USDC when nothing routes there.
    pub fn quotes(cfg: &RunConfig) -> BTreeMap<String, String> {
        let mut q = BTreeMap::new();
        for (_, r) in &cfg.routing {
            q.insert(r.exch.clone(), r.quote.clone());
        }
        q.entry("revx".into()).or_insert_with(|| "USD".into());
        q.entry("binance".into()).or_insert_with(|| "USDC".into());
        q
    }

    /// A coin's pair on a venue: its route there, else its Revolut X listing.
    pub fn pair_for(cfg: &RunConfig, exch: &str, sym: &str) -> Result<String, String> {
        let routed = cfg.route(sym);
        if exch == "revx" && routed.map(|r| r.exch.as_str()) != Some("revx") {
            return cfg
                .revx_pair(sym)
                .map(str::to_string)
                .ok_or_else(|| format!("'{sym}'"));
        }
        routed
            .map(|r| r.pair.clone())
            .ok_or_else(|| format!("'{sym}'"))
    }

    /// The coins a venue ladders: those routed there, or the Revolut X listings when
    /// nothing routes to it.
    pub fn venue_syms(cfg: &RunConfig, exch: &str) -> Vec<String> {
        let mut syms: Vec<String> = cfg
            .routing
            .iter()
            .filter(|(_, r)| r.exch == exch)
            .map(|(s, _)| s.clone())
            .collect();
        if exch == "revx" && syms.is_empty() {
            syms = cfg.revx_pairs.iter().map(|(s, _)| s.clone()).collect();
        }
        syms
    }

    /// The regime label, `chop` when it cannot be read.
    pub fn regime_label(&self) -> String {
        match self.regime.regime() {
            Ok(r) => match r.get("market") {
                None => "chop".into(),
                Some(Value::String(s)) => s.clone(),
                Some(v) => crate::pyfmt::value_str(v),
            },
            Err(_) => "chop".into(),
        }
    }

    fn structure(
        &self,
        exch: &str,
        pair: &str,
        cache: &mut BTreeMap<String, (Option<f64>, Option<f64>)>,
    ) -> (Option<f64>, Option<f64>) {
        if let Some(s) = cache.get(pair) {
            return *s;
        }
        let s = match self.regime.closes(exch, pair, 35) {
            Ok(c) => plan::structure(&c),
            Err(_) => (None, None),
        };
        cache.insert(pair.to_string(), s);
        s
    }

    fn venue_mins(client: &dyn Venue, pair: &str) -> Result<(f64, f64), String> {
        let l = client.limits(pair).map_err(|e| e.to_string())?;
        Ok((l.min_base, l.min_quote.max(1.0)))
    }

    fn fit_qty(client: &dyn Venue, pair: &str, budget: f64, price: f64) -> Result<f64, String> {
        let step = client.qty_step(pair).map_err(|e| e.to_string())?;
        plan::fit_qty(budget, price, step, |x| {
            client.round_amount(pair, x).map_err(|e| e.to_string())
        })
    }

    /// One coin's zones: `Ok(Ok(rungs))`, `Ok(Err(skip reason))`, or `Err` for a venue
    /// failure the layer does not catch.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_coin(
        &self,
        client: &dyn Venue,
        exch: &str,
        pair: &str,
        amount: f64,
        label: &str,
        cache: &mut BTreeMap<String, (Option<f64>, Option<f64>)>,
        sym: Option<&str>,
        pinned: Option<&BTreeMap<i64, Pinned>>,
    ) -> Result<Result<Vec<Rung>, String>, String> {
        let amount = plan::floor_cents(amount);
        let got = client
            .price(pair)
            .map_err(|e| e.to_string())
            .and_then(|spot| Ok((spot, Self::venue_mins(client, pair)?)));
        let (spot, (minb, minq)) = match got {
            Ok(x) => x,
            Err(e) => return Ok(Err(format!("venue data: {e}"))),
        };
        if !(spot.is_finite() && spot > 0.0) {
            return Ok(Err(format!(
                "venue data: spot {} is not a positive price",
                crate::pyfmt::float_repr(spot)
            )));
        }
        let (profile, warn) = plan::zone_profile(label, sym, &self.overrides());
        if let Some(w) = warn {
            (self.stderr)(&w);
        }
        let st = self.structure(exch, pair, cache);
        let knobs = PlanKnobs {
            max_depth_pct: self.cfg.deploy_max_depth_pct,
            pin_prices: self.cfg.deploy_pin_prices,
        };
        let levels = plan::plan_levels(spot, amount, &profile, st, minq, knobs, pinned);
        let mut out = Vec::new();
        for (i, r) in levels.iter().enumerate() {
            if r.usd <= 0.0 {
                continue;
            }
            let price = client
                .round_price(pair, r.price)
                .map_err(|e| e.to_string())?;
            let qty = Self::fit_qty(client, pair, r.usd, price)?;
            if qty <= 0.0 || qty < minb || qty * price < minq {
                continue;
            }
            out.push(Rung {
                rung: i as i64 + 1,
                price,
                qty,
                usd: qty * price,
                note: r.note.clone(),
                spot,
                held: r.held,
                since: if r.held { r.since } else { None },
            });
        }
        if out.is_empty() {
            return Ok(Err(format!(
                "${} below venue minimum (~${}/order)",
                fixed(amount, 2),
                fixed(minq, 2)
            )));
        }
        Ok(Ok(out))
    }

    /// Split a venue budget over its coins by allocation weight, renormalised within the
    /// venue; a coin whose share cannot clear the venue minimum drops out and its share
    /// goes to the rest.
    pub fn venue_plan(
        &self,
        client: &dyn Venue,
        exch: &str,
        budget: f64,
        label: &str,
        only: &Only,
        pinned: &BTreeMap<String, BTreeMap<i64, Pinned>>,
    ) -> Result<VenuePlan, String> {
        let syms: Vec<String> = Self::venue_syms(self.cfg, exch)
            .into_iter()
            .filter(|s| in_only(only, s))
            .collect();
        let mut weights: Vec<(String, f64)> = syms
            .iter()
            .filter(|s| self.alloc_weight(s) > 0.0)
            .map(|s| (s.clone(), self.alloc_weight(s)))
            .collect();
        let fallback = weights.is_empty();
        if fallback {
            weights = syms.iter().map(|s| (s.clone(), 1.0)).collect();
        }
        let mut cache = BTreeMap::new();
        let mut skips = Vec::new();
        loop {
            let tw = py_sum(weights.iter().map(|(_, w)| *w));
            let mut plans = Vec::new();
            let mut dropped = Vec::new();
            for (s, wt) in &weights {
                let pair = Self::pair_for(self.cfg, exch, s)?;
                match self.plan_coin(
                    client,
                    exch,
                    &pair,
                    budget * wt / tw,
                    label,
                    &mut cache,
                    Some(s),
                    pinned.get(s),
                )? {
                    Err(skip) => {
                        dropped.push(s.clone());
                        skips.push(format!("{s}: {skip}"));
                    }
                    Ok(r) => plans.push((s.clone(), r)),
                }
            }
            if dropped.is_empty() || plans.is_empty() {
                return Ok(VenuePlan {
                    plans,
                    skips,
                    fallback,
                });
            }
            weights.retain(|(s, _)| !dropped.contains(s));
        }
    }

    /// The price levels still resting on a venue, which a re-ladder holds. Rungs placed
    /// under another regime label are left out: a regime flip is when the ladder moves.
    pub fn resting_levels(
        &self,
        exch: &str,
        only: &Only,
    ) -> BTreeMap<String, BTreeMap<i64, Pinned>> {
        let label = self.regime_label();
        let mut out: BTreeMap<String, BTreeMap<i64, Pinned>> = BTreeMap::new();
        for o in self.j.open_orders(Some("deploy_buy")) {
            if o.exch != exch || !has_id(o) || !truthy_i(o.rung) {
                continue;
            }
            if !in_only(only, &o.sym) {
                continue;
            }
            if o.label
                .as_deref()
                .is_some_and(|l| !l.is_empty() && l != label)
            {
                continue;
            }
            if !truthy_f(o.price) {
                continue;
            }
            let since = if truthy_f(o.first_ts) {
                o.first_ts
            } else {
                o.ts
            };
            out.entry(o.sym.clone()).or_default().insert(
                o.rung.unwrap_or(0),
                Pinned {
                    price: o.price.unwrap_or(0.0),
                    note: o.note.clone(),
                    since,
                },
            );
        }
        out
    }

    /// Quote a cancelled zone never spent.
    fn unspent(o: &Order, settled: &Settled) -> f64 {
        let from_settle = match settled {
            Settled::Booked(row) => row.filled_quote.filter(|x| *x != 0.0),
            _ => None,
        };
        let spent = from_settle
            .or(o.part_quote.filter(|x| *x != 0.0))
            .unwrap_or(0.0);
        (o.quote.unwrap_or(0.0) - spent).max(0.0)
    }

    /// Cancel `o` as one of ours with `note`, and book whatever it filled. The inner
    /// error is the venue refusing the cancel; the outer one a journal write failing.
    fn cancel_ours(
        &mut self,
        client: &dyn Venue,
        o: &Order,
        note: &str,
    ) -> Result<Result<Settled, String>, String> {
        let oid = o.order_id.clone().unwrap_or_default();
        if let Err(e) = client.cancel(&o.pair, &oid) {
            return Ok(Err(e.to_string()));
        }
        self.j.update(&o.client_id, |r| {
            r.status = "canceled".into();
            r.note = Some(note.into());
        });
        self.save()?;
        let now = self.now();
        let s = settle_cancel(self.j, client, &o.client_id, now);
        if !matches!(s, Settled::NothingFilled | Settled::NotPlaced) {
            self.save()?;
        }
        Ok(Ok(s))
    }

    /// Roll the venue's unfilled zones: cancel them and return their unspent budget.
    pub fn cancel_open_zones(
        &mut self,
        client: &dyn Venue,
        exch: &str,
        only: &Only,
    ) -> Result<f64, String> {
        let mut rolled = 0.0;
        let rows: Vec<Order> = self
            .j
            .open_orders(Some("deploy_buy"))
            .into_iter()
            .cloned()
            .collect();
        for o in rows {
            if o.exch != exch || !has_id(&o) || !in_only(only, &o.sym) || !truthy_i(o.rung) {
                continue;
            }
            match self.cancel_ours(client, &o, ROLL_NOTE)? {
                Ok(s) => rolled += Self::unspent(&o, &s),
                Err(e) => self.push(
                    Some(&o.sym),
                    None,
                    false,
                    Lvl::Warn,
                    format!(
                        "deploy rollover: cancel failed (may have just filled, reconciles next run): {e}"
                    ),
                ),
            }
        }
        Ok(rolled)
    }

    /// Journal a plan's rungs (skipping ids already journaled) and return the new ids.
    pub fn journal_and_place(
        &mut self,
        exch: &str,
        plans: &[(String, Vec<Rung>)],
        ts_cut: f64,
        label: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let mut placements = Vec::new();
        let prefix = if exch == "revx" { "depx" } else { "dep" };
        for (s, rungs) in plans {
            let pair = Self::pair_for(self.cfg, exch, s)?;
            for r in rungs {
                let cid = format!("{prefix}{s}{}r{}", ts_cut as i64, r.rung);
                if self.j.exists(&cid) {
                    continue;
                }
                self.j.record(Order {
                    client_id: cid.clone(),
                    sym: s.clone(),
                    exch: exch.into(),
                    pair: pair.clone(),
                    side: "buy".into(),
                    kind: "deploy_buy".into(),
                    status: "pending".into(),
                    quote: Some(r.usd),
                    price: Some(r.price),
                    base: Some(r.qty),
                    rung: Some(r.rung),
                    ts: Some(ts_cut),
                    note: Some(r.note.clone()),
                    label: label.map(str::to_string),
                    first_ts: Some(if truthy_f(r.since) {
                        r.since.unwrap_or(ts_cut)
                    } else {
                        ts_cut
                    }),
                    ..Default::default()
                });
                self.save()?;
                placements.push(cid);
            }
            let legs: Vec<String> = rungs
                .iter()
                .map(|r| format!("${} @ ${} ({})", fixed(r.usd, 2), g(r.price, 6), r.note))
                .collect();
            self.push(
                Some(s),
                Some("buy"),
                true,
                Lvl::Done,
                format!(
                    "DEPLOY {s} [spot ${}]: {}",
                    g(rungs[0].spot, 6),
                    legs.join("; ")
                ),
            );
        }
        Ok(placements)
    }

    /// Roll the venue's zones and ladder `tranche` plus what they held. A negative
    /// tranche re-ladders lighter (the withhold). `only` re-ladders just those coins.
    pub fn deploy_tranche(
        &mut self,
        client: &dyn Venue,
        exch: &str,
        quote: &str,
        tranche: f64,
        ts_cut: f64,
        only: &Only,
    ) -> Result<Vec<String>, String> {
        self.deploy_tranche_rolled(client, exch, quote, tranche, ts_cut, only)
            .map(|(cids, _)| cids)
    }

    /// [`Layer::deploy_tranche`], also returning the unspent budget it rolled in.
    pub fn deploy_tranche_rolled(
        &mut self,
        client: &dyn Venue,
        exch: &str,
        quote: &str,
        tranche: f64,
        ts_cut: f64,
        only: &Only,
    ) -> Result<(Vec<String>, f64), String> {
        let label = self.regime_label();
        let pinned = if self.cfg.deploy_pin_prices {
            self.resting_levels(exch, only)
        } else {
            BTreeMap::new()
        };
        let rolled = self.cancel_open_zones(client, exch, only)?;
        let budget = tranche + rolled;
        let vp = self.venue_plan(client, exch, budget, &label, only, &pinned)?;
        if vp.fallback && self.cfg.deploy_alloc.iter().any(|(_, w)| *w != 0.0) {
            self.warn(format!(
                "DEPLOY_ALLOC_JSON names no {exch} coin — falling back to equal split for this venue"
            ));
        }
        for sk in &vp.skips {
            self.warn(format!("deploy skip {sk}"));
        }
        let mut text = if tranche < 0.0 {
            format!(
                "DEPLOY re-ladder on {exch}: ${} rolled from unfilled zones minus ${} withheld for the Gate top-up",
                fixed(rolled, 2),
                fixed(-tranche, 2)
            )
        } else {
            let mut t = format!(
                "DEPLOY tranche on {exch}: ${} fresh {quote}",
                fixed(tranche, 2)
            );
            if rolled != 0.0 {
                t.push_str(&format!(
                    " + ${} rolled from unfilled zones",
                    fixed(rolled, 2)
                ));
            }
            t
        };
        text.push_str(&format!(
            " -> {} coins, regime {}",
            vp.plans.len(),
            label.to_uppercase()
        ));
        if self.cfg.deploy_pin_prices {
            let held = vp
                .plans
                .iter()
                .flat_map(|(_, rs)| rs.iter())
                .filter(|r| r.held)
                .count();
            text.push_str(&format!("; held {held} resting price level(s)"));
        }
        self.push(None, None, !vp.plans.is_empty(), Lvl::Done, text);
        if vp.plans.is_empty() {
            return Ok((Vec::new(), rolled));
        }
        let cids = self.journal_and_place(exch, &vp.plans, ts_cut, Some(&label))?;
        Ok((cids, rolled))
    }

    /// Re-fit a rung to the venue's real free balance when only pennies are missing
    /// (at least 99% of the notional kept); a bigger gap stays a loud failure.
    fn trim_to_free(&mut self, client: &dyn Venue, o: &mut Order) -> Result<bool, String> {
        let (exch, pair, sym) = (o.exch.clone(), o.pair.clone(), o.sym.clone());
        let price = o.price.unwrap_or(0.0);
        let base = o.base.unwrap_or(0.0);
        let quote = o.quote.unwrap_or(0.0);
        if exch.is_empty() || pair.is_empty() || price <= 0.0 || base <= 0.0 {
            return Ok(false);
        }
        let cur = match self.cfg.route(&sym).map(|r| r.quote.clone()) {
            Some(q) if !q.is_empty() => q,
            _ => {
                if pair.contains('/') {
                    pair.rsplit('/').next().unwrap_or_default().to_string()
                } else {
                    pair.rsplit('_').next().unwrap_or_default().to_string()
                }
            }
        };
        let got = (|| -> Result<(f64, f64, (f64, f64)), String> {
            let full = client.balances_full().map_err(|e| e.to_string())?;
            let free = bal(&full, &cur).free;
            let qty = Self::fit_qty(client, &pair, free, price)?;
            Ok((free, qty, Self::venue_mins(client, &pair)?))
        })();
        let Ok((free, qty, (minb, minq))) = got else {
            return Ok(false);
        };
        if !(0.0 < qty && qty < base) || qty < minb || qty * price < minq.max(0.99 * quote) {
            return Ok(false);
        }
        let note = format!(
            "{} (trimmed to free ${})",
            o.note.clone().unwrap_or_default(),
            fixed(free, 2)
        )
        .trim()
        .to_string();
        self.j.update(&o.client_id, |r| {
            r.base = Some(qty);
            r.quote = Some(qty * price);
            r.note = Some(note);
        });
        self.save()?;
        self.push(
            Some(&sym),
            None,
            false,
            Lvl::Done,
            format!(
                "deploy buy {sym} rung trimmed ${} -> ${} to fit free ${}",
                fixed(quote, 2),
                fixed(qty * price, 2),
                fixed(free, 2)
            ),
        );
        o.base = Some(qty);
        o.quote = Some(qty * price);
        Ok(true)
    }

    /// Place one journaled rung. `Ok(false)` when the venue refused it (the row is marked
    /// `error` and retried by a later run).
    pub fn place_entry(
        &mut self,
        client: &dyn Venue,
        o: &mut Order,
        trimmed: bool,
    ) -> Result<bool, String> {
        let cid = o.client_id.clone();
        let placed = client
            .limit_buy(
                &o.pair,
                o.base.unwrap_or(0.0),
                o.price.unwrap_or(0.0),
                Some(&cid),
            )
            .and_then(|raw| client.parse_order(&raw));
        match placed {
            Ok(lf) => {
                self.j.update(&cid, |r| {
                    r.status = if lf.status.is_empty() {
                        "open".into()
                    } else {
                        lf.status.clone()
                    };
                    r.order_id = Some(lf.order_id.clone()).filter(|s| !s.is_empty());
                    r.last_error = None;
                });
                self.save()?;
                Ok(true)
            }
            Err(e) => {
                let e = e.to_string();
                if !trimmed && short_by_pennies(&e) && self.trim_to_free(client, o)? {
                    return self.place_entry(client, o, true);
                }
                let tries = o.retries.unwrap_or(0) + 1;
                self.j.update(&cid, |r| {
                    r.status = "error".into();
                    r.retries = Some(tries);
                    r.last_error = Some(e.clone());
                });
                self.save()?;
                let sym = o.sym.clone();
                self.push(
                    Some(&sym),
                    Some("buy"),
                    false,
                    Lvl::Warn,
                    format!(
                        "deploy limit buy {sym} ${} @ ${} failed (auto-retries): {e}",
                        fixed(o.quote.unwrap_or(0.0), 2),
                        g(o.price.unwrap_or(0.0), 6)
                    ),
                );
                Ok(false)
            }
        }
    }

    /// Place the journaled ids in `cids` that have no venue id yet.
    pub fn place_unplaced(&mut self, client: &dyn Venue, cids: &[String]) -> Result<(), String> {
        for cid in cids {
            if let Some(mut o) = self.j.get(cid).cloned() {
                if !has_id(&o) {
                    self.place_entry(client, &mut o, false)?;
                }
            }
        }
        Ok(())
    }

    /// Crash and error recovery: place journaled rungs that never reached the venue,
    /// adopting an order that already rests there (the placement raced an error).
    pub fn resume_pending(&mut self) -> Result<(), String> {
        let mut rows: Vec<Order> = self
            .j
            .open_orders(Some("deploy_buy"))
            .into_iter()
            .cloned()
            .collect();
        rows.extend(self.j.errored(Some("deploy_buy")).into_iter().cloned());
        for mut o in rows {
            if has_id(&o) || !truthy_i(o.rung) {
                continue;
            }
            let (cid, sym, exch, pair) = (
                o.client_id.clone(),
                o.sym.clone(),
                o.exch.clone(),
                o.pair.clone(),
            );
            if cid.is_empty() || sym.is_empty() || exch.is_empty() || pair.is_empty() {
                continue;
            }
            let tries = o.retries.unwrap_or(0);
            if tries >= MAX_RETRIES {
                if o.gave_up != Some(true) {
                    self.j.update(&cid, |r| r.gave_up = Some(true));
                    self.save()?;
                    self.push(
                        Some(&sym),
                        None,
                        false,
                        Lvl::Warn,
                        format!(
                            "deploy buy failed {tries}x, giving up — place {} {sym} @ ${} manually",
                            g(o.base.unwrap_or(0.0), 6),
                            g(o.price.unwrap_or(0.0), 6)
                        ),
                    );
                }
                continue;
            }
            let Ok(client) = self.client(&exch) else {
                continue;
            };
            let Ok(resting) = client.open_orders(&pair) else {
                continue;
            };
            let claimed: BTreeSet<String> = self
                .j
                .orders
                .values()
                .filter_map(|x| x.order_id.clone().filter(|s| !s.is_empty()))
                .collect();
            let mut adopted = resting
                .iter()
                .find(|x| venue_id_matches(&exch, &x.client_id, &cid))
                .cloned();
            if adopted.is_none() {
                let same: Vec<_> = resting
                    .iter()
                    .filter(|x| !claimed.contains(&x.order_id))
                    .filter(|x| x.side.to_lowercase() == "buy")
                    .filter(|x| price_eq(x.price, o.price))
                    .collect();
                if same.len() == 1 {
                    adopted = Some(same[0].clone());
                }
            }
            if let Some(a) = adopted {
                self.j.update(&cid, |r| {
                    r.status = "open".into();
                    r.order_id = Some(a.order_id.clone()).filter(|s| !s.is_empty());
                    r.last_error = None;
                });
                self.save()?;
                self.push(
                    Some(&sym),
                    None,
                    false,
                    Lvl::Done,
                    format!(
                        "adopted resting deploy buy {sym} @ ${}",
                        g(a.price.unwrap_or(0.0), 6)
                    ),
                );
                continue;
            }
            if self.place_entry(client, &mut o, false)? {
                self.push(
                    Some(&sym),
                    Some("buy"),
                    true,
                    Lvl::Done,
                    format!(
                        "RETRY OK: deploy buy ${} {sym} @ ${}",
                        fixed(o.quote.unwrap_or(0.0), 2),
                        g(o.price.unwrap_or(0.0), 6)
                    ),
                );
            }
        }
        Ok(())
    }

    /// Stable credited on Gate since `since`, from Gate's own deposit history; `None`
    /// when the venue cannot say.
    fn gate_landed(&self, since: f64) -> Option<f64> {
        let gate = self.client("gate").ok()?;
        let rows = gate.deposits(since)?.ok()?;
        Some(py_sum(
            rows.iter()
                .filter(|r| {
                    matches!(r.currency.as_deref(), Some("USDC") | Some("USDT"))
                        && r.status.as_deref() == Some("DONE")
                        && r.ts >= since
                })
                .map(|r| r.amount),
        ))
    }

    fn reserve(&self, rx: f64, gate: f64) -> f64 {
        let routing: Vec<(String, String)> = self
            .cfg
            .routing
            .iter()
            .map(|(s, r)| (s.clone(), r.exch.clone()))
            .collect();
        let alloc: Vec<(String, f64)> = self
            .cfg
            .deploy_alloc
            .iter()
            .map(|(s, w)| (s.to_uppercase(), *w))
            .collect();
        funding::onramp_reserve(rx, gate, &alloc, &routing)
    }

    /// USDC that must stay free on Revolut X for the pending Gate top-up, or 0 while a
    /// top-up travels (the in-flight guard; see the module docs).
    fn onramp_keep(
        &mut self,
        st: &mut Map<String, Value>,
        rxf: &BTreeMap<String, Balance>,
        gtf: &BTreeMap<String, Balance>,
    ) -> f64 {
        if let Some(v) = st
            .get("inflight")
            .filter(|v| truthy_obj(v) && !v.is_object())
        {
            // A hand-edited or corrupt marker: read it as no top-up in flight.
            let shown = crate::pyfmt::dumps(v, None);
            st.remove("inflight");
            self.warn(format!(
                "deploy state: `inflight` is not an object ({shown}), treated as no top-up in flight"
            ));
        }
        let usdc = bal(rxf, "USDC");
        let usdc_now = usdc.free + usdc.locked;
        let gate_stable = funding::gate_stable_from(gtf);
        let prev = st
            .get("offquote")
            .and_then(|o| o.get("revx:USDC"))
            .and_then(Value::as_f64);
        let prev_run = match num(st.get("ts")) {
            x if x != 0.0 => x,
            _ => self.now() - 1800.0,
        };
        let min = self.cfg.deploy_min_usd;
        if let Some(prev) = prev.filter(|p| p - usdc_now >= min) {
            let out = prev - usdc_now;
            let since = (prev_run - INFLIGHT_SLACK_S).max(0.0);
            match self.gate_landed(since) {
                Some(landed) if landed >= 0.8 * out => self.done(format!(
                    "REVX: ${} USDC left the venue and ${} already landed on Gate -> no in-flight hold, reserve tracking continues",
                    fixed(out, 2),
                    fixed(landed, 2)
                )),
                _ => {
                    let present = st
                        .get_mut("inflight")
                        .filter(|v| inflight_marker(v))
                        .and_then(Value::as_object_mut);
                    if let Some(fl) = present {
                        let amt = fl.get("amount").and_then(Value::as_f64).unwrap_or(0.0) + out;
                        fl.insert("amount".into(), json!(amt));
                    } else {
                        let at = match st.get("stable").and_then(|s| s.get("gate")) {
                            Some(v) => v.as_f64().unwrap_or(0.0),
                            None => gate_stable,
                        };
                        st.insert(
                            "inflight".into(),
                            json!({"amount": out, "ts": self.now(), "since": since,
                                   "gate_stable_at": at}),
                        );
                    }
                    self.warn(format!(
                        "REVX: ${} USDC left the venue -> Gate top-up in flight; the reserve is paused until it lands on Gate",
                        fixed(out, 2)
                    ));
                }
            }
        }
        if let Some(fl) = st.get("inflight").filter(|v| inflight_marker(v)).cloned() {
            let fts = fl.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            let since = match num(fl.get("since")) {
                x if x != 0.0 => x,
                _ => fts - 1800.0,
            };
            let (landed, how) = match self.gate_landed(since) {
                Some(l) => (l, "Gate deposits"),
                None => (
                    gate_stable
                        - fl.get("gate_stable_at")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0),
                    "Gate stable",
                ),
            };
            let age = self.now() - fts;
            let amount = fl.get("amount").and_then(Value::as_f64).unwrap_or(0.0);
            if landed >= 0.8 * amount || age >= INFLIGHT_MAX_S {
                st.remove("inflight");
                self.done(format!(
                    "REVX: Gate top-up landed ({how} +${}) -> reserve tracking resumes",
                    fixed(landed, 2)
                ));
            } else {
                return 0.0;
            }
        }
        self.reserve(funding::revx_stable_from(rxf), gate_stable)
    }

    /// Turn free USD into USDC, up to `want`, leaving fee headroom. Not journaled.
    fn stage_usdc(&mut self, client: &dyn Venue, want: f64, usd_free: f64) -> Result<f64, String> {
        let stage = py_round(want.min(usd_free * 0.998 - ONRAMP_HEADROOM_USD), 2);
        if stage < self.cfg.deploy_min_usd {
            return Ok(0.0);
        }
        client
            .market_buy("USDC/USD", stage, None)
            .map_err(|e| e.to_string())?;
        self.push(
            None,
            None,
            true,
            Lvl::Done,
            format!(
                "REVX onramp: staged ${} USDC for the pending Gate top-up (send it from the app; the bot keeps it in USDC)",
                fixed(stage, 2)
            ),
        );
        (self.sleep)(2.0);
        Ok(stage)
    }

    /// The Revolut X onramp pre-step: EUR → USDC, then USDC → USD except what the Gate
    /// top-up keeps, or stage the top-up as USDC. Returns the kept amount and whether
    /// the reserve is unknown (Gate's balances could not be read, or Gate has no client):
    /// then nothing moves out of USDC this run.
    fn onramp(&mut self, st: &mut Map<String, Value>) -> Result<(f64, bool), String> {
        let min = self.cfg.deploy_min_usd;
        let revx = self.client("revx")?;
        let mut rxb = revx.balances().map_err(|e| e.to_string())?;
        let rx_eur = rxb.get("EUR").copied().unwrap_or(0.0);
        if rx_eur >= min {
            let spend = py_round(rx_eur * 0.998, 2);
            revx.market_buy("USDC/EUR", spend, None)
                .map_err(|e| e.to_string())?;
            self.push(
                None,
                None,
                true,
                Lvl::Done,
                format!(
                    "REVX onramp: converted EUR {} -> USDC for auto-deploy",
                    fixed(spend, 2)
                ),
            );
            (self.sleep)(2.0);
            rxb = revx.balances().map_err(|e| e.to_string())?;
        }
        let rx_usdc = rxb.get("USDC").copied().unwrap_or(0.0);
        let mut keep = 0.0;
        let mut unknown = false;
        let mut rxf = BTreeMap::new();
        let both = (|| -> Result<_, String> {
            let a = revx.balances_full().map_err(|e| e.to_string())?;
            let b = self
                .client("gate")?
                .balances_full()
                .map_err(|e| e.to_string())?;
            Ok((a, b))
        })();
        match both {
            Ok((a, b)) => {
                rxf = a;
                keep = self.onramp_keep(st, &rxf, &b);
            }
            Err(e) => {
                // Fail closed: without Gate's balance the reserve is unknown, and selling
                // the staged USDC back to USD would let this run ladder the top-up.
                unknown = true;
                self.warn(format!(
                    "Gate top-up reserve unavailable in the revx pre-step; staged USDC stays USDC this run: {e}"
                ))
            }
        }
        let rx_usdc_out = bal(&rxf, "USDC").locked;
        if unknown {
            return Ok((keep, true));
        }
        if rx_usdc - keep >= min {
            let qty = revx
                .round_amount("USDC/USD", rx_usdc - keep)
                .map_err(|e| e.to_string())?;
            revx.market_sell("USDC/USD", qty, None)
                .map_err(|e| e.to_string())?;
            let mut t = format!(
                "REVX onramp: converted {} USDC -> USD for auto-deploy",
                g(qty, 6)
            );
            if keep != 0.0 {
                t.push_str(&format!(
                    " (kept ${} USDC back for the pending Gate top-up)",
                    fixed(keep, 2)
                ));
            }
            self.push(None, None, true, Lvl::Done, t);
            (self.sleep)(2.0);
        } else if keep - rx_usdc - rx_usdc_out >= min {
            let usd = rxb.get("USD").copied().unwrap_or(0.0);
            self.stage_usdc(revx, keep - rx_usdc - rx_usdc_out, usd)?;
        }
        Ok((keep, false))
    }

    /// Gate: USDC that landed (the top-up's bridge asset) becomes USDT. Not journaled.
    fn gate_convert(&mut self) -> Result<(), String> {
        let gate = self.client("gate")?;
        let full = gate.balances_full().map_err(|e| e.to_string())?;
        let g_usdc = bal(&full, "USDC").free;
        if g_usdc >= self.cfg.deploy_min_usd {
            let qty = gate
                .round_amount("USDC_USDT", g_usdc)
                .map_err(|e| e.to_string())?;
            gate.market_sell("USDC_USDT", qty, None)
                .map_err(|e| e.to_string())?;
            self.push(
                None,
                None,
                true,
                Lvl::Done,
                format!(
                    "GATE: auto-converted {} USDC -> USDT (revx top-up bridge asset) for auto-deploy",
                    g(qty, 6)
                ),
            );
            (self.sleep)(2.0);
        }
        Ok(())
    }

    /// The bull sweep: market-buy the unspent budget of zones resting at least
    /// `deploy_bull_sweep_days`, per coin, while the bull is young and the coin runs.
    /// The buy is capped at the venue's free quote minus what `reserves` holds there.
    pub fn bull_sweep(
        &mut self,
        clients: &[&str],
        now: f64,
        reserves: &BTreeMap<&str, f64>,
    ) -> Result<Vec<String>, String> {
        let cfg = self.cfg;
        if !cfg.deploy_bull_sweep {
            return Ok(Vec::new());
        }
        let got = self
            .regime
            .regime()
            .and_then(|r| Ok((r, self.regime.label_history()?)));
        let (reg, hist) = match got {
            Ok(x) => x,
            Err(e) => {
                self.warn(format!("bull sweep skipped: regime unreadable ({e})"));
                return Ok(Vec::new());
            }
        };
        if reg.get("market").and_then(Value::as_str) != Some("bull") {
            return Ok(Vec::new());
        }
        let labels: Vec<String> = hist
            .get("labels")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(crate::pyfmt::value_str).collect())
            .unwrap_or_default();
        let Some(age) = plan::bull_age_days(&labels) else {
            return Ok(Vec::new());
        };
        if age as f64 > cfg.deploy_bull_sweep_max_age {
            return Ok(Vec::new());
        }
        let days = cfg.deploy_bull_sweep_days;
        let mut stale: BTreeMap<(String, String), Vec<Order>> = BTreeMap::new();
        for o in self.j.open_orders(Some("deploy_buy")) {
            if !truthy_i(o.rung) || !has_id(o) {
                continue;
            }
            let at = if truthy_f(o.first_ts) {
                o.first_ts.unwrap_or(now)
            } else if truthy_f(o.ts) {
                o.ts.unwrap_or(now)
            } else {
                now
            };
            if now - at < days * 86_400.0 {
                continue;
            }
            stale
                .entry((o.exch.clone(), o.sym.clone()))
                .or_default()
                .push(o.clone());
        }
        let min = cfg.deploy_min_usd;
        let mut done = Vec::new();
        for ((exch, sym), rows) in stale {
            let running = reg
                .get("coins")
                .and_then(|c| c.get(&sym))
                .and_then(|c| c.get("running"))
                .is_some_and(|v| crate::pyfmt::truthy(Some(v)));
            if !running {
                continue;
            }
            if !clients.contains(&exch.as_str()) {
                continue;
            }
            let Ok(client) = self.client(&exch) else {
                continue;
            };
            let mut budget = 0.0;
            for o in &rows {
                match self.cancel_ours(client, o, BULL_NOTE)? {
                    Ok(Settled::Unreadable(_)) => {
                        // Final status unreadable: it may have filled since the last poll.
                        // A market buy of the stale unspent would spend other free cash.
                        self.push(
                            Some(&sym),
                            None,
                            false,
                            Lvl::Warn,
                            format!(
                                "bull sweep: {} final status unreadable, its budget is not swept",
                                o.client_id
                            ),
                        );
                    }
                    Ok(s) => budget += Self::unspent(o, &s),
                    Err(e) => self.push(
                        Some(&sym),
                        None,
                        false,
                        Lvl::Warn,
                        format!(
                            "bull sweep: cancel failed (may have just filled, reconciles next run): {e}"
                        ),
                    ),
                }
            }
            let mut mkt = plan::floor_cents(budget * 0.997);
            if mkt >= min {
                // Never spend more than the venue holds free beyond the top-up reserve:
                // the journal's unspent can be stale (a fill the venue has not reported).
                let q = Self::quotes(cfg).get(&exch).cloned().unwrap_or_default();
                match client.balances_full() {
                    Ok(b) => {
                        let held = reserves.get(exch.as_str()).copied().unwrap_or(0.0);
                        let spendable =
                            plan::floor_cents((bal(&b, &q).free - held).max(0.0) * 0.997);
                        if spendable < mkt {
                            self.push(
                                Some(&sym),
                                None,
                                false,
                                Lvl::Warn,
                                format!(
                                    "bull sweep: {sym} on {exch} capped at ${} (free {q} ${} minus ${} held) instead of ${}",
                                    fixed(spendable, 2),
                                    fixed(bal(&b, &q).free, 2),
                                    fixed(held, 2),
                                    fixed(mkt, 2)
                                ),
                            );
                            mkt = spendable;
                        }
                    }
                    Err(e) => {
                        self.push(
                            Some(&sym),
                            None,
                            false,
                            Lvl::Warn,
                            format!(
                                "bull sweep: {exch} balances unreadable, {sym} cash left free: {e}"
                            ),
                        );
                        continue;
                    }
                }
            }
            if mkt < min {
                if budget != 0.0 {
                    self.push(
                        Some(&sym),
                        None,
                        false,
                        Lvl::Warn,
                        format!(
                            "bull sweep: ${} {sym} on {exch} under ${}, left free",
                            fixed(budget, 2),
                            g(min, 6)
                        ),
                    );
                }
                continue;
            }
            let pair = rows[0].pair.clone();
            let prefix = if exch == "revx" { "depx" } else { "dep" };
            let cid = format!("{prefix}{sym}{}s", now as i64);
            self.j.record(Order {
                client_id: cid.clone(),
                sym: sym.clone(),
                exch: exch.clone(),
                pair: pair.clone(),
                side: "buy".into(),
                kind: "deploy_buy".into(),
                status: "pending".into(),
                quote: Some(mkt),
                rung: Some(0),
                ts: Some(now),
                note: Some(format!(
                    "bull sweep: {} zone(s) resting >= {}d, bull {age}d old",
                    rows.len(),
                    g(days, 6)
                )),
                ..Default::default()
            });
            self.save()?;
            let r = client
                .market_buy(&pair, mkt, Some(&cid))
                .and_then(|raw| client.parse_order(&raw));
            match r {
                Ok(lf) => {
                    self.j.update(&cid, |o| {
                        o.order_id = Some(lf.order_id.clone()).filter(|s| !s.is_empty());
                        o.status = "new".into();
                        o.last_error = None;
                    });
                    self.save()?;
                    self.push(
                        Some(&sym),
                        Some("buy"),
                        true,
                        Lvl::Done,
                        format!(
                            "BULL SWEEP {sym} on {exch}: market ${} from {} zone(s) resting >= {}d (bull {age}d old, cutoff {}d)",
                            fixed(mkt, 2),
                            rows.len(),
                            g(days, 6),
                            g(cfg.deploy_bull_sweep_max_age, 6)
                        ),
                    );
                    done.push(cid);
                }
                Err(e) => {
                    let e = e.to_string();
                    self.j.update(&cid, |o| {
                        o.status = "error".into();
                        o.last_error = Some(e.clone());
                    });
                    self.save()?;
                    self.push(
                        Some(&sym),
                        None,
                        false,
                        Lvl::Warn,
                        format!(
                            "bull sweep: market buy {sym} ${} FAILED, cash left free: {e}",
                            fixed(mkt, 2)
                        ),
                    );
                }
            }
        }
        Ok(done)
    }

    /// Journal-explained quote deltas per venue since the baseline in `st` (fills since
    /// its `ts`, partial fills already netted while open counted once), and the partial
    /// fills now on open zones (the next `part`).
    pub fn explained(
        &self,
        st: &Map<String, Value>,
        venues: &[&str],
    ) -> (BTreeMap<String, f64>, Map<String, Value>) {
        let prev_ts = num(st.get("ts"));
        let part_prev: Map<String, Value> = st
            .get("part")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut explained: BTreeMap<String, f64> =
            venues.iter().map(|e| (e.to_string(), 0.0)).collect();
        if prev_ts != 0.0 {
            for o in self.j.filled_since(prev_ts, false) {
                let Some(x) = explained.get_mut(o.exch.as_str()) else {
                    continue;
                };
                let fq = o.filled_quote.unwrap_or(0.0);
                if o.side == "buy" {
                    *x -= (fq - num(part_prev.get(&o.client_id))).max(0.0);
                } else {
                    *x += fq;
                }
            }
        }
        let mut part_cur = Map::new();
        for o in self.j.open_orders(Some("deploy_buy")) {
            let pq = o.part_quote.unwrap_or(0.0);
            if let Some(x) = explained.get_mut(o.exch.as_str()) {
                part_cur.insert(o.client_id.clone(), json!(pq));
                *x -= (pq - num(part_prev.get(&o.client_id))).max(0.0);
            }
        }
        (explained, part_cur)
    }

    /// Why the layer may not trade, if it may not.
    pub fn blocked(cfg: &RunConfig) -> Option<String> {
        if !cfg.live_trading_enabled {
            return Some("LIVE_TRADING_ENABLED is not 'yes'".into());
        }
        if cfg.halt_file.exists() {
            return Some(format!("halt file present ({})", cfg.halt_file.display()));
        }
        None
    }

    /// The venues that have a client, in the layer's order.
    pub fn clients(&self) -> Vec<&'static str> {
        VENUES
            .iter()
            .copied()
            .filter(|e| self.venues.venue(e).is_ok())
            .collect()
    }

    /// One run of the layer. Appends its results to `self.results`. `Err` is a failure
    /// the layer does not catch (an unreadable state file, a venue read the reference let
    /// propagate); the run reports it as `deploy layer: …`.
    pub fn check(&mut self) -> Result<(), String> {
        let cfg = self.cfg;
        if cfg.deploy != "live" {
            return Ok(());
        }
        let spath = cfg.deploy_state_path();
        let mut st = load_state(&spath)?;
        if let Some(reason) = Self::blocked(cfg) {
            if !st
                .get("blocked")
                .is_some_and(|v| crate::pyfmt::truthy(Some(v)))
            {
                st.insert("blocked".into(), json!(reason));
                save_state(&spath, &st)?;
                self.warn(format!("deploy layer idle: {reason}"));
            }
            return Ok(());
        }
        if st
            .remove("blocked")
            .is_some_and(|v| crate::pyfmt::truthy(Some(&v)))
        {
            save_state(&spath, &st)?;
        }
        let clients = self.clients();
        let ts_cut = self.now();
        let min = cfg.deploy_min_usd;

        self.resume_pending()?;

        let (onramp_keep, mut reserve_unknown) = match self.onramp(&mut st) {
            Ok(k) => k,
            Err(e) => {
                self.warn(format!(
                    "revx onramp conversion failed (deposit keeps its currency, retried next run): {e}"
                ));
                (0.0, false)
            }
        };
        let inflight_active = st.get("inflight").is_some_and(inflight_marker);

        if let Err(e) = self.gate_convert() {
            self.warn(format!(
                "gate USDC -> USDT conversion failed (top-up keeps its currency, retried next run): {e}"
            ));
        }

        let quotes = Self::quotes(cfg);

        let mut full: BTreeMap<&str, Option<BTreeMap<String, Balance>>> = BTreeMap::new();
        for &exch in &clients {
            match self.client(exch)?.balances_full() {
                Ok(b) => {
                    full.insert(exch, Some(b));
                }
                Err(e) => {
                    full.insert(exch, None);
                    self.push(
                        None,
                        None,
                        false,
                        Lvl::Err,
                        format!("deploy: {exch} balances: {e}"),
                    );
                }
            }
        }

        let (explained, part_cur) = self.explained(&st, &clients);
        let open_zones: Vec<Order> = self
            .j
            .open_orders(Some("deploy_buy"))
            .into_iter()
            .cloned()
            .collect();

        let baselines: Map<String, Value> = st
            .get("stable")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut offquote: Map<String, Value> = st
            .get("offquote")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let mut reserves: BTreeMap<&str, f64> = BTreeMap::new();
        let mut rx_reserve_full = 0.0;
        let rx_full = full
            .get("revx")
            .and_then(Option::as_ref)
            .filter(|b| !b.is_empty());
        let gt_full = full
            .get("gate")
            .and_then(Option::as_ref)
            .filter(|b| !b.is_empty());
        if rx_full.is_some() && gt_full.is_none() {
            // Gate unreadable or without a client: the reserve is unknown.
            reserve_unknown = true;
        }
        if let (Some(rxb), Some(gtb)) = (rx_full, gt_full) {
            let rx_stable = funding::revx_stable_from(rxb);
            let gate_stable = funding::gate_stable_from(gtb);
            let u = bal(rxb, "USDC");
            let staged = u.free + u.locked;
            rx_reserve_full = if inflight_active {
                0.0
            } else {
                self.reserve(rx_stable, gate_stable)
            };
            let held = (rx_reserve_full - staged).max(0.0);
            if held > 0.0 {
                reserves.insert("revx", held);
            }
        }
        if let (true, Some(rxb)) = (reserve_unknown, rx_full) {
            // Fail closed: hold ALL free revx stable this run. New capital is not absorbed
            // into the baseline (it carries to the next run), nothing is swept or laddered.
            let q = quotes.get("revx").cloned().unwrap_or_default();
            reserves.insert("revx", bal(rxb, &q).free);
            self.warn(
                "Gate top-up reserve unknown (Gate balance unreadable): no new capital laddered on revx this run"
                    .into(),
            );
        }

        type Tranche = (&'static str, String, f64);
        type Sweep = (&'static str, String, f64, Only, Vec<String>);
        let mut new_baseline = Map::new();
        let mut tranches: Vec<Tranche> = Vec::new();
        let mut sweeps: Vec<Sweep> = Vec::new();
        let mut withholds: Vec<Tranche> = Vec::new();
        for &exch in &clients {
            let Some(Some(fb)) = full.get(exch).cloned() else {
                if let Some(b) = baselines.get(exch) {
                    new_baseline.insert(exch.into(), b.clone());
                }
                continue;
            };
            let q = quotes.get(exch).cloned().unwrap_or_default();
            let b = bal(&fb, &q);
            let total = b.free + b.locked;
            if !baselines.contains_key(exch) {
                new_baseline.insert(exch.into(), json!(total));
                self.done(format!(
                    "DEPLOY baseline initialized: {exch} {q} ${} — deposits from now on auto-ladder into buy zones",
                    fixed(total, 2)
                ));
            } else {
                let expected = num(baselines.get(exch)) + explained[exch];
                let unexplained = total - expected;
                let held = reserves.get(exch).copied().unwrap_or(0.0);
                let tranche = unexplained
                    .min(cfg.deploy_max_tranche_usd)
                    .min((b.free - held).max(0.0));
                if unexplained >= min && tranche >= min {
                    tranches.push((exch, q.clone(), tranche));
                    new_baseline.insert(exch.into(), json!(total - (unexplained - tranche)));
                    if held != 0.0 {
                        self.warn(format!(
                            "holding ${} free on {exch} for the pending Gate top-up — laddered ${} of ${} new capital",
                            fixed(held, 2),
                            fixed(tranche, 2),
                            fixed(unexplained, 2)
                        ));
                    }
                } else if unexplained > 0.0 {
                    new_baseline.insert(exch.into(), json!(expected));
                    if held != 0.0 && unexplained >= min {
                        self.warn(format!(
                            "${} new capital on {exch} left UNDEPLOYED — ${} is reserved for the pending Gate top-up. Send it; the rest ladders on the next run.",
                            fixed(unexplained, 2),
                            fixed(held, 2)
                        ));
                    }
                } else {
                    new_baseline.insert(exch.into(), json!(total));
                }
                let stuck_at = num(st.get("sweep_stuck").and_then(|s| s.get(exch)));
                let freed_rows: Vec<Order> = self
                    .j
                    .venue_cancelled_unswept(Some("deploy_buy"))
                    .into_iter()
                    .filter(|o| o.exch == exch)
                    .cloned()
                    .collect();
                let freed = py_sum(
                    freed_rows
                        .iter()
                        .map(|o| o.quote.unwrap_or(0.0) - o.part_quote.unwrap_or(0.0)),
                );
                let idle = (b.free - held).min(freed);
                if cfg.deploy_sweep_idle
                    && unexplained < min
                    && idle >= min
                    && ts_cut - stuck_at >= SWEEP_RETRY_S
                {
                    let zoned: BTreeSet<&str> = open_zones
                        .iter()
                        .filter(|o| o.exch == exch && truthy_i(o.rung))
                        .map(|o| o.sym.as_str())
                        .collect();
                    let bare: BTreeSet<String> = Self::venue_syms(cfg, exch)
                        .into_iter()
                        .filter(|s| !zoned.contains(s.as_str()))
                        .collect();
                    let what = if bare.is_empty() {
                        "the whole venue".to_string()
                    } else {
                        format!(
                            "{} (coins without zones)",
                            bare.iter().cloned().collect::<Vec<_>>().join(", ")
                        )
                    };
                    sweeps.push((
                        exch,
                        q.clone(),
                        idle,
                        (!bare.is_empty()).then_some(bare),
                        freed_rows.iter().map(|o| o.client_id.clone()).collect(),
                    ));
                    new_baseline.insert(exch.into(), json!(total));
                    self.push(
                        None,
                        None,
                        true,
                        Lvl::Done,
                        format!(
                            "DEPLOY sweep: ${} {q} sitting FREE on {exch} with no new capital behind it (a zone was cancelled venue-side or left over) -> re-laddering {what}",
                            fixed(idle, 2)
                        ),
                    );
                }
            }
            if exch == "revx"
                && baselines.contains_key(exch)
                && rx_reserve_full > 0.0
                && !tranches.iter().any(|t| t.0 == exch)
                && !sweeps.iter().any(|s| s.0 == exch)
            {
                let u = bal(&fb, "USDC");
                let free_stable = b.free + (u.free + u.locked);
                let zones: Vec<&Order> = open_zones
                    .iter()
                    .filter(|o| o.exch == exch && truthy_i(o.rung))
                    .collect();
                let resting = py_sum(
                    zones
                        .iter()
                        .map(|o| (o.quote.unwrap_or(0.0) - o.part_quote.unwrap_or(0.0)).max(0.0)),
                );
                let short = (rx_reserve_full - free_stable).min(resting);
                let stuck_at = num(st.get("withhold_stuck").and_then(|s| s.get(exch)));
                if short >= min && !zones.is_empty() && ts_cut - stuck_at >= SWEEP_RETRY_S {
                    withholds.push((exch, q.clone(), short));
                    self.push(
                        None,
                        None,
                        true,
                        Lvl::Done,
                        format!(
                            "DEPLOY withhold on {exch}: Gate top-up ${} due but only ${} free -> re-laddering the venue ${} lighter so it can leave as USDC",
                            fixed(rx_reserve_full, 2),
                            fixed(free_stable, 2),
                            fixed(short, 2)
                        ),
                    );
                }
            }
            let oq = if q == "USDC" { "USDT" } else { "USDC" };
            let ob = bal(&fb, oq);
            let ototal = ob.free + ob.locked;
            let okey = format!("{exch}:{oq}");
            if let Some(prev) = offquote.get(&okey).map(|v| num(Some(v))) {
                if ototal - prev >= min && !(exch == "revx" && ototal <= onramp_keep + min) {
                    self.warn(format!(
                        "${} {oq} landed on {exch}, but its pairs are {q}-quoted — convert to {q} for auto-deploy",
                        fixed(ototal - prev, 2)
                    ));
                }
            }
            offquote.insert(okey, json!(ototal));
        }

        // A venue with no client this run keeps its baseline: dropping it would read the
        // venue's whole balance as a cold start (or, once re-baselined, lose a deposit).
        for (exch, b) in &baselines {
            if !clients.contains(&exch.as_str()) {
                new_baseline
                    .entry(exch.clone())
                    .or_insert_with(|| b.clone());
            }
        }
        // The baseline commits before anything is journaled or placed.
        st.insert("ts".into(), json!(ts_cut));
        st.insert("stable".into(), Value::Object(new_baseline));
        st.insert("part".into(), Value::Object(part_cur));
        st.insert("offquote".into(), Value::Object(offquote));
        save_state(&spath, &st)?;

        let mut placements: Vec<(&'static str, String)> = Vec::new();
        for (exch, q, tranche) in &tranches {
            let client = self.client(exch)?;
            for cid in self.deploy_tranche(client, exch, q, *tranche, ts_cut, &None)? {
                placements.push((exch, cid));
            }
        }
        for (exch, q, idle, only, freed) in &sweeps {
            let client = self.client(exch)?;
            let cids = self.deploy_tranche(client, exch, q, *idle, ts_cut, only)?;
            let stuck = st
                .entry("sweep_stuck")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or("sweep_stuck is not an object")?;
            if cids.is_empty() {
                stuck.insert((*exch).into(), json!(ts_cut));
                self.warn(format!(
                    "DEPLOY sweep: ${} {q} on {exch} could not be laddered (see skips above) — retrying in 24h; if a pair is gone, move the cash or route the coin elsewhere",
                    fixed(*idle, 2)
                ));
            } else {
                stuck.remove(*exch);
                self.j.mark_swept(freed);
                self.save()?;
            }
            save_state(&spath, &st)?;
            placements.extend(cids.into_iter().map(|c| (*exch, c)));
        }
        for (exch, q, short) in &withholds {
            let client = self.client(exch)?;
            let cids = self.deploy_tranche(client, exch, q, -short, ts_cut, &None)?;
            let stuck = st
                .entry("withhold_stuck")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or("withhold_stuck is not an object")?;
            if cids.is_empty() {
                stuck.insert((*exch).into(), json!(ts_cut));
                self.warn(format!(
                    "DEPLOY withhold: {exch} could not be re-laddered ${} lighter (see skips above) — retrying in 24h; the cash stays free meanwhile",
                    fixed(*short, 2)
                ));
            } else {
                stuck.remove(*exch);
            }
            save_state(&spath, &st)?;
            placements.extend(cids.into_iter().map(|c| (*exch, c)));
        }
        for (exch, cid) in &placements {
            let client = self.client(exch)?;
            self.place_unplaced(client, std::slice::from_ref(cid))?;
        }

        self.bull_sweep(&clients, ts_cut, &reserves)?;

        let touched_revx = tranches.iter().any(|t| t.0 == "revx")
            || sweeps.iter().any(|s| s.0 == "revx")
            || withholds.iter().any(|w| w.0 == "revx");
        if onramp_keep > 0.0 && touched_revx {
            if let Err(e) = self.stage_after_roll(&mut st, &spath, onramp_keep) {
                self.warn(format!(
                    "revx USDC staging after the re-ladder failed (retried next run): {e}"
                ));
            }
        }
        Ok(())
    }

    fn stage_after_roll(
        &mut self,
        st: &mut Map<String, Value>,
        spath: &Path,
        keep: f64,
    ) -> Result<(), String> {
        let revx = self.client("revx")?;
        let rxf = revx.balances_full().map_err(|e| e.to_string())?;
        let u = bal(&rxf, "USDC");
        let usdc = u.free + u.locked;
        if keep - usdc >= self.cfg.deploy_min_usd
            && self.stage_usdc(revx, keep - usdc, bal(&rxf, "USD").free)? != 0.0
        {
            let rxf = revx.balances_full().map_err(|e| e.to_string())?;
            let u = bal(&rxf, "USDC");
            let oq = st
                .entry("offquote")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or("offquote is not an object")?;
            oq.insert("revx:USDC".into(), json!(u.free + u.locked));
            save_state(spath, st)?;
        }
        Ok(())
    }
}

fn has_id(o: &Order) -> bool {
    o.order_id.as_deref().is_some_and(|s| !s.is_empty())
}

fn truthy_i(x: Option<i64>) -> bool {
    x.is_some_and(|v| v != 0)
}

fn truthy_obj(v: &Value) -> bool {
    crate::pyfmt::truthy(Some(v))
}

/// A top-up-in-flight marker: a non-empty object. Anything else is no marker.
fn inflight_marker(v: &Value) -> bool {
    v.as_object().is_some_and(|m| !m.is_empty())
}

/// The deploy hook a live run calls, bound to its config.
#[derive(Debug, Default, Clone, Copy)]
pub struct DeployLayer;

impl crate::run::hooks::DeployHook for DeployLayer {
    fn check(
        &mut self,
        ctx: &mut crate::run::hooks::HookCtx,
        execution: &mut Vec<RunResult>,
    ) -> Result<(), String> {
        let feed = LiveRegime {
            cfg: ctx.cfg,
            market: ctx.market,
            now: ctx.now,
        };
        let jpath = ctx.cfg.journal_path();
        let persist = move |j: &Journal| crate::store::save_journal(&jpath, j);
        let stderr = |s: &str| eprintln!("{s}");
        let mut layer = Layer {
            cfg: ctx.cfg,
            venues: ctx.venues,
            regime: &feed,
            clock: ctx.clock,
            sleep: ctx.sleep,
            persist: &persist,
            stderr: &stderr,
            j: ctx.journal,
            results: Vec::new(),
        };
        let r = layer.check();
        execution.append(&mut layer.results);
        r
    }
}
