//! Personal identifiers in text an org's people and sources write, replaced before it is stored (`Settings::pii`).
//!
//! What it finds is what can be told from other digits by a check, so it rarely hits a number that is only a number:
//!
//! - payment card numbers: 13 to 19 digits, in groups split by spaces or dashes, that pass the Luhn check → `[card]`;
//! - US social security numbers: `123-45-6789`, with the ranges the SSA never issues left alone → `[ssn]`;
//! - IBANs: a country code, two check digits and up to 30 letters or digits, grouped by fours or not, that pass the
//!   mod-97 check → `[iban]`;
//! - international phone numbers: `+` and 8 to 15 digits, with spaces, dashes, dots and brackets → `[phone]`.
//!
//! Email addresses and names are not touched: a memory is mostly about who owns what, and an org's people are its
//! users' ids. Credentials are redacted where the text is made ([`memstore_client::hook::redact`]). The episode
//! is stored redacted, so what is extracted from it, searched in it and exported from it is too, and an id made
//! from the redacted text is the same whenever the same text is sent again.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use prometheus::IntCounterVec;

/// Identifiers redacted, by kind. Served as `memstore_pii_redactions_total`.
pub static REDACTIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        prometheus::Opts::new(
            "memstore_pii_redactions_total",
            "Personal identifiers replaced in text before it was stored, by kind",
        ),
        &["kind"],
    )
    .unwrap()
});

/// What a pass redacted, by kind.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Found(pub BTreeMap<&'static str, usize>);

impl Found {
    /// Count these in `memstore_pii_redactions_total`.
    pub fn count(&self) {
        for (k, n) in &self.0 {
            REDACTIONS.with_label_values(&[k]).inc_by(*n as u64);
        }
    }

    pub fn total(&self) -> usize {
        self.0.values().sum()
    }

    pub fn add(&mut self, other: &Found) {
        for (k, n) in &other.0 {
            *self.0.entry(k).or_default() += n;
        }
    }

    fn one(&mut self, kind: &'static str) {
        *self.0.entry(kind).or_default() += 1;
    }
}

/// `text` with its personal identifiers replaced by `[card]`, `[ssn]`, `[iban]` and `[phone]`, and what was found.
pub fn redact(text: &str) -> (String, Found) {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut found = Found::default();
    let mut i = 0;
    while i < chars.len() {
        let boundary = i == 0 || !chars[i - 1].is_alphanumeric();
        if boundary && let Some((end, kind)) = candidate(&chars, i) {
            out.push_str(&format!("[{kind}]"));
            found.one(kind);
            i = end;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    (out, found)
}

/// The identifier that starts at `i`, if one does: where it ends (exclusive) and its kind.
fn candidate(c: &[char], i: usize) -> Option<(usize, &'static str)> {
    match c[i] {
        '+' => phone(c, i),
        d if d.is_ascii_digit() => digits(c, i),
        l if l.is_ascii_uppercase() => iban(c, i),
        _ => None,
    }
}

/// Whether the char at `end` (or the end of the text) leaves the identifier standing alone.
fn ends_at(c: &[char], end: usize) -> bool {
    end >= c.len() || !c[end].is_alphanumeric()
}

/// The most digits a card holds, and so the most a run is read for: past it nothing can match, and a text of
/// nothing but digits and spaces would otherwise cost a read of the whole run from each of its digit groups.
const MAX_DIGITS: usize = 19;

/// A run of digit groups split by single spaces or dashes, starting at `i`: each group's range. It stops once
/// the groups hold more digits than any identifier does, so its work is bounded whatever the text is.
fn groups(c: &[char], i: usize) -> Vec<(usize, usize)> {
    let mut g = vec![];
    let (mut p, mut total) = (i, 0);
    loop {
        let s = p;
        while p < c.len() && c[p].is_ascii_digit() && p - s <= MAX_DIGITS {
            p += 1;
        }
        if p == s {
            break;
        }
        total += p - s;
        g.push((s, p));
        if total > MAX_DIGITS {
            break;
        }
        if p + 1 < c.len() && (c[p] == ' ' || c[p] == '-') && c[p + 1].is_ascii_digit() {
            p += 1;
        } else {
            break;
        }
    }
    g
}

/// A card or an SSN made of the first groups of the run at `i`: the longest that checks out.
fn digits(c: &[char], i: usize) -> Option<(usize, &'static str)> {
    let g = groups(c, i);
    for e in (0..g.len()).rev() {
        let end = g[e].1;
        if !ends_at(c, end) {
            continue;
        }
        let n: usize = g[..=e].iter().map(|(s, t)| t - s).sum();
        // Only a prefix the right size for a card, or the shape of an SSN, is worth reading the digits of.
        let ssn_shape = e == 2 && [3, 2, 4] == [g[0].1 - g[0].0, g[1].1 - g[1].0, g[2].1 - g[2].0];
        if !(13..=19).contains(&n) && !ssn_shape {
            continue;
        }
        let ds: Vec<u8> = g[..=e]
            .iter()
            .flat_map(|(s, t)| c[*s..*t].iter().map(|d| *d as u8 - b'0'))
            .collect();
        if (13..=19).contains(&n) && luhn(&ds) {
            return Some((end, "card"));
        }
        // 123-45-6789: three groups of 3, 2 and 4 digits, split by dashes only.
        if e == 2
            && [3, 2, 4] == [g[0].1 - g[0].0, g[1].1 - g[1].0, g[2].1 - g[2].0]
            && c[g[0].1] == '-'
            && c[g[1].1] == '-'
            && ssn(&ds)
        {
            return Some((end, "ssn"));
        }
    }
    None
}

fn luhn(d: &[u8]) -> bool {
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &x)| {
            let x = x as u32;
            if i % 2 == 1 {
                let y = x * 2;
                if y > 9 { y - 9 } else { y }
            } else {
                x
            }
        })
        .sum();
    // All the same digit (0000 0000 0000 0000) passes the check and is no card.
    sum.is_multiple_of(10) && d.iter().any(|x| *x != d[0])
}

/// Nine digits the SSA could have issued: no area 000, 666 or 900 and up, no group 00, no serial 0000.
fn ssn(d: &[u8]) -> bool {
    let n = |r: std::ops::Range<usize>| d[r].iter().fold(0u32, |a, x| a * 10 + *x as u32);
    let (area, group, serial) = (n(0..3), n(3..5), n(5..9));
    area != 0 && area != 666 && area < 900 && group != 0 && serial != 0
}

/// `+` and 8 to 15 digits, with spaces, dashes, dots and brackets between them.
fn phone(c: &[char], i: usize) -> Option<(usize, &'static str)> {
    let mut p = i + 1;
    let (mut n, mut last_digit) = (0, i);
    // Bounded by digits, not characters: padding a number with spaces doesn't hide it, and a `+` is read only as
    // far as the next character that can't be in a number, so the text is read once however many `+` it holds.
    while p < c.len() && n <= 15 && (c[p].is_ascii_digit() || " -.()".contains(c[p])) {
        if c[p].is_ascii_digit() {
            n += 1;
            last_digit = p;
        }
        p += 1;
    }
    let end = last_digit + 1;
    ((8..=15).contains(&n) && n > 0 && ends_at(c, end)).then_some((end, "phone"))
}

/// An IBAN starting at `i`: `XX99`, then groups of four letters or digits and a last of one to four, with or
/// without a space between them, that pass mod 97.
fn iban(c: &[char], i: usize) -> Option<(usize, &'static str)> {
    if !(c.get(i + 1)?.is_ascii_uppercase()
        && c.get(i + 2)?.is_ascii_digit()
        && c.get(i + 3)?.is_ascii_digit())
    {
        return None;
    }
    let mut p = i;
    let mut chars: Vec<char> = vec![];
    while p < c.len() && chars.len() < 34 {
        if c[p].is_ascii_alphanumeric() && !c[p].is_ascii_lowercase() {
            chars.push(c[p]);
            p += 1;
            // A space between groups of four.
            if chars.len().is_multiple_of(4)
                && p + 1 < c.len()
                && c[p] == ' '
                && c[p + 1].is_ascii_alphanumeric()
            {
                p += 1;
            }
        } else {
            break;
        }
    }
    // The longest prefix that stands alone and checks out.
    for len in (15..=chars.len()).rev() {
        if iban_ok(&chars[..len]) {
            // Where that prefix ends in the text.
            let (mut seen, mut q) = (0, i);
            while seen < len {
                if c[q] != ' ' {
                    seen += 1;
                }
                q += 1;
            }
            if ends_at(c, q) {
                return Some((q, "iban"));
            }
        }
    }
    None
}

fn iban_ok(s: &[char]) -> bool {
    if s.len() < 15 || s.len() > 34 || !s[0].is_ascii_uppercase() || !s[1].is_ascii_uppercase() {
        return false;
    }
    // Move the first four to the end, letters become 10..35, and the number mod 97 is 1.
    let mut rem: u32 = 0;
    for ch in s[4..].iter().chain(&s[..4]) {
        let v = match ch.to_digit(36) {
            Some(v) => v,
            None => return false,
        };
        rem = if v >= 10 {
            (rem * 100 + v) % 97
        } else {
            (rem * 10 + v) % 97
        };
    }
    rem == 1
}
