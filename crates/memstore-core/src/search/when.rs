//! The time a fact search names, read deterministically from its words: years ("2023"), months ("May 2023",
//! "Jul–Sep 2026"), quarters ("Q3 2026"), days ("April 18, 2023", "2026-03-02") and relative times ("last
//! year", "this quarter"), relative to the request's own date (a leading `[2028-01-01]`) or else now. The
//! window it gives decides which facts are in time (see `search_with`): no window, no time term.

use chrono::{Datelike, Days, NaiveDate, TimeZone, Utc};

/// The span of time a query names, `[from, to)` in µs since the epoch, and the years it names.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub from: i64,
    pub to: i64,
    pub years: Vec<i32>,
}

impl Window {
    pub fn contains(&self, t: i64) -> bool {
        t >= self.from && t < self.to
    }
}

/// A query's leading request date (`[2028-01-01] Email …`, as a request carries the day it was made) and the
/// query without it.
pub fn request_date(query: &str) -> (Option<NaiveDate>, &str) {
    let q = query.trim_start();
    if let Some(rest) = q.strip_prefix('[')
        && let Some(end) = rest.find(']')
        && let Some(d) = iso_date(rest[..end].trim())
    {
        return (Some(d), rest[end + 1..].trim_start());
    }
    (None, query)
}

/// The years a date may name: anything else (chrono reads `+262143-12-31` and `-0001-01-01`) is not a date
/// a person means, and arithmetic near chrono's limits would overflow.
const SANE_YEARS: std::ops::RangeInclusive<i32> = 1900..=2200;

/// `YYYY-MM-DD`, four digits of a sane year, or nothing.
fn iso_date(w: &str) -> Option<NaiveDate> {
    let b = w.as_bytes();
    let shaped = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    let d = NaiveDate::parse_from_str(w, "%Y-%m-%d")
        .ok()
        .filter(|_| shaped)?;
    SANE_YEARS.contains(&d.year()).then_some(d)
}

fn plus(d: NaiveDate, n: u64) -> Option<NaiveDate> {
    d.checked_add_days(Days::new(n))
}

fn minus(d: NaiveDate, n: u64) -> Option<NaiveDate> {
    d.checked_sub_days(Days::new(n))
}

fn micros(d: NaiveDate) -> i64 {
    Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .timestamp_micros()
}

fn month_of(w: &str) -> Option<u32> {
    const M: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let w = w.to_lowercase();
    // A month's name or its three-letter start ("sept" too); "may" is a month only by its name.
    if w.len() < 3 {
        return None;
    }
    let i = M.iter().position(|m| w.starts_with(m))?;
    const FULL: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    (w == M[i] || FULL[i] == w || (i == 8 && w == "sept")).then_some(i as u32 + 1)
}

fn year_of(w: &str) -> Option<i32> {
    let y: i32 = w.parse().ok()?;
    (w.len() == 4 && (1990..=2100).contains(&y)).then_some(y)
}

fn day_of(w: &str) -> Option<u32> {
    let w = w
        .trim_end_matches("st")
        .trim_end_matches("nd")
        .trim_end_matches("rd")
        .trim_end_matches("th");
    let d: u32 = w.parse().ok()?;
    (1..=31).contains(&d).then_some(d)
}

fn month_start(y: i32, m: u32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(y, m, 1)
}

fn next_month(y: i32, m: u32) -> Option<NaiveDate> {
    if m == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(y, m + 1, 1)
    }
}

/// The window `query` names, read as of `today` (the request's date, else now). None when it names no time.
pub fn window(query: &str, today: NaiveDate) -> Option<Window> {
    // Relative words need a day to read from; an absurd one (a request dated far off) reads none.
    parse(query, today, SANE_YEARS.contains(&today.year()))
}

/// The time a fact's text states outright (its dates, months, quarters, years), for telling whether it
/// speaks of the time a query names; words that need a day to read from ("last year", "March 3") don't count.
pub fn stated(text: &str) -> Option<Window> {
    parse(text, NaiveDate::MAX, false)
}

impl Window {
    /// Whether the two spans share any time.
    pub fn overlaps(&self, other: &Window) -> bool {
        self.from < other.to && other.from < self.to
    }
}

fn parse(query: &str, today: NaiveDate, relative: bool) -> Option<Window> {
    // Words, cut at spaces and punctuation that never sits inside a date part; a range's dash splits too.
    let words: Vec<String> = query
        .split(|c: char| c.is_whitespace() || ",;:()[]?!\"“”'/".contains(c))
        .flat_map(|w| {
            let w = w.trim_matches(|c: char| c == '.' || c == '-');
            // "2026-03-02" stays whole; "Jul–Sep" and "Jul-Sep" split.
            if iso_date(w).is_some() {
                vec![w.to_string()]
            } else {
                w.split(['–', '—', '-'])
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            }
        })
        .filter(|w| !w.is_empty())
        .collect();
    let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    let mut spans: Vec<(NaiveDate, NaiveDate)> = vec![];
    let mut years: Vec<i32> = vec![];
    let mut used = vec![false; words.len()];
    // A day, a day either side: a fact's time is often stamped in another time zone than the one asked in.
    let day_span = |d: NaiveDate| Some((minus(d, 1)?, plus(d, 2)?));
    for i in 0..words.len() {
        let w = &lower[i];
        // 2026-03-02
        if let Some(d) = iso_date(w) {
            spans.extend(day_span(d));
            years.push(d.year());
            used[i] = true;
            continue;
        }
        // Q3 2026 / 2026 Q3, H1 2026
        if (w.len() == 2) && (w.starts_with('q') || w.starts_with('h')) {
            let n: u32 = w[1..].parse().unwrap_or(0);
            let y = lower
                .get(i + 1)
                .and_then(|x| year_of(x))
                .or_else(|| i.checked_sub(1).and_then(|j| year_of(&lower[j])));
            let (first, len) = match (w.as_bytes()[0], n) {
                (b'q', 1..=4) => ((n - 1) * 3 + 1, 3),
                (b'h', 1..=2) => ((n - 1) * 6 + 1, 6),
                _ => continue,
            };
            let Some(y) = y.or(relative.then(|| today.year())) else {
                continue;
            };
            if let (Some(a), Some(b)) = (month_start(y, first), next_month(y, first + len - 1)) {
                spans.push((a, b));
                years.push(y);
                used[i] = true;
                for j in [i.wrapping_sub(1), i + 1] {
                    if lower.get(j).is_some_and(|x| year_of(x) == Some(y)) {
                        used[j] = true;
                    }
                }
            }
            continue;
        }
        // April 18, 2023 / April 18 2023 / April 2023 / Jul–Sep 2026 / 18 April 2023
        if let Some(m) = month_of(w) {
            // "may" alone is a verb more often than a month: only with a day or a year.
            let next = lower.get(i + 1);
            let (day, yi) = match next.and_then(|x| day_of(x)) {
                Some(d) if !next.is_some_and(|x| year_of(x).is_some()) => (Some(d), i + 2),
                _ => (None, i + 1),
            };
            let day = day.or_else(|| i.checked_sub(1).and_then(|j| day_of(&lower[j])));
            let y = lower.get(yi).and_then(|x| year_of(x));
            // A month range: "Jul–Sep 2026", the second month followed by the year.
            let range_end = (day.is_none() && y.is_none())
                .then(|| next.and_then(|x| month_of(x)))
                .flatten()
                .filter(|_| lower.get(i + 2).and_then(|x| year_of(x)).is_some());
            if let Some(m2) = range_end {
                let y = year_of(&lower[i + 2]).unwrap();
                if let (Some(a), Some(b)) = (month_start(y, m), next_month(y, m2)) {
                    spans.push((a, b));
                    years.push(y);
                    used[i] = true;
                }
                continue;
            }
            match (day, y) {
                (Some(d), Some(y)) => {
                    if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
                        spans.extend(day_span(date));
                        years.push(y);
                        used[i] = true;
                        used[yi] = true;
                    }
                }
                (None, Some(y)) => {
                    if let (Some(a), Some(b)) = (month_start(y, m), next_month(y, m)) {
                        spans.push((a, b));
                        years.push(y);
                        used[i] = true;
                        used[yi] = true;
                    }
                }
                // A day without a year: the latest such day by `today`.
                (Some(d), None)
                    if relative && (w != "may" || next.is_some_and(|x| day_of(x).is_some())) =>
                {
                    let mut y = today.year();
                    let date = loop {
                        match NaiveDate::from_ymd_opt(y, m, d) {
                            Some(x) if x <= today => break Some(x),
                            _ if y < today.year() - 1 => break None,
                            _ => y -= 1,
                        }
                    };
                    if let Some(date) = date {
                        spans.extend(day_span(date));
                        years.push(date.year());
                        used[i] = true;
                    }
                }
                _ => {}
            }
            continue;
        }
    }
    // Relative times, as of `today`.
    for i in (0..lower.len()).filter(|_| relative) {
        let (a, b) = (lower[i].as_str(), lower.get(i + 1).map(String::as_str));
        let quarter = |a: &str| {
            let q0 = (today.month() - 1) / 3;
            let (y, q) = match (a, q0) {
                ("this", q) => (today.year(), q),
                (_, 0) => (today.year() - 1, 3),
                (_, q) => (today.year(), q - 1),
            };
            Some((month_start(y, q * 3 + 1)?, next_month(y, q * 3 + 3)?))
        };
        let last_month = || {
            let (y, m) = match today.month() {
                1 => (today.year() - 1, 12),
                m => (today.year(), m - 1),
            };
            Some((month_start(y, m)?, next_month(y, m)?))
        };
        let span: Option<(NaiveDate, NaiveDate)> = match (a, b) {
            ("today", _) => day_span(today),
            ("yesterday", _) => minus(today, 1).and_then(day_span),
            ("last" | "past" | "previous", Some("year")) => {
                let y = today.year() - 1;
                month_start(y, 1).zip(month_start(y + 1, 1))
            }
            ("this", Some("year")) => month_start(today.year(), 1).zip(plus(today, 1)),
            ("last" | "past" | "previous", Some("month")) => last_month(),
            ("this", Some("month")) => month_start(today.year(), today.month()).zip(plus(today, 1)),
            ("last" | "past" | "previous", Some("week")) => minus(today, 7).zip(plus(today, 1)),
            ("this", Some("week")) => {
                minus(today, today.weekday().num_days_from_monday() as u64).zip(plus(today, 1))
            }
            ("last" | "previous" | "this", Some("quarter")) => quarter(a),
            _ => None,
        };
        if let Some(s) = span {
            years.push(s.0.year());
            spans.push(s);
        }
    }
    // Bare years not already part of a date.
    for i in 0..lower.len() {
        if used[i] {
            continue;
        }
        if let Some(y) = year_of(&lower[i])
            && let Some(span) = month_start(y, 1).zip(month_start(y + 1, 1))
        {
            spans.push(span);
            years.push(y);
        }
    }
    let from = spans.iter().map(|s| s.0).min()?;
    let to = spans.iter().map(|s| s.1).max()?;
    years.sort_unstable();
    years.dedup();
    Some(Window {
        from: micros(from),
        to: micros(to),
        years,
    })
}

/// The years (1990–2100) `text` names, as four-digit words.
pub fn years_named(text: &str) -> Vec<i32> {
    let mut out: Vec<i32> = text
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(year_of)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}
