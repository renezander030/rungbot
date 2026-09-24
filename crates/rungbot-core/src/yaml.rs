//! A dependency-free reader for the nested-mapping subset a rungbot config uses.
//!
//! Supports arbitrarily nested `key:` blocks with scalar leaves, `#` comments and
//! quoted strings. It deliberately does not support lists, anchors or multi-line
//! strings: a config that needs those is a config that has outgrown this format.
//!
//! Mapping order is preserved, because the order of `coins:` is the report's tie-break
//! order and silently alphabetising it would change output.
//!
//! Scalars follow **YAML 1.1**, which is what PyYAML and most other readers do: a bare
//! `off` is the boolean `false`, not the string `"off"`. Callers that accept an on/off
//! setting must therefore accept a bool — see `Trail::parse`.

#[derive(Debug, Clone, PartialEq)]
pub enum Yaml {
    Str(String),
    /// Inline `{...}` or `[...]`, which this reader does not parse.
    Flow(String),
    Num(f64),
    Bool(bool),
    Null,
    Map(Vec<(String, Yaml)>),
}

impl Yaml {
    pub fn get(&self, key: &str) -> Option<&Yaml> {
        match self {
            Yaml::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&[(String, Yaml)]> {
        match self {
            Yaml::Map(e) => Some(e),
            _ => None,
        }
    }

    /// The scalar as a number, accepting the string form a quoted value would produce.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Yaml::Num(n) => Some(*n),
            Yaml::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<String> {
        match self {
            Yaml::Str(s) => Some(s.clone()),
            Yaml::Num(n) => Some(format!("{n}")),
            Yaml::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Yaml::Null)
    }

    /// The error a flow-style value should produce, if it is one.
    pub fn flow_error(&self, key: &str) -> Option<String> {
        match self {
            Yaml::Flow(raw) => Some(format!(
                "`{key}: {raw}` uses inline YAML, which this config format does not \
                 support. Write it as an indented block instead."
            )),
            _ => None,
        }
    }
}

fn scalar(raw: &str) -> Yaml {
    let v = raw.trim();
    if v.is_empty() || v == "~" || v == "null" {
        return Yaml::Null;
    }
    let bytes = v.as_bytes();
    if v.len() > 1 && (bytes[0] == b'"' || bytes[0] == b'\'') && bytes[bytes.len() - 1] == bytes[0]
    {
        return Yaml::Str(v[1..v.len() - 1].to_string());
    }
    // Flow style (`{a: 1}` / `[1, 2]`) is not supported. Without this it would parse as
    // a string and surface much later as a confusing type error.
    if bytes[0] == b'{' || bytes[0] == b'[' {
        return Yaml::Flow(v.to_string());
    }
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "y" => return Yaml::Bool(true),
        "false" | "no" | "off" | "n" => return Yaml::Bool(false),
        _ => {}
    }
    if let Ok(n) = v.parse::<f64>() {
        if n.is_finite() {
            return Yaml::Num(n);
        }
    }
    Yaml::Str(v.to_string())
}

/// Strip a `#` comment that is not inside quotes.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let (mut quote, mut i) = (0u8, 0usize);
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'#' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

pub fn parse(text: &str) -> Result<Yaml, String> {
    // (indent, path into the tree) — the stack of open blocks.
    let mut root: Vec<(String, Yaml)> = Vec::new();
    let mut stack: Vec<(usize, Vec<String>)> = vec![(usize::MAX, Vec::new())];

    for (lineno, raw) in text.lines().enumerate() {
        let lineno = lineno + 1;
        let line = strip_comment(raw);
        if line.trim().is_empty() {
            continue;
        }
        if line.trim_start().starts_with("- ") {
            return Err(format!(
                "line {lineno}: YAML lists are not supported by this config format"
            ));
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        let Some(colon) = trimmed.find(':') else {
            return Err(format!(
                "line {lineno}: expected `key: value`, got {trimmed:?}"
            ));
        };
        let key = trimmed[..colon].trim().to_string();
        if key.is_empty() {
            return Err(format!("line {lineno}: empty key"));
        }
        let rest = trimmed[colon + 1..].trim();

        while stack.len() > 1 && indent <= stack[stack.len() - 1].0 {
            stack.pop();
        }
        let path = stack[stack.len() - 1].1.clone();
        let value = if rest.is_empty() {
            Yaml::Map(Vec::new())
        } else {
            scalar(rest)
        };
        let is_block = rest.is_empty();

        insert_at(&mut root, &path, key.clone(), value)
            .map_err(|e| format!("line {lineno}: {e}"))?;

        if is_block {
            let mut child = path;
            child.push(key);
            stack.push((indent, child));
        }
    }
    Ok(Yaml::Map(root))
}

fn insert_at(
    root: &mut Vec<(String, Yaml)>,
    path: &[String],
    key: String,
    value: Yaml,
) -> Result<(), String> {
    let mut entries = root;
    for step in path {
        let idx = entries
            .iter()
            .position(|(k, _)| k == step)
            .ok_or_else(|| format!("bad indentation near {step:?}"))?;
        match &mut entries[idx].1 {
            Yaml::Map(m) => entries = m,
            _ => {
                return Err(format!(
                    "{step:?} has a value and cannot also have children"
                ))
            }
        }
    }
    if let Some(existing) = entries.iter_mut().find(|(k, _)| *k == key) {
        existing.1 = value;
    } else {
        entries.push((key, value));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# a comment
bands:
  first_pct: 12
  step_pct: 6

ladder:
  min_core_pct: 25
  trail: off

coins:
  BTC:
    name: Bitcoin       # trailing comment
    venue: binance
    pair: BTCUSDT
    entry: 61000
  XYZ:
    venue: gate
    pair: XYZ_USDT
    bands:
      first_pct: 20
      step_pct: 10
";

    #[test]
    fn reads_nested_blocks_and_scalars() {
        let y = parse(SAMPLE).expect("parses");
        assert_eq!(
            y.get("bands").unwrap().get("first_pct").unwrap().as_f64(),
            Some(12.0)
        );
        assert_eq!(
            y.get("coins")
                .unwrap()
                .get("BTC")
                .unwrap()
                .get("name")
                .unwrap()
                .as_str(),
            Some("Bitcoin".into()),
            "a trailing comment is stripped"
        );
        assert_eq!(
            y.get("coins")
                .unwrap()
                .get("XYZ")
                .unwrap()
                .get("bands")
                .unwrap()
                .get("step_pct")
                .unwrap()
                .as_f64(),
            Some(10.0),
            "three levels deep"
        );
    }

    #[test]
    fn yaml_11_booleans_match_pyyaml() {
        let y = parse(SAMPLE).unwrap();
        assert_eq!(
            y.get("ladder").unwrap().get("trail"),
            Some(&Yaml::Bool(false)),
            "a bare `off` is the boolean false, exactly as PyYAML reads it"
        );
    }

    #[test]
    fn mapping_order_is_preserved() {
        let y = parse(SAMPLE).unwrap();
        let coins = y.get("coins").unwrap().as_map().unwrap();
        let names: Vec<&str> = coins.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec!["BTC", "XYZ"],
            "config order drives report tie-breaks"
        );
    }

    #[test]
    fn lists_are_refused_with_a_reason() {
        let e = parse("coins:\n  - BTC\n").unwrap_err();
        assert!(e.contains("lists are not supported"), "got {e}");
    }

    #[test]
    fn a_line_without_a_colon_is_an_error_naming_the_line() {
        let e = parse("bands:\n  oops\n").unwrap_err();
        assert!(e.contains("line 2"), "got {e}");
    }

    #[test]
    fn inline_flow_style_is_reported_not_silently_stringified() {
        let y = parse("bands: {first_pct: 10, step_pct: 5}\n").unwrap();
        let node = y.get("bands").unwrap();
        let e = node.flow_error("bands").expect("a flow value is flagged");
        assert!(e.contains("indented block"), "{e}");
    }

    #[test]
    fn quoted_strings_keep_their_content() {
        let y = parse("a:\n  b: \"off\"\n  c: '12'\n").unwrap();
        assert_eq!(
            y.get("a").unwrap().get("b").unwrap(),
            &Yaml::Str("off".into())
        );
        assert_eq!(
            y.get("a").unwrap().get("c").unwrap(),
            &Yaml::Str("12".into())
        );
    }
}
