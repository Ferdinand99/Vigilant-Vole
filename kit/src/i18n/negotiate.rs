//! Choosing a language from `Accept-Language`.

use axum::http::{HeaderMap, header};

use super::Lang;

/// The best supported language from `Accept-Language` (highest q first).
pub fn negotiate(headers: &HeaderMap) -> Option<Lang> {
    let value = headers.get(header::ACCEPT_LANGUAGE)?.to_str().ok()?;
    parse_accept_language(value)
}

pub fn parse_accept_language(value: &str) -> Option<Lang> {
    let mut ranges: Vec<(f32, usize, &str)> = value
        .split(',')
        .enumerate()
        .filter_map(|(i, part)| {
            let mut pieces = part.split(';');
            let tag = pieces.next()?.trim();
            let q = pieces
                .find_map(|p| p.trim().strip_prefix("q="))
                .map_or(Some(1.0), |q| q.trim().parse::<f32>().ok())?;
            (!tag.is_empty() && q > 0.0).then_some((q, i, tag))
        })
        .collect();
    // Highest q first; ties keep header order.
    ranges.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    ranges
        .into_iter()
        .find_map(|(_, _, tag)| Lang::from_code(tag))
}
