//! Names a user typed, checked against the names a command knows: flags and config keys.

/// Edit distance between two short names; swapping two neighbours counts as one edit.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// The known name closest to `word`, when it is close enough to be a typo of it.
pub fn closest<'a>(word: &str, known: &[&'a str]) -> Option<&'a str> {
    let norm = |s: &str| s.to_ascii_lowercase().replace('_', "-");
    let w = norm(word);
    let limit = (w.chars().count() / 3).max(1);
    known
        .iter()
        .map(|k| (distance(&w, &norm(k)), *k))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// `unknown flag --dryrun (did you mean --dry-run?)`, or `None` when every flag is known.
pub fn unknown_flag<'a, I>(given: I, known: &[&str]) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    given
        .into_iter()
        .find(|f| !known.contains(f))
        .map(|f| match closest(f, known) {
            Some(k) => format!("unknown flag --{f} (did you mean --{k}?)"),
            None => format!("unknown flag --{f}"),
        })
}

/// `unknown key `ladder.breaker_pc` (did you mean `breaker_pct`?)` for one key.
pub fn unknown_key(path: &str, key: &str, known: &[&str]) -> String {
    let full = if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    };
    match closest(key, known) {
        Some(k) => format!("unknown key `{full}` (did you mean `{k}`?)"),
        None => format!("unknown key `{full}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typo_names_the_flag_it_meant() {
        let known = ["dry-run", "verbose", "config", "state-dir"];
        assert_eq!(
            unknown_flag(["dryrun"], &known).unwrap(),
            "unknown flag --dryrun (did you mean --dry-run?)"
        );
        assert_eq!(
            unknown_flag(["dry_run"], &known).unwrap(),
            "unknown flag --dry_run (did you mean --dry-run?)"
        );
        assert_eq!(unknown_flag(["dry-run", "verbose"], &known), None);
    }

    #[test]
    fn a_name_far_from_every_known_one_gets_no_guess() {
        assert_eq!(
            unknown_flag(["teleport"], &["dry-run", "verbose"]).unwrap(),
            "unknown flag --teleport"
        );
    }

    #[test]
    fn config_keys_name_their_section() {
        assert_eq!(
            unknown_key("ladder", "breaker_pc", &["breaker_pct", "breaker_days"]),
            "unknown key `ladder.breaker_pc` (did you mean `breaker_pct`?)"
        );
        assert_eq!(
            unknown_key("", "coinz", &["coins"]),
            "unknown key `coinz` (did you mean `coins`?)"
        );
    }
}
