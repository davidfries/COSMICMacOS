// SPDX-License-Identifier: GPL-3.0-or-later
//! Query matching and ranking shared by the apps and windows plugins.
//!
//! Matching is case-insensitive. A candidate matches in one of four tiers,
//! best first:
//!
//! 1. [`Tier::Prefix`]: the name starts with the query ("saf" → "Safari").
//! 2. [`Tier::WordPrefix`]: a word inside the name starts with the query
//!    ("mon" → "Activity Monitor", "edit" → "TextEdit"), or every word of a
//!    multi-word query starts a word of the name, in order
//!    ("act mon" → "Activity Monitor").
//! 3. [`Tier::Substring`]: the query occurs anywhere ("fari" → "Safari").
//! 4. [`Tier::Fuzzy`]: the query's characters occur in order (queries of at
//!    least [`FUZZY_MIN_CHARS`] characters only, so short queries stay
//!    precise).
//!
//! Within a tier, earlier match positions, then shorter names, rank higher;
//! [`Rank`]'s `Ord` encodes that, lower is better.

use std::cmp::Ordering;

/// Shortest query (in characters) that may match as a subsequence.
pub const FUZZY_MIN_CHARS: usize = 3;

/// Match quality tier, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// Results that are not name matches at all (calculator, web search).
    /// They sort before every name match.
    Exact,
    Prefix,
    WordPrefix,
    Substring,
    Fuzzy,
}

/// How well a candidate matched; lower sorts first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rank {
    pub tier: Tier,
    /// Character index of the match start within the candidate.
    pub position: usize,
    /// Candidate length in characters (shorter wins ties).
    pub length: usize,
}

impl Rank {
    /// Rank for results that are not name matches (calculator, web search).
    pub const EXACT: Rank = Rank {
        tier: Tier::Exact,
        position: 0,
        length: 0,
    };
}

impl Ord for Rank {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.tier, self.position, self.length).cmp(&(other.tier, other.position, other.length))
    }
}

impl PartialOrd for Rank {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Matches `query` against `candidate`. Returns `None` for no match or an
/// empty (whitespace-only) query.
pub fn rank(candidate: &str, query: &str) -> Option<Rank> {
    let query: Vec<char> = query.trim().to_lowercase().chars().collect();
    if query.is_empty() {
        return None;
    }
    let original: Vec<char> = candidate.chars().collect();
    let lowered: Vec<char> = candidate.to_lowercase().chars().collect();
    // `to_lowercase` can change the length for a few characters (e.g. 'İ');
    // word boundaries need the original casing, so fall back to the lowered
    // text without camel-case boundaries in that rare case.
    let starts = if original.len() == lowered.len() {
        word_starts(&original)
    } else {
        word_starts(&lowered)
    };
    let length = lowered.len();
    let make = |tier, position| Rank {
        tier,
        position,
        length,
    };

    if lowered.starts_with(&query) {
        return Some(make(Tier::Prefix, 0));
    }
    if let Some(&start) = starts
        .iter()
        .find(|&&start| lowered[start..].starts_with(&query))
    {
        return Some(make(Tier::WordPrefix, start));
    }
    if let Some(position) = words_in_order(&lowered, &starts, &query) {
        return Some(make(Tier::WordPrefix, position));
    }
    if let Some(position) = find(&lowered, &query) {
        return Some(make(Tier::Substring, position));
    }
    if query.len() >= FUZZY_MIN_CHARS
        && let Some(position) = subsequence(&lowered, &query)
    {
        return Some(make(Tier::Fuzzy, position));
    }
    None
}

/// Best rank of `query` over several candidate strings (name plus keywords).
pub fn best_rank<'a>(candidates: impl IntoIterator<Item = &'a str>, query: &str) -> Option<Rank> {
    candidates
        .into_iter()
        .filter_map(|candidate| rank(candidate, query))
        .min()
}

/// Character indices where a word starts: index 0, after a non-alphanumeric
/// separator, at a lower→upper camel-case transition, and at a letter↔digit
/// transition.
fn word_starts(chars: &[char]) -> Vec<usize> {
    let mut starts = Vec::new();
    for (index, &current) in chars.iter().enumerate() {
        if !current.is_alphanumeric() {
            continue;
        }
        let starts_word = match index.checked_sub(1).map(|previous| chars[previous]) {
            None => true,
            Some(previous) => {
                !previous.is_alphanumeric()
                    || (previous.is_lowercase() && current.is_uppercase())
                    || (previous.is_alphabetic() && current.is_numeric())
                    || (previous.is_numeric() && current.is_alphabetic())
            }
        };
        if starts_word {
            starts.push(index);
        }
    }
    starts
}

/// Multi-word queries: every query word must prefix a distinct word of the
/// candidate, in order. Returns the position of the first matched word.
fn words_in_order(lowered: &[char], starts: &[usize], query: &[char]) -> Option<usize> {
    let words: Vec<&[char]> = query
        .split(|c| c.is_whitespace())
        .filter(|word| !word.is_empty())
        .collect();
    if words.len() < 2 {
        return None;
    }
    let mut remaining = starts.iter();
    let mut first = None;
    for word in words {
        let start = remaining.find(|&&start| lowered[start..].starts_with(word))?;
        first.get_or_insert(*start);
    }
    first
}

fn find(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Position of the first character of an in-order subsequence match.
fn subsequence(haystack: &[char], needle: &[char]) -> Option<usize> {
    let needle: Vec<char> = needle
        .iter()
        .copied()
        .filter(|c| !c.is_whitespace())
        .collect();
    let mut wanted = needle.iter().peekable();
    let mut first = None;
    for (index, c) in haystack.iter().enumerate() {
        if wanted.peek() == Some(&c) {
            first.get_or_insert(index);
            wanted.next();
        }
        if wanted.peek().is_none() {
            return first;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier(candidate: &str, query: &str) -> Option<Tier> {
        rank(candidate, query).map(|rank| rank.tier)
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert_eq!(rank("Safari", ""), None);
        assert_eq!(rank("Safari", "   "), None);
    }

    #[test]
    fn prefix_is_case_insensitive() {
        assert_eq!(tier("Safari", "saf"), Some(Tier::Prefix));
        assert_eq!(tier("Safari", "SAFARI"), Some(Tier::Prefix));
        assert_eq!(tier("safari", "Saf"), Some(Tier::Prefix));
        // Leading/trailing whitespace in the query is ignored.
        assert_eq!(tier("Safari", " saf "), Some(Tier::Prefix));
    }

    #[test]
    fn word_prefix_after_separators_and_camel_case() {
        assert_eq!(tier("Activity Monitor", "mon"), Some(Tier::WordPrefix));
        assert_eq!(tier("TextEdit", "edit"), Some(Tier::WordPrefix));
        assert_eq!(tier("cosmic-mac-ctl", "mac"), Some(Tier::WordPrefix));
        assert_eq!(tier("Xcode15", "15"), Some(Tier::WordPrefix));
        assert_eq!(
            rank("Activity Monitor", "mon").unwrap().position,
            "Activity ".len()
        );
    }

    #[test]
    fn multi_word_query_matches_word_prefixes_in_order() {
        assert_eq!(tier("Activity Monitor", "act mon"), Some(Tier::WordPrefix));
        assert_eq!(
            tier("System Information", "sys inf"),
            Some(Tier::WordPrefix)
        );
        // Out of order is not a word-prefix match (and not a substring).
        assert_ne!(tier("Activity Monitor", "mon act"), Some(Tier::WordPrefix));
    }

    #[test]
    fn substring_and_fuzzy() {
        assert_eq!(tier("Safari", "fari"), Some(Tier::Substring));
        assert_eq!(tier("Terminal", "rmn"), Some(Tier::Fuzzy));
        // Fuzzy needs at least FUZZY_MIN_CHARS characters.
        assert_eq!(tier("Terminal", "tl"), None);
        assert_eq!(tier("Safari", "xyz"), None);
        assert_eq!(tier("Safari", "safarix"), None);
    }

    #[test]
    fn ranking_order_is_prefix_word_prefix_substring_fuzzy() {
        let prefix = rank("Notes", "no").unwrap();
        let word = rank("Font Book Notes", "no").unwrap();
        let substring = rank("Piano", "no").unwrap();
        let fuzzy = rank("Network Utility", "nwu").unwrap();
        assert!(prefix < word);
        assert!(word < substring);
        assert_eq!(fuzzy.tier, Tier::Fuzzy);
        assert!(substring < fuzzy);
        assert!(Rank::EXACT < prefix);
    }

    #[test]
    fn ties_prefer_earlier_then_shorter() {
        let short = rank("Mail", "ma").unwrap();
        let long = rank("Maps Helper", "ma").unwrap();
        assert!(short < long);
        let early = rank("A Mail", "mail").unwrap();
        let late = rank("Big Mail", "mail").unwrap();
        assert!(early < late);
    }

    #[test]
    fn best_rank_uses_the_best_candidate() {
        let rank = best_rank(["Code", "Visual Studio Code"], "visual").unwrap();
        assert_eq!(rank.tier, Tier::Prefix);
        assert_eq!(best_rank(["Code", "Visual Studio Code"], "zzz"), None);
    }

    #[test]
    fn non_ascii_names_do_not_panic() {
        assert_eq!(tier("Systemeinstellungen", "einst"), Some(Tier::Substring));
        assert_eq!(tier("Ünïcödé App", "app"), Some(Tier::WordPrefix));
        assert!(rank("İstanbul", "stan").is_some());
    }
}
