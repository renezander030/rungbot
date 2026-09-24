//! Python's `textwrap.wrap(text, width)` with its default options, reproduced exactly.
//!
//! The zone and regime mails wrap their paragraphs, and a mail compared byte for byte
//! must break its lines where the old one did. The defaults that matter: tabs expand to
//! 8 columns, every whitespace character becomes a space, a line never starts or ends
//! with whitespace, words longer than the width are broken, and words break after a
//! hyphen between letters (`re-`/`anchor`), not after one next to a digit (`30-min`).

/// `textwrap.wrap(text, width)`. Width counts characters, as Python's `len` does.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let text = munge_whitespace(text);
    let chars: Vec<char> = text.chars().collect();
    let chunks = split(&chars);
    wrap_chunks(chunks, width.max(1))
}

fn is_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ')
}

/// `\w`: a Unicode letter or digit, or `_`.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `[^\d\W]`: a word character that is not a digit.
fn is_letter(c: char) -> bool {
    is_word(c) && !c.is_numeric()
}

fn is_word_punct(c: char) -> bool {
    is_word(c) || matches!(c, '!' | '"' | '\'' | '&' | '.' | ',' | '?')
}

fn munge_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut col = 0usize;
    for c in text.chars() {
        match c {
            '\t' => {
                let n = 8 - col % 8;
                out.push_str(&" ".repeat(n));
                col += n;
            }
            '\n' | '\r' => {
                out.push(' ');
                col = 0;
            }
            c if is_ws(c) => {
                out.push(' ');
                col += 1;
            }
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

/// At `i`, a run of two or more `-` followed by a word character: its length.
fn em_dash_at(s: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    while j < s.len() && s[j] == '-' {
        j += 1;
    }
    (j - i >= 2 && j < s.len() && is_word(s[j])).then_some(j - i)
}

/// `TextWrapper.wordsep_re.split`, as a scanner over the same alternatives.
fn split(s: &[char]) -> Vec<String> {
    let at = |k: isize| -> Option<char> {
        if k < 0 {
            None
        } else {
            s.get(k as usize).copied()
        }
    };
    let letter = |k: isize| at(k).is_some_and(is_letter);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < s.len() {
        if is_ws(s[i]) {
            let mut j = i;
            while j < s.len() && is_ws(s[j]) {
                j += 1;
            }
            out.push(s[i..j].iter().collect());
            i = j;
            continue;
        }
        // An em-dash between words: `(?<=wp) -{2,} (?=\w)`.
        if i > 0 && is_word_punct(s[i - 1]) {
            if let Some(n) = em_dash_at(s, i) {
                out.push(s[i..i + n].iter().collect());
                i += n;
                continue;
            }
        }
        // A word, lazily extended until one of the three endings matches.
        let mut j = i + 1;
        loop {
            let jj = j as isize;
            // hyphenated: `-` with two letters (or letter-letter-) behind, a letter ahead
            if s.get(j) == Some(&'-') {
                let behind = (letter(jj - 2) && letter(jj - 1))
                    || (letter(jj - 3) && at(jj - 2) == Some('-') && letter(jj - 1));
                let ahead = letter(jj + 1)
                    && (letter(jj + 2) || (at(jj + 2) == Some('-') && letter(jj + 3)));
                if behind && ahead {
                    j += 1;
                    break;
                }
            }
            // end of word
            if j >= s.len() || is_ws(s[j]) {
                break;
            }
            // em-dash after a word
            if is_word_punct(s[j - 1]) && em_dash_at(s, j).is_some() {
                break;
            }
            j += 1;
        }
        out.push(s[i..j].iter().collect());
        i = j;
    }
    out
}

fn wrap_chunks(mut chunks: Vec<String>, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    chunks.reverse();
    let len = |s: &str| s.chars().count();
    let blank = |s: &str| s.chars().all(is_ws);
    while !chunks.is_empty() {
        let mut cur: Vec<String> = Vec::new();
        let mut cur_len = 0usize;
        if !lines.is_empty() && chunks.last().is_some_and(|c| blank(c)) {
            chunks.pop();
        }
        while let Some(c) = chunks.last() {
            let l = len(c);
            if cur_len + l <= width {
                cur_len += l;
                cur.push(chunks.pop().expect("just peeked"));
            } else {
                break;
            }
        }
        if chunks.last().is_some_and(|c| len(c) > width) {
            handle_long_word(&mut chunks, &mut cur, cur_len, width);
        }
        if cur.last().is_some_and(|c| blank(c)) {
            cur.pop();
        }
        if !cur.is_empty() {
            lines.push(cur.concat());
        }
    }
    lines
}

fn handle_long_word(chunks: &mut [String], cur: &mut Vec<String>, cur_len: usize, width: usize) {
    let space_left = if width < 1 { 1 } else { width - cur_len };
    let last = chunks.len() - 1;
    let chunk: Vec<char> = chunks[last].chars().collect();
    let mut end = space_left;
    if chunk.len() > space_left {
        if let Some(h) = chunk[..space_left.min(chunk.len())]
            .iter()
            .rposition(|c| *c == '-')
        {
            if h > 0 && chunk[..h].iter().any(|c| *c != '-') {
                end = h + 1;
            }
        }
    }
    let end = end.min(chunk.len());
    cur.push(chunk[..end].iter().collect());
    chunks[last] = chunk[end..].iter().collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyphens_between_letters_break_and_digits_do_not() {
        assert_eq!(
            split(&"re-anchor 30-min".chars().collect::<Vec<_>>()),
            vec!["re-", "anchor", " ", "30-min"]
        );
        assert_eq!(wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert!(wrap("   ", 10).is_empty());
        assert!(wrap("", 10).is_empty());
    }
}
