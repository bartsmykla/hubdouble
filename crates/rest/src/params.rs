use std::borrow::Cow;

use percent_encoding::percent_decode_str;

/// Decoded path parameters, in path order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params(Vec<(Cow<'static, str>, String)>);

impl Params {
    #[must_use]
    pub fn new(pairs: Vec<(Cow<'static, str>, String)>) -> Params {
        Params(pairs)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(n, v)| (n.as_ref(), v.as_str()))
    }
}

/// Decoded query parameters, in request order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query(Vec<(String, String)>);

impl Query {
    /// Parses `a=1&b=x+y`; `+` is a space, and a pair that is not UTF-8 after decoding is dropped.
    #[must_use]
    pub fn parse(query: &str) -> Query {
        let decode = |s: &str| {
            let spaced = s.replace('+', " ");
            percent_decode_str(&spaced)
                .decode_utf8()
                .ok()
                .map(Cow::into_owned)
        };
        Query(
            query
                .split('&')
                .filter(|p| !p.is_empty())
                .filter_map(|pair| {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    Some((decode(k)?, decode(v)?))
                })
                .collect(),
        )
    }

    /// The last value of `name`, as GitHub reads a repeated parameter.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .rfind(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every value of `name`, for repeated parameters such as `labels[]`.
    pub fn all<'q>(&'q self, name: &'q str) -> impl Iterator<Item = &'q str> {
        self.0
            .iter()
            .filter(move |(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }
}
