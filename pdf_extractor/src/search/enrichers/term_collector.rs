use tantivy::query::Query;
use crate::search::traits::ResultEnricher;
use crate::search::types::*;
use crate::search::errors::SearchError;

pub struct TermCollectorEnricher;

impl ResultEnricher for TermCollectorEnricher {
    fn enrich(
        &self,
        _ctx: &SearchContext,
        input: &SearchInput,
        _query: &dyn Query,
        results: &mut Vec<RichResult>,
    ) -> Result<(), SearchError> {
        let (phrase_groups, matched_terms) = match input.strategy {
            SearchStrategy::AutoPhrase => extract_auto_phrase_groups(&input.query_str),
            SearchStrategy::BooleanPhrase => extract_boolean_phrase_groups(&input.query_str),
        };

        if matched_terms.is_empty() {
            return Ok(());
        }

        for r in results.iter_mut() {
            r.matched_terms = matched_terms.clone();
            r.phrase_groups = phrase_groups.clone();
        }
        Ok(())
    }
}

/// Split a string respecting double-quoted groups.
/// Returns tokens with quotes stripped.
fn split_respecting_quotes(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        // skip whitespace
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() { chars.next(); } else { break; }
        }
        if chars.peek().is_none() { break; }

        if chars.peek() == Some(&'"') {
            // quoted group
            chars.next();
            let mut group = String::new();
            while let Some(c) = chars.next() {
                if c == '"' { break; }
                group.push(c);
            }
            let trimmed = group.trim().to_string();
            if !trimmed.is_empty() {
                tokens.push(trimmed);
            }
        } else {
            // bare word
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '"' { break; }
                word.push(c);
                chars.next();
            }
            if !word.is_empty() {
                tokens.push(word);
            }
        }
    }
    tokens
}

/// Extract phrase groups and matched terms for AutoPhrase strategy.
///
/// AutoPhrase always wraps bare multi-word queries in quotes, so the entire
/// query is treated as a single phrase (or multiple quoted phrases).
///
/// Examples:
///   "hello world"           → groups=[["hello","world"]]
///   "hello"                 → groups=[["hello"]]
///   "hello world" AND "signal processing"
///                           → groups=[[,"hello","world"],["and"],["signal","processing"]]
fn extract_auto_phrase_groups(query: &str) -> (Vec<Vec<String>>, Vec<String>) {
    let tokens = split_respecting_quotes(query);
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut flat: Vec<String> = Vec::new();

    for token in &tokens {
        let words: Vec<String> = token.split_whitespace()
            .map(|w| w.to_lowercase())
            .filter(|w| !w.is_empty())
            .collect();

        if let Some(last) = groups.last_mut() {
            last.extend(words.clone());
        } else {
            groups.push(words.clone());
        }

        for w in words {
            if !flat.contains(&w) {
                flat.push(w);
            }
        }
    }

    // If bare multiword (no quotes, contains space), group as single phrase
    if groups.is_empty() && query.contains(' ') && !query.contains('"') {
        let words: Vec<String> = query.split_whitespace()
            .map(|w| w.to_lowercase())
            .filter(|w| !w.is_empty())
            .collect();
        if words.len() > 1 {
            groups.push(words.clone());
            for w in &words {
                if !flat.contains(w) {
                    flat.push(w.clone());
                }
            }
        }
    }

    (groups, flat)
}

/// Extract phrase groups and matched terms for BooleanPhrase strategy.
///
/// BooleanPhrase supports:
///   - Quoted phrases: "machine learning"
///   - Boolean operators: AND, OR, NOT (case-insensitive)
///   - Parentheses: (hello OR world)
///   - +/- syntax: +hello -world
///
/// Operators, parentheses, and +/- are filtered out. Quoted multi-word tokens
/// form groups. Bare words form single-term groups.
///
/// Examples:
///   "hello"                             → groups=[["hello"]], flat=["hello"]
///   "hello AND world"                   → groups=[["hello"],["world"]], flat=["hello","world"]
///   "hello world"                       → groups=[["hello","world"]], flat=["hello","world"]
///   "\"machine learning\" AND \"signal processing\""
///                                       → groups=[["machine","learning"],["signal","processing"]]
///                                       → flat=["machine","learning","signal","processing"]
///   "(hello OR world) -foo"            → groups=[["hello"],["world"],["foo"]]
///                                       → flat=["hello","world","foo"]
fn is_bare_multiword(s: &str) -> bool {
    let t = s.trim();
    if !t.contains(' ') { return false; }
    if t.contains('"') || t.contains('(') || t.contains(')')
        || t.contains('+') || t.contains('-') { return false; }
    !t.split_whitespace().any(|w| {
        w.eq_ignore_ascii_case("AND") || w.eq_ignore_ascii_case("OR") || w.eq_ignore_ascii_case("NOT")
    })
}

fn extract_boolean_phrase_groups(query: &str) -> (Vec<Vec<String>>, Vec<String>) {
    // Bare multiword without operators → treat as single phrase group (auto-phrased)
    if is_bare_multiword(query) {
        let words: Vec<String> = query.split_whitespace()
            .map(|w| w.to_lowercase())
            .filter(|w| !w.is_empty())
            .collect();
        let mut flat: Vec<String> = Vec::new();
        for w in &words {
            if !flat.contains(w) {
                flat.push(w.clone());
            }
        }
        return (vec![words], flat);
    }

    let tokens = split_respecting_quotes(query);
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut flat: Vec<String> = Vec::new();

    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];

        // Skip boolean operators
        let lower = token.to_lowercase();
        if lower == "and" || lower == "or" || lower == "not" {
            i += 1;
            continue;
        }

        // Skip parentheses and +/- modifiers (standalone tokens)
        if token == "(" || token == ")" || token == "+" || token == "-" {
            i += 1;
            continue;
        }

        // Token may have leading +/- stripped already by split_respecting_quotes
        // But bare words like "-foo" need to be handled
        let clean_token = token.trim_start_matches(|c| c == '+' || c == '-');
        if clean_token.is_empty() {
            i += 1;
            continue;
        }

        // Split into words, filter empty
        let words: Vec<String> = clean_token.split_whitespace()
            .map(|w| w.trim_matches(|c: char| c.is_ascii_punctuation() && c != '\''))

            .map(|w| w.to_lowercase())
            .filter(|w| !w.is_empty())
            .collect();

        if !words.is_empty() {
            // Quote-respecting split already merged quoted groups.
            // If the original token had spaces (it was quoted), keep as one group.
            let had_spaces = token.contains(char::is_whitespace);
            if had_spaces && words.len() > 1 {
                groups.push(words.clone());
            } else {
                // Each word is its own group
                for w in &words {
                    groups.push(vec![w.clone()]);
                }
            }

            for w in &words {
                if !flat.contains(w) {
                    flat.push(w.clone());
                }
            }
        }

        i += 1;
    }

    // Fallback: if no groups but query has content, treat bare multiword as single phrase
    if groups.is_empty() && !query.is_empty() {
        let words: Vec<String> = query.split_whitespace()
            .map(|w| w.trim_matches(|c: char| c.is_ascii_punctuation() && c != '\''))
            .map(|w| w.to_lowercase())
            .filter(|w| !w.is_empty())
            .filter(|w| !matches!(w.as_str(), "and" | "or" | "not" | "(" | ")" | "+" | "-"))
            .collect();
        if words.len() > 1 {
            groups.push(words.clone());
            for w in &words {
                if !flat.contains(w) {
                    flat.push(w.clone());
                }
            }
        } else if words.len() == 1 {
            groups.push(words.clone());
            flat = words;
        }
    }

    (groups, flat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_phrase_single_word() {
        let (groups, flat) = extract_auto_phrase_groups("hello");
        assert_eq!(groups, vec![vec!["hello"]]);
        assert_eq!(flat, vec!["hello"]);
    }

    #[test]
    fn test_auto_phrase_bare_multiword() {
        let (groups, flat) = extract_auto_phrase_groups("hello world");
        assert_eq!(groups, vec![vec!["hello", "world"]]);
        assert_eq!(flat, vec!["hello", "world"]);
    }

    #[test]
    fn test_auto_phrase_quoted() {
        let (groups, flat) = extract_auto_phrase_groups("\"hello world\"");
        assert_eq!(groups, vec![vec!["hello", "world"]]);
        assert_eq!(flat, vec!["hello", "world"]);
    }

    #[test]
    fn test_boolean_single_word() {
        let (groups, flat) = extract_boolean_phrase_groups("hello");
        assert_eq!(groups, vec![vec!["hello"]]);
        assert_eq!(flat, vec!["hello"]);
    }

    #[test]
    fn test_boolean_and_operator() {
        let (groups, flat) = extract_boolean_phrase_groups("hello AND world");
        assert_eq!(groups, vec![vec!["hello"], vec!["world"]]);
        assert_eq!(flat, vec!["hello", "world"]);
    }

    #[test]
    fn test_boolean_quoted_phrases() {
        let input = "\"machine learning\" AND \"signal processing\"";
        let (groups, flat) = extract_boolean_phrase_groups(input);
        assert_eq!(groups, vec![
            vec!["machine", "learning"],
            vec!["signal", "processing"],
        ]);
        assert_eq!(flat, vec!["machine", "learning", "signal", "processing"]);
    }

    #[test]
    fn test_boolean_bare_multiword_no_operators() {
        let (groups, flat) = extract_boolean_phrase_groups("hello world");
        assert_eq!(groups, vec![vec!["hello", "world"]]);
        assert_eq!(flat, vec!["hello", "world"]);
    }

    #[test]
    fn test_boolean_parentheses_and_or() {
        let input = "(hello OR world) -foo";
        let (groups, flat) = extract_boolean_phrase_groups(input);
        assert_eq!(groups, vec![vec!["hello"], vec!["world"], vec!["foo"]]);
        assert_eq!(flat, vec!["hello", "world", "foo"]);
    }
}
