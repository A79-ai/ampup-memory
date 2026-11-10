//! The people an episode lists (`{id, name}`, such as a call's invitees) and the speakers its units name: which of
//! them speaks, and by what name. A list often names someone only by their address ("Srosenbach" for
//! srosenbach@), while the units hear their real name ("Scott Rosenbach"); that name is what they are shown by, and
//! a full one is also theirs to be found by. Any type whose body lists people and has speakers uses this.

use std::collections::HashSet;

use serde::Deserialize;

use super::Unit;

/// Someone an episode lists: their id (`contact:jo@acme.com`) and the name the list gives them.
#[derive(Clone, Debug, Deserialize)]
pub struct Participant {
    pub id: String,
    pub name: String,
}

/// How a listed participant is heard in the units.
#[derive(Clone, Debug, PartialEq)]
pub struct Heard<'a> {
    /// The unit they first speak in, if they speak.
    pub turn: Option<&'a Unit>,
    /// What to call them: their listed name, or, when it is only their address, the name they speak under. Empty
    /// when neither says more than the address, so a reader names them some other way.
    pub name: String,
    /// A full name they speak under that their address is made from, when their listed name is only their
    /// address: a name to find them by.
    pub alias: Option<String>,
}

/// How surely a label that is a full name the address is made from names a participant ([`names`]).
const FULL_NAME: u8 = 2;

/// Each participant, in order, as the units hear them.
pub fn heard<'a>(people: &[Participant], units: &'a [Unit]) -> Vec<Heard<'a>> {
    people
        .iter()
        .zip(attendance(people, units))
        .map(|(p, first)| {
            if named(p) {
                return Heard {
                    turn: first.map(|(u, _)| u),
                    name: p.name.trim().into(),
                    alias: None,
                };
            }
            let label = first
                .and_then(|(u, _)| u.speaker.as_deref())
                .map(str::trim)
                .filter(|l| name_words(l).is_some() && named_by(l, &local_part(p)));
            Heard {
                turn: first.map(|(u, _)| u),
                name: label.unwrap_or_default().into(),
                alias: first
                    .filter(|(_, n)| *n == FULL_NAME)
                    .and(label)
                    .map(Into::into),
            }
        })
        .collect()
}

/// The unit each participant first speaks in, if they speak, with how surely its label names them. A speaker's
/// label is the participants it names most surely ([`names`]). A label that names no one and is one capitalised
/// word ("Mark") is the participant whose name is only their address, still unheard, whose address starts with its
/// initial (mzokle@), when they are the only such participant and it the only such label with that initial.
fn attendance<'a>(people: &[Participant], units: &'a [Unit]) -> Vec<Option<(&'a Unit, u8)>> {
    let mut first: Vec<Option<(&Unit, u8)>> = vec![None; people.len()];
    let mut nobody: Vec<(&Unit, char)> = vec![];
    let mut seen = HashSet::new();
    for u in units {
        let Some(label) = u.speaker.as_deref().filter(|s| seen.insert(*s)) else {
            continue;
        };
        let sure: Vec<(usize, u8)> = people
            .iter()
            .enumerate()
            .filter_map(|(i, p)| names(label, p).map(|n| (i, n)))
            .collect();
        match sure.iter().map(|(_, n)| *n).max() {
            Some(best) => {
                for (i, n) in sure {
                    if n == best && first[i].is_none() {
                        first[i] = Some((u, n));
                    }
                }
            }
            None => nobody.extend(initial(label).map(|c| (u, c))),
        }
    }
    for &(u, c) in &nobody {
        let unheard: Vec<usize> = (0..people.len())
            .filter(|&i| {
                first[i].is_none() && !named(&people[i]) && local_part(&people[i]).starts_with(c)
            })
            .collect();
        if let [i] = unheard[..]
            && nobody.iter().filter(|(_, d)| *d == c).count() == 1
        {
            first[i] = Some((u, 1));
        }
    }
    first
}

/// How surely a speaker's label names a participant: 3 by their id, address or name; 1 by their first name. When
/// their name is only their address ([`named`]), also 2 by a full name the address is made from in a common way
/// (first.last, flast, firstl, last.first, lastf, first or last: "Scott Rosenbach" is srosenbach@), and 1 by a
/// first name the address starts with ("Mark" is mark.zokle@). None: not them.
fn names(label: &str, p: &Participant) -> Option<u8> {
    let label = label.trim().to_lowercase();
    if label.is_empty() {
        return None;
    }
    let name = p.name.trim().to_lowercase();
    if label == p.id.to_lowercase() || label == address(p) || label == name {
        return Some(3);
    }
    if !named(p)
        && let Some(words) = name_words(&label)
    {
        let local = local_part(p);
        match words.as_slice() {
            [w] if w.chars().count() >= 3 && local.starts_with(w.as_str()) => return Some(1),
            [f, .., l] => {
                let (f0, l0): (String, String) =
                    (f.chars().take(1).collect(), l.chars().take(1).collect());
                let shapes = [
                    format!("{f}{l}"),
                    format!("{f0}{l}"),
                    format!("{f}{l0}"),
                    format!("{l}{f}"),
                    format!("{l}{f0}"),
                ];
                if shapes.contains(&local) || local == *f || local == *l {
                    return Some(FULL_NAME);
                }
            }
            _ => {}
        }
    }
    let first_word = |s: &str| s.split_whitespace().next().map(str::to_owned);
    (first_word(&label) == first_word(&name)).then_some(1)
}

/// Whether a participant's name says more than their address ([`named_by`]). A participant with no address is
/// named.
fn named(p: &Participant) -> bool {
    named_by(&p.name, &local_part(p))
}

/// Whether a name says more than an address with this local part: not empty, not an address, and not the local
/// part, perhaps followed by a surname cut from it ("Srosenbach", "Mzokle Zokle" for srosenbach@ and mzokle@ say
/// nothing more). Any name says more than no address.
fn named_by(name: &str, local: &str) -> bool {
    if name.contains('@') {
        return false;
    }
    let words: Vec<String> = name
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(char::is_ascii_alphabetic)
                .map(|c| c.to_ascii_lowercase())
                .collect()
        })
        .filter(|w: &String| !w.is_empty())
        .collect();
    let Some((first, rest)) = words.split_first() else {
        return false;
    };
    local.is_empty() || first != local || !rest.iter().all(|w| local.ends_with(w.as_str()))
}

/// A participant's address, from their id (`contact:jo@acme.com`), in lower case; empty when it holds none.
fn address(p: &Participant) -> String {
    let a = p.id.split_once(':').map_or(p.id.as_str(), |(_, a)| a);
    if a.contains('@') {
        a.to_lowercase()
    } else {
        String::new()
    }
}

/// The letters of a participant's address before the `@` (`srosenbach`), in lower case; empty without one.
fn local_part(p: &Participant) -> String {
    address(p)
        .split_once('@')
        .map_or(String::new(), |(local, _)| {
            local.chars().filter(char::is_ascii_alphabetic).collect()
        })
}

/// A label's words in lower case, without punctuation; None for an address or a label with a digit in it (a phone
/// number, "Speaker 2").
fn name_words(label: &str) -> Option<Vec<String>> {
    if label.contains('@') || label.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let words: Vec<String> = label
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphabetic())
                .flat_map(char::to_lowercase)
                .collect()
        })
        .filter(|w: &String| !w.is_empty())
        .collect();
    (!words.is_empty()).then_some(words)
}

/// The initial, in lower case, of a label that is one capitalised word ("Mark").
fn initial(label: &str) -> Option<char> {
    let mut chars = label.trim().chars();
    let c = chars.next().filter(|c| c.is_uppercase())?;
    let rest: Vec<char> = chars.collect();
    (!rest.is_empty() && rest.iter().all(|c| c.is_lowercase()))
        .then(|| c.to_lowercase().next().unwrap_or(c))
}
