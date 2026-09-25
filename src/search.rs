//! Literal search returns byte ranges into the original text, even when Unicode
//! lowercase conversion expands a character. Memory grows with the query only.
use std::collections::VecDeque;

pub fn find_match(
    text: &str,
    query: &str,
    match_case: bool,
    start: usize,
    backwards: bool,
) -> Option<(usize, usize)> {
    if query.is_empty() {
        return None;
    }
    let mut first = None;
    let mut last = None;
    let mut previous = None;
    let mut next = None;
    let mut visit = |range: (usize, usize)| {
        first.get_or_insert(range);
        last = Some(range);
        if backwards {
            if range.0 < start {
                previous = Some(range);
            }
        } else if range.0 >= start {
            next = Some(range);
            return false;
        }
        true
    };
    if match_case {
        for (offset, value) in text.match_indices(query) {
            if !visit((offset, offset + value.len())) {
                break;
            }
        }
    } else {
        folded_matches(text, query, &mut visit);
    }
    if backwards {
        previous.or(last)
    } else {
        next.or(first)
    }
}

fn folded_matches(text: &str, query: &str, visit: &mut impl FnMut((usize, usize)) -> bool) {
    let pattern: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    // Prefix lengths let a partial match restart without rescanning the document.
    let mut prefixes = vec![0; pattern.len()];
    let mut matched = 0;
    for i in 1..pattern.len() {
        while matched > 0 && pattern[i] != pattern[matched] {
            matched = prefixes[matched - 1];
        }
        if pattern[i] == pattern[matched] {
            matched += 1;
        }
        prefixes[i] = matched;
    }
    matched = 0;
    let mut origins = VecDeque::with_capacity(pattern.len());
    for (offset, original) in text.char_indices() {
        let lower = original.to_lowercase();
        let count = lower.clone().count();
        for (index, ch) in lower.enumerate() {
            if origins.len() == pattern.len() {
                origins.pop_front();
            }
            origins.push_back((offset, offset + original.len_utf8(), index == 0));
            while matched > 0 && ch != pattern[matched] {
                matched = prefixes[matched - 1];
            }
            if ch == pattern[matched] {
                matched += 1;
            }
            if matched == pattern.len() {
                let &(begin, _, begins_character) = origins.front().unwrap();
                let &(_, end, _) = origins.back().unwrap();
                // Never expose a partial lowercase expansion as a document range.
                if begins_character && index + 1 == count {
                    if !visit((begin, end)) {
                        return;
                    }
                    matched = 0; // Literal matches, like str::match_indices, do not overlap.
                } else {
                    matched = prefixes[matched - 1];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_offsets_survive_expansion_and_contraction() {
        assert_eq!(find_match("İ A", "a", false, 0, false), Some((3, 4)));
        assert_eq!(find_match("K A", "a", false, 0, false), Some((4, 5)));
        assert_eq!(find_match("K", "k", false, 0, false), Some((0, 3)));
        assert_eq!(find_match("İ", "i\u{307}", false, 0, false), Some((0, 2)));
        assert_eq!(find_match("İ", "i", false, 0, false), None);
    }

    #[test]
    fn repeated_prefixes_and_wraparound_use_original_ranges() {
        assert_eq!(find_match("aaaab", "aab", false, 0, false), Some((2, 5)));
        assert_eq!(find_match("İ a a", "a", false, 5, true), Some((3, 4)));
        assert_eq!(find_match("İ a a", "a", false, 0, true), Some((5, 6)));
        assert_eq!(find_match("İ a a", "a", false, 6, false), Some((3, 4)));
    }

    #[test]
    fn query_is_literal_and_sensitive_search_is_exact() {
        assert_eq!(find_match("a.b a*b", ".", false, 0, false), Some((1, 2)));
        assert_eq!(find_match("É é", "é", true, 0, false), Some((3, 5)));
        assert_eq!(find_match("abc", "", false, 0, false), None);
    }
}
