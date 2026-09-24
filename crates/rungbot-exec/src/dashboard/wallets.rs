//! `wallets.json`: self-custody holdings and staking, and the "start unbonding" alert.
//!
//! Read-only: public chain APIs, no keys. A wallet whose read fails keeps its last good
//! reading, marked `stale`; one that never read is left out. Staked coins take days to
//! unlock, so the alert fires while price is still inside a lead band below the coin's
//! first sell tranche (`wallet_tranche_x` times the bot's cost, or a manual target),
//! sized to the coin's unbonding time. Coins the sell policy only trails also need a
//! confirmed bull. One alert per crossing; it re-arms once price falls back
//! `wallet_rearm_pct` under the trigger.

use rungbot_core::watch::json::{obj, Json};
use rungbot_core::watch::pyfmt::{comma, fixed, g, round};

use super::net::{self, NetError};
use super::{head, py_float, py_sum, read_json_or, DashConfig, Io, Num, WalletSpec};
use crate::run::config::RunConfig;

const UA: [(&str, &str); 2] = [
    ("User-Agent", "Mozilla/5.0"),
    ("Content-Type", "application/json"),
];

/// One public read, retried once after two seconds.
fn get(io: &mut Io, url: &str, body: Option<&str>) -> Result<Json, String> {
    match net::fetch(&io.http, url, body, &UA, 15) {
        Ok(v) => Ok(v),
        Err(_) => {
            (io.sleep)(2.0);
            net::fetch(&io.http, url, body, &UA, 15).map_err(|e: NetError| e.text())
        }
    }
}

/// `d[key]`, with Python's `KeyError` text.
fn at<'a>(d: &'a Json, key: &str) -> Result<&'a Json, String> {
    d.get(key).ok_or_else(|| format!("'{key}'"))
}

/// `int(s)` for an integer written as text or a JSON integer.
fn py_int(v: &Json) -> Result<i128, String> {
    match v {
        Json::Int(i) => Ok(*i as i128),
        Json::Str(s) => s.trim().replace('_', "").parse::<i128>().map_err(|_| {
            format!(
                "invalid literal for int() with base 10: {}",
                super::repr_str(s)
            )
        }),
        Json::Float(f) => Ok(f.trunc() as i128),
        other => Err(format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            super::type_name(other)
        )),
    }
}

/// `n / 10**dec`, correctly rounded as Python's integer true division is.
fn div_pow10(n: i128, dec: u32) -> f64 {
    format!("{n}e-{dec}").parse().unwrap_or(0.0)
}

fn cosmos(io: &mut Io, rest: &str, chain: &str, addr: &str, dec: u32) -> Result<Json, String> {
    let base = format!("{rest}/{chain}");
    let params = at(
        &get(io, &format!("{base}/cosmos/staking/v1beta1/params"), None)?,
        "params",
    )?
    .clone();
    let denom = at(&params, "bond_denom")?.clone();
    let bank = get(
        io,
        &format!("{base}/cosmos/bank/v1beta1/balances/{addr}"),
        None,
    )?;
    let mut liquid: i128 = 0;
    for b in at(&bank, "balances")?.items() {
        if at(b, "denom")?.py_eq(&denom) {
            liquid += py_int(at(b, "amount")?)?;
        }
    }
    let dels_j = get(
        io,
        &format!("{base}/cosmos/staking/v1beta1/delegations/{addr}"),
        None,
    )?;
    let dels = at(&dels_j, "delegation_responses")?.items().to_vec();
    let mut staked: i128 = 0;
    for dl in &dels {
        staked += py_int(at(at(dl, "balance")?, "amount")?)?;
    }
    let ub = get(
        io,
        &format!("{base}/cosmos/staking/v1beta1/delegators/{addr}/unbonding_delegations"),
        None,
    )?;
    let mut unbonding = Vec::new();
    for u in at(&ub, "unbonding_responses")?.items() {
        for e in at(u, "entries")?.items() {
            unbonding.push(obj(vec![
                (
                    "amount",
                    Json::Float(div_pow10(py_int(at(e, "balance")?)?, dec)),
                ),
                ("done", at(e, "completion_time")?.clone()),
            ]));
        }
    }
    let rew_j = get(
        io,
        &format!("{base}/cosmos/distribution/v1beta1/delegators/{addr}/rewards"),
        None,
    )?;
    let mut rew = Vec::new();
    for r in rew_j.get("total").map(Json::items).unwrap_or(&[]) {
        if at(r, "denom")?.py_eq(&denom) {
            rew.push(Num::Float(py_float(at(r, "amount")?)?));
        }
    }
    let rewards = match py_sum(rew) {
        Num::Int(i) => div_pow10(i as i128, dec),
        Num::Float(f) => f / format!("1e{dec}").parse::<f64>().unwrap_or(1.0),
    };
    let ut = at(&params, "unbonding_time")?.py_str();
    let secs = py_int(&Json::Str(ut.trim_end_matches('s').to_string()))?;
    let days = round((secs as f64) / 86_400.0, 0) as i64;
    Ok(obj(vec![
        ("liquid", Json::Float(div_pow10(liquid, dec))),
        ("staked", Json::Float(div_pow10(staked, dec))),
        ("rewards", Json::Float(rewards)),
        ("unbonding", Json::Arr(unbonding)),
        ("unbond_days", Json::Int(days)),
        ("validators", Json::Int(dels.len() as i64)),
        ("chain", chain.into()),
        ("address", addr.into()),
    ]))
}

/// `eth_call` at `latest`, the 256-bit result read as a number of 18-decimal units.
fn eth_call(io: &mut Io, rpc: &str, to: &str, data: &str) -> Result<f64, String> {
    let body = obj(vec![
        ("jsonrpc", "2.0".into()),
        ("id", Json::Int(1)),
        ("method", "eth_call".into()),
        (
            "params",
            Json::Arr(vec![
                obj(vec![("to", to.into()), ("data", data.into())]),
                "latest".into(),
            ]),
        ),
    ])
    .dumps(None);
    let r = get(io, rpc, Some(&body))?;
    let hex = at(&r, "result")?.py_str();
    let digits = hex.trim().trim_start_matches("0x").trim_start_matches("0X");
    let digits = digits.trim_start_matches('0');
    let n = if digits.is_empty() {
        0u128
    } else {
        u128::from_str_radix(digits, 16).map_err(|_| {
            format!(
                "invalid literal for int() with base 16: {}",
                super::repr_str(&hex)
            )
        })?
    };
    Ok(n as f64 / 1e18)
}

fn balance_of(io: &mut Io, rpc: &str, token: &str, holder: &str) -> Result<f64, String> {
    let h = holder.get(2..).unwrap_or("").to_lowercase();
    eth_call(io, rpc, token, &format!("0x70a08231{h:0>64}"))
}

fn vault(
    io: &mut Io,
    sym: &str,
    address: &str,
    chain: &str,
    unbond_days: i64,
    legs: &[(String, String, String, String)],
) -> Result<Json, String> {
    let (_, rpc0, _, vault0) = &legs[0];
    // convertToAssets(1e18): coin per vault share
    let rate = eth_call(
        io,
        rpc0,
        vault0,
        &format!("0x07a2d13a{:0>64}", "de0b6b3a7640000"),
    )?;
    let mut shares = Vec::new();
    for (_, rpc, _, v) in legs {
        shares.push(balance_of(io, rpc, v, address)?);
    }
    let mut liquid: Option<f64> = None;
    for (_, rpc, t, _) in legs {
        let b = balance_of(io, rpc, t, address)?;
        liquid = Some(liquid.map_or(b, |l| l + b));
    }
    let staked_shares = shares[1..].iter().fold(shares[0], |a, b| a + b);
    let parts: Vec<String> = legs
        .iter()
        .zip(&shares)
        .map(|((label, _, _, _), s)| format!("{} on {label}", comma(*s, 0)))
        .collect();
    Ok(obj(vec![
        ("liquid", Json::Float(liquid.unwrap_or(0.0))),
        ("staked", Json::Float(staked_shares * rate)),
        ("rewards", Json::Float(0.0)),
        ("unbonding", Json::Arr(Vec::new())),
        ("unbond_days", Json::Int(unbond_days)),
        ("chain", chain.into()),
        ("address", address.into()),
        (
            "note",
            format!(
                "v{sym} {} at {} {sym}/v{sym}; pending unstake requests not read",
                parts.join(" + "),
                fixed(rate, 4)
            )
            .into(),
        ),
    ]))
}

fn btc(io: &mut Io, api: &str, address: &str, chain: &str) -> Result<Json, String> {
    let r = get(io, &format!("{api}/address/{address}"), None)?;
    let s = at(&r, "chain_stats")?;
    let n = |k: &str| -> Result<Num, String> {
        let v = at(s, k)?;
        Num::of(v).ok_or_else(|| {
            format!(
                "unsupported operand type(s) for -: '{}'",
                super::type_name(v)
            )
        })
    };
    let diff = match (n("funded_txo_sum")?, n("spent_txo_sum")?) {
        (Num::Int(a), Num::Int(b)) => (a as i128 - b as i128) as f64,
        (a, b) => a.f() - b.f(),
    };
    Ok(obj(vec![
        ("liquid", Json::Float(diff / 1e8)),
        ("staked", Json::Float(0.0)),
        ("rewards", Json::Float(0.0)),
        ("unbonding", Json::Arr(Vec::new())),
        ("unbond_days", Json::Int(0)),
        ("chain", chain.into()),
        ("address", address.into()),
    ]))
}

fn read(io: &mut Io, sym: &str, spec: &WalletSpec) -> Result<Json, String> {
    match spec {
        WalletSpec::Cosmos {
            chain,
            address,
            decimals,
            rest,
        } => cosmos(io, rest, chain, address, *decimals),
        WalletSpec::Vault {
            address,
            chain,
            unbond_days,
            legs,
        } => vault(io, sym, address, chain, *unbond_days, legs),
        WalletSpec::Btc {
            address,
            chain,
            api,
        } => btc(io, api, address, chain),
    }
}

fn num(v: Option<&Json>) -> Result<Num, String> {
    let v = v.ok_or("KeyError")?;
    Num::of(v).ok_or_else(|| format!("unsupported operand type: '{}'", super::type_name(v)))
}

/// Read every wallet, write `wallets.json`, the alert state and the last-good readings,
/// then send the alerts.
pub fn run(_cfg: &RunConfig, d: &DashConfig, io: &mut Io) -> Result<(), String> {
    let started = (io.clock)();
    let w = &d.wallets;
    let snap = read_json_or(&d.data_path(), Json::obj());
    let mut bot: Vec<(String, Json)> = Vec::new();
    for c in snap.get("coins").map(Json::items).unwrap_or(&[]) {
        let sym = at(c, "sym").map_err(|e| format!("KeyError: {e}"))?.py_str();
        match bot.iter_mut().find(|(s, _)| *s == sym) {
            Some(slot) => slot.1 = c.clone(),
            None => bot.push((sym, c.clone())),
        }
    }
    let bot_get = |sym: &str, k: &str| -> Option<Json> {
        bot.iter()
            .find(|(s, _)| s == sym)
            .and_then(|(_, c)| c.get(k).cloned())
    };
    let regime = snap
        .get("regime")
        .filter(|r| r.truthy())
        .cloned()
        .unwrap_or_default();
    let confirmed_bull = matches!(regime.get("label"), Some(Json::Str(l)) if l == "bull")
        && regime.get("confirmed").is_some_and(Json::truthy);
    let targets = read_json_or(&w.targets, Json::obj());
    let mut errors: Vec<String> = Vec::new();

    let ids: Vec<&str> = w
        .list
        .iter()
        .map(|(s, _)| d.coingecko_id(s).unwrap_or(""))
        .collect();
    let url = format!(
        "https://api.coingecko.com/api/v3/simple/price?vs_currencies=usd&ids={}",
        ids.join(",")
    );
    let cg = match get(io, &url, None) {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("coingecko: {e}"));
            Json::obj()
        }
    };

    let mut state = read_json_or(&d.wallet_state_path(), Json::obj());
    let mut last_good = read_json_or(&d.wallet_last_good_path(), Json::obj());
    let (mut alerts, mut rows) = (Vec::new(), Vec::new());
    for (sym, spec) in &w.list {
        if (io.clock)() - started > w.timeout_s {
            return Err(format!("timed out after {}s", g(w.timeout_s, 6)));
        }
        let now = (io.clock)();
        let wal = match read(io, sym, spec) {
            Ok(mut x) => {
                x.set("read_ts", Json::Int(now.trunc() as i64));
                last_good.set(sym, x.clone());
                x
            }
            Err(e) => {
                errors.push(format!("{sym}: {}", head(&e, 120)));
                match last_good.get(sym) {
                    Some(prev) => {
                        let mut x = prev.clone();
                        x.set("stale", Json::Bool(true));
                        x
                    }
                    None => continue,
                }
            }
        };
        let gid = d.coingecko_id(sym).unwrap_or("");
        let price = match cg
            .get(gid)
            .and_then(|c| c.get("usd"))
            .filter(|p| p.truthy())
        {
            Some(p) => p.clone(),
            None => bot_get(sym, "price").unwrap_or(Json::Null),
        };
        let price_n = Num::of(&price).filter(|p| p.f() != 0.0);
        let ubd = py_sum(
            wal.get("unbonding")
                .ok_or("'unbonding'")?
                .items()
                .iter()
                .map(|u| num(u.get("amount")))
                .collect::<Result<Vec<_>, _>>()?,
        );
        let staked = num(wal.get("staked"))?;
        let unbond_days = num(wal.get("unbond_days"))?;
        let total = num(wal.get("liquid"))?
            .plus(staked)
            .plus(ubd)
            .plus(num(wal.get("rewards"))?);
        let cost = bot_get(sym, "entry").unwrap_or(Json::Null);
        let (rung, rung_src) = if let Some(t) = targets.get(sym) {
            (Some(py_float(t)?), Some("manual target".to_string()))
        } else if cost.truthy() {
            (
                Some(cost.to_float().unwrap_or(0.0) * w.tranche_x),
                Some(format!("{}x bot cost", g(w.tranche_x, 6))),
            )
        } else {
            (None, None)
        };
        let rung_t = rung.filter(|r| *r != 0.0);
        let lead = w.lead_pct * unbond_days.f() / 21.0;
        let trigger = rung_t.map(|r| r * (1.0 - lead / 100.0));
        let mut status = if rung_t.is_none() {
            "no rung"
        } else {
            "watching"
        }
        .to_string();
        let trail_only = w.trail_only.iter().any(|s| s == sym);
        if staked.f() <= 0.0 {
            status = if unbond_days.f() != 0.0 {
                "nothing staked"
            } else {
                "liquid (no lock)"
            }
            .into();
        } else if let (Some(trig), Some(p)) = (trigger.filter(|t| *t != 0.0), price_n) {
            let p = p.f();
            let hit = p >= trig && (!trail_only || confirmed_bull);
            let fired = state
                .get(sym)
                .and_then(|s| s.get("fired"))
                .is_some_and(Json::truthy);
            if hit {
                status = "UNBOND NOW".into();
                if !fired {
                    alerts.push(format!(
                        "{sym}: start unbonding {} staked now. Price ${} is inside the {}% lead band of the {} rung ${}; unlock takes {}d.",
                        comma(staked.f(), 0),
                        g(p, 4),
                        fixed(lead, 0),
                        rung_src.clone().unwrap_or_default(),
                        g(rung.unwrap_or(0.0), 4),
                        unbond_days.json().py_str()
                    ));
                    state.set(
                        sym,
                        obj(vec![
                            ("fired", Json::Bool(true)),
                            ("ts", Json::Int(now.trunc() as i64)),
                        ]),
                    );
                }
            } else if fired && p < trig * (1.0 - w.rearm_pct / 100.0) {
                state.set(
                    sym,
                    obj(vec![
                        ("fired", Json::Bool(false)),
                        ("ts", Json::Int(now.trunc() as i64)),
                    ]),
                );
            }
            if !hit && p >= trig && trail_only {
                status = "in band, waits for confirmed bull".into();
            }
        }
        let mut row = obj(vec![("sym", sym.as_str().into())]);
        for (k, v) in wal.entries() {
            row.set(k, v.clone());
        }
        let total_t = total.f() != 0.0;
        row.set("unbonding_total", ubd.json());
        row.set("total", total.json());
        row.set("price", price.clone());
        row.set(
            "value",
            match price_n {
                Some(p) => total.times(p).json(),
                None => Json::Null,
            },
        );
        row.set(
            "staked_pct",
            if total_t {
                Json::Float(staked.plus(ubd).f() / total.f() * 100.0)
            } else {
                Json::Float(0.0)
            },
        );
        row.set("cost", cost);
        row.set("rung", rung.map(Json::Float).unwrap_or(Json::Null));
        row.set("rung_src", rung_src.map(Json::Str).unwrap_or(Json::Null));
        row.set("lead_pct", Json::Float(lead));
        row.set("trigger", trigger.map(Json::Float).unwrap_or(Json::Null));
        row.set(
            "to_trigger_pct",
            match (trigger.filter(|t| *t != 0.0), price_n) {
                (Some(t), Some(p)) => Json::Float((t / p.f() - 1.0) * 100.0),
                _ => Json::Null,
            },
        );
        row.set("status", status.into());
        rows.push(row);
    }

    let value = py_sum(rows.iter().map(|r| {
        r.get("value")
            .filter(|v| v.truthy())
            .and_then(Num::of)
            .unwrap_or(Num::Int(0))
    }));
    let staked_val = py_sum(
        rows.iter()
            .map(|r| {
                let s = num(r.get("staked"))?.plus(num(r.get("unbonding_total"))?);
                let p = r
                    .get("price")
                    .filter(|p| p.truthy())
                    .and_then(Num::of)
                    .unwrap_or(Num::Int(0));
                Ok(s.times(p))
            })
            .collect::<Result<Vec<_>, String>>()?,
    );
    let cex = snap
        .get("portfolio")
        .filter(|p| p.truthy())
        .and_then(|p| p.get("total_value"))
        .filter(|v| v.truthy())
        .and_then(Num::of)
        .unwrap_or(Num::Int(0));
    let all = cex.plus(value);
    let staked_pct = if value.f() != 0.0 {
        staked_val.f() / value.f() * 100.0
    } else {
        0.0
    };
    let staked_pct_all = if all.f() != 0.0 {
        staked_val.f() / all.f() * 100.0
    } else {
        0.0
    };
    let mut trail: Vec<String> = w.trail_only.clone();
    trail.sort();
    trail.dedup();
    let n_rows = rows.len();
    let out = obj(vec![
        (
            "generated",
            rungbot_core::iso8601_micros((io.clock)()).into(),
        ),
        ("wallets", Json::Arr(rows)),
        ("total_value", value.json()),
        ("staked_value", staked_val.json()),
        ("staked_pct", Json::Float(staked_pct)),
        ("staked_pct_all", Json::Float(staked_pct_all)),
        ("cex_value", cex.json()),
        ("all_holdings", all.json()),
        ("confirmed_bull", Json::Bool(confirmed_bull)),
        (
            "rule",
            obj(vec![
                ("tranche_x", Json::Float(w.tranche_x)),
                ("lead_pct_per_21d", Json::Float(w.lead_pct)),
                (
                    "trail_only",
                    Json::Arr(trail.into_iter().map(Json::Str).collect()),
                ),
            ]),
        ),
        (
            "errors",
            Json::Arr(errors.iter().cloned().map(Json::Str).collect()),
        ),
    ]);
    let path = d.wallets_path();
    crate::store::write_atomic(&path, &out.dumps(Some(2)))?;
    crate::store::write_atomic(&d.wallet_state_path(), &state.dumps(None))?;
    crate::store::write_atomic(&d.wallet_last_good_path(), &last_good.dumps(None))?;
    for a in &alerts {
        io.outbox.telegram(&format!("Wallet unbond alert. {a}"));
    }
    let mut line = format!(
        "wrote {} | {n_rows} wallets | ${} | staked {}% | of all {}% | alerts {} | errors {}",
        path.display(),
        comma(value.f(), 2),
        fixed(staked_pct, 0),
        fixed(staked_pct_all, 0),
        alerts.len(),
        errors.len()
    );
    if !errors.is_empty() {
        line.push_str(&format!(" ({})", errors.join("; ")));
    }
    let _ = writeln!(io.out, "{line}");
    Ok(())
}
