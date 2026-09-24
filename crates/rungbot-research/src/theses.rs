//! Which screen runs in which market regime, and how its report reads.
//!
//! The regime label (bull | chop | bear) picks a [`Thesis`]: the steps to run, the
//! ledger field holding each coin's verdict line, the buckets those verdicts sort into,
//! and the wording of the note and email. It is data, kept in a YAML file a human edits
//! (`assets/theses.example.yaml` is the built-in set); an unknown label falls back to
//! the bear thesis, the one that demands real value and ignores hype.

use std::fmt;

use crate::py::{self, Json};

/// The built-in theses as the report reads them.
pub const BUILTIN_JSON: &str = include_str!("../assets/theses.json");
/// The same set as an editable YAML file, what `rungbot research theses --example`
/// prints.
pub const EXAMPLE_YAML: &str = include_str!("../assets/theses.example.yaml");

/// One pipeline stage the report can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Oppscan,
    CatalystSearch,
    CatalystSynthesize,
    SurvivorBuildIndex,
    SurvivorSelect,
    SurvivorRun,
    UnlocksBuildIndex,
    UnlocksEnrich,
}

impl Step {
    pub const ALL: [Step; 8] = [
        Step::Oppscan,
        Step::CatalystSearch,
        Step::CatalystSynthesize,
        Step::SurvivorBuildIndex,
        Step::SurvivorSelect,
        Step::SurvivorRun,
        Step::UnlocksBuildIndex,
        Step::UnlocksEnrich,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Step::Oppscan => "oppscan",
            Step::CatalystSearch => "catalyst search",
            Step::CatalystSynthesize => "catalyst synthesize",
            Step::SurvivorBuildIndex => "survivor build-index",
            Step::SurvivorSelect => "survivor select",
            Step::SurvivorRun => "survivor run",
            Step::UnlocksBuildIndex => "unlocks build-index",
            Step::UnlocksEnrich => "unlocks enrich",
        }
    }

    pub fn parse(s: &str) -> Option<Step> {
        let norm = s.split_whitespace().collect::<Vec<_>>().join(" ");
        Step::ALL.into_iter().find(|x| x.as_str() == norm)
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Thesis {
    pub regime: String,
    pub title: String,
    pub emoji: String,
    pub intro: Vec<String>,
    pub refresh: Vec<Step>,
    pub synth: Vec<Step>,
    pub verdict_field: String,
    pub order: Vec<String>,
    /// Bucket → markdown heading, one per `order` entry.
    pub heads: Vec<(String, String)>,
    pub top_bucket: String,
    /// `durability`, `confidence`, or `range` (computed by the report); any other key
    /// is read from the verdict line and labelled like durability.
    pub score_key: String,
    pub noun: String,
    pub how: String,
    pub empty: String,
}

impl Thesis {
    pub fn head(&self, bucket: &str) -> &str {
        self.heads
            .iter()
            .find(|(k, _)| k == bucket)
            .map(|(_, v)| v.as_str())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Theses {
    pub theses: Vec<Thesis>,
    /// The thesis an unknown regime label gets.
    pub fallback: String,
    /// The regime line of the note; `{market}` is replaced with the label.
    pub regime_line: String,
}

/// A config tree as the YAML reader delivers it: scalars as text, mappings in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Text(String),
    Map(Vec<(String, Node)>),
}

impl Node {
    fn get(&self, k: &str) -> Option<&Node> {
        match self {
            Node::Map(e) => e.iter().find(|(key, _)| key == k).map(|(_, v)| v),
            Node::Text(_) => None,
        }
    }

    fn text(&self, k: &str, ctx: &str) -> Result<String, String> {
        match self.get(k) {
            Some(Node::Text(s)) => Ok(s.clone()),
            Some(Node::Map(_)) => Err(format!("`{ctx}.{k}` must be text, not a block")),
            None => Err(format!("`{ctx}.{k}` is missing")),
        }
    }

    /// A comma-separated scalar, or a block whose values are the items in order.
    fn list(&self, k: &str, ctx: &str) -> Result<Vec<String>, String> {
        match self.get(k) {
            Some(Node::Text(s)) => Ok(s
                .split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()),
            Some(Node::Map(e)) => e
                .iter()
                .map(|(key, v)| match v {
                    Node::Text(s) => Ok(s.clone()),
                    Node::Map(_) => Err(format!("`{ctx}.{k}.{key}` must be text")),
                })
                .collect(),
            None => Err(format!("`{ctx}.{k}` is missing")),
        }
    }
}

fn json_to_node(v: &Json) -> Node {
    match v {
        Json::Obj(e) => Node::Map(
            e.iter()
                .map(|(k, v)| (k.clone(), json_to_node(v)))
                .collect(),
        ),
        Json::Arr(a) => Node::Map(
            a.iter()
                .enumerate()
                .map(|(i, v)| ((i + 1).to_string(), json_to_node(v)))
                .collect(),
        ),
        other => Node::Text(other.display()),
    }
}

impl Theses {
    /// The built-in set.
    pub fn builtin() -> Theses {
        let json = py::parse(BUILTIN_JSON).expect("the built-in theses are valid JSON");
        Theses::from_node(&json_to_node(&json)).expect("the built-in theses are valid")
    }

    /// Read a theses tree, checking every step name and that each bucket has a heading.
    pub fn from_node(root: &Node) -> Result<Theses, String> {
        let regime_line = root.text("regime_line", "theses file")?;
        let fallback = root
            .text("fallback", "theses file")
            .unwrap_or_else(|_| "bear".into())
            .to_lowercase();
        let Some(Node::Map(entries)) = root.get("theses") else {
            return Err("the theses file needs a `theses:` block".into());
        };
        let mut theses = Vec::new();
        for (name, node) in entries {
            let ctx = format!("theses.{name}");
            let steps = |k: &str| -> Result<Vec<Step>, String> {
                node.list(k, &ctx)?
                    .iter()
                    .map(|s| {
                        Step::parse(s).ok_or_else(|| {
                            format!(
                                "`{ctx}.{k}`: unknown step {s:?}; known steps: {}",
                                Step::ALL.map(|x| x.as_str()).join(", ")
                            )
                        })
                    })
                    .collect()
            };
            let order = node.list("order", &ctx)?;
            if order.is_empty() {
                return Err(format!("`{ctx}.order` needs at least one bucket"));
            }
            let heads = match node.get("heads") {
                Some(Node::Map(e)) => e
                    .iter()
                    .map(|(k, v)| match v {
                        Node::Text(s) => Ok((k.clone(), s.clone())),
                        Node::Map(_) => Err(format!("`{ctx}.heads.{k}` must be text")),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err(format!("`{ctx}.heads` must be a block")),
            };
            for b in &order {
                if !heads.iter().any(|(k, _)| k == b) {
                    return Err(format!("`{ctx}.heads` has no heading for bucket {b}"));
                }
            }
            let top_bucket = node.text("top_bucket", &ctx)?;
            if !order.contains(&top_bucket) {
                return Err(format!("`{ctx}.top_bucket` {top_bucket} is not in order"));
            }
            theses.push(Thesis {
                regime: name.to_lowercase(),
                title: node.text("title", &ctx)?,
                emoji: node.text("emoji", &ctx).unwrap_or_default(),
                // One paragraph as text, or several as a block of numbered lines.
                intro: match node.get("intro") {
                    Some(Node::Text(s)) => vec![s.clone()],
                    Some(Node::Map(_)) => node.list("intro", &ctx)?,
                    None => Vec::new(),
                },
                refresh: steps("refresh")?,
                synth: steps("synth")?,
                verdict_field: node.text("verdict_field", &ctx)?,
                order,
                heads,
                top_bucket,
                score_key: node.text("score_key", &ctx)?,
                noun: node.text("noun", &ctx)?,
                how: node.text("how", &ctx)?,
                empty: node.text("empty", &ctx)?,
            });
        }
        if !theses.iter().any(|t| t.regime == fallback) {
            return Err(format!("the fallback thesis {fallback:?} is not defined"));
        }
        Ok(Theses {
            theses,
            fallback,
            regime_line,
        })
    }

    /// The thesis for a regime label: trimmed and lower-cased, unknown → fallback.
    pub fn for_regime(&self, market: &str) -> &Thesis {
        let key = py::strip(market).to_lowercase();
        self.get(&key)
            .or_else(|| self.get(&self.fallback))
            .unwrap_or(&self.theses[0])
    }

    pub fn get(&self, regime: &str) -> Option<&Thesis> {
        self.theses.iter().find(|t| t.regime == regime)
    }

    pub fn names(&self) -> Vec<&str> {
        self.theses.iter().map(|t| t.regime.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_set_maps_each_regime_to_its_screen() {
        let t = Theses::builtin();
        assert_eq!(t.names(), vec!["bear", "chop", "bull"]);
        let bull = t.for_regime(" BULL ");
        assert_eq!(bull.title, "Catalyst Watch");
        assert_eq!(bull.refresh, vec![Step::Oppscan, Step::CatalystSearch]);
        assert_eq!(bull.synth, vec![Step::CatalystSynthesize]);
        assert_eq!(t.for_regime("chop").score_key, "range");
        assert_eq!(t.for_regime("sideways").title, "Survivor Watch");
        assert_eq!(t.for_regime("").title, "Survivor Watch");
        assert!(t.for_regime("bear").how.contains("LLM survivor verdict"));
    }

    #[test]
    fn a_bad_step_or_missing_heading_is_named() {
        let mut n = json_to_node(&py::parse(BUILTIN_JSON).unwrap());
        if let Node::Map(root) = &mut n {
            let theses = root.iter_mut().find(|(k, _)| k == "theses").unwrap();
            if let Node::Map(t) = &mut theses.1 {
                if let Node::Map(bear) = &mut t[0].1 {
                    let r = bear.iter_mut().find(|(k, _)| k == "refresh").unwrap();
                    r.1 = Node::Text("survivor sprint".into());
                }
            }
        }
        let e = Theses::from_node(&n).unwrap_err();
        assert!(e.contains("unknown step \"survivor sprint\""), "{e}");
    }
}
