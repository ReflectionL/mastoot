//! Mastodon's Link-header cursor pagination.
//!
//! Every list endpoint carries pagination cursors in the HTTP `Link`
//! response header, not in the JSON body. Example:
//!
//! ```text
//! Link: <https://…?max_id=109>; rel="next", <https://…?since_id=115>; rel="prev"
//! ```
//!
//! Callers paginate by taking `max_id` (older) or `min_id` / `since_id`
//! (newer) from the parsed [`Page`] and passing them back as request
//! params.

use url::Url;

/// A page of results plus the cursors needed to fetch adjacent pages.
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: T,
    pub next: Option<Cursor>,
    pub prev: Option<Cursor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub max_id: Option<String>,
    pub since_id: Option<String>,
    pub min_id: Option<String>,
}

impl Cursor {
    /// Render into query params for a subsequent request.
    #[must_use]
    pub fn as_query(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(v) = &self.max_id {
            out.push(("max_id", v.clone()));
        }
        if let Some(v) = &self.since_id {
            out.push(("since_id", v.clone()));
        }
        if let Some(v) = &self.min_id {
            out.push(("min_id", v.clone()));
        }
        out
    }
}

/// Parse a `Link:` header value into next/prev cursors.
#[must_use]
pub fn parse_link_header(value: &str) -> (Option<Cursor>, Option<Cursor>) {
    let mut next = None;
    let mut prev = None;
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Shape: "<URL>; rel=\"next\""
        let Some(rel_start) = part.find("rel=") else {
            continue;
        };
        let rel = part[rel_start + 4..]
            .trim_matches('"')
            .trim_end_matches(';')
            .trim_matches('"');
        let Some(url_start) = part.find('<') else {
            continue;
        };
        let Some(url_end) = part.find('>') else {
            continue;
        };
        if url_end <= url_start {
            continue;
        }
        let url_str = &part[url_start + 1..url_end];
        let Ok(url) = Url::parse(url_str) else {
            continue;
        };
        let cursor = cursor_from_url(&url);
        match rel {
            "next" => next = Some(cursor),
            "prev" => prev = Some(cursor),
            _ => {}
        }
    }
    (next, prev)
}

fn cursor_from_url(url: &Url) -> Cursor {
    let mut cursor = Cursor {
        max_id: None,
        since_id: None,
        min_id: None,
    };
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "max_id" => cursor.max_id = Some(v.into_owned()),
            "since_id" => cursor.since_id = Some(v.into_owned()),
            "min_id" => cursor.min_id = Some(v.into_owned()),
            _ => {}
        }
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_link_header() {
        let h = "<https://ex.com/api/v1/timelines/home?max_id=109>; rel=\"next\", \
                 <https://ex.com/api/v1/timelines/home?min_id=115>; rel=\"prev\"";
        let (next, prev) = parse_link_header(h);
        assert_eq!(next.unwrap().max_id.as_deref(), Some("109"));
        assert_eq!(prev.unwrap().min_id.as_deref(), Some("115"));
    }

    #[test]
    fn empty_header_yields_nothing() {
        let (next, prev) = parse_link_header("");
        assert!(next.is_none() && prev.is_none());
    }
}
