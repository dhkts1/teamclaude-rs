//! What a request would have cost on Anthropic's public API.
//!
//! # What the number means
//!
//! Every account in this fleet is a **subscription**, so no dollar here is ever
//! billed. The figure is the API list-price equivalent — "what this traffic
//! would have cost had it gone through the API" — which is the only comparable
//! unit across accounts, models and days. `docs/cli.md` says the same thing to
//! the operator; keep the two sentences in step.
//!
//! # Unknown models are `None`, never `0.0`
//!
//! A model id this table does not know is *unpriced*, not free. ccusage returns
//! `0.0` there and the zero then flows into a total that reads as measured. This
//! module returns `None` and the caller counts the request in
//! `unpriced_requests`, the same honest-null discipline `cacheHitRatio` follows
//! in `cli.rs`: a number nobody measured must never be published as one.
//!
//! # Longest prefix, so dated ids resolve
//!
//! Anthropic ships both `claude-haiku-4-5` and `claude-haiku-4-5-20251001`, and
//! the dated form is what actually arrives in request bodies. Matching is
//! longest-prefix so a dated id resolves to its family, and a future
//! `claude-opus-5-1` cannot be silently priced as `claude-opus-5` — a longer,
//! more specific entry always wins over a shorter one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// USD per million tokens for one model family, all five billing dimensions
/// resolved. Built either from [`TABLE`] plus [`CACHE_READ_MULTIPLIER`] and
/// friends, or from a config override.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    /// Base (non-cached) input tokens.
    pub input: f64,
    pub output: f64,
    /// Writing the default 5-minute ephemeral cache.
    pub cache_write_5m: f64,
    /// Writing the extended 1-hour cache.
    pub cache_write_1h: f64,
    /// Reading either cache.
    pub cache_read: f64,
}

/// Cache reads bill at a tenth of base input.
const CACHE_READ_MULTIPLIER: f64 = 0.1;
/// A 5-minute cache write bills at 1.25x base input.
const CACHE_WRITE_5M_MULTIPLIER: f64 = 1.25;
/// A 1-hour cache write bills at 2x base input — which is why the ledger has to
/// keep the two cache-creation dimensions apart at all.
const CACHE_WRITE_1H_MULTIPLIER: f64 = 2.0;

/// `(model-id prefix, input USD/MTok, output USD/MTok)`, transcribed from
/// Anthropic's published first-party API rates (the `claude-api` reference
/// table, cached 2026-06-24). Order is irrelevant — [`PricingTable::lookup`]
/// takes the LONGEST matching prefix, not the first.
///
/// **Every row here has a source.** Models Anthropic still lists as active but
/// whose rates are not in that table — `claude-opus-4-5`, `claude-sonnet-4-5`
/// and the deprecated 4.0/3.x families — are deliberately ABSENT rather than
/// filled in from memory: they price as `None` and count as unpriced, which is
/// visible and correctable, whereas a guessed rate is a wrong number nobody can
/// tell from a right one. `docs/configuration.md` documents the `pricing`
/// override as the way to add one.
///
/// Two known under-counts, both from facts this proxy cannot see in a response:
/// Bedrock/Vertex are partner-operated at separate rates (irrelevant here — the
/// fleet is first-party), and Opus 5 **fast mode** bills at $10/$50 rather than
/// $5/$25. Fast mode is reported in `usage.speed`, which nothing in this crate
/// parses yet, so a fast-mode request is priced as standard and reads low.
const TABLE: &[(&str, f64, f64)] = &[
    ("claude-fable-5", 10.0, 50.0),
    // Project Glasswing's Fable 5: same capabilities, same pricing, different id.
    ("claude-mythos-5", 10.0, 50.0),
    ("claude-opus-5", 5.0, 25.0),
    ("claude-opus-4-8", 5.0, 25.0),
    ("claude-opus-4-7", 5.0, 25.0),
    ("claude-opus-4-6", 5.0, 25.0),
    ("claude-sonnet-5", 2.0, 10.0),
    ("claude-sonnet-4-6", 3.0, 15.0),
    ("claude-haiku-4-5", 1.0, 5.0),
];

/// A per-model pricing override from `~/.config/teamclaude.json`'s `pricing`
/// map. `input` and `output` are required (a half-specified price is a mistake,
/// not a default); the three cache dimensions fall back to the same multipliers
/// the built-in table uses.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingOverride {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
}

impl PricingOverride {
    fn resolve(&self) -> ModelPrice {
        ModelPrice {
            input: self.input,
            output: self.output,
            cache_write_5m: self
                .cache_write_5m
                .unwrap_or(self.input * CACHE_WRITE_5M_MULTIPLIER),
            cache_write_1h: self
                .cache_write_1h
                .unwrap_or(self.input * CACHE_WRITE_1H_MULTIPLIER),
            cache_read: self
                .cache_read
                .unwrap_or(self.input * CACHE_READ_MULTIPLIER),
        }
    }
}

/// The built-in table plus whatever the config overrides.
#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    overrides: BTreeMap<String, PricingOverride>,
}

impl PricingTable {
    pub fn new(overrides: BTreeMap<String, PricingOverride>) -> Self {
        Self { overrides }
    }

    /// The price for `model`, or `None` when nothing matches — see the module
    /// docs on why that is not a zero.
    ///
    /// An override is consulted FIRST and wins outright when any of its keys is
    /// a prefix of `model`, even a shorter one than a built-in entry would
    /// match: an operator who writes a price into their config is stating what
    /// this proxy should believe, and a table baked into the binary must not
    /// out-vote it on specificity.
    pub fn lookup(&self, model: &str) -> Option<ModelPrice> {
        let over = self
            .overrides
            .iter()
            .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len());
        if let Some((_, price)) = over {
            return Some(price.resolve());
        }
        let (_, input, output) = TABLE
            .iter()
            .filter(|(prefix, _, _)| model.starts_with(prefix))
            .max_by_key(|(prefix, _, _)| prefix.len())?;
        Some(ModelPrice {
            input: *input,
            output: *output,
            cache_write_5m: input * CACHE_WRITE_5M_MULTIPLIER,
            cache_write_1h: input * CACHE_WRITE_1H_MULTIPLIER,
            cache_read: input * CACHE_READ_MULTIPLIER,
        })
    }
}

/// Cost in **nanodollars** (1e-9 USD) for one request's five token dimensions.
///
/// Nanodollars, not micros: a single cache-read token on Haiku costs 1e-7 USD,
/// which rounds to zero in micros. At ~17k requests a day that rounding is a
/// visible drift in the daily total, and the whole point of this ledger is that
/// the number is trustworthy. `u64` nanodollars tops out around $1.8e10, which
/// no personal fleet will reach.
pub fn cost_nanos(
    price: &ModelPrice,
    input: u64,
    cache_5m: u64,
    cache_1h: u64,
    cache_read: u64,
    output: u64,
) -> u64 {
    // USD/MTok x tokens = USD/1e6; x 1e9 nanodollars/USD = x 1e3.
    let dim = |tokens: u64, usd_per_mtok: f64| (tokens as f64 * usd_per_mtok * 1_000.0).round();
    let total = dim(input, price.input)
        + dim(cache_5m, price.cache_write_5m)
        + dim(cache_1h, price.cache_write_1h)
        + dim(cache_read, price.cache_read)
        + dim(output, price.output);
    if total <= 0.0 {
        0
    } else {
        total as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> PricingTable {
        PricingTable::default()
    }

    /// The anchor figure: a million base input tokens on Opus 5 is $5.00 exactly.
    #[test]
    fn one_million_opus_input_tokens_costs_five_dollars() {
        let price = table()
            .lookup("claude-opus-5")
            .expect("opus-5 is in the table");
        let nanos = cost_nanos(&price, 1_000_000, 0, 0, 0, 0);
        assert_eq!(nanos, 5_000_000_000, "1M opus-5 input tokens = $5.00");
    }

    /// The reason the ledger splits cache creation at all: a 1h write is twice
    /// base input, a 5m write is 1.25x, so the same token count costs different
    /// money depending on which TTL the client asked for.
    #[test]
    fn a_one_hour_cache_write_bills_at_twice_input() {
        let price = table()
            .lookup("claude-opus-5")
            .expect("opus-5 is in the table");
        let one_hour = cost_nanos(&price, 0, 0, 1_000_000, 0, 0);
        let five_minute = cost_nanos(&price, 0, 1_000_000, 0, 0, 0);
        let base = cost_nanos(&price, 1_000_000, 0, 0, 0, 0);
        assert_eq!(one_hour, base * 2, "1h cache write = 2x input");
        assert_eq!(five_minute, base * 5 / 4, "5m cache write = 1.25x input");
    }

    /// A dated model id — the form that actually arrives in request bodies —
    /// resolves to its family by longest prefix.
    #[test]
    fn a_dated_model_id_resolves_by_prefix() {
        let price = table()
            .lookup("claude-haiku-4-5-20251001")
            .expect("a dated haiku id resolves to the haiku family");
        assert_eq!(price.input, 1.0);
        assert_eq!(price.output, 5.0);
    }

    /// An unknown model is unpriced, never free. This is the case ccusage gets
    /// wrong by returning `0.0`.
    #[test]
    fn an_unknown_model_has_no_price() {
        assert!(table().lookup("gpt-9").is_none());
        assert!(table().lookup("").is_none());
    }

    /// A config override wins over the built-in table for the same model.
    #[test]
    fn a_config_override_beats_the_table() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "claude-opus-5".to_string(),
            PricingOverride {
                input: 1.0,
                output: 2.0,
                cache_write_5m: None,
                cache_write_1h: None,
                cache_read: None,
            },
        );
        let price = PricingTable::new(overrides)
            .lookup("claude-opus-5-20260101")
            .expect("the override matches by prefix too");
        assert_eq!(price.input, 1.0, "the override's input price, not $5");
        assert_eq!(price.output, 2.0);
        assert_eq!(price.cache_write_1h, 2.0, "derived from the override, 2x");
    }

    /// A longer built-in prefix beats a shorter one, so a family that shares a
    /// stem with another cannot inherit the wrong price.
    #[test]
    fn a_longer_prefix_wins_over_a_shorter_one() {
        let sonnet_5 = table().lookup("claude-sonnet-5").expect("in the table");
        let sonnet_4_6 = table().lookup("claude-sonnet-4-6").expect("in the table");
        assert_eq!(sonnet_5.input, 2.0);
        assert_eq!(sonnet_4_6.input, 3.0, "4-6 is its own row, not sonnet-5's");
    }

    /// Every model id the cached Anthropic reference prices is priced here, at
    /// that reference's numbers. A drift in either direction is a wrong dollar
    /// figure on an operator's screen, which is worse than no figure.
    #[test]
    fn the_table_matches_the_published_rates() {
        let expected = [
            ("claude-fable-5", 10.0, 50.0),
            ("claude-mythos-5", 10.0, 50.0),
            ("claude-opus-5", 5.0, 25.0),
            ("claude-opus-4-8", 5.0, 25.0),
            ("claude-opus-4-7", 5.0, 25.0),
            ("claude-opus-4-6", 5.0, 25.0),
            ("claude-sonnet-5", 2.0, 10.0),
            ("claude-sonnet-4-6", 3.0, 15.0),
            ("claude-haiku-4-5", 1.0, 5.0),
        ];
        for (model, input, output) in expected {
            let price = table()
                .lookup(model)
                .unwrap_or_else(|| panic!("{model} must be priced"));
            assert_eq!(price.input, input, "{model} input rate");
            assert_eq!(price.output, output, "{model} output rate");
        }
    }

    /// A model Anthropic lists as active but whose rate this table has no source
    /// for is UNPRICED, not guessed. It is the config override's job to add one.
    #[test]
    fn an_unsourced_active_model_is_left_unpriced() {
        assert!(
            table().lookup("claude-sonnet-4-5-20250929").is_none(),
            "no published rate was read for sonnet 4.5, so it must not carry one"
        );
    }
}
