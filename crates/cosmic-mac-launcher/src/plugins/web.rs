// SPDX-License-Identifier: GPL-3.0-or-later
//! Web search plugin: `g <terms>` searches Google, `ddg <terms>` searches
//! DuckDuckGo (pop-launcher's web plugin uses the same prefixes). The
//! prefix owns the query once search terms follow it.

use crate::matching::Rank;
use crate::plugin::{Action, IconSource, Indice, Plugin, SearchResult};

/// A search engine selected by a query prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Engine {
    /// Query prefix including the trailing space.
    pub prefix: &'static str,
    pub name: &'static str,
    /// URL the percent-encoded terms are appended to.
    pub url: &'static str,
}

pub const ENGINES: &[Engine] = &[
    Engine {
        prefix: "g ",
        name: "Google",
        url: "https://www.google.com/search?q=",
    },
    Engine {
        prefix: "ddg ",
        name: "DuckDuckGo",
        url: "https://duckduckgo.com/?q=",
    },
];

/// Splits `query` into its engine and the (trimmed, non-empty) terms.
pub fn parse(query: &str) -> Option<(&'static Engine, &str)> {
    let query = query.trim_start();
    ENGINES.iter().find_map(|engine| {
        let terms = strip_prefix_ignore_case(query, engine.prefix)?.trim();
        (!terms.is_empty()).then_some((engine, terms))
    })
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// The search URL for `terms`.
pub fn search_url(engine: &Engine, terms: &str) -> String {
    format!("{}{}", engine.url, percent_encode(terms))
}

/// Percent-encodes everything except RFC 3986 unreserved characters, so the
/// result is safe as a query-string value (spaces become `%20`).
pub fn percent_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[derive(Default)]
pub struct WebPlugin {
    url: Option<String>,
}

impl Plugin for WebPlugin {
    fn name(&self) -> &'static str {
        "web"
    }

    fn isolates(&self, query: &str) -> bool {
        parse(query).is_some()
    }

    fn search(&mut self, query: &str) -> Vec<SearchResult> {
        self.url = None;
        let Some((engine, terms)) = parse(query) else {
            return Vec::new();
        };
        let url = search_url(engine, terms);
        self.url = Some(url.clone());
        vec![SearchResult {
            id: 0,
            name: format!("Search {} for “{terms}”", engine.name),
            description: url,
            icon: Some(IconSource::Name("system-search-symbolic")),
            window: None,
            rank: Rank::EXACT,
        }]
    }

    fn activate(&mut self, id: Indice) -> Option<Action> {
        (id == 0)
            .then(|| self.url.clone().map(Action::OpenUrl))
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prefixes() {
        let (engine, terms) = parse("g rust lang").unwrap();
        assert_eq!(engine.name, "Google");
        assert_eq!(terms, "rust lang");
        let (engine, terms) = parse("  DDG   cosmic  ").unwrap();
        assert_eq!(engine.name, "DuckDuckGo");
        assert_eq!(terms, "cosmic");
        // No terms, no prefix, or a word that merely starts with "g".
        assert!(parse("g ").is_none());
        assert!(parse("g    ").is_none());
        assert!(parse("garageband").is_none());
        assert!(parse("ddgx foo").is_none());
        assert!(parse("").is_none());
        // Multi-byte input right after a short prefix does not panic.
        assert!(parse("ü").is_none());
        assert!(parse("dü foo").is_none());
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(percent_encode("rust lang"), "rust%20lang");
        assert_eq!(percent_encode("a&b=c?d#e+f/g"), "a%26b%3Dc%3Fd%23e%2Bf%2Fg");
        assert_eq!(percent_encode("~-._AZaz09"), "~-._AZaz09");
        assert_eq!(percent_encode("über"), "%C3%BCber");
        assert_eq!(percent_encode("100%"), "100%25");
    }

    #[test]
    fn builds_urls() {
        let (engine, terms) = parse("g what is 2+2?").unwrap();
        assert_eq!(
            search_url(engine, terms),
            "https://www.google.com/search?q=what%20is%202%2B2%3F"
        );
        let (engine, terms) = parse("ddg cosmic desktop").unwrap();
        assert_eq!(
            search_url(engine, terms),
            "https://duckduckgo.com/?q=cosmic%20desktop"
        );
    }

    #[test]
    fn plugin_isolates_and_activates() {
        let mut web = WebPlugin::default();
        assert!(web.isolates("g rust"));
        assert!(!web.isolates("g "));
        assert!(!web.isolates("safari"));

        let results = web.search("g rust");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Search Google for “rust”");
        assert_eq!(
            web.activate(0),
            Some(Action::OpenUrl(
                "https://www.google.com/search?q=rust".into()
            ))
        );
        assert_eq!(web.activate(1), None);

        assert!(web.search("safari").is_empty());
        assert_eq!(web.activate(0), None);
    }
}
