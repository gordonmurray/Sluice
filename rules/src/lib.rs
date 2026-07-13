//! Route/caller → price policy for the sluice gateway.
//!
//! A [`RuleSet`] is loaded from a JSON table and answers one question:
//! given a request path and an optional caller id, is the request free,
//! paid (and for how much), or denied?
//!
//! This module is deliberately ignorant of x402, chains, and assets: prices
//! are plain micro-USDC amounts (6 decimals, the token's atomic unit) and the
//! gateway translates them into payment requirements. Matching is
//! longest-prefix-wins on the raw path bytes — no decoding, no
//! normalisation (the gateway rejects percent-encoded paths before they
//! reach this crate); anything unmatched is denied. `/firn` and `/firn/`
//! are distinct paths but both match a `/firn` prefix.
//!
//! ```json
//! {
//!   "rules": [
//!     { "prefix": "/firn/metrics", "pricing": "free" },
//!     { "prefix": "/firn/health",  "price_usdc": "0.01",
//!       "caller_prices": { "tenant-a": "0.002" } }
//!   ]
//! }
//! ```

use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;

/// Six decimals: 1 USDC = 1_000_000 micro-USDC.
const USDC_DECIMALS: u32 = 6;

/// What the gateway should do with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Forward without payment.
    Free,
    /// Demand payment of this many micro-USDC before forwarding.
    Paid { micro_usdc: u64 },
    /// Not covered by any rule: do not forward.
    Deny,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RuleError {
    /// The JSON itself failed to parse.
    Json(String),
    /// A price string was not a valid USDC decimal amount.
    BadPrice { prefix: String, price: String },
    /// A rule has neither `pricing: "free"` nor a `price_usdc`.
    NoPricing { prefix: String },
    /// A rule has both `pricing: "free"` and a `price_usdc` — ambiguous.
    ConflictingPricing { prefix: String },
    /// `pricing` is set to something other than `"free"`.
    UnknownPricing { prefix: String, pricing: String },
    /// `caller_prices` on a free rule is ambiguous config; rejected.
    CallerPricesOnFree { prefix: String },
    /// Prefixes must be absolute paths (start with `/`).
    BadPrefix { prefix: String },
    /// Two rules share the same prefix; matching would be ambiguous.
    DuplicatePrefix { prefix: String },
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleError::Json(e) => write!(f, "rules config is not valid JSON: {e}"),
            RuleError::BadPrice { prefix, price } => {
                write!(f, "rule {prefix:?}: {price:?} is not a valid USDC amount")
            }
            RuleError::NoPricing { prefix } => {
                write!(
                    f,
                    "rule {prefix:?} needs either pricing=\"free\" or price_usdc"
                )
            }
            RuleError::ConflictingPricing { prefix } => {
                write!(
                    f,
                    "rule {prefix:?} has both pricing=\"free\" and price_usdc"
                )
            }
            RuleError::UnknownPricing { prefix, pricing } => {
                write!(
                    f,
                    "rule {prefix:?}: unknown pricing {pricing:?} (only \"free\" is valid)"
                )
            }
            RuleError::CallerPricesOnFree { prefix } => {
                write!(
                    f,
                    "rule {prefix:?} is free but sets caller_prices; remove one"
                )
            }
            RuleError::BadPrefix { prefix } => {
                write!(
                    f,
                    "rule prefix {prefix:?} must be an absolute path starting with '/'"
                )
            }
            RuleError::DuplicatePrefix { prefix } => {
                write!(f, "duplicate rule prefix {prefix:?}")
            }
        }
    }
}

impl std::error::Error for RuleError {}

#[derive(Deserialize)]
struct RawConfig {
    rules: Vec<RawRule>,
}

#[derive(Deserialize)]
struct RawRule {
    prefix: String,
    #[serde(default)]
    pricing: Option<String>, // only "free" is meaningful
    #[serde(default)]
    price_usdc: Option<String>,
    #[serde(default)]
    caller_prices: HashMap<String, String>,
}

#[derive(Debug)]
struct Rule {
    prefix: String,
    base: Decision,
    caller_prices: HashMap<String, u64>,
}

/// A compiled, immutable price policy table.
#[derive(Debug)]
pub struct RuleSet {
    /// Sorted by descending prefix length so the first match wins.
    rules: Vec<Rule>,
}

impl RuleSet {
    pub fn from_json(json: &str) -> Result<Self, RuleError> {
        let raw: RawConfig =
            serde_json::from_str(json).map_err(|e| RuleError::Json(e.to_string()))?;

        let mut rules = Vec::with_capacity(raw.rules.len());
        for r in raw.rules {
            if !r.prefix.starts_with('/') {
                return Err(RuleError::BadPrefix { prefix: r.prefix });
            }
            if rules
                .iter()
                .any(|existing: &Rule| existing.prefix == r.prefix)
            {
                return Err(RuleError::DuplicatePrefix { prefix: r.prefix });
            }
            let base = match (r.pricing.as_deref(), &r.price_usdc) {
                (Some("free"), Some(_)) => {
                    return Err(RuleError::ConflictingPricing { prefix: r.prefix });
                }
                (Some("free"), None) => {
                    if !r.caller_prices.is_empty() {
                        return Err(RuleError::CallerPricesOnFree { prefix: r.prefix });
                    }
                    Decision::Free
                }
                (Some(other), _) => {
                    return Err(RuleError::UnknownPricing {
                        prefix: r.prefix,
                        pricing: other.to_string(),
                    });
                }
                (None, Some(price)) => Decision::Paid {
                    micro_usdc: parse_usdc(price).ok_or_else(|| RuleError::BadPrice {
                        prefix: r.prefix.clone(),
                        price: price.clone(),
                    })?,
                },
                (None, None) => return Err(RuleError::NoPricing { prefix: r.prefix }),
            };
            let mut caller_prices = HashMap::new();
            for (caller, price) in r.caller_prices {
                let micro = parse_usdc(&price).ok_or_else(|| RuleError::BadPrice {
                    prefix: r.prefix.clone(),
                    price: price.clone(),
                })?;
                caller_prices.insert(caller, micro);
            }
            rules.push(Rule {
                prefix: r.prefix,
                base,
                caller_prices,
            });
        }
        rules.sort_by_key(|b| std::cmp::Reverse(b.prefix.len()));
        Ok(RuleSet { rules })
    }

    /// Longest-prefix match; per-caller price overrides the rule's base price.
    /// Unmatched paths are denied.
    pub fn decide(&self, path: &str, caller: Option<&str>) -> Decision {
        for rule in &self.rules {
            if !path_has_prefix(path, &rule.prefix) {
                continue;
            }
            if let Some(caller) = caller
                && let Some(&micro_usdc) = rule.caller_prices.get(caller)
            {
                return Decision::Paid { micro_usdc };
            }
            return rule.base;
        }
        Decision::Deny
    }
}

/// Prefix match on whole path segments: `/firn` matches `/firn` and
/// `/firn/health` but not `/firnabc`.
fn path_has_prefix(path: &str, prefix: &str) -> bool {
    match path.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with('/') || prefix.ends_with('/'),
        None => false,
    }
}

/// Parse a decimal USDC amount ("0.01", "1", "0.000001") into micro-USDC
/// without going through floats. Rejects empty, negative, malformed, and
/// finer-than-6-decimals input.
fn parse_usdc(s: &str) -> Option<u64> {
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    if whole.is_empty() && frac.is_empty() {
        return None;
    }
    if frac.len() > USDC_DECIMALS as usize {
        return None;
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let frac_micro: u64 = if frac.is_empty() {
        0
    } else {
        let parsed: u64 = frac.parse().ok()?;
        parsed * 10u64.pow(USDC_DECIMALS - frac.len() as u32)
    };
    whole
        .checked_mul(10u64.pow(USDC_DECIMALS))?
        .checked_add(frac_micro)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r#"{
        "rules": [
            { "prefix": "/firn/metrics", "pricing": "free" },
            { "prefix": "/firn/health", "price_usdc": "0.01",
              "caller_prices": { "tenant-a": "0.002" } },
            { "prefix": "/firn", "price_usdc": "0.05" }
        ]
    }"#;

    fn ruleset() -> RuleSet {
        RuleSet::from_json(TABLE).unwrap()
    }

    #[test]
    fn free_route() {
        assert_eq!(ruleset().decide("/firn/metrics", None), Decision::Free);
    }

    #[test]
    fn paid_route() {
        assert_eq!(
            ruleset().decide("/firn/health", None),
            Decision::Paid { micro_usdc: 10_000 }
        );
    }

    #[test]
    fn per_caller_price_overrides_base() {
        assert_eq!(
            ruleset().decide("/firn/health", Some("tenant-a")),
            Decision::Paid { micro_usdc: 2_000 }
        );
    }

    #[test]
    fn unknown_caller_gets_base_price() {
        assert_eq!(
            ruleset().decide("/firn/health", Some("tenant-b")),
            Decision::Paid { micro_usdc: 10_000 }
        );
    }

    #[test]
    fn longest_prefix_wins() {
        assert_eq!(
            ruleset().decide("/firn/query", None),
            Decision::Paid { micro_usdc: 50_000 }
        );
        assert_eq!(
            ruleset().decide("/firn/health/sub", None),
            Decision::Paid { micro_usdc: 10_000 }
        );
    }

    #[test]
    fn unmatched_is_denied() {
        assert_eq!(ruleset().decide("/other", None), Decision::Deny);
        assert_eq!(ruleset().decide("/", None), Decision::Deny);
    }

    #[test]
    fn prefix_matches_whole_segments_only() {
        assert_eq!(ruleset().decide("/firnabc", None), Decision::Deny);
        assert_eq!(
            ruleset().decide("/firn", None),
            Decision::Paid { micro_usdc: 50_000 }
        );
    }

    #[test]
    fn trailing_slash_prices_like_the_bare_path() {
        assert_eq!(
            ruleset().decide("/firn/", None),
            Decision::Paid { micro_usdc: 50_000 }
        );
        assert_eq!(
            ruleset().decide("/firn/health/", None),
            Decision::Paid { micro_usdc: 10_000 }
        );
    }

    #[test]
    fn encoded_alias_of_a_rule_does_not_match_it() {
        // Byte-exact matching: `%68ealth` is not `health`, so the longer
        // rule does not apply. (The gateway rejects such paths outright;
        // this pins the fail-closed behaviour if one ever got here.)
        assert_ne!(
            ruleset().decide("/firn/%68ealth", None),
            Decision::Paid { micro_usdc: 10_000 }
        );
    }

    #[test]
    fn caller_on_free_route_stays_free() {
        assert_eq!(
            ruleset().decide("/firn/metrics", Some("tenant-a")),
            Decision::Free
        );
    }

    #[test]
    fn free_rule_with_caller_prices_is_rejected_at_load() {
        let err = RuleSet::from_json(
            r#"{ "rules": [ { "prefix": "/x", "pricing": "free",
                 "caller_prices": { "a": "1" } } ] }"#,
        )
        .unwrap_err();
        assert!(matches!(err, RuleError::CallerPricesOnFree { .. }));
    }

    #[test]
    fn conflicting_pricing_is_rejected_at_load() {
        let err = RuleSet::from_json(
            r#"{ "rules": [ { "prefix": "/x", "pricing": "free", "price_usdc": "1" } ] }"#,
        )
        .unwrap_err();
        assert!(matches!(err, RuleError::ConflictingPricing { .. }));
    }

    #[test]
    fn unknown_pricing_is_rejected_at_load() {
        let err = RuleSet::from_json(
            r#"{ "rules": [ { "prefix": "/x", "pricing": "deny", "price_usdc": "1" } ] }"#,
        )
        .unwrap_err();
        assert!(matches!(err, RuleError::UnknownPricing { .. }));
    }

    #[test]
    fn non_absolute_prefix_is_rejected_at_load() {
        for bad in ["", "x", "firn/health"] {
            let err = RuleSet::from_json(&format!(
                r#"{{ "rules": [ {{ "prefix": "{bad}", "pricing": "free" }} ] }}"#
            ))
            .unwrap_err();
            assert!(matches!(err, RuleError::BadPrefix { .. }), "prefix {bad:?}");
        }
    }

    #[test]
    fn usdc_parsing() {
        assert_eq!(parse_usdc("0.01"), Some(10_000));
        assert_eq!(parse_usdc("1"), Some(1_000_000));
        assert_eq!(parse_usdc("0.000001"), Some(1));
        assert_eq!(parse_usdc(".5"), Some(500_000));
        assert_eq!(parse_usdc("2."), Some(2_000_000));
        assert_eq!(parse_usdc("0.0000001"), None); // finer than 6 decimals
        assert_eq!(parse_usdc("-1"), None);
        assert_eq!(parse_usdc("1.2.3"), None);
        assert_eq!(parse_usdc(""), None);
        assert_eq!(parse_usdc("."), None);
        assert_eq!(parse_usdc("abc"), None);
    }

    #[test]
    fn bad_price_is_rejected_at_load() {
        let err =
            RuleSet::from_json(r#"{ "rules": [ { "prefix": "/x", "price_usdc": "0.0000001" } ] }"#)
                .unwrap_err();
        assert!(matches!(err, RuleError::BadPrice { .. }));
    }

    #[test]
    fn missing_pricing_is_rejected_at_load() {
        let err = RuleSet::from_json(r#"{ "rules": [ { "prefix": "/x" } ] }"#).unwrap_err();
        assert!(matches!(err, RuleError::NoPricing { .. }));
    }

    #[test]
    fn duplicate_prefix_is_rejected_at_load() {
        let err = RuleSet::from_json(
            r#"{ "rules": [
                { "prefix": "/x", "pricing": "free" },
                { "prefix": "/x", "price_usdc": "1" }
            ] }"#,
        )
        .unwrap_err();
        assert!(matches!(err, RuleError::DuplicatePrefix { .. }));
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(matches!(
            RuleSet::from_json("not json").unwrap_err(),
            RuleError::Json(_)
        ));
    }
}
