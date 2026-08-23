//! Drop-in config on `~/.config/teamclaude.json`.
//!
//! The file is the SAME one the JS proxy uses, so the structs mirror its
//! camelCase shape and stay tolerant of fields we do not model (`routes`, `sx`,
//! `quotaProbeSeconds`, `warmupSeconds`, …). Every struct carries a flattened
//! `extra` map so an unknown key survives a load→save round-trip untouched.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Typed config-layer errors: I/O vs malformed JSON stay distinguishable so a
/// caller can tell "no config yet" from "config is corrupt".
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(#[from] serde_json::Error),
}

fn default_port() -> u16 {
    3456
}
fn default_upstream() -> String {
    "https://api.anthropic.com".to_string()
}
fn default_switch_threshold() -> f64 {
    0.95
}
/// Default headroom (§3, control-account part 2) reserved for the control
/// account against general (non-control-preferred) picks — see
/// [`Config::control_reserve`] and [`crate::manager::select::effective_threshold`].
fn default_control_reserve() -> f64 {
    0.05
}
/// Default pacing when the `pacing` key is absent: OFF — no in-flight cap, no
/// min-spacing, so an unconfigured proxy runs the no-pacing selection path.
///
/// A per-account concurrency cap trades prompt-cache locality for load spread:
/// every request it diverts lands on an account whose prefix is cold. On a
/// single-user proxy the cache is the scarce resource and the accounts are not,
/// so the trade is the wrong way round and the cap ships off. It stays a
/// supported knob — set `"pacing": {"maxInFlightPerAccount": N}` (and/or
/// `"minSpacingMs"`) to turn it back on, with exactly the behaviour it has today.
///
/// This is NOT covered by the global egress throttle ([`default_throttle`],
/// `src/manager/throttle.rs`): that is a RATE limiter (min-spacing + burst over
/// the aggregate send site), not a concurrency bound, and it is deliberately not
/// a substitute for one. Turning the cap off leaves per-account concurrency
/// genuinely unbounded.
fn default_pacing() -> PacingConfig {
    PacingConfig {
        max_in_flight_per_account: None,
        min_spacing_ms: None,
    }
}
/// Default global outbound throttle: ON. Absent `throttle` key → these
/// evidence-anchored starting values; `"throttle": {}` → off (escape hatch).
fn default_throttle() -> ThrottleConfig {
    ThrottleConfig {
        min_spacing_ms: Some(350),
        burst: Some(4),
    }
}
fn default_account_type() -> String {
    "oauth".to_string()
}

/// Proxy-level settings (`proxy` object in the file).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Any `proxy.*` keys we do not model, preserved verbatim on save.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            api_key: None,
            extra: Map::new(),
        }
    }
}

/// One rotatable upstream account.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub name: String,
    #[serde(rename = "type", default = "default_account_type")]
    pub account_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Epoch **milliseconds** at which `access_token` expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_threshold: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// Labels used to prefer-route a session (`tcr run --group <name>`) to this
    /// account. Absent or empty means the account belongs to no group. Backward
    /// compatible in both directions: `Account` has no `deny_unknown_fields` and
    /// a flattened `extra` map, so a config already carrying `groups` round-trips
    /// today even before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<String>>,
    /// Any per-account keys we do not model (e.g. `models`, `upstream`, `sx`).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Account {
    /// Whether this account belongs to `group`. An account with no `groups` key
    /// belongs to none — it is reachable only by ungrouped routing.
    pub fn in_group(&self, group: &str) -> bool {
        self.groups
            .as_deref()
            .is_some_and(|groups| groups.iter().any(|g| g == group))
    }
}

/// Properties of one group label (`groupSettings.<name>` in the file) — a home
/// for facts about the GROUP itself, distinct from `Account::groups`, which only
/// records membership. Absent for a group name that no account carries a
/// `groupSettings` entry for; `#[serde(default)]` so an older config (or a group
/// with no properties set) round-trips untouched.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GroupSettings {
    /// When `true`, an account carrying this group is off-limits to traffic
    /// that did not ask for one of its groups — see
    /// [`crate::manager::select::eligible`]'s doc-comment for the exact rule.
    /// Absent/`false` (default) keeps today's prefer-only behaviour.
    #[serde(default)]
    pub reserved: bool,
    /// A configured `#RRGGBB` (or `#RGB`, normalized on write) color for this
    /// group's panel tag. Absent → no color was ever set, and
    /// [`Config::group_color`] derives one deterministically from the group
    /// name instead — see that function's doc-comment for the band. Never
    /// read directly by a consumer that wants "the" color for a group; go
    /// through [`Config::group_color`] / [`Config::group_colors`] so derived
    /// and configured colors resolve through one seam.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Any `groupSettings.<name>.*` keys we do not model.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Whether a group's resolved color came from an operator's explicit
/// `tcr group color` write or was invented by [`derive_group_color`] because
/// none was set. Purely informational — both are equally valid "the" color
/// for a group — but an operator asking `tcr group ls` needs to be able to
/// tell which is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSource {
    Derived,
    Set,
}

impl ColorSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Derived => "derived",
            Self::Set => "set",
        }
    }
}

/// Invent a stable color for `group` when no `tcr group color` was ever run
/// for it. Same name always yields the same color (a deterministic hash of
/// the bytes, not randomness or insertion order), and different names spread
/// around the hue wheel so adjacent groups are visually distinguishable.
///
/// Derived in **OKLCH** at a **fixed lightness** (`L = 0.80`), varying only
/// hue — not HSL, which this replaced. HSL's `L` is not perceptual: the old
/// `HSL(hue, 62%, 58%)` band measured **L 0.518–0.852 in OKLCH**, so chips
/// carried wildly different visual weight for no reason, and around hue 30 it
/// was unreadable in BOTH text polarities (APCA `|Lc|` 52.5 white / 56.5
/// black, against the ≥60 threshold non-body text needs) — roughly one group
/// name in twelve. Fixed `L` is what makes every chip carry the same visual
/// weight, and it also keeps a single foreground choice correct for every
/// derived color at once: at `L = 0.80` black text beats white for every hue
/// on this wheel (measured worst case APCA ≥ 60, see this module's
/// `derived_colors_clear_the_apca_floor_at_every_hue` test).
///
/// Chroma targets `0.12` but is **reduced (never clipped)** per hue to stay
/// inside the sRGB gamut: at `L = 0.80, C = 0.12` several hues would clip a
/// channel to 255, which silently distorts the hue rather than dimming it —
/// see [`oklch_to_gamut_srgb`]. Still emits plain `#rrggbb`: OKLCH is the
/// math used to pick the byte values, never a notation this crate writes to
/// the wire, the config file, or anywhere else.
pub fn derive_group_color(group: &str) -> String {
    // FNV-1a: no dependency, good-enough avalanche for hashing short ASCII
    // labels into a hue — this is a display color, not a security boundary,
    // so cryptographic strength buys nothing here.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in group.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let hue = (hash % 360) as f64;
    oklch_to_hex(DERIVED_GROUP_COLOR_L, DERIVED_GROUP_COLOR_C, hue)
}

/// Fixed OKLCH lightness [`derive_group_color`] varies hue against — see its
/// doc-comment for why this specific value.
const DERIVED_GROUP_COLOR_L: f64 = 0.80;
/// Target OKLCH chroma [`derive_group_color`] aims for before per-hue gamut
/// reduction — see [`oklch_to_gamut_srgb`].
const DERIVED_GROUP_COLOR_C: f64 = 0.12;

/// OKLCH(`l`, `c`, `h`-degrees) to a lowercase `#rrggbb`, reducing `c` per hue
/// to stay in the sRGB gamut — see [`oklch_to_gamut_srgb`].
fn oklch_to_hex(l: f64, c: f64, h_deg: f64) -> String {
    let (r, g, b) = oklch_to_gamut_srgb(l, c, h_deg);
    let to_byte = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", to_byte(r), to_byte(g), to_byte(b))
}

/// OKLCH(`l`, `c`, `h`-degrees) to gamma-encoded sRGB `(r, g, b)` in `[0, 1]`,
/// reducing `c` toward 0 in small steps until the linear-sRGB result has no
/// channel outside `[0, 1]` — i.e. no clipping. Clipping a channel instead
/// (the naive approach) silently shifts the HUE, not just the brightness,
/// which is exactly the distortion the bridge asked this to avoid. `c <= 0`
/// (grey at `l`) is always in gamut, so this always terminates.
fn oklch_to_gamut_srgb(l: f64, c: f64, h_deg: f64) -> (f64, f64, f64) {
    let mut chroma = c;
    loop {
        if let Some(rgb) = oklch_to_srgb_if_in_gamut(l, chroma, h_deg) {
            return rgb;
        }
        if chroma <= 0.0 {
            return oklch_to_srgb_clamped(l, 0.0, h_deg);
        }
        chroma = (chroma - 0.0005).max(0.0);
    }
}

/// A tiny slack on the `[0, 1]` gamut bound — linear-sRGB round-trips through
/// `powf` and cube roots, so an in-gamut color can land at `1.0000000003`
/// from floating-point noise alone; without slack that would be rejected as
/// "out of gamut" and needlessly desaturated.
const GAMUT_EPS: f64 = 1e-4;

/// `Some((r, g, b))` (gamma-encoded, in `[0, 1]`) when OKLCH(`l`, `c`,
/// `h`-degrees) converts to linear sRGB with every channel inside
/// `[-GAMUT_EPS, 1 + GAMUT_EPS]`; `None` when a channel would clip.
fn oklch_to_srgb_if_in_gamut(l: f64, c: f64, h_deg: f64) -> Option<(f64, f64, f64)> {
    let (lr, lg, lb) = oklch_to_linear_srgb(l, c, h_deg);
    if !(-GAMUT_EPS..=1.0 + GAMUT_EPS).contains(&lr)
        || !(-GAMUT_EPS..=1.0 + GAMUT_EPS).contains(&lg)
        || !(-GAMUT_EPS..=1.0 + GAMUT_EPS).contains(&lb)
    {
        return None;
    }
    Some((
        linear_to_srgb_component(lr.clamp(0.0, 1.0)),
        linear_to_srgb_component(lg.clamp(0.0, 1.0)),
        linear_to_srgb_component(lb.clamp(0.0, 1.0)),
    ))
}

/// Same conversion as [`oklch_to_srgb_if_in_gamut`], but clamps unconditionally
/// instead of reporting an out-of-gamut channel — only ever called at `c = 0`
/// (a grey), which is always representable, by [`oklch_to_gamut_srgb`]'s
/// terminating case.
fn oklch_to_srgb_clamped(l: f64, c: f64, h_deg: f64) -> (f64, f64, f64) {
    let (lr, lg, lb) = oklch_to_linear_srgb(l, c, h_deg);
    (
        linear_to_srgb_component(lr.clamp(0.0, 1.0)),
        linear_to_srgb_component(lg.clamp(0.0, 1.0)),
        linear_to_srgb_component(lb.clamp(0.0, 1.0)),
    )
}

/// OKLCH(`l`, `c`, `h`-degrees) to linear sRGB, via OKLab — Björn Ottosson's
/// published matrices (https://bottosson.github.io/posts/oklab/), the
/// reference conversion every OKLCH implementation uses. Channels are NOT
/// clamped here; the caller decides what an out-of-`[0,1]` channel means.
fn oklch_to_linear_srgb(l: f64, c: f64, h_deg: f64) -> (f64, f64, f64) {
    let h = h_deg.to_radians();
    let a = c * h.cos();
    let b = c * h.sin();

    let l_ = l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = l - 0.0894841775 * a - 1.2914855480 * b;

    let l3 = l_ * l_ * l_;
    let m3 = m_ * m_ * m_;
    let s3 = s_ * s_ * s_;

    let r = 4.0767416621 * l3 - 3.3077115913 * m3 + 0.2309699292 * s3;
    let g = -1.2684380046 * l3 + 2.6097574011 * m3 - 0.3413193965 * s3;
    let b = -0.0041960863 * l3 - 0.7034186147 * m3 + 1.7076147010 * s3;
    (r, g, b)
}

/// Linear-light sRGB component in `[0, 1]` to gamma-encoded sRGB in `[0, 1]`
/// — the standard piecewise sRGB EOTF inverse.
fn linear_to_srgb_component(v: f64) -> f64 {
    if v <= 0.0031308 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// APCA-W3 (SAPC-8) perceptual contrast `Lc`, signed: positive is dark text
/// on a light background, negative is light text on a dark one — only the
/// magnitude (`|Lc|`) matters for "is this readable" and that is what every
/// caller here takes. Ported from the public APCA-W3 0.1.9 reference
/// algorithm (https://github.com/Myndex/apca-w3) — not WCAG 2's ratio, which
/// is a poor predictor of perceived contrast for exactly the kind of
/// mid-lightness saturated color this module generates.
fn apca_contrast(text_rgb: (u8, u8, u8), bg_rgb: (u8, u8, u8)) -> f64 {
    const BLACK_THRESHOLD: f64 = 0.022;
    const BLACK_CLAMP: f64 = 1.414;
    const DELTA_Y_MIN: f64 = 0.0005;
    const SCALE: f64 = 1.14;
    const LO_CLIP: f64 = 0.1;
    const LO_OFFSET: f64 = 0.027;

    let y = |rgb: (u8, u8, u8)| -> f64 {
        let f = |v: u8| (v as f64 / 255.0).powf(2.4);
        0.2126729 * f(rgb.0) + 0.7151522 * f(rgb.1) + 0.0721750 * f(rgb.2)
    };
    let soft_black_clamp = |y: f64| -> f64 {
        if y > BLACK_THRESHOLD {
            y
        } else {
            y + (BLACK_THRESHOLD - y).powf(BLACK_CLAMP)
        }
    };

    let txt_y = soft_black_clamp(y(text_rgb));
    let bg_y = soft_black_clamp(y(bg_rgb));
    if (bg_y - txt_y).abs() < DELTA_Y_MIN {
        return 0.0;
    }

    if bg_y > txt_y {
        // Normal polarity: dark(er) text on a light(er) background.
        let sapc = (bg_y.powf(0.56) - txt_y.powf(0.57)) * SCALE;
        if sapc < LO_CLIP {
            0.0
        } else {
            sapc - LO_OFFSET
        }
    } else {
        // Reverse polarity: light(er) text on a dark(er) background.
        let sapc = (bg_y.powf(0.62) - txt_y.powf(0.65)) * SCALE;
        if sapc > -LO_CLIP {
            0.0
        } else {
            sapc + LO_OFFSET
        }
    }
    .abs()
        * 100.0
}

/// `(r, g, b)` bytes of a `#rrggbb`/`#rgb`-shaped hex string, as already
/// normalized by [`validate_hex_color`]. `unwrap_or(0)` rather than a
/// `Result`: every caller here passes a string that already round-tripped
/// through `validate_hex_color`, so a parse failure would mean that
/// normalization itself is broken — not something a color-contrast helper
/// should surface as its own error type.
fn hex_to_rgb(hex: &str) -> (u8, u8, u8) {
    let digits = hex.trim_start_matches('#');
    let byte_at = |start: usize| u8::from_str_radix(&digits[start..start + 2], 16).unwrap_or(0);
    (byte_at(0), byte_at(2), byte_at(4))
}

/// The best achievable APCA `|Lc|` for `bg_hex` — whichever of pure white or
/// pure black text reads better against it. This is the same "which
/// foreground wins" question the panel asks per color chip, and the number
/// [`crate::cli::set_group_color`]'s low-contrast warning names.
pub fn best_readable_apca(bg_hex: &str) -> f64 {
    let bg = hex_to_rgb(bg_hex);
    let white = apca_contrast((255, 255, 255), bg);
    let black = apca_contrast((0, 0, 0), bg);
    white.max(black)
}

/// Strictly validate a user-supplied hex color and normalize it to lowercase
/// `#rrggbb`. Accepts `#RGB` and `#RRGGBB`, case-insensitive; rejects
/// anything else (missing `#`, wrong digit count, non-hex characters)
/// rather than trying to guess what garbage input meant.
pub fn validate_hex_color(input: &str) -> Result<String, &'static str> {
    let digits = input
        .strip_prefix('#')
        .ok_or("must start with '#' — accepted forms: #RGB, #RRGGBB")?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("must contain only hex digits after '#' — accepted forms: #RGB, #RRGGBB");
    }
    match digits.len() {
        3 => {
            let mut out = String::with_capacity(7);
            out.push('#');
            for c in digits.chars() {
                let lower = c.to_ascii_lowercase();
                out.push(lower);
                out.push(lower);
            }
            Ok(out)
        }
        6 => Ok(format!("#{}", digits.to_ascii_lowercase())),
        _ => Err("must be #RGB or #RRGGBB — accepted forms: #RGB, #RRGGBB"),
    }
}

/// Per-account request pacing (opt-in; default OFF).
///
/// Both knobs are `Option`: absent in the config file → `None` → pacing is inert,
/// so an unconfigured proxy behaves byte-for-byte as before. When set, pacing can
/// only ever DELAY/SPREAD selection across the fleet — never turn a servable
/// request into a failure (the soft fallback in [`crate::manager::Manager::select`]).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PacingConfig {
    /// Cap on requests concurrently being served on one account. An account at or
    /// over the cap is temporarily skipped in selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight_per_account: Option<u32>,
    /// Minimum gap (ms) between two selects of the SAME account. An account
    /// selected less than this ago is temporarily skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_spacing_ms: Option<u64>,
}

impl PacingConfig {
    /// The in-flight cap, treating a configured `0` as "disabled" (identical to
    /// leaving it unset). A literal `Some(0)` would make `in_flight >= 0` true for
    /// every account, holding out the ENTIRE fleet permanently — collapsing every
    /// request onto the least-loaded soft fallback and flooding the pacing log with
    /// a "skip in selection" line per account per request. Normalising it here keeps
    /// that footgun out of every read site.
    pub fn effective_max_in_flight(&self) -> Option<u32> {
        match self.max_in_flight_per_account {
            Some(0) => None,
            other => other,
        }
    }

    /// Whether either knob is configured. When `false`, pacing is fully inert and
    /// eligibility/selection are byte-identical to the no-pacing build. A cap of
    /// `0` counts as unset (see [`Self::effective_max_in_flight`]).
    pub fn is_active(&self) -> bool {
        self.effective_max_in_flight().is_some() || self.min_spacing_ms.is_some()
    }
}

/// Global (fleet-wide) outbound request-initiation throttle (opt-in; default OFF).
///
/// A GCRA token bucket over the SINGLE upstream send site: `burst` requests admit
/// instantly after idle, then one per `minSpacingMs`. Unlike [`PacingConfig`] (which
/// is PER-ACCOUNT and cannot damp a cross-account burst), this paces the AGGREGATE
/// egress that Anthropic's shared IP/client_id burst limiter actually keys on —
/// mirroring the probe path's `PROBE_SPACING`. Ships ON by default
/// ([`default_throttle`]): absent `throttle` key → `minSpacingMs: 350, burst: 4`;
/// `"throttle": {}` (empty object present) → all `None` → inert (escape hatch).
///
/// 350ms mirrors the σ5-proven probe-path aggregate rate (PROBE_SPACING); burst 4
/// covers a normal within-turn fan-out (main+haiku+quota) untaxed while staying far
/// below a ~15-20 cold-start fan-out so the throttle engages on the burst. Both are
/// evidence-anchored STARTING values, tunable live (docs/plans/throttle-live-sweep-runbook.md).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThrottleConfig {
    /// Steady-state emission interval T (ms): after the burst budget is spent,
    /// at most one upstream send is initiated per this many ms across the WHOLE fleet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_spacing_ms: Option<u64>,
    /// Bucket capacity B: how many sends may fire instantly after an idle period.
    /// Absent → treated as 1 (strict spacing). Keep it BELOW the cold fan-out size
    /// so the burst is actually paced, ABOVE the normal within-turn fan-out (~3) so
    /// interactive turns are never delayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
}

impl ThrottleConfig {
    /// Emission interval, treating `Some(0)` as unset (mirrors
    /// [`PacingConfig::effective_max_in_flight`]'s footgun normalization).
    pub fn effective_min_spacing(&self) -> Option<u64> {
        match self.min_spacing_ms {
            Some(0) => None,
            other => other,
        }
    }
    /// Bucket capacity, clamped to >= 1 (B=1 ⇒ strict min-spacing).
    pub fn effective_burst(&self) -> u32 {
        self.burst.unwrap_or(1).max(1)
    }
    /// Whether the throttle does anything. `min_spacing_ms` is the required knob —
    /// a burst without a spacing interval is meaningless. When false the throttle is
    /// fully inert (see [`crate::manager::Manager::throttle_send`]).
    pub fn is_active(&self) -> bool {
        self.effective_min_spacing().is_some()
    }
}

/// Top-level config document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default = "default_upstream")]
    pub upstream: String,
    #[serde(default = "default_switch_threshold")]
    pub switch_threshold: f64,
    /// Per-account request pacing. Absent in JSON → [`default_pacing`] → all knobs
    /// `None`, i.e. OFF: a per-account concurrency cap trades prompt-cache locality
    /// for load spread, and on a single-user proxy the cache is the scarce resource.
    /// Set `"pacing": {"maxInFlightPerAccount": N}` to opt back in. The global
    /// [`ThrottleConfig`] is a RATE limiter and is deliberately not a substitute for
    /// a concurrency bound.
    #[serde(default = "default_pacing")]
    pub pacing: PacingConfig,
    /// Global outbound throttle. Absent → [`default_throttle`] (ON:
    /// `minSpacingMs: 350, burst: 4`). Set `"throttle": {}` to disable (all knobs
    /// `None`), or override the knobs to tune the live rate (read at boot).
    #[serde(default = "default_throttle")]
    pub throttle: ThrottleConfig,
    /// Hard account lock: when set to an account `name`, ALL traffic is pinned to
    /// that one account — LRU rotation, session affinity, and load-balancing
    /// migration are ALL bypassed. Absent → normal routing (default). Tradeoff:
    /// a locked account has NO failover; if it is throttled/disabled/down, requests
    /// fail rather than rotating. Set to the exact `accounts[].name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_account: Option<String>,
    /// The identity-bound control account: the one name that `_tcr/accounts/control`
    /// resolves to and that stays PROBEABLE (usage tracked) even while `disabled`.
    /// Unlike [`Self::lock_account`] this does NOT change selection by itself —
    /// see the manager's `control_idx` doc for the resolution and the routing this
    /// key only sets up for (part 2). Absent → no control account (default).
    /// `skip_serializing_if` so clearing it REMOVES the key rather than writing
    /// `null` — same contract as `lock_account` would want if it grew a setter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_account: Option<String>,
    /// Quota headroom reserved for the control account against GENERAL
    /// (non-control-preferred) picks — part of the routing half (part 2) of the
    /// control-account feature. `threshold - control_reserve` is the effective
    /// switch threshold a general pool pick applies to the control account
    /// specifically; a control-preferred pick still uses the full threshold.
    /// Absent → 0.05. Clamped to `[0.0, 0.5]` by the manager at construction — a
    /// value outside that range in a hand-edited file is not trusted as-is.
    /// **Inert while the control account stays `disabled`** (the default): a
    /// disabled account never reaches a general pick's quota check at all. See
    /// [`crate::manager::select::effective_threshold`].
    #[serde(default = "default_control_reserve")]
    pub control_reserve: f64,
    /// Force the upstream-forwarding client onto HTTP/1.1 instead of the
    /// h2-and-fall-back-to-h1 negotiation reqwest does by default. Absent →
    /// `false` (h2). OFF by default deliberately: an intermittent
    /// connection-level fault (GOAWAY / framing error) on HTTP/2 kills EVERY
    /// multiplexed stream sharing that connection at once, so one fault takes
    /// down every concurrent session an account happens to be serving. h1
    /// does not multiplex, so a fault there costs exactly one in-flight
    /// request — a structural cap on blast radius, not a statistical one.
    /// The trade: h1 gives up multiplexing, opens one TCP+TLS connection per
    /// concurrent request instead of sharing one, and raises the open-socket
    /// count against Anthropic's edge — real costs, which is why this stays
    /// opt-in. Set `"http1Only": true` to enable.
    #[serde(default)]
    pub http1_only: bool,
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Properties of GROUPS themselves (`"groupSettings": {"codereview":
    /// {"reserved": true}}`), keyed by the same label `Account::groups` carries.
    /// Absent → no group is reserved (default). Membership stays per-account;
    /// this map holds only properties, so a key here naming a group with no
    /// members is harmless — it reserves nothing — and is deliberately NOT
    /// validated against current membership.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub group_settings: HashMap<String, GroupSettings>,
    /// Any top-level keys we do not model, preserved verbatim on save.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Config {
    /// The set of group labels currently marked `reserved`. Cheap to call —
    /// intended for one-shot use (CLI verbs, [`crate::manager::Manager`]
    /// construction caching its own copy), not a per-request hot path.
    pub fn reserved_group_names(&self) -> HashSet<String> {
        self.group_settings
            .iter()
            .filter(|(_, s)| s.reserved)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether `group` is currently marked `reserved`. `false` for a group
    /// name with no `groupSettings` entry at all — same "absent means not
    /// reserved" default the field's own doc-comment promises.
    pub fn is_group_reserved(&self, group: &str) -> bool {
        self.group_settings.get(group).is_some_and(|s| s.reserved)
    }

    /// Every group label that currently has at least one member — the same
    /// "a group exists only while some account carries the label" definition
    /// [`crate::cli::remove_from_group`]'s `--all` arm already uses. A
    /// `groupSettings` entry for a name no account carries (color configured
    /// ahead of membership, or the last member since removed) is deliberately
    /// NOT included: [`Self::group_colors`] answers "what should the panel
    /// tag right now", not "what has ever been configured".
    pub fn all_group_names(&self) -> BTreeSet<String> {
        self.accounts
            .iter()
            .filter_map(|a| a.groups.as_ref())
            .flatten()
            .cloned()
            .collect()
    }

    /// The resolved color for `group` plus whether it was configured or
    /// derived. Every group gets a color from this whether or not one was
    /// ever set — see [`derive_group_color`]'s doc-comment for the
    /// hue/saturation/lightness band and why it must be picked HERE, once,
    /// rather than left for each client to invent independently.
    pub fn group_color(&self, group: &str) -> (String, ColorSource) {
        match self
            .group_settings
            .get(group)
            .and_then(|s| s.color.as_deref())
        {
            Some(hex) => (hex.to_string(), ColorSource::Set),
            None => (derive_group_color(group), ColorSource::Derived),
        }
    }

    /// Every group that exists on the fleet ([`Self::all_group_names`])
    /// mapped to its resolved color ([`Self::group_color`]) — the exact
    /// shape `groupColors` puts on the wire. A `BTreeMap` so the wire order
    /// (and any snapshot/test comparing it) is deterministic run to run,
    /// independent of `HashMap` iteration order.
    pub fn group_colors(&self) -> BTreeMap<String, String> {
        self.all_group_names()
            .into_iter()
            .map(|g| {
                let (hex, _) = self.group_color(&g);
                (g, hex)
            })
            .collect()
    }

    /// How many ENABLED accounts would remain reachable by traffic that asks
    /// for nothing, if exactly the group names in `reserved` were reserved —
    /// the safety-rail figure `tcr group reserve` prints and refuses on. Takes
    /// the candidate set explicitly (rather than reading
    /// [`Self::reserved_group_names`] itself) so a caller can ask "what WOULD
    /// this leave" before writing the change.
    pub fn unreserved_enabled_count(&self, reserved: &HashSet<String>) -> usize {
        self.accounts
            .iter()
            .filter(|a| !a.disabled.unwrap_or(false))
            .filter(|a| {
                a.groups
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .all(|g| !reserved.contains(g))
            })
            .count()
    }
}

/// The default config path: `$HOME/.config/teamclaude.json`.
///
/// Deliberately NOT the platform config dir (`directories` would pick
/// `~/Library/Application Support` on macOS) — the JS proxy hard-codes
/// `~/.config`, and this binary is a drop-in for it.
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".config").join("teamclaude.json")
}

/// Whether an on-disk `accounts[]` entry can yield a usable credential, and if
/// not, the account's `name` and why — for the warning [`load`] emits when it
/// drops the entry. An `apikey` account needs `apiKey`; every other type
/// (including the implicit `oauth` default) needs a non-empty `accessToken`.
/// An `importFrom`-shaped entry (a bare pointer at another file, resolved by
/// upstream at startup — not implemented here, see `config-bridge-coder.md`)
/// has no inline token either, so it falls out through the same check.
fn unusable_account(entry: &Value) -> Option<(String, &'static str)> {
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<unnamed>")
        .to_string();
    let has_nonempty = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    };
    let is_apikey = entry.get("type").and_then(Value::as_str) == Some("apikey");
    if is_apikey {
        if has_nonempty("apiKey") {
            None
        } else {
            Some((name, "apiKey"))
        }
    } else if has_nonempty("accessToken") {
        None
    } else {
        Some((name, "accessToken"))
    }
}

/// Load and parse the config at `path`.
///
/// Deserializes leniently at the account boundary: an `accounts[]` entry that
/// cannot yield a usable credential ([`unusable_account`]) is dropped, with a
/// warning naming it, rather than failing the WHOLE file the way a plain
/// `serde_json::from_str` over `Config` would — `Account::access_token` is a
/// required `String` with no `serde(default)` (deliberately: an empty-string
/// token must never reach rotation and send `Bearer ` upstream), so one such
/// entry used to take every other account in the file down with it. Upstream
/// (`resolve-accounts.js`) skips an unusable account the same way rather than
/// crashing the fleet. Every `Account` that survives into memory still has a
/// real, non-empty credential — the 142 call sites reading `access_token` as a
/// plain `String` are entitled to assume that.
///
/// If every account entry is unusable, that is still an error (an empty fleet
/// booting silently is its own bug) — reported through the same
/// [`ConfigError::Parse`] shape a malformed file already uses, not a new
/// variant.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let data = fs::read_to_string(path)?;
    let mut doc: Value = serde_json::from_str(&data)?;

    if let Some(accounts) = doc.get_mut("accounts").and_then(Value::as_array_mut) {
        let had_accounts = !accounts.is_empty();
        let mut skipped = Vec::new();
        accounts.retain(|entry| match unusable_account(entry) {
            None => true,
            Some((name, reason)) => {
                tracing::warn!(account = %name, missing = reason, "No token for \"{name}\", skipping");
                skipped.push(name);
                false
            }
        });
        if had_accounts && accounts.is_empty() {
            return Err(ConfigError::Parse(
                <serde_json::Error as serde::de::Error>::custom(format!(
                    "no usable accounts remain after skipping {} without a credential: {}",
                    skipped.len(),
                    skipped.join(", ")
                )),
            ));
        }
    }

    let config = serde_json::from_value(doc)?;
    Ok(config)
}

/// Persist `config` to `path` atomically (temp file in the same dir + rename),
/// with `0600` permissions so refreshed tokens never land world-readable.
///
/// Same-directory temp + rename keeps the swap atomic (rename is atomic within a
/// filesystem); a crash mid-write leaves the original intact.
pub fn save(path: &Path, config: &Config) -> Result<(), ConfigError> {
    write_atomic(path, &serde_json::to_string_pretty(config)?)
}

/// The atomic 0600 write itself, shared by [`save`], [`save_tokens`] and the
/// session-affinity pin file ([`crate::affinity::save`]) so every path gets the
/// same durability and permission guarantees. One implementation deliberately:
/// a second hand-rolled temp+rename is a second place for the ordering to be
/// subtly wrong.
pub(crate) fn write_atomic(path: &Path, json: &str) -> Result<(), ConfigError> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));

    // Ensure the parent dir exists (a freshly-provisioned box may lack
    // `~/.config`), so a token-refresh save never fails with ENOENT and drops the
    // rotated refresh token. Fails loudly on a real perms error (finding #2).
    fs::create_dir_all(dir)?;

    // `tempfile_in`, never `NamedTempFile::new()`: the temp file MUST live in the
    // destination's own directory. The system temp dir is routinely a different
    // filesystem, and `rename(2)` across filesystems fails with `EXDEV` — which
    // would silently cost us the atomic swap this function exists to provide.
    //
    // The unique temp name is now the crate's job (finding #6): two concurrent
    // saves (e.g. `probe_all` refreshing several expired accounts at once) must
    // never open and truncate the SAME temp file and interleave into a corrupt
    // write. The `.{name}.{pid}.{seq}.tmp` scheme this replaced was already sound
    // for that — a live pid is unique and the counter is unique within it — so on
    // the concurrency property alone this is a lateral move.
    //
    // What it genuinely buys is a different, previously undocumented property.
    // The old open was `create(true).truncate(true)` — no `O_EXCL`, so it FOLLOWED
    // SYMLINKS — on a fully predictable path. Anyone able to create a file in the
    // config dir could pre-plant that path as a symlink and have a token refresh
    // write live OAuth credentials into a file of their choosing. Measured: the
    // old open succeeds and writes through the symlink; `create_new` (`O_EXCL`)
    // refuses with AlreadyExists. `O_EXCL` also guarantees a FRESH inode, which
    // matters because `open(2)` applies its `mode` argument only on creation — the
    // old code's `.mode(0o600)` was silently ignored whenever the path already
    // existed (measured: a pre-existing 0666 file stays 0666).
    //
    // The name is prefixed rather than left as the crate's default. `tempfile`
    // names a file `.tmpXXXXXX`, which carries no attribution at all: a SIGKILL
    // between the create below and the `persist` further down strands a file
    // holding EVERY account's OAuth access and refresh tokens in `~/.config`,
    // and this proxy is SIGKILLed as documented operational reality. The old
    // `.{name}.{pid}.{seq}.tmp` scheme at least left an orphan that was greppable
    // and obviously ours; `.{name}.tcr-XXXXXX.tmp` restores that. Note this is
    // attribution only — nothing in this crate reaps orphans (`rg 'read_dir' src/`
    // finds none), and the random component means they no longer recycle the way
    // a pid×seq space did, so they accumulate until an operator removes them.
    let prefix = match path.file_name() {
        Some(name) => format!(".{}.tcr-", name.to_string_lossy()),
        None => ".tcr-".to_string(),
    };
    let mut file = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o600))
        .prefix(prefix.as_str())
        .suffix(".tmp")
        .tempfile_in(dir)?;
    file.write_all(json.as_bytes())?;
    // Make the temp file's DATA durable before the rename that publishes it, so a
    // crash cannot expose the destination as empty or half-written. This is the
    // only durability guarantee that holds unconditionally here; see the
    // directory sync below for the part that does not.
    file.as_file().sync_all()?;

    match file.persist(path) {
        // The returned `File` is the persisted handle; nothing here writes through
        // it (every save is a fresh temp plus a rename), so it is dropped at once.
        Ok(_) => {}
        Err(tempfile::PersistError { error, file }) => {
            // NEVER `map_err(|e| e.error)`. `PersistError` OWNS the temp file
            // (`tempfile-3.27.0 src/file/mod.rs:516-521`) and `TempPath`'s `Drop` is
            // `fs::remove_file` unless cleanup is disabled (`:402-408`), so
            // discarding the struct to keep only its `io::Error` UNLINKS the
            // complete, fsynced JSON that is sitting right there.
            //
            // This is defence in depth, not the only line of it, and the
            // distinction is worth stating precisely. A rename failure here — full
            // disk, EACCES on the destination, an immutable flag, a read-only
            // remount — does not by itself lose the rotated single-use refresh
            // token: `Manager::persist_tokens` keeps it in the in-memory snapshot
            // on purpose for `persist_now` to flush at shutdown, and says so
            // (`src/manager/mod.rs:862-866`). What the retained file adds is a
            // SECOND, independent recovery artifact at a known path, which is what
            // covers the conjunction — a failed rename AND a process that never
            // reaches its shutdown flush. That conjunction is not hypothetical
            // here: `persist_now` is skipped on a SIGKILL and on the
            // never-served/early-cancel paths (`src/server.rs:1025`, `:1063`), and
            // this proxy is SIGKILLed as documented operational reality. The
            // `fs::rename` this replaced left exactly that artifact behind; the
            // migration removed it.
            //
            // `keep()` disables the delete-on-drop and hands back the path. The path
            // is logged (the contents are NOT — they are live OAuth tokens) because
            // a preserved file whose location never reaches an operator is barely
            // better than a deleted one.
            match file.keep() {
                Ok((_file, kept)) => tracing::warn!(
                    path = %path.display(),
                    retained = %kept.display(),
                    error = %error,
                    "could not rename the new state file into place; the fully-written temp file has been RETAINED at `retained` — move it over `path` by hand to recover"
                ),
                Err(keep_error) => tracing::error!(
                    path = %path.display(),
                    error = %error,
                    keep_error = %keep_error,
                    "could not rename the new state file into place, AND could not retain the temp file; its contents are lost"
                ),
            }
            return Err(error.into());
        }
    }

    // What is and is not durable, precisely. `sync_all` above makes the temp
    // file's DATA durable. `persist` is a bare `rename(2)`, which dirties only the
    // parent directory's inode — so without the sync below, a power loss seconds
    // after a successful return can roll the directory entry back and leave the
    // PREVIOUS contents in place. For `save_tokens` that means the previous
    // refresh token, which Anthropic has already consumed: the same dead-account
    // outcome, just by a different route. This gap predates the tempfile
    // migration; it is closed here.
    //
    // Best-effort ON PURPOSE, and this is the one place in this function where a
    // failure is not returned. The rename has ALREADY succeeded by this point —
    // the new bytes are the ones a reader sees. Turning a directory-sync failure
    // into an `Err` would tell `save_tokens` its write failed when it did not, and
    // the caller's recovery for that is to treat the rotated token as unpersisted.
    // A warn keeps the failure visible without inventing one. Portability is the
    // second reason: directory `fsync` is well-defined on Linux, while on macOS
    // `sync_all` issues `F_FULLFSYNC`, which a filesystem is free to refuse on a
    // directory fd. Measured on APFS (macOS 25.6, the deployment target): it
    // returns `Ok`, so the sync is real here and not a no-op — but that is one
    // filesystem, not a guarantee, which is why the failure is tolerated.
    //
    // The cost is real and was measured, not assumed: it roughly DOUBLES this
    // function (5.0 → 9.5 ms/write, N=200, APFS), because a second `F_FULLFSYNC`
    // is a second barrier. That is affordable only because of the actual call
    // rates. The affinity flusher is the hot caller and is the one that would hurt
    // — it does a blocking `std::fs` write inside async code (`src/server.rs:425`)
    // — but it is a dirty-gated 5-second ticker whose steady state is no writes at
    // all (`src/server.rs:670-683`), so this adds ~4.5ms to a background task at
    // most once per 5s. The credential writers run per token rotation. If a future
    // caller writes at request rate, revisit this line before assuming it is free.
    if let Err(e) = fs::File::open(dir).and_then(|d| d.sync_all()) {
        tracing::warn!(
            dir = %dir.display(),
            error = %e,
            "state file renamed into place, but the parent directory could not be fsynced; the rename may not survive a power loss"
        );
    }

    // NOT a guard against a looser pre-existing destination: `persist` is a
    // rename, so the destination's old inode — and its mode — are gone. This
    // cannot tighten anything.
    //
    // Nor is it needed for a later write: nothing here ever reopens a state file
    // for writing — every save is a fresh temp plus a rename, and `rename(2)`
    // needs permission on the DIRECTORY, not on the destination file. A 0400
    // config is rewritten perfectly (measured).
    //
    // It survives as a NORMALISATION, not a guard: the create mode above is
    // `0600 & ~umask`, so under a restrictive umask the file would land 0400 —
    // still correct, but surprising for a file the user is invited to hand-edit
    // when it goes corrupt. This keeps the mode from varying with the operator's
    // umask.
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Persist ONLY the per-account credential state from `memory` into the file at
/// `path`, leaving every user-owned setting on disk exactly as the user left it.
///
/// The running server's `Config` is a BOOT-TIME snapshot: it goes stale the
/// moment the user edits the file, and only the credential fields
/// (`access_token` / `refresh_token` / `expires_at`) are ever mutated in memory
/// afterwards. Writing that whole snapshot back — which is what a plain [`save`]
/// does — therefore reverts every edit made while the proxy runs (observed live
/// 2026-07-25: a deleted `pacing` key was restored by the shutdown flush and read
/// back by the next boot, three restarts running). So persisting is a
/// read-modify-write: the FILE is the authority for everything except tokens.
///
/// Accounts are matched by identity ([`crate::identity::same_identity`], which
/// reduces to name equality for the current config shape) and never by index —
/// indices shift when the user adds or removes an account. Iterating the on-disk
/// list and pulling tokens IN gives the two removal semantics for free: an
/// account the user deleted from the file is not resurrected, and an account on
/// disk that the server never loaded is left untouched.
///
/// An unreadable file, a malformed one, or one carrying no usable `accounts`
/// list falls back to writing the in-memory config: a just-rotated refresh token
/// is single-use, so dropping it strands that account on `invalid_grant`
/// forever, which is strictly worse than overwriting a file whose account list
/// we cannot find anyway. Every fallback logs a warning naming the cause.
///
/// A credential that cannot be placed on a SPECIFIC entry — the entry is
/// malformed, or its identity matches nothing the server loaded — is never a
/// reason to fail the whole write: every other account's token still lands, and
/// [`merge_tokens`] hands back what it could not place so each miss is warned
/// about by name here. Silence was the old defect: the merge skipped, the write
/// succeeded, `Ok(())` came back, and the caller's error branch never ran.
///
/// The merge runs on the file's raw JSON document, NOT on a `Config` round-trip:
/// deserializing would materialize every serde default back into the file, so a
/// key the user just DELETED would reappear as its default (`"pacing": {}`) and a
/// key they never wrote would appear for the first time. Editing the parsed
/// document leaves the file byte-identical apart from the credential fields.
pub fn save_tokens(path: &Path, memory: &Config) -> Result<(), ConfigError> {
    let mut doc = match read_document(path) {
        Ok(doc) => doc,
        Err(ConfigError::Io(err)) => {
            tracing::warn!(
                error = %err,
                path = %path.display(),
                "config unreadable at persist time; falling back to writing the in-memory config"
            );
            return save(path, memory);
        }
        Err(ConfigError::Parse(err)) => {
            tracing::warn!(
                error = %err,
                path = %path.display(),
                "config on disk is malformed JSON at persist time; falling back to writing the in-memory config"
            );
            return save(path, memory);
        }
    };
    let report = match merge_tokens(&mut doc, memory) {
        Ok(report) => report,
        Err(reason) => {
            tracing::warn!(
                reason = %reason,
                path = %path.display(),
                "config on disk has no usable accounts list at persist time; falling back to writing the in-memory config so the rotated tokens are not lost"
            );
            return save(path, memory);
        }
    };
    report.warn_unpersisted(path);
    write_atomic(path, &serde_json::to_string_pretty(&doc)?)
}

/// Read the config file as a raw JSON object. Parsing as a MAP (not a bare
/// `Value`) is deliberate: a file that is valid JSON but not an object — `[]`,
/// `null`, a half-written fragment — must take the malformed fallback rather
/// than be written back verbatim with the fresh tokens silently dropped.
fn read_document(path: &Path) -> Result<Map<String, Value>, ConfigError> {
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

/// The identity fields of an on-disk account entry — the only part of a stored
/// account this layer needs to read. Deliberately narrower than [`Account`]: an
/// entry the user is mid-edit on (no `accessToken` yet) still gets matched
/// rather than skipped.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiskIdentity {
    name: String,
    #[serde(default)]
    account_uuid: Option<String>,
    #[serde(default)]
    org_uuid: Option<String>,
    #[serde(default)]
    org_name: Option<String>,
}

/// The mutable credential triple, carrying the SAME serde renames as
/// [`Account`] so the merged keys are spelled exactly as the account struct
/// spells them — one source of truth for the wire names.
///
/// The `Option`s skip rather than clear: memory holds `None` only when the file
/// had no such field at boot, so an absent value means "nothing to say about
/// this key", never "delete what the user has since written there".
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Credentials<'a> {
    access_token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
}

/// Why the on-disk `accounts` list could not be merged into AT ALL. Not one
/// account's problem but the whole document's, so [`save_tokens`] answers it with
/// the same whole-config fallback an unparseable file takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unmergeable {
    /// No `accounts` key in the document.
    Missing,
    /// `accounts` is present but is not a JSON array.
    NotAnArray,
}

impl std::fmt::Display for Unmergeable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Missing => "the document has no accounts key",
            Self::NotAnArray => "the accounts key is not an array",
        })
    }
}

/// Why ONE on-disk account entry did not receive its rotated credentials. The
/// other entries in the same document are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    /// The array element is not a JSON object (a string, a number, `null`).
    NotAnObject,
    /// The element is an object but carries no readable identity — no string
    /// `name`, which every match needs.
    NoIdentity,
    /// The entry is well-formed but no loaded account shares its identity. The
    /// signature of an account renamed on disk while the proxy was running,
    /// which is exactly the live-edit workflow [`save_tokens`] exists to support.
    NoMemoryMatch,
    /// The entry is well-formed and has a loaded account it could belong to, but
    /// the pairing is not unique: either several loaded accounts carry its
    /// identity, or another on-disk entry carries the same identity and nothing
    /// stored says which entry is which. Writing here means picking one of them at
    /// random and stamping a rotated credential over another account's own
    /// single-use refresh token, so nothing is written and the entry keeps what it
    /// had. See [`crate::identity::resolve`].
    Ambiguous,
    /// The credential triple would not serialize. Structurally unreachable —
    /// reported rather than swallowed precisely because reaching it would mean an
    /// assumption this module rests on has broken.
    CredentialEncoding,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAnObject => "the entry is not a JSON object",
            Self::NoIdentity => "the entry has no readable account name",
            Self::NoMemoryMatch => "no loaded account has that identity (renamed on disk?)",
            Self::Ambiguous => {
                "that identity does not pick out one loaded account and one entry, so no credential could be chosen"
            }
            Self::CredentialEncoding => "the credentials would not serialize",
        })
    }
}

/// One on-disk entry the merge could not write into. It keeps whatever
/// credential it already held — which, after a rotation, is a consumed one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SkippedEntry {
    /// Position in the on-disk `accounts` array — the only handle the user has on
    /// an entry too malformed to carry a name.
    index: usize,
    /// The entry's `name` when one is readable, which it is for the common
    /// [`SkipReason::NoMemoryMatch`] case.
    name: Option<String>,
    reason: SkipReason,
}

impl SkippedEntry {
    /// How the entry is named in a log line: its `name`, else its position.
    fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("accounts[{}]", self.index))
    }
}

/// What a merge could not persist. Both lists are REPORTS, not failures: the
/// merge places every credential it can and hands the rest back, so
/// [`save_tokens`] logs each miss with the config path attached instead of the
/// helper logging blind.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct MergeReport {
    /// On-disk entries left holding whatever credential they already had.
    skipped: Vec<SkippedEntry>,
    /// Names of loaded accounts with no on-disk entry to write into. Benign when
    /// the user deleted the account; a token loss when they renamed it.
    absent_from_disk: Vec<String>,
}

impl MergeReport {
    /// Emit one line per credential that did NOT reach the file. A rotated
    /// refresh token is single-use, so a skip means that token is now consumed
    /// and unrecoverable — the account has to be re-authed. Nothing else in the
    /// stack can say this: the write itself succeeds and returns `Ok(())`.
    fn warn_unpersisted(&self, path: &Path) {
        for entry in &self.skipped {
            tracing::warn!(
                account = %entry.label(),
                index = entry.index,
                reason = %entry.reason,
                path = %path.display(),
                "rotated credential not persisted for this account; it may need `tcr login`"
            );
        }
        // A loaded account with no on-disk entry is USUALLY the user deleting it
        // from the file — correct, expected, and not worth a warning on every
        // persist for the rest of the process's life. Paired with an unmatched
        // on-disk entry in the same write it is instead the signature of a
        // RENAME, where a rotated credential really was dropped. The pairing is
        // what makes the two cases distinguishable, so only it escalates.
        let renamed = self
            .skipped
            .iter()
            .any(|s| s.reason == SkipReason::NoMemoryMatch);
        for name in &self.absent_from_disk {
            if renamed {
                tracing::warn!(
                    account = %name,
                    path = %path.display(),
                    "loaded account has no entry on disk while another entry matched nothing; a rename would drop its rotated credential, and it may need `tcr login`"
                );
            } else {
                tracing::debug!(
                    account = %name,
                    path = %path.display(),
                    "loaded account has no entry on disk; nothing persisted for it (removed from the file?)"
                );
            }
        }
    }
}

/// Overwrite the credential fields of every account present in BOTH `memory` and
/// the on-disk `doc`, matched by identity and never by position. Nothing else in
/// the document is touched.
///
/// Iterating the ON-DISK list and pulling tokens in gives both removal semantics
/// for free: an account the user deleted from the file is never resurrected (it
/// has no entry to write into), and an account on disk the server never loaded
/// is left alone (no memory match).
///
/// Every path that declines to write a credential is REPORTED, never silent: the
/// per-entry misses come back in the [`MergeReport`], and a document with no
/// usable `accounts` list comes back as [`Unmergeable`] so the caller can fall
/// back to writing the config whole rather than lose every rotated token in the
/// one write. A skipped entry is not an error for the others — the merge runs to
/// the end of the list either way.
fn merge_tokens(doc: &mut Map<String, Value>, memory: &Config) -> Result<MergeReport, Unmergeable> {
    // Plan against an IMMUTABLE view first. Deciding one entry at a time under a
    // mutable borrow is what made the old first-match resolution unfixable: the
    // assignment for entry N depends on which accounts the other entries claim,
    // which a single mutable pass cannot see. Same split, and for the same reason,
    // as `locate_account_entry` vs `find_account_entry`.
    let Some(accounts) = doc.get("accounts") else {
        return Err(Unmergeable::Missing);
    };
    let Some(entries) = accounts.as_array() else {
        return Err(Unmergeable::NotAnArray);
    };
    let plan = plan_merge(entries, &memory.accounts);

    let Some(accounts) = doc.get_mut("accounts").and_then(Value::as_array_mut) else {
        // The immutable read above already proved both of these; this is the
        // total-function tail, not a reachable outcome.
        return Err(Unmergeable::Missing);
    };

    let mut report = MergeReport::default();
    // Which loaded accounts found a home, so the ones that did not can be named
    // afterwards — a rename shows up here as well as in `skipped`.
    let mut placed = vec![false; memory.accounts.len()];

    for (index, (entry, (name, plan))) in accounts.iter_mut().zip(plan).enumerate() {
        let position = match plan {
            EntryPlan::Skip(reason) => {
                report.skipped.push(SkippedEntry {
                    index,
                    name,
                    reason,
                });
                continue;
            }
            EntryPlan::Write(position) => position,
        };
        // A `Write` plan only comes from an entry the planner parsed, so both of
        // these hold by construction.
        let (Some(object), Some(fresh)) = (entry.as_object_mut(), memory.accounts.get(position))
        else {
            continue;
        };
        let credentials = Credentials {
            access_token: &fresh.access_token,
            refresh_token: fresh.refresh_token.as_deref(),
            expires_at: fresh.expires_at,
        };
        let Ok(Value::Object(fields)) = serde_json::to_value(&credentials) else {
            report.skipped.push(SkippedEntry {
                index,
                name,
                reason: SkipReason::CredentialEncoding,
            });
            continue;
        };
        object.extend(fields);
        if let Some(seen) = placed.get_mut(position) {
            *seen = true;
        }
    }

    report.absent_from_disk = memory
        .accounts
        .iter()
        .zip(&placed)
        .filter(|(_, seen)| !**seen)
        .map(|(account, _)| account.name.clone())
        .collect();
    Ok(report)
}

/// What the read-only planning pass decided about one on-disk entry.
enum EntryPlan {
    /// Write the loaded account at this position into the entry.
    Write(usize),
    /// Leave the entry's credentials exactly as they are, for this reason.
    Skip(SkipReason),
}

/// Decide, for the whole `accounts` array at once, which loaded account owns each
/// on-disk entry — paired with the entry's readable `name` for the report.
///
/// An entry is written ONLY when the pairing is unambiguous in BOTH directions:
/// the entry resolves to exactly one loaded account, AND that account is claimed
/// by exactly one entry. One direction is not enough. Resolving each entry
/// independently — which is what a first-match search does — let two entries both
/// take the same loaded account, stamping that account's freshly rotated
/// credential onto an entry belonging to a DIFFERENT account and destroying that
/// account's own single-use refresh token. And an entry that has only one
/// candidate is still a guess when that candidate has two suitors: writing picks
/// one of two entries to be "the real one" on nothing.
///
/// Pairings are committed REPEATEDLY until no more can be, because each commitment
/// removes an account from one pool and an entry from the other, which can break a
/// tie that was unbreakable a moment ago. That is what keeps the legacy two-org
/// shape working — one person, two orgs, where the older entry predates org UUIDs
/// and so carries none. The org-carrying entry pairs off first (it is the only
/// *strict* match on either side), and the pre-org entry, which matched BOTH
/// accounts while they were both in the pool, is then left facing exactly one.
/// Resolved in a single pass it would tie forever and neither account would ever
/// have its rotated token persisted. Iterating also makes the result independent
/// of the order the entries happen to sit in the file.
///
/// Whatever is still unpaired when the fixed point is reached is reported, never
/// guessed.
fn plan_merge(entries: &[Value], memory: &[Account]) -> Vec<(Option<String>, EntryPlan)> {
    // Parse each entry's identity once. A `None` probe is an entry no match can
    // reach, and its plan is already final — so `plan[i].is_some()` is exactly
    // "this entry is settled", for both the unmatchable and the paired.
    let mut probes: Vec<Option<Account>> = Vec::with_capacity(entries.len());
    let mut names: Vec<Option<String>> = Vec::with_capacity(entries.len());
    let mut plan: Vec<Option<EntryPlan>> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(object) = entry.as_object() else {
            probes.push(None);
            names.push(None);
            plan.push(Some(EntryPlan::Skip(SkipReason::NotAnObject)));
            continue;
        };
        // Read the name BEFORE the identity parse, so a half-edited entry that
        // fails to deserialize can still be named in the warning.
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        names.push(name);
        let Ok(stored) = serde_json::from_value::<DiskIdentity>(Value::Object(object.clone()))
        else {
            probes.push(None);
            plan.push(Some(EntryPlan::Skip(SkipReason::NoIdentity)));
            continue;
        };
        probes.push(Some(crate::identity::probe(
            &stored.name,
            stored.account_uuid,
            stored.org_uuid,
            stored.org_name,
        )));
        plan.push(None);
    }

    let mut claimed = vec![false; memory.len()];

    // Commit the mutually-unambiguous pairings until there are none left. Each
    // round settles at least one entry or ends the loop, so this terminates in at
    // most `entries.len()` rounds.
    loop {
        let mut progressed = false;
        for index in 0..probes.len() {
            let Some(probe) = probes[index].as_ref().filter(|_| plan[index].is_none()) else {
                continue;
            };
            // Which unclaimed account does this entry point at?
            let crate::identity::Resolved::One(position) = crate::identity::resolve(
                memory
                    .iter()
                    .enumerate()
                    .filter(|(position, _)| !claimed[*position]),
                probe,
            ) else {
                continue;
            };
            // …and does that account point back at this entry alone? Any other
            // unsettled entry with the same identity makes the write a coin flip.
            let mutual = crate::identity::resolve(
                probes
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| plan[*other].is_none())
                    .filter_map(|(other, probe)| probe.as_ref().map(|probe| (other, probe))),
                &memory[position],
            ) == crate::identity::Resolved::One(index);
            if mutual {
                plan[index] = Some(EntryPlan::Write(position));
                claimed[position] = true;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    // Everything the fixed point could not settle. An entry with no candidate left
    // at all is the rename/removal case the report already had a name for; an entry
    // that still has candidates is one this function refused to guess between.
    for index in 0..probes.len() {
        let Some(probe) = probes[index].as_ref().filter(|_| plan[index].is_none()) else {
            continue;
        };
        let unmatched = crate::identity::resolve(
            memory
                .iter()
                .enumerate()
                .filter(|(position, _)| !claimed[*position]),
            probe,
        ) == crate::identity::Resolved::None;
        plan[index] = Some(EntryPlan::Skip(if unmatched {
            SkipReason::NoMemoryMatch
        } else {
            SkipReason::Ambiguous
        }));
    }

    names
        .into_iter()
        .zip(plan)
        // Every slot was assigned above: unmatchable at parse, paired in the fixed
        // point, or classified in the sweep. This is the total-function tail.
        .map(|(name, plan)| {
            (
                name,
                plan.unwrap_or(EntryPlan::Skip(SkipReason::NoMemoryMatch)),
            )
        })
        .collect()
}

/// What a targeted [`save_disabled`] did to the on-disk document. Reported
/// rather than swallowed so the caller can say, in one line, whether a benched
/// account will actually still be benched after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledWrite {
    /// The flag was set (or dropped) on the matching entry and the file rewritten.
    Updated,
    /// The document already said exactly this, so nothing was written and the
    /// file is byte-identical. A file holding single-use refresh tokens is
    /// rewritten only when it must be.
    Unchanged,
    /// Nothing on disk carries that identity — the entry was deleted or renamed
    /// while the proxy ran — or the document has no usable `accounts` array.
    /// Nothing was written, so the flag will NOT survive a restart.
    NoEntry,
    /// More than one entry carries that identity and nothing stored breaks the
    /// tie, so no entry can be chosen without guessing. Nothing was written.
    ///
    /// [`crate::identity::same_identity`] falls back to name equality when either
    /// side lacks a uuid, so two entries sharing a name both match either runtime
    /// row. The caller selects by ROW INDEX (the TUI does), so silently taking the
    /// first match lands the flag on whichever entry happens to be earlier —
    /// benching a healthy account and returning an exhausted one to rotation, with
    /// the TUI showing the opposite. Refusing is the same posture the CLI takes on
    /// an ambiguous query.
    ///
    /// This is now genuine ambiguity only. `same_identity` ALSO treats an unknown
    /// org as a match, which used to make the legacy two-org shape (an entry with
    /// an org beside a pre-org entry that has none) report `Ambiguous` even though
    /// the two entries are trivially distinguishable — so neither of two real
    /// accounts could ever be durably benched. [`crate::identity::resolve`] breaks
    /// that tie on the org key; see [`find_account_entry`].
    Ambiguous,
}

impl std::fmt::Display for DisabledWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::NoEntry => "no matching entry on disk",
            Self::Ambiguous => "more than one entry on disk carries this identity",
        })
    }
}

/// Persist ONLY the `disabled` flag of the one account matching `target`'s
/// identity into the file at `path`, leaving every other key — and every other
/// account — exactly as the user left it.
///
/// Same read-modify-write shape as [`save_tokens`], and for the same reason: the
/// running server's `Config` is a boot-time snapshot, so writing it whole (what
/// [`save`] does) reverts every setting the user edited while the proxy ran. The
/// edit therefore runs on the file's RAW JSON document and never on a `Config`
/// round-trip, so a key the user just deleted cannot reappear as its serde
/// default.
///
/// `disabled == false` REMOVES the key rather than writing `false` — matching
/// the CLI contract pinned by `cli::tests::set_enabled_false_drops_the_disabled_key`
/// and the JS `delete account.disabled` it was ported from. A stale `false`
/// already on disk is dropped for the same reason.
///
/// Unlike [`save_tokens`] there is deliberately NO whole-config fallback when the
/// file is unreadable or malformed. A rotated refresh token is single-use, so
/// losing one is unrecoverable and worth the clobber risk; a lost `disabled` flag
/// costs one un-benched account and is fixed by pressing `d` again. Writing a
/// whole boot-time snapshot over a file we could not even parse is the exact
/// clobber this module exists to prevent, so the error comes back instead.
///
/// An identity matching MORE than one entry writes nothing and reports
/// [`DisabledWrite::Ambiguous`] — see [`find_account_entry`] for why guessing the
/// first match is worse than refusing.
pub fn save_disabled(
    path: &Path,
    target: &Account,
    disabled: bool,
) -> Result<DisabledWrite, ConfigError> {
    let mut doc = read_document(path)?;
    let outcome = merge_disabled(&mut doc, target, disabled);
    if outcome == DisabledWrite::Updated {
        write_atomic(path, &serde_json::to_string_pretty(&doc)?)?;
    }
    Ok(outcome)
}

/// Set or remove `disabled` on the one entry in `doc` matching `target`. Reports
/// whether the document actually changed, so the caller can skip a pointless
/// rewrite of a credential file.
fn merge_disabled(doc: &mut Map<String, Value>, target: &Account, disabled: bool) -> DisabledWrite {
    let entry = match find_account_entry(doc, target) {
        Ok(entry) => entry,
        // No entry, or too many to choose between — either way nothing is written.
        Err(refusal) => return refusal,
    };
    // `true` writes the key; `false` DROPS it (never a `false` literal).
    let desired = disabled.then_some(Value::Bool(true));
    if entry.get("disabled").cloned() == desired {
        return DisabledWrite::Unchanged;
    }
    match desired {
        Some(value) => entry.insert("disabled".to_string(), value),
        None => entry.remove("disabled"),
    };
    DisabledWrite::Updated
}

/// What a targeted [`save_control_account`] did to the on-disk document.
/// Simpler than [`DisabledWrite`]: `controlAccount` is a single top-level
/// string, not a per-entry flag located by identity, so there is no
/// `NoEntry`/`Ambiguous` — the key is either already what was asked, or it
/// isn't, and the caller is free to name an account nothing on disk carries
/// yet (resolution to a live rotation slot happens in the manager, not here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlWrite {
    /// The key was set (or removed) and the file rewritten.
    Updated,
    /// The document already said exactly this; nothing was written.
    Unchanged,
}

/// Persist ONLY the top-level `controlAccount` key into the file at `path`,
/// leaving every other key exactly as the user left it. Same read-modify-write
/// shape as [`save_disabled`] and for the same reason: the running server's
/// `Config` is a boot-time snapshot, so writing it whole (what [`save`] does)
/// would revert every setting the user edited while the proxy ran — that
/// clobber is the exact bug fixed in `1d978ce`. The edit therefore runs on the
/// file's RAW JSON document via [`read_document`] + [`write_atomic`], never on
/// a `Config` round-trip through [`save`].
///
/// `name == None` REMOVES the key (matches `#[serde(skip_serializing_if =
/// "Option::is_none")]` on [`Config::control_account`]) rather than writing
/// `null` — the same "clear removes, never sets to a null-ish literal"
/// contract [`save_disabled`] uses for `disabled == false`.
pub fn save_control_account(path: &Path, name: Option<&str>) -> Result<ControlWrite, ConfigError> {
    let mut doc = read_document(path)?;
    let outcome = merge_control_account(&mut doc, name);
    if outcome == ControlWrite::Updated {
        write_atomic(path, &serde_json::to_string_pretty(&doc)?)?;
    }
    Ok(outcome)
}

/// What a targeted [`save_group_membership`] did to the on-disk document. Same
/// shape as [`DisabledWrite`], for the same reason — the caller must be able to
/// say, in one line, whether a group edit will actually survive a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupWrite {
    /// The label was added to (or removed from) the matching entry's `groups`
    /// array and the file rewritten.
    Updated,
    /// The entry already carried (or already lacked) this label, so nothing
    /// was written and the file is byte-identical.
    Unchanged,
    /// Nothing on disk carries that identity — deleted or renamed since the
    /// caller loaded the config, or no usable `accounts` array. Nothing was
    /// written.
    NoEntry,
    /// More than one entry carries that identity and nothing stored breaks the
    /// tie. Nothing was written — same refusal [`save_disabled`] makes, for
    /// the same reason: guessing which entry to edit risks labelling the
    /// wrong account.
    Ambiguous,
}

impl std::fmt::Display for GroupWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::NoEntry => "no matching entry on disk",
            Self::Ambiguous => "more than one entry on disk carries this identity",
        })
    }
}

/// Persist ONLY one `groups` label of the one account matching `target`'s
/// identity into the file at `path` — add it when `member`, remove it
/// otherwise — leaving every other key, and every other account, exactly as
/// the user left it.
///
/// Same read-modify-write shape as [`save_disabled`] and for the same reason:
/// `Config` is a boot-time snapshot, and rewriting the whole file ([`save`])
/// would revert every out-of-band edit and drop unmodelled forward-compat
/// keys. The edit runs on the file's raw JSON document via [`read_document`] +
/// [`write_atomic`], never on a `Config` round-trip.
///
/// Removing an account's LAST label drops the `groups` key entirely rather
/// than leaving `[]` on disk. `Account::in_group` treats an absent key and an
/// empty array identically, so either choice is behaviourally safe — this
/// picks "absent" so an account that has ever had a label removed does not
/// carry a permanent empty-array litter, matching the `disabled == false`
/// drops-the-key contract [`save_disabled`] uses. `groups_round_trip...` and
/// this module's own tests pin the choice.
pub fn save_group_membership(
    path: &Path,
    target: &Account,
    group: &str,
    member: bool,
) -> Result<GroupWrite, ConfigError> {
    let mut doc = read_document(path)?;
    let outcome = merge_group_membership(&mut doc, target, group, member);
    if outcome == GroupWrite::Updated {
        write_atomic(path, &serde_json::to_string_pretty(&doc)?)?;
    }
    Ok(outcome)
}

/// Add or remove `group` on the one entry in `doc` matching `target`. Reports
/// whether the document actually changed, so the caller can skip a pointless
/// rewrite of a credential file.
fn merge_group_membership(
    doc: &mut Map<String, Value>,
    target: &Account,
    group: &str,
    member: bool,
) -> GroupWrite {
    let entry = match find_account_entry(doc, target) {
        Ok(entry) => entry,
        Err(refusal) => {
            return match refusal {
                DisabledWrite::NoEntry => GroupWrite::NoEntry,
                DisabledWrite::Ambiguous => GroupWrite::Ambiguous,
                // `find_account_entry` only ever returns these two on `Err` —
                // `Updated`/`Unchanged` are `DisabledWrite`'s success arms and
                // never reach a caller through this path.
                DisabledWrite::Updated | DisabledWrite::Unchanged => {
                    unreachable!("find_account_entry only returns NoEntry/Ambiguous on Err")
                }
            };
        }
    };
    let mut groups: Vec<String> = entry
        .get("groups")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let already = groups.iter().any(|g| g == group);
    if member == already {
        return GroupWrite::Unchanged;
    }
    if member {
        groups.push(group.to_string());
    } else {
        groups.retain(|g| g != group);
    }
    if groups.is_empty() {
        entry.remove("groups");
    } else {
        entry.insert(
            "groups".to_string(),
            Value::Array(groups.into_iter().map(Value::String).collect()),
        );
    }
    GroupWrite::Updated
}

/// What a targeted [`save_group_reserved`] did to the on-disk document. Same
/// two-arm shape as [`GroupWrite`]'s success cases — this write can never fail
/// on an identity mismatch (it targets a GROUP NAME, not an `accounts` entry),
/// so it has no `NoEntry`/`Ambiguous` counterpart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupReserveWrite {
    /// `groupSettings.<group>.reserved` was set (or its whole entry removed)
    /// and the file rewritten.
    Updated,
    /// The group already carried (or already lacked) `reserved`, so nothing
    /// was written and the file is byte-identical.
    Unchanged,
}

/// Persist ONLY `groupSettings.<group>.reserved` into the file at `path`,
/// leaving every other key — every account, every other group's settings —
/// exactly as the user left it. Same read-modify-write shape as
/// [`save_group_membership`] and for the same reason: `Config` is a boot-time
/// snapshot, and a whole-file [`save`] would revert any out-of-band edit.
///
/// Setting `reserved` to `false` drops the `reserved` key rather than writing
/// `false`, and drops the group's whole `groupSettings` entry once it is
/// empty — mirroring the drops-the-key contract [`merge_group_membership`]
/// uses for an account's last label, so a group that has ever been reserved
/// and then unreserved carries no permanent litter.
pub fn save_group_reserved(
    path: &Path,
    group: &str,
    reserved: bool,
) -> Result<GroupReserveWrite, ConfigError> {
    let mut doc = read_document(path)?;
    let outcome = merge_group_reserved(&mut doc, group, reserved);
    if outcome == GroupReserveWrite::Updated {
        write_atomic(path, &serde_json::to_string_pretty(&doc)?)?;
    }
    Ok(outcome)
}

fn merge_group_reserved(
    doc: &mut Map<String, Value>,
    group: &str,
    reserved: bool,
) -> GroupReserveWrite {
    let already = doc
        .get("groupSettings")
        .and_then(Value::as_object)
        .and_then(|settings| settings.get(group))
        .and_then(Value::as_object)
        .and_then(|entry| entry.get("reserved"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if already == reserved {
        return GroupReserveWrite::Unchanged;
    }

    let settings = doc
        .entry("groupSettings".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    // A hand-edited `groupSettings` that is not an object is corrupt input we
    // must not silently coerce — replace it with a fresh object rather than
    // panicking, since `already` above already proved it carried no usable
    // `reserved` value for this group either way.
    if !settings.is_object() {
        *settings = Value::Object(Map::new());
    }
    let settings_obj = settings.as_object_mut().expect("just ensured object");

    if reserved {
        let entry = settings_obj
            .entry(group.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        entry
            .as_object_mut()
            .expect("just ensured object")
            .insert("reserved".to_string(), Value::Bool(true));
    } else if let Some(entry) = settings_obj.get_mut(group) {
        if let Some(obj) = entry.as_object_mut() {
            obj.remove("reserved");
            if obj.is_empty() {
                settings_obj.remove(group);
            }
        }
    }
    if settings_obj.is_empty() {
        doc.remove("groupSettings");
    }
    GroupReserveWrite::Updated
}

/// What a targeted [`save_group_color`] did to the on-disk document. Same
/// shape as [`GroupReserveWrite`] and for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupColorWrite {
    /// `groupSettings.<group>.color` was set (or its whole entry removed)
    /// and the file rewritten.
    Updated,
    /// The group already carried (or already lacked) that exact color, so
    /// nothing was written and the file is byte-identical.
    Unchanged,
}

/// Persist ONLY `groupSettings.<group>.color` into the file at `path`,
/// leaving every other key — every account, every other group's settings —
/// exactly as the user left it. Same read-modify-write shape as
/// [`save_group_reserved`] and for the same reason.
///
/// `color = None` (`tcr group color --clear`) drops the `color` key rather
/// than writing anything, and drops the group's whole `groupSettings` entry
/// once it is empty — same drops-the-key contract [`merge_group_reserved`]
/// uses, so a group that has ever had a color set and then cleared carries
/// no permanent litter and reverts to [`Config::group_color`]'s derived
/// value on the very next read.
///
/// `color`, when `Some`, is trusted as already validated/normalized — call
/// through [`validate_hex_color`] first; this function does not re-check it.
pub fn save_group_color(
    path: &Path,
    group: &str,
    color: Option<&str>,
) -> Result<GroupColorWrite, ConfigError> {
    let mut doc = read_document(path)?;
    let outcome = merge_group_color(&mut doc, group, color);
    if outcome == GroupColorWrite::Updated {
        write_atomic(path, &serde_json::to_string_pretty(&doc)?)?;
    }
    Ok(outcome)
}

fn merge_group_color(
    doc: &mut Map<String, Value>,
    group: &str,
    color: Option<&str>,
) -> GroupColorWrite {
    let already = doc
        .get("groupSettings")
        .and_then(Value::as_object)
        .and_then(|settings| settings.get(group))
        .and_then(Value::as_object)
        .and_then(|entry| entry.get("color"))
        .and_then(Value::as_str);
    if already == color {
        return GroupColorWrite::Unchanged;
    }

    let settings = doc
        .entry("groupSettings".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    // Same corrupt-hand-edit guard as `merge_group_reserved`: `already` above
    // already proved this was not a usable object for this group either way.
    if !settings.is_object() {
        *settings = Value::Object(Map::new());
    }
    let settings_obj = settings.as_object_mut().expect("just ensured object");

    match color {
        Some(hex) => {
            let entry = settings_obj
                .entry(group.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            entry
                .as_object_mut()
                .expect("just ensured object")
                .insert("color".to_string(), Value::String(hex.to_string()));
        }
        None => {
            if let Some(entry) = settings_obj.get_mut(group) {
                if let Some(obj) = entry.as_object_mut() {
                    obj.remove("color");
                    if obj.is_empty() {
                        settings_obj.remove(group);
                    }
                }
            }
        }
    }
    if settings_obj.is_empty() {
        doc.remove("groupSettings");
    }
    GroupColorWrite::Updated
}

/// Set or remove the top-level `controlAccount` key in `doc`. Reports whether
/// the document actually changed, so the caller can skip a pointless rewrite
/// of a credential file.
fn merge_control_account(doc: &mut Map<String, Value>, name: Option<&str>) -> ControlWrite {
    let desired = name.map(|n| Value::String(n.to_string()));
    if doc.get("controlAccount").cloned() == desired {
        return ControlWrite::Unchanged;
    }
    match desired {
        Some(value) => doc.insert("controlAccount".to_string(), value),
        None => doc.remove("controlAccount"),
    };
    ControlWrite::Updated
}

/// Where the ONE `accounts` entry carrying `target`'s identity lives — or why
/// there is no one entry to write into. Separate from [`find_account_entry`] so
/// the whole array can be scanned immutably (counting matches) before any mutable
/// borrow is taken; a single mutable pass cannot both count and hand back a
/// reference.
enum EntryMatch {
    /// Exactly one entry matches, at this index of the `accounts` array.
    One(usize),
    /// No usable `accounts` array, or nothing in it carries that identity.
    None,
    /// Two or more entries match.
    Many,
}

fn locate_account_entry(doc: &Map<String, Value>, target: &Account) -> EntryMatch {
    let Some(entries) = doc.get("accounts").and_then(Value::as_array) else {
        return EntryMatch::None;
    };
    // Parse every entry's identity first, then resolve over the whole set at once.
    // Returning `Many` on the second `same_identity` hit — which is what a single
    // scanning pass can do — refuses the LEGACY TWO-ORG SHAPE, where entry
    // `{name, uuid U, orgUuid "org-a"}` sits beside a pre-org entry `{name, uuid
    // U}` written before org UUIDs were stored. Those are two real accounts, one
    // person in two orgs, and `same_identity` matches the pre-org entry against
    // both of them because an unknown org is deliberately tolerated. Refused that
    // way, NEITHER account can ever be durably benched.
    //
    // `resolve` breaks exactly that tie and only that tie: when one candidate
    // matches on fully known identity and the rest matched only because something
    // was missing, the known one wins. A tie with nothing stricter to prefer is
    // still `Many`, and still refused.
    let probes: Vec<(usize, Account)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let object = entry.as_object()?;
            let stored =
                serde_json::from_value::<DiskIdentity>(Value::Object(object.clone())).ok()?;
            Some((
                index,
                crate::identity::probe(
                    &stored.name,
                    stored.account_uuid,
                    stored.org_uuid,
                    stored.org_name,
                ),
            ))
        })
        .collect();
    match crate::identity::resolve(probes.iter().map(|(index, probe)| (*index, probe)), target) {
        crate::identity::Resolved::One(index) => EntryMatch::One(index),
        crate::identity::Resolved::None => EntryMatch::None,
        crate::identity::Resolved::Many => EntryMatch::Many,
    }
}

/// The on-disk `accounts` entry whose identity matches `target`, or the
/// [`DisabledWrite`] refusal explaining why there is no single entry to write.
///
/// Matching reuses the [`DiskIdentity`] probe + [`crate::identity::resolve`]
/// pairing that [`merge_tokens`] uses, so a rotated credential and a disabled flag
/// can never land on two different entries.
///
/// An AMBIGUOUS identity is refused, not resolved to the first match. The old
/// first-match-wins rested on "the CLI already refuses an ambiguous query", but
/// the TUI is a caller that never goes through that check — it selects by row
/// index — and `config::load` validates no uniqueness, so nothing upstream makes
/// the identity unique. `same_identity` falls back to name equality when either
/// side lacks a uuid, so two entries sharing a name match either runtime row: the
/// flag would land on whichever is earlier in the file, benching a healthy account
/// while the TUI shows the other one disabled.
///
/// Refused, but only where the tie is real — which is not the same as "a second
/// entry satisfied `same_identity`". `resolve` first tries the org key, so the
/// legacy two-org shape (one entry with an org, one written before org UUIDs
/// existed and carrying none) resolves both of its rows instead of locking both
/// out of ever being benched.
fn find_account_entry<'a>(
    doc: &'a mut Map<String, Value>,
    target: &Account,
) -> Result<&'a mut Map<String, Value>, DisabledWrite> {
    let index = match locate_account_entry(doc, target) {
        EntryMatch::One(index) => index,
        EntryMatch::None => return Err(DisabledWrite::NoEntry),
        EntryMatch::Many => return Err(DisabledWrite::Ambiguous),
    };
    // The immutable scan above proved this path resolves; `NoEntry` here is the
    // total-function tail, not a reachable outcome.
    doc.get_mut("accounts")
        .and_then(Value::as_array_mut)
        .and_then(|entries| entries.get_mut(index))
        .and_then(Value::as_object_mut)
        .ok_or(DisabledWrite::NoEntry)
}

/// What a targeted [`save_account`] upsert did to the on-disk document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountWrite {
    /// No entry on disk carried this identity — a new one was appended, holding
    /// every field `account` carries (identity, credentials, and routing
    /// knobs) — with one deliberate exception: an ABSENT `priority` is filled
    /// in as `max(existing priorities) + 1`, joining the back of the fleet,
    /// rather than left absent (which reads as 0 at runtime and silently
    /// promotes the new account to the primary tier). An explicit `priority`
    /// the caller submitted is never overridden. See [`merge_account`]'s
    /// doc-comment on the Added arm for the full rationale.
    Added,
    /// Exactly one entry carried this identity — its CREDENTIAL fields
    /// (`accessToken` / `refreshToken` / `expiresAt`) were updated in place,
    /// `type` was stamped to `account.account_type` unconditionally, and any
    /// of `accountUuid` / `orgUuid` / `orgName` the entry was MISSING got
    /// backfilled from `account` (never overwriting a value already present —
    /// this is what permanently stops a legacy no-org entry from loosely
    /// matching every org variant of that person forever). `priority`,
    /// `disabled`, `switchThreshold`, and any unmodelled `extra` key are
    /// untouched, exactly as [`merge_tokens`] leaves them.
    Updated,
    /// More than one on-disk entry carries this identity; nothing was written.
    /// Same refusal as [`DisabledWrite::Ambiguous`], for the same reason: a
    /// write here would have to guess which entry to overwrite.
    Ambiguous,
    /// The document has an `accounts` key that exists but is not a JSON array —
    /// too corrupt a shape to append into blindly. Nothing was written.
    Unwritable,
}

impl std::fmt::Display for AccountWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Added => "added",
            Self::Updated => "updated",
            Self::Ambiguous => "more than one entry on disk carries this identity",
            Self::Unwritable => "the accounts key is not an array",
        })
    }
}

/// Insert-or-replace, by identity, the ONE `accounts` entry matching `account` —
/// the durable half of live account-add (`POST /_tcr/accounts`,
/// [`crate::manager::Manager::add_or_update_account`]).
///
/// Sibling of [`save_disabled`], and it shares its whole shape: read the raw
/// document, mutate ONLY what needs to change, write back atomically only if
/// something did. It differs from `save_disabled` in exactly one place — a MISS
/// is not a refusal here. `save_disabled` refuses on [`DisabledWrite::NoEntry`]
/// because its `target` is an identity-only probe with no real credential to
/// write ([`crate::identity::probe`] leaves `access_token` empty); appending it
/// would create a useless entry. `account` here is always a COMPLETE record —
/// either a brand-new login or an existing account's own identity with fresh
/// credentials merged in (`Manager::persist_replaced`) — so a miss is filled in
/// rather than refused. That is also why `find_account_entry` (which returns
/// only the refuse-on-miss `DisabledWrite`) is not reused directly here; this
/// calls the lower-level [`locate_account_entry`] it is itself built on, so both
/// functions still run the identical resolution scan.
///
/// An identity matching MORE than one on-disk entry still refuses: guessing
/// which one to overwrite risks stamping fresh credentials over a DIFFERENT
/// account's single-use refresh token — exactly the failure `find_account_entry`
/// exists to rule out.
pub fn save_account(path: &Path, account: &Account) -> Result<AccountWrite, ConfigError> {
    let mut doc = read_document(path)?;
    let outcome = merge_account(&mut doc, account)?;
    if matches!(outcome, AccountWrite::Added | AccountWrite::Updated) {
        write_atomic(path, &serde_json::to_string_pretty(&doc)?)?;
    }
    Ok(outcome)
}

/// Insert-or-replace `account` by identity in `doc`. See [`save_account`].
fn merge_account(
    doc: &mut Map<String, Value>,
    account: &Account,
) -> Result<AccountWrite, ConfigError> {
    match locate_account_entry(doc, account) {
        EntryMatch::Many => Ok(AccountWrite::Ambiguous),
        EntryMatch::One(index) => {
            // Proven present by the immutable scan `locate_account_entry` just
            // ran; this chain cannot miss in practice, but a save must never
            // silently drop a credential if it somehow does.
            if let Some(entry) = doc
                .get_mut("accounts")
                .and_then(Value::as_array_mut)
                .and_then(|entries| entries.get_mut(index))
                .and_then(Value::as_object_mut)
            {
                let credentials = Credentials {
                    access_token: &account.access_token,
                    refresh_token: account.refresh_token.as_deref(),
                    expires_at: account.expires_at,
                };
                if let Value::Object(fields) = serde_json::to_value(&credentials)? {
                    entry.extend(fields);
                }
                // `type` always wins — `save_account` is only ever called with a
                // real credential, so a stale stored type (e.g. an API-key row
                // re-added as OAuth) is corrected here; leaving it wrong is a
                // silent death (`refresh_plan` skips any account whose type is
                // not `"oauth"`).
                //
                // Identity fields are BACKFILLED — written only where the entry
                // does not already carry a value, never overwriting one that is
                // present. `locate_account_entry` may have matched loosely (the
                // org-unknown tolerance), so this is what turns a legacy no-org
                // entry into a fully-known one: the NEXT submission for a
                // genuinely different org of the same person then requires an
                // exact match instead of tolerating the still-unknown org key,
                // closing the split-brain trigger for good rather than only
                // for this one write.
                entry.insert(
                    "type".to_string(),
                    Value::String(account.account_type.clone()),
                );
                let present = |entry: &Map<String, Value>, key: &str| matches!(entry.get(key), Some(Value::String(s)) if !s.is_empty());
                if !present(entry, "accountUuid") {
                    if let Some(uuid) = &account.account_uuid {
                        entry.insert("accountUuid".to_string(), Value::String(uuid.clone()));
                    }
                }
                if !present(entry, "orgUuid") {
                    if let Some(uuid) = &account.org_uuid {
                        entry.insert("orgUuid".to_string(), Value::String(uuid.clone()));
                    }
                }
                if !present(entry, "orgName") {
                    if let Some(name) = &account.org_name {
                        entry.insert("orgName".to_string(), Value::String(name.clone()));
                    }
                }
            }
            Ok(AccountWrite::Updated)
        }
        EntryMatch::None => {
            // Distinguished from "no `accounts` key at all" (fine — create it)
            // so a document where `accounts` is present but the wrong JSON type
            // is never silently overwritten with a fresh single-element array.
            if matches!(doc.get("accounts"), Some(v) if !v.is_array()) {
                return Ok(AccountWrite::Unwritable);
            }
            let entries = doc
                .entry("accounts".to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            let Value::Array(entries) = entries else {
                // Unreachable given the check above, but never index into a
                // shape that was not actually verified to be an array.
                return Ok(AccountWrite::Unwritable);
            };
            let mut new_entry = serde_json::to_value(account)?;
            // An appended account with no explicit priority joins the BACK
            // of the fleet — `max(existing priorities) + 1` — rather than
            // being left absent. `priority` is deliberate-default here, not
            // incidental: `skip_serializing_if` means a `None` would otherwise
            // serialize to no key at all, and an absent `priority` reads as 0
            // at runtime (`AccountRuntime::from_config`'s `unwrap_or(0)`),
            // which silently promotes a freshly added account to the PRIMARY
            // tier ahead of the established fleet. Mirrors
            // `oauth::upsert_account`'s historical default. Skipped when the
            // caller submitted an explicit priority (the documented
            // new-account case — see `AddAccountRequest`'s doc-comment in
            // `proxy.rs`), which is never overridden.
            if account.priority.is_none() {
                let next_priority = entries
                    .iter()
                    .filter_map(|e| e.get("priority").and_then(Value::as_i64))
                    .max()
                    .map_or(0, |max| max + 1);
                if let Value::Object(fields) = &mut new_entry {
                    fields.insert("priority".to_string(), Value::from(next_priority));
                }
            }
            entries.push(new_entry);
            Ok(AccountWrite::Added)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = r#"{
      "proxy": { "port": 3456, "apiKey": "sk-proxy-secret", "customFlag": true },
      "upstream": "https://api.anthropic.com",
      "switchThreshold": 0.9,
      "quotaProbeSeconds": 120,
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        {
          "name": "acct-a",
          "type": "oauth",
          "accountUuid": "uuid-a",
          "orgName": "Org A",
          "accessToken": "at-a",
          "refreshToken": "rt-a",
          "expiresAt": 1893456000000,
          "priority": 0,
          "models": ["claude-fable-5"]
        }
      ]
    }"#;

    #[test]
    fn load_parses_known_fields() {
        let config: Config = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(config.proxy.port, 3456);
        assert_eq!(config.proxy.api_key.as_deref(), Some("sk-proxy-secret"));
        assert_eq!(config.switch_threshold, 0.9);
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "acct-a");
        assert_eq!(config.accounts[0].expires_at, Some(1893456000000));
    }

    #[test]
    fn save_round_trip_preserves_unknown_fields() {
        let config: Config = serde_json::from_str(SAMPLE).unwrap();
        let tmp = std::env::temp_dir().join(format!("tcr-cfg-{}.json", std::process::id()));
        save(&tmp, &config).unwrap();
        let reloaded = fs::read_to_string(&tmp).unwrap();
        let value: Value = serde_json::from_str(&reloaded).unwrap();

        // Unmodelled top-level keys survive.
        assert_eq!(value["quotaProbeSeconds"], serde_json::json!(120));
        assert!(value["routes"].is_array());
        // Unmodelled proxy key survives.
        assert_eq!(value["proxy"]["customFlag"], serde_json::json!(true));
        // Unmodelled per-account key survives.
        assert_eq!(
            value["accounts"][0]["models"],
            serde_json::json!(["claude-fable-5"])
        );

        fs::remove_file(&tmp).ok();
    }

    /// An account config carrying `groups` survives a load→save round-trip
    /// unchanged, and — via [`SAMPLE`], whose one account has no `groups` key —
    /// an account without the key still loads (defaulting to `None`/no groups).
    #[test]
    fn groups_round_trip_and_are_optional() {
        // No `groups` key at all: loads fine, defaults to no groups.
        let config: Config = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(config.accounts[0].groups, None);
        assert!(!config.accounts[0].in_group("codereview"));

        // A `groups` key present: round-trips byte-for-byte through save→reload.
        let with_groups = r#"{
          "accounts": [
            {
              "name": "acct-grouped",
              "type": "oauth",
              "accessToken": "at-grouped",
              "priority": 0,
              "groups": ["codereview", "burst"]
            }
          ]
        }"#;
        let config: Config = serde_json::from_str(with_groups).unwrap();
        assert_eq!(
            config.accounts[0].groups,
            Some(vec!["codereview".to_string(), "burst".to_string()])
        );
        assert!(config.accounts[0].in_group("codereview"));
        assert!(
            !config.accounts[0].in_group("CodeReview"),
            "matching is case-sensitive"
        );
        assert!(!config.accounts[0].in_group("nope"));

        let tmp = std::env::temp_dir().join(format!("tcr-cfg-groups-{}.json", std::process::id()));
        save(&tmp, &config).unwrap();
        let reloaded: Config = serde_json::from_str(&fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(reloaded.accounts[0].groups, config.accounts[0].groups);
        fs::remove_file(&tmp).ok();
    }

    // --- load: skip unusable accounts, keep the rest -------------------------

    /// One account with no `accessToken` (an `importFrom`-shaped entry, e.g.:
    /// upstream reads the credential from another file at startup — not
    /// implemented here) does not take the other, usable accounts down with
    /// it, and the skipped account is named in a warning rather than silently
    /// vanishing.
    #[test]
    fn load_skips_one_unusable_account_and_keeps_the_rest() {
        let path = tmp_path("load-skip-one");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "acct-good", "accessToken": "at-good" },
                 { "name": "acct-import", "importFrom": "/some/other/file.json" }
               ] }"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "acct-good");
        assert_eq!(config.accounts[0].access_token, "at-good");
        fs::remove_file(&path).ok();
    }

    /// An `apikey` account with no `apiKey` is unusable by the same rule an
    /// `oauth` account with no `accessToken` is — skipped, not fatal.
    #[test]
    fn load_skips_apikey_account_missing_api_key() {
        let path = tmp_path("load-skip-apikey");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "acct-good", "accessToken": "at-good" },
                 { "name": "acct-apikey", "type": "apikey" }
               ] }"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "acct-good");
        fs::remove_file(&path).ok();
    }

    /// Every account unusable is still a load error — an empty fleet must
    /// never boot silently — reported through the same `ConfigError::Parse`
    /// shape a malformed file already uses.
    #[test]
    fn load_errors_when_every_account_is_unusable() {
        let path = tmp_path("load-all-unusable");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "acct-import", "importFrom": "/some/other/file.json" }
               ] }"#,
        )
        .unwrap();

        let err = load(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse(_)),
            "expected a Parse-shaped error, got {err:?}"
        );
        fs::remove_file(&path).ok();
    }

    /// An empty `accounts` list is not "all unusable" — it is unchanged,
    /// pre-existing, valid behaviour (no accounts configured at all).
    #[test]
    fn load_tolerates_empty_accounts_list() {
        let path = tmp_path("load-empty-accounts");
        fs::write(&path, r#"{ "accounts": [] }"#).unwrap();
        let config = load(&path).unwrap();
        assert!(config.accounts.is_empty());
        fs::remove_file(&path).ok();
    }

    // --- group color ---------------------------------------------------------

    #[test]
    fn validate_hex_color_accepts_both_forms_case_insensitively_and_normalizes() {
        assert_eq!(validate_hex_color("#32d74b"), Ok("#32d74b".to_string()));
        assert_eq!(validate_hex_color("#32D74B"), Ok("#32d74b".to_string()));
        assert_eq!(validate_hex_color("#ABC"), Ok("#aabbcc".to_string()));
        assert_eq!(validate_hex_color("#abc"), Ok("#aabbcc".to_string()));
    }

    #[test]
    fn validate_hex_color_rejects_garbage() {
        for bad in [
            "32d74b",  // missing '#'
            "#32d74",  // 5 digits: neither #RGB nor #RRGGBB
            "#zzzzzz", // non-hex characters
            "#12345g", // trailing non-hex character
            "",        // empty
            "#",       // '#' alone
        ] {
            assert!(validate_hex_color(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    // --- derived color band: OKLCH lightness/gamut/APCA (problem 2) ---------

    fn srgb_byte_to_linear(v: u8) -> f64 {
        let v = v as f64 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    /// The OKLab `(L, a, b)` a produced `#rrggbb` ACTUALLY measures at —
    /// independent of [`oklch_to_linear_srgb`] (the FORWARD conversion under
    /// test), via a standalone inverse. So a test built on this is not merely
    /// checking that the forward function returns the constant/hue it was
    /// told to return; it checks what the byte-quantized output really is.
    fn measured_oklab_lab(hex: &str) -> (f64, f64, f64) {
        let (r, g, b) = hex_to_rgb(hex);
        let (r, g, b) = (
            srgb_byte_to_linear(r),
            srgb_byte_to_linear(g),
            srgb_byte_to_linear(b),
        );
        let l_ = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
        let m_ = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
        let s_ = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
        let l = 0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_;
        let a = 1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_;
        let b = 0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_;
        (l, a, b)
    }

    fn measured_oklab_l(hex: &str) -> f64 {
        measured_oklab_lab(hex).0
    }

    /// Wiring check: [`derive_group_color`] itself (not just the
    /// `oklch_to_hex`/`DERIVED_GROUP_COLOR_*` machinery the tests below probe
    /// directly) actually emits the fixed-lightness band for real group
    /// names, so a regression that decouples `derive_group_color` from that
    /// machinery — e.g. reintroducing a per-hue-varying lightness inside
    /// `derive_group_color`'s own body while leaving the constants and
    /// `oklch_to_hex` untouched — is caught here even though the other
    /// OKLCH tests below would not see it.
    #[test]
    fn derive_group_color_actually_uses_the_fixed_lightness_band() {
        for name in ["codereview", "dev", "burst", "ops", "night-shift", "qa"] {
            let hex = derive_group_color(name);
            let l = measured_oklab_l(&hex);
            assert!(
                (l - DERIVED_GROUP_COLOR_L).abs() < 0.02,
                "derive_group_color({name:?}) = {hex} measures OKLab L={l:.4}, \
                 expected ~{DERIVED_GROUP_COLOR_L}"
            );
        }
    }

    /// THE test for the perceptual-uniformity half of problem 2: every hue on
    /// the derived-color wheel measures the SAME OKLab `L` (up to 8-bit
    /// rounding noise), unlike the old HSL band which measured L 0.518–0.852.
    /// Measured (via `measured_oklab_l`, an independent inverse conversion,
    /// not `derive_group_color`'s own forward math): spread is ~0.003, so
    /// 0.02 is a generous margin over quantization noise while still catching
    /// a real regression back toward HSL's ~0.33 spread.
    #[test]
    fn derived_colors_share_one_oklch_lightness_across_the_wheel() {
        let mut min_l = f64::MAX;
        let mut max_l = f64::MIN;
        for hue in 0..360u32 {
            let hex = oklch_to_hex(DERIVED_GROUP_COLOR_L, DERIVED_GROUP_COLOR_C, hue as f64);
            let l = measured_oklab_l(&hex);
            min_l = min_l.min(l);
            max_l = max_l.max(l);
        }
        let spread = max_l - min_l;
        assert!(
            spread < 0.02,
            "OKLCH lightness spread across the wheel is {spread:.4}, expected near-zero \
             (measured L range {min_l:.4}..{max_l:.4})"
        );
    }

    /// THE test for the readability half of problem 2: black text over every
    /// derived color on the wheel clears the APCA `|Lc| >= 60` floor —
    /// unlike the old HSL band, whose hue-30 slice scored 52.5/56.5 in
    /// EITHER text polarity. Measured worst case with this implementation is
    /// ~65.9 and best case ~71.7 (the bridge's own APCA implementation
    /// measured 67.8–72.9 on the same design; the two-point difference is
    /// implementation variance in the two independently-derived reference
    /// algorithms, not a gamut/lightness defect — both clear the floor by a
    /// wide margin).
    #[test]
    fn derived_colors_clear_the_apca_floor_at_every_hue() {
        let mut worst = f64::MAX;
        let mut worst_hue = 0u32;
        for hue in 0..360u32 {
            let hex = oklch_to_hex(DERIVED_GROUP_COLOR_L, DERIVED_GROUP_COLOR_C, hue as f64);
            let apca = best_readable_apca(&hex);
            if apca < worst {
                worst = apca;
                worst_hue = hue;
            }
        }
        assert!(
            worst >= 60.0,
            "hue {worst_hue} scores APCA |Lc| {worst:.1}, below the 60 floor for non-body text"
        );
    }

    /// Every derived color is fully in-gamut: converting it BACK through the
    /// same OKLab math it was derived from must not require clamping any
    /// linear-sRGB channel outside `[0, 1]` (beyond `GAMUT_EPS` float noise).
    /// Guards against a regression back to clipping (which silently shifts
    /// hue) rather than reducing chroma.
    #[test]
    fn derived_colors_are_fully_in_gamut_no_channel_clipped() {
        for hue in 0..360u32 {
            let hex = oklch_to_hex(DERIVED_GROUP_COLOR_L, DERIVED_GROUP_COLOR_C, hue as f64);
            // Independent of `oklch_to_gamut_srgb`'s own internals (comparing
            // its output to itself would be tautological and blind to a bug
            // INSIDE that function, e.g. a naive clamp instead of a chroma
            // reduction): reconstruct the EMITTED color's actual OKLab (a, b)
            // by inverting the byte-quantized hex, and check its hue matches
            // the requested hue. A naive per-channel clamp distorts hue (it
            // moves a/b independently); a correct chroma reduction scales a
            // and b together, which by construction preserves
            // `atan2(b, a)`. So a hue mismatch here is direct, independent
            // evidence of clipping regardless of how the implementation
            // reached it.
            let (_, a, b) = measured_oklab_lab(&hex);
            let chroma = a.hypot(b);
            assert!(
                chroma > 1e-4,
                "hue {hue}: emitted color {hex} is achromatic (chroma {chroma:.5}) — \
                 the fixed-lightness band should never fully desaturate"
            );
            let measured_hue = b.atan2(a).to_degrees().rem_euclid(360.0);
            let mut delta = (measured_hue - hue as f64).abs();
            if delta > 180.0 {
                delta = 360.0 - delta;
            }
            assert!(
                delta < 1.0,
                "hue {hue}: emitted color {hex} measures hue {measured_hue:.2}° \
                 (drifted {delta:.2}°) — a channel was clipped instead of chroma reduced"
            );
        }
    }

    /// [`best_readable_apca`] against a known-bad color from the OLD HSL band
    /// (hue 30, S 62%, L 58% — the bridge's own illegible example) actually
    /// scores below the 60 floor with this implementation too — otherwise the
    /// two tests above would be trivially true no matter how the floor is
    /// computed.
    #[test]
    fn best_readable_apca_flags_the_old_bands_illegible_hue() {
        // Recompute the OLD HSL(30, 0.62, 0.58) color — the removed
        // `hsl_to_hex(30.0, 0.62, 0.58)` this replaced — via a standalone
        // HSL-to-RGB, so this test does not hand-guess the hex and instead
        // proves the NEW APCA implementation genuinely rejects the OLD band's
        // documented illegible hue, not just that it agrees with the
        // bridge's own numbers.
        fn old_hsl_to_hex(h: f64, s: f64, l: f64) -> String {
            let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
            let h_prime = h / 60.0;
            let x = c * (1.0 - (h_prime.rem_euclid(2.0) - 1.0).abs());
            let (r1, g1, b1) = match h_prime as u32 {
                0 => (c, x, 0.0),
                1 => (x, c, 0.0),
                2 => (0.0, c, x),
                3 => (0.0, x, c),
                4 => (x, 0.0, c),
                _ => (c, 0.0, x),
            };
            let m = l - c / 2.0;
            let to_byte = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
            format!("#{:02x}{:02x}{:02x}", to_byte(r1), to_byte(g1), to_byte(b1))
        }
        let old_hsl_hue_30 = old_hsl_to_hex(30.0, 0.62, 0.58);
        let apca = best_readable_apca(&old_hsl_hue_30);
        assert!(
            apca < 60.0,
            "the known-illegible old-band color {old_hsl_hue_30} scored {apca:.1}, expected < 60 \
             (sanity check that best_readable_apca is a real gate, not a rubber stamp)"
        );
    }

    #[test]
    fn save_group_color_round_trips_and_clear_drops_the_key() {
        let tmp = std::env::temp_dir().join(format!("tcr-cfg-color-{}.json", std::process::id()));
        fs::write(&tmp, GROUPED_ACCOUNTS_JSON).unwrap();

        let outcome = save_group_color(&tmp, "codereview", Some("#32d74b")).unwrap();
        assert_eq!(outcome, GroupColorWrite::Updated);
        let config: Config = serde_json::from_str(&fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(
            config.group_color("codereview"),
            ("#32d74b".to_string(), ColorSource::Set)
        );

        // Setting the SAME value again is a no-op write.
        let outcome = save_group_color(&tmp, "codereview", Some("#32d74b")).unwrap();
        assert_eq!(outcome, GroupColorWrite::Unchanged);

        // Clearing drops the `color` key (and the whole `groupSettings.<group>`
        // entry, since color was the only thing in it) — the derived value
        // returns.
        let outcome = save_group_color(&tmp, "codereview", None).unwrap();
        assert_eq!(outcome, GroupColorWrite::Updated);
        let config: Config = serde_json::from_str(&fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(
            config.group_color("codereview"),
            (derive_group_color("codereview"), ColorSource::Derived)
        );
        let raw = fs::read_to_string(&tmp).unwrap();
        assert!(
            !raw.contains("groupSettings"),
            "the last property on the group's settings entry is gone, so the \
             whole map is dropped rather than left as `{{}}`: {raw}"
        );

        fs::remove_file(&tmp).ok();
    }

    #[test]
    fn all_group_names_is_the_union_of_every_accounts_groups() {
        let config: Config = serde_json::from_str(GROUPED_ACCOUNTS_JSON).unwrap();
        let names: Vec<String> = config.all_group_names().into_iter().collect();
        assert_eq!(names, vec!["codereview".to_string(), "dev".to_string()]);
    }

    #[test]
    fn group_colors_covers_every_fleet_group_including_unconfigured_ones() {
        let tmp = std::env::temp_dir().join(format!("tcr-cfg-colors-{}.json", std::process::id()));
        fs::write(&tmp, GROUPED_ACCOUNTS_JSON).unwrap();
        save_group_color(&tmp, "dev", Some("#0a84ff")).unwrap();
        let config: Config = serde_json::from_str(&fs::read_to_string(&tmp).unwrap()).unwrap();

        let colors = config.group_colors();
        assert_eq!(colors.len(), 2, "both codereview and dev are present");
        assert_eq!(colors["dev"], "#0a84ff");
        assert_eq!(colors["codereview"], derive_group_color("codereview"));

        fs::remove_file(&tmp).ok();
    }

    /// Two accounts, `alice` in `codereview` and `dev`, `bob` in `codereview`
    /// only — enough to exercise a shared group and a group with only one
    /// member without a third fixture.
    const GROUPED_ACCOUNTS_JSON: &str = r#"{
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
          "priority": 0, "groups": ["codereview", "dev"] },
        { "name": "bob@example.com", "type": "oauth", "accessToken": "at-b",
          "priority": 1, "groups": ["codereview"] }
      ]
    }"#;

    #[test]
    fn save_writes_owner_only_permissions() {
        let config: Config = serde_json::from_str(SAMPLE).unwrap();
        let tmp = std::env::temp_dir().join(format!("tcr-cfg-perm-{}.json", std::process::id()));
        save(&tmp, &config).unwrap();
        let mode = fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_file(&tmp).ok();
    }

    #[test]
    fn defaults_apply_to_minimal_config() {
        let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        assert_eq!(config.proxy.port, 3456);
        assert_eq!(config.upstream, "https://api.anthropic.com");
        assert_eq!(config.switch_threshold, 0.95);
        // Absent `pacing` key → pacing OFF: no in-flight cap, no min-spacing.
        assert_eq!(config.pacing.max_in_flight_per_account, None);
        assert_eq!(config.pacing.min_spacing_ms, None);
        assert!(!config.pacing.is_active());
        // Absent `http1Only` key → OFF: the serving client stays on h2.
        assert!(!config.http1_only);
    }

    #[test]
    fn http1_only_true_deserializes() {
        let config: Config =
            serde_json::from_str(r#"{ "accounts": [], "http1Only": true }"#).unwrap();
        assert!(config.http1_only);
    }

    #[test]
    fn default_pacing_ships_off() {
        // Guards the DEFAULT itself, not just deserialization: a per-account
        // concurrency cap costs prompt-cache locality, so it must stay opt-in.
        // If this flips, pacing was silently turned back on for every user.
        let pacing = default_pacing();
        assert_eq!(pacing.max_in_flight_per_account, None);
        assert_eq!(pacing.min_spacing_ms, None);
        assert_eq!(pacing.effective_max_in_flight(), None);
        assert!(!pacing.is_active());
    }

    #[test]
    fn empty_pacing_object_disables_pacing() {
        // `"pacing": {}` spells out what the default already is: both knobs
        // None → inert. Kept so a config that writes the key stays supported.
        let config: Config = serde_json::from_str(r#"{ "accounts": [], "pacing": {} }"#).unwrap();
        assert_eq!(config.pacing.max_in_flight_per_account, None);
        assert_eq!(config.pacing.min_spacing_ms, None);
        assert!(!config.pacing.is_active());
    }

    #[test]
    fn explicit_pacing_overrides_the_default() {
        let config: Config = serde_json::from_str(
            r#"{ "accounts": [], "pacing": { "maxInFlightPerAccount": 5, "minSpacingMs": 200 } }"#,
        )
        .unwrap();
        assert_eq!(config.pacing.max_in_flight_per_account, Some(5));
        assert_eq!(config.pacing.min_spacing_ms, Some(200));
    }

    #[test]
    fn absent_throttle_defaults_on() {
        // No `throttle` key → default_throttle → ON with evidence-anchored knobs.
        let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        assert!(config.throttle.is_active());
        assert_eq!(config.throttle.effective_min_spacing(), Some(350));
        assert_eq!(config.throttle.effective_burst(), 4);
    }

    #[test]
    fn empty_throttle_object_disables_throttle() {
        // `"throttle": {}` is the escape hatch: empty object → both knobs None → inert.
        let config: Config = serde_json::from_str(r#"{ "accounts": [], "throttle": {} }"#).unwrap();
        assert_eq!(config.throttle.min_spacing_ms, None);
        assert_eq!(config.throttle.burst, None);
        assert!(!config.throttle.is_active());
    }

    #[test]
    fn explicit_throttle_enables() {
        let config: Config = serde_json::from_str(
            r#"{ "accounts": [], "throttle": { "minSpacingMs": 350, "burst": 5 } }"#,
        )
        .unwrap();
        assert!(config.throttle.is_active());
        assert_eq!(config.throttle.effective_min_spacing(), Some(350));
        assert_eq!(config.throttle.effective_burst(), 5);
    }

    #[test]
    fn lock_account_parses_when_present() {
        let config: Config =
            serde_json::from_str(r#"{ "accounts": [], "lockAccount": "acme" }"#).unwrap();
        assert_eq!(config.lock_account, Some("acme".to_string()));
    }

    #[test]
    fn lock_account_absent_defaults_none() {
        let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        assert_eq!(config.lock_account, None);
    }

    #[test]
    fn control_account_parses_when_present() {
        let config: Config =
            serde_json::from_str(r#"{ "accounts": [], "controlAccount": "alice@example.com" }"#)
                .unwrap();
        assert_eq!(
            config.control_account,
            Some("alice@example.com".to_string())
        );
    }

    #[test]
    fn control_account_absent_defaults_none() {
        let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        assert_eq!(config.control_account, None);
    }

    /// `skip_serializing_if` round trip: setting then serializing must NOT emit
    /// a `null`, only either the string or an absent key — this is what makes
    /// `save_control_account`'s clear-removes-the-key contract representable.
    #[test]
    fn control_account_absent_does_not_serialize_a_null() {
        let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
        let json = serde_json::to_value(&config).unwrap();
        assert!(
            json.get("controlAccount").is_none(),
            "an absent control account must not serialize at all: {json}"
        );
    }

    /// A unique temp path per test — the suite runs tests in parallel threads of
    /// ONE process, so a pid-only name collides.
    fn tmp_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("tcr-{tag}-{}-{seq}.json", std::process::id()))
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).expect("read persisted config"))
            .expect("persisted config is valid JSON")
    }

    /// One account, tokens as given, written as the file the server booted from.
    fn one_account_file(access: &str, refresh: &str, expires: i64) -> String {
        format!(
            r#"{{ "accounts": [ {{ "name": "acct-a", "accessToken": "{access}", "refreshToken": "{refresh}", "expiresAt": {expires} }} ] }}"#
        )
    }

    /// THE regression guard. Reproduces what happened live on 2026-07-25: the
    /// server booted with `pacing.maxInFlightPerAccount = 3`, the user deleted
    /// the key while it ran, and the next persist stamped the boot-time snapshot
    /// back over the file — so the deleted setting returned and the next boot
    /// read it. Persisting must write the rotated tokens and NOTHING else.
    #[test]
    fn persist_does_not_clobber_a_user_edit() {
        let path = tmp_path("persist-user-edit");
        fs::write(
            &path,
            r#"{ "pacing": { "maxInFlightPerAccount": 3 },
                 "accounts": [ { "name": "acct-a", "accessToken": "at-old", "refreshToken": "rt-old", "expiresAt": 1 } ] }"#,
        )
        .unwrap();

        // The server's boot-time snapshot still carries the setting…
        let mut memory = load(&path).unwrap();
        assert_eq!(memory.pacing.max_in_flight_per_account, Some(3));
        // …the user deletes it while the proxy runs…
        fs::write(&path, one_account_file("at-old", "rt-old", 1)).unwrap();
        // …and a token rotates, triggering a persist.
        memory.accounts[0].access_token = "at-new".to_string();
        memory.accounts[0].refresh_token = Some("rt-new".to_string());
        memory.accounts[0].expires_at = Some(2);
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert!(
            value.get("pacing").is_none(),
            "the server restored a key the user deleted: {value}"
        );
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-new"));
        assert_eq!(value["accounts"][0]["expiresAt"], json!(2));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_preserves_unknown_top_level_keys() {
        let path = tmp_path("persist-unknown");
        fs::write(&path, one_account_file("at-old", "rt-old", 1)).unwrap();
        let mut memory = load(&path).unwrap();

        // The user adds keys the server does not model — including one it has
        // never seen — then a token rotates.
        fs::write(
            &path,
            r#"{ "quotaProbeSeconds": 120,
                 "routes": [{ "name": "r1", "match": "*fable*" }],
                 "accounts": [ { "name": "acct-a", "accessToken": "at-old", "refreshToken": "rt-old", "expiresAt": 1, "models": ["claude-fable-5"] } ] }"#,
        )
        .unwrap();
        memory.accounts[0].access_token = "at-new".to_string();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["quotaProbeSeconds"], json!(120));
        assert!(value["routes"].is_array());
        assert_eq!(value["accounts"][0]["models"], json!(["claude-fable-5"]));
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_falls_back_when_file_is_unreadable() {
        let path = tmp_path("persist-missing");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // No file at all (deleted under the running server, or a first-boot path
        // that has yet to create it): the rotated tokens must still land.
        assert!(!path.exists());
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-new"));
        assert_eq!(value["accounts"][0]["expiresAt"], json!(7));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_falls_back_when_file_is_malformed() {
        let path = tmp_path("persist-malformed");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // A single-use refresh token is worth more than an unparseable file.
        fs::write(&path, "{ this is not json").unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            read_json(&path)["accounts"][0]["refreshToken"],
            json!("rt-new")
        );

        // Valid JSON that is not an object takes the same path.
        fs::write(&path, "[]").unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            read_json(&path)["accounts"][0]["refreshToken"],
            json!("rt-new")
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_matches_accounts_by_name_not_index() {
        let path = tmp_path("persist-by-name");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-new", "refreshToken": "rt-a-new", "expiresAt": 11 },
                   { "name": "acct-b", "accessToken": "at-b-new", "refreshToken": "rt-b-new", "expiresAt": 22 } ] }"#,
        )
        .unwrap();
        // The user reorders the accounts on disk while the proxy runs. Index 0 in
        // memory is acct-a; index 0 on disk is now acct-b.
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-b", "accessToken": "at-b-old" },
                   { "name": "acct-a", "accessToken": "at-a-old" } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["name"], json!("acct-b"));
        assert_eq!(
            value["accounts"][0]["accessToken"],
            json!("at-b-new"),
            "tokens landed by position, not identity"
        );
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-b-new"));
        assert_eq!(value["accounts"][1]["name"], json!("acct-a"));
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-a-new"));
        assert_eq!(value["accounts"][1]["expiresAt"], json!(11));
        fs::remove_file(&path).ok();
    }

    /// **THE token-clobber guard.** Two on-disk entries whose identities both match
    /// one loaded account, with nothing stored to tell them apart. The old
    /// first-match resolution walked the array and stamped the SAME account's
    /// freshly rotated credential onto both, so the second entry's own single-use
    /// refresh token was destroyed on disk — its next refresh 400s
    /// (`invalid_grant`) and the account is dead until re-authed by hand.
    ///
    /// Nothing may be written into either entry: an unbreakable tie is reported,
    /// never guessed. The entries keep the credentials they already held, which is
    /// recoverable; a foreign account's token in their place is not.
    #[test]
    fn persist_refuses_to_write_a_credential_into_an_ambiguous_entry() {
        let path = tmp_path("persist-ambiguous");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-new", "refreshToken": "rt-new", "expiresAt": 99 } ] }"#,
        )
        .unwrap();
        // Two entries share the name and carry no UUID, so `same_identity` matches
        // the loaded account against both. Entry 1 holds its OWN refresh token.
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-one-old", "refreshToken": "rt-one-old" },
                   { "name": "acct-a", "accessToken": "at-two-old", "refreshToken": "rt-two-old" } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(
            value["accounts"][1]["refreshToken"],
            json!("rt-two-old"),
            "the second entry's own single-use refresh token was overwritten: {value}"
        );
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-two-old"));
        assert_eq!(
            value["accounts"][0]["refreshToken"],
            json!("rt-one-old"),
            "a tie is refused on BOTH sides — picking the earlier entry is still a guess: {value}"
        );
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-one-old"));
        fs::remove_file(&path).ok();
    }

    /// The refusal above is reported, not silent: both entries come back as
    /// skipped, and the loaded account comes back as having found no home — which
    /// is what makes `save_tokens` warn by name that a rotated credential did not
    /// reach the file.
    #[test]
    fn an_ambiguous_entry_is_reported_as_skipped() {
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-new" } ] }"#,
        )
        .unwrap();
        let mut doc: Map<String, Value> = serde_json::from_str(
            r#"{ "accounts": [ { "name": "acct-a" }, { "name": "acct-a" } ] }"#,
        )
        .unwrap();

        let report = merge_tokens(&mut doc, &memory).unwrap();

        assert_eq!(
            report.skipped,
            vec![
                SkippedEntry {
                    index: 0,
                    name: Some("acct-a".to_string()),
                    reason: SkipReason::Ambiguous,
                },
                SkippedEntry {
                    index: 1,
                    name: Some("acct-a".to_string()),
                    reason: SkipReason::Ambiguous,
                },
            ]
        );
        assert_eq!(report.absent_from_disk, vec!["acct-a".to_string()]);
    }

    /// The legacy two-org shape must keep working: one person, two orgs, where the
    /// older entry predates org UUIDs and so carries none. `same_identity` matches
    /// the pre-org entry against BOTH accounts, so resolving each entry
    /// independently ties forever and neither account's rotated token is ever
    /// persisted. The strict pairing settles it: each side has exactly one partner
    /// whose org key it actually equals.
    ///
    /// Asserted in BOTH disk orders — the real config has the older (pre-org) entry
    /// first, which is precisely the order a single forward pass gets wrong.
    #[test]
    fn persist_places_both_tokens_on_the_legacy_two_org_shape() {
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "me@example.com", "accountUuid": "u1", "orgUuid": "org-a",
                     "accessToken": "at-a-new", "refreshToken": "rt-a-new" },
                   { "name": "me@example.com", "accountUuid": "u1",
                     "accessToken": "at-legacy-new", "refreshToken": "rt-legacy-new" } ] }"#,
        )
        .unwrap();

        for (label, disk) in [
            (
                "org-carrying entry first",
                r#"{ "accounts": [
                       { "name": "me@example.com", "accountUuid": "u1", "orgUuid": "org-a", "accessToken": "at-a-old" },
                       { "name": "me@example.com", "accountUuid": "u1", "accessToken": "at-legacy-old" } ] }"#,
            ),
            (
                "pre-org entry first",
                r#"{ "accounts": [
                       { "name": "me@example.com", "accountUuid": "u1", "accessToken": "at-legacy-old" },
                       { "name": "me@example.com", "accountUuid": "u1", "orgUuid": "org-a", "accessToken": "at-a-old" } ] }"#,
            ),
        ] {
            let path = tmp_path("persist-two-org");
            fs::write(&path, disk).unwrap();
            save_tokens(&path, &memory).unwrap();

            let value = read_json(&path);
            let by_org = |org: Option<&str>| {
                value["accounts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e.get("orgUuid").and_then(Value::as_str) == org)
                    .unwrap_or_else(|| panic!("no entry with orgUuid {org:?} ({label}): {value}"))
                    .clone()
            };
            assert_eq!(
                by_org(Some("org-a"))["refreshToken"],
                json!("rt-a-new"),
                "the org-carrying entry missed its rotated token ({label}): {value}"
            );
            assert_eq!(
                by_org(None)["refreshToken"],
                json!("rt-legacy-new"),
                "the pre-org entry missed its rotated token ({label}): {value}"
            );
            fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn persist_does_not_resurrect_an_account_the_user_removed() {
        let path = tmp_path("persist-removed");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a" },
                   { "name": "acct-gone", "accessToken": "at-gone" } ] }"#,
        )
        .unwrap();
        fs::write(
            &path,
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-old" } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        let accounts = value["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 1, "a removed account came back: {value}");
        assert_eq!(accounts[0]["name"], json!("acct-a"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_leaves_an_account_the_server_never_loaded_untouched() {
        let path = tmp_path("persist-added");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-new" } ] }"#,
        )
        .unwrap();
        // The user added a second account by hand after boot.
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-old" },
                   { "name": "acct-new", "accessToken": "at-new", "refreshToken": "rt-new" } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-a-new"));
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][1]["refreshToken"], json!("rt-new"));
        fs::remove_file(&path).ok();
    }

    /// A document that parses but carries no `accounts` list at all — a
    /// truncated hand-edit, or a file another tool rewrote. The merge used to
    /// return before writing ANYTHING while `save_tokens` still reported `Ok`, so
    /// one write consumed and dropped the freshly rotated refresh token of every
    /// account at once. It is a malformed document, and takes the same
    /// whole-config fallback an unparseable one does.
    #[test]
    fn merge_with_missing_accounts_array_falls_back_and_warns() {
        let path = tmp_path("persist-no-accounts-key");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        let malformed = r#"{ "proxy": { "port": 3456 } }"#;
        fs::write(&path, malformed).unwrap();

        let mut doc: Map<String, Value> = serde_json::from_str(malformed).unwrap();
        assert_eq!(
            merge_tokens(&mut doc, &memory),
            Err(Unmergeable::Missing),
            "the caller must be told, not handed a silently untouched document"
        );

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["refreshToken"],
            json!("rt-new"),
            "a single-use rotated token was dropped: {value}"
        );
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][0]["expiresAt"], json!(7));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the fallback must not loosen permissions on a token file"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn merge_with_non_array_accounts_falls_back() {
        let path = tmp_path("persist-accounts-not-array");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // `accounts` present but the wrong JSON type: nothing to merge into, and
        // writing the document back verbatim would drop the rotated token.
        let malformed = r#"{ "accounts": {} }"#;
        fs::write(&path, malformed).unwrap();

        let mut doc: Map<String, Value> = serde_json::from_str(malformed).unwrap();
        assert_eq!(
            merge_tokens(&mut doc, &memory),
            Err(Unmergeable::NotAnArray)
        );

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-new"));
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        fs::remove_file(&path).ok();
    }

    /// The live-edit workflow `save_tokens` exists to support, in its one losing
    /// shape: renaming an account leaves its on-disk entry matching nothing, so
    /// its rotated credential cannot be placed. That is unavoidable — being
    /// silent about it is not. Both halves of the rename must be reported, and
    /// the OTHER account's token must still land.
    #[test]
    fn renamed_account_is_reported_not_silently_skipped() {
        let path = tmp_path("persist-renamed");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-new", "refreshToken": "rt-a-new", "expiresAt": 11 },
                   { "name": "acct-b", "accessToken": "at-b-new", "refreshToken": "rt-b-new", "expiresAt": 22 } ] }"#,
        )
        .unwrap();
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-a-renamed", "accessToken": "at-a-old", "refreshToken": "rt-a-old" },
                   { "name": "acct-b", "accessToken": "at-b-old", "refreshToken": "rt-b-old" } ] }"#,
        )
        .unwrap();

        let mut doc = read_document(&path).unwrap();
        let report = merge_tokens(&mut doc, &memory).expect("the document has an accounts array");
        assert_eq!(
            report.skipped,
            vec![SkippedEntry {
                index: 0,
                name: Some("acct-a-renamed".to_string()),
                reason: SkipReason::NoMemoryMatch,
            }],
            "the unmatched on-disk entry must come back named"
        );
        assert_eq!(
            report.absent_from_disk,
            vec!["acct-a".to_string()],
            "the memory side of the rename must be visible too — that is what makes it a rename and not a deletion"
        );

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["refreshToken"],
            json!("rt-a-old"),
            "a token landed on an entry that is not its own: {value}"
        );
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-b-new"));
        assert_eq!(
            value["accounts"][1]["refreshToken"],
            json!("rt-b-new"),
            "one skipped entry must not cost the other accounts their tokens"
        );
        assert_eq!(value["accounts"][1]["expiresAt"], json!(22));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn undeserializable_entry_does_not_block_its_siblings() {
        let path = tmp_path("persist-junk-entry");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-new", "refreshToken": "rt-a-new", "expiresAt": 33 } ] }"#,
        )
        .unwrap();
        // Two entries a mid-edit file can hold — a bare string and an object with
        // no name — ahead of the real account.
        fs::write(
            &path,
            r#"{ "accounts": [
                   "acct-a",
                   { "accessToken": "at-orphan" },
                   { "name": "acct-a", "accessToken": "at-a-old", "refreshToken": "rt-a-old" } ] }"#,
        )
        .unwrap();

        let mut doc = read_document(&path).unwrap();
        let report = merge_tokens(&mut doc, &memory).expect("the document has an accounts array");
        assert_eq!(
            report.skipped,
            vec![
                SkippedEntry {
                    index: 0,
                    name: None,
                    reason: SkipReason::NotAnObject,
                },
                SkippedEntry {
                    index: 1,
                    name: None,
                    reason: SkipReason::NoIdentity,
                },
            ]
        );
        assert!(
            report.absent_from_disk.is_empty(),
            "the loaded account did find its entry"
        );
        // A nameless entry is still addressable by the user: they can count rows.
        assert_eq!(report.skipped[0].label(), "accounts[0]");

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0],
            json!("acct-a"),
            "a malformed entry was rewritten instead of left alone"
        );
        assert_eq!(value["accounts"][1], json!({ "accessToken": "at-orphan" }));
        assert_eq!(value["accounts"][2]["accessToken"], json!("at-a-new"));
        assert_eq!(
            value["accounts"][2]["refreshToken"],
            json!("rt-a-new"),
            "junk ahead of a good entry blocked its rotated token: {value}"
        );
        assert_eq!(value["accounts"][2]["expiresAt"], json!(33));
        fs::remove_file(&path).ok();
    }

    /// The benign twin of the rename: an account deleted from the file is
    /// reported as absent, with NO unmatched on-disk entry beside it. That
    /// pairing is the only thing distinguishing a correct deletion from a
    /// rename that just cost an account its refresh token.
    #[test]
    fn account_removed_from_disk_is_reported_without_a_skip() {
        let path = tmp_path("persist-removed-report");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-new" },
                   { "name": "acct-gone", "accessToken": "at-gone-new" } ] }"#,
        )
        .unwrap();
        fs::write(
            &path,
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-old" } ] }"#,
        )
        .unwrap();

        let mut doc = read_document(&path).unwrap();
        let report = merge_tokens(&mut doc, &memory).expect("the document has an accounts array");
        assert!(
            report.skipped.is_empty(),
            "a deletion leaves no unmatched on-disk entry: {report:?}"
        );
        assert_eq!(report.absent_from_disk, vec!["acct-gone".to_string()]);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn save_tokens_writes_owner_only_permissions() {
        let path = tmp_path("persist-perm");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // Both paths through save_tokens must land 0600: the merge…
        fs::write(&path, one_account_file("at-old", "rt-old", 1)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // …and the fallback.
        fs::remove_file(&path).unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(&path).ok();
    }

    // --- write_atomic's failure path ---------------------------------------

    /// A destination that `rename(2)` cannot replace, in a directory we can still
    /// write to. Renaming a non-directory onto a directory fails (`EISDIR`/
    /// `ENOTDIR`) while `tempfile_in` on the parent still succeeds, which is
    /// exactly the shape needed: the temp file gets created and fully written, and
    /// only the final rename fails.
    fn unrenamable_target(tag: &str) -> (PathBuf, PathBuf) {
        let dir = tmp_path(tag).with_extension("d");
        let target = dir.join("teamclaude.json");
        fs::create_dir_all(&target).expect("create the blocking directory");
        (dir, target)
    }

    /// The recovery-artifact guard. `PersistError` owns the `NamedTempFile`, whose
    /// `Drop` is `fs::remove_file`, so `map_err(|e| e.error)` unlinks the complete,
    /// fsynced JSON on a rename failure. The rotated single-use token also lives in
    /// the in-memory snapshot (`src/manager/mod.rs:862-866`), so this file is the
    /// SECOND recovery path, not the only one — it is what covers a failed rename
    /// on a process that then dies before its shutdown flush.
    #[test]
    fn a_failed_rename_retains_the_written_temp_file() {
        let (dir, target) = unrenamable_target("persist-fail-retain");
        let json = r#"{"accounts":[{"name":"acct-a","accessToken":"at-a"}]}"#;

        let err = write_atomic(&target, json).expect_err("rename onto a directory must fail");
        assert!(
            matches!(err, ConfigError::Io(_)),
            "a rename failure is an I/O error, got {err:?}"
        );

        let orphans: Vec<PathBuf> = fs::read_dir(&dir)
            .expect("read the destination directory")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.is_file())
            .collect();
        assert_eq!(
            orphans.len(),
            1,
            "the written temp file must survive a failed rename, found {orphans:?}"
        );
        assert_eq!(
            fs::read_to_string(&orphans[0]).expect("read the retained temp file"),
            json,
            "the retained file must hold the bytes we wrote"
        );

        fs::remove_dir_all(&dir).ok();
    }

    /// Attribution for the orphan the test above proves exists — and for the one a
    /// SIGKILL between create and rename strands. `tempfile`'s default `.tmpXXXXXX`
    /// says nothing about who wrote it or which file it was becoming; in `~/.config`
    /// that file holds every account's OAuth tokens.
    #[test]
    fn a_retained_temp_file_is_attributable_to_tcr_and_its_destination() {
        let (dir, target) = unrenamable_target("persist-fail-name");
        write_atomic(&target, "{}").expect_err("rename onto a directory must fail");

        let orphan = fs::read_dir(&dir)
            .expect("read the destination directory")
            .map(|e| e.expect("dir entry").path())
            .find(|p| p.is_file())
            .expect("a retained temp file");
        let name = orphan
            .file_name()
            .and_then(|n| n.to_str())
            .expect("a utf-8 temp file name")
            .to_string();

        assert!(
            name.starts_with(".teamclaude.json.tcr-"),
            "an orphan must name its owner and its destination, got {name}"
        );
        assert!(
            name.ends_with(".tmp"),
            "an orphan must be recognisable as a temp file, got {name}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    // --- save_disabled (the TUI's `d`/`e`, made durable) -------------------

    /// A file carrying keys the server does not model, plus a second account, so
    /// a targeted flag write can be checked for collateral damage.
    const DISABLE_SAMPLE: &str = r#"{
      "warmupSeconds": 900,
      "pacing": { "maxInFlightPerAccount": 3 },
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        { "name": "acct-a", "type": "oauth", "accessToken": "at-a",
          "refreshToken": "rt-a", "expiresAt": 1, "models": ["claude-fable-5"] },
        { "name": "acct-b", "type": "oauth", "accessToken": "at-b",
          "refreshToken": "rt-b", "expiresAt": 2 }
      ]
    }"#;

    /// An identity probe in the legacy (no-uuid) shape every real config uses,
    /// where `same_identity` reduces to name equality.
    fn by_name(name: &str) -> Account {
        crate::identity::probe(name, None, None, None)
    }

    /// Disabling writes `"disabled": true` onto the right entry and changes
    /// NOTHING else — the whole point of editing the raw document instead of
    /// round-tripping a boot-time `Config`. Strip the one key we asked for and
    /// the document must be identical to what the user had.
    #[test]
    fn disable_writes_the_flag_and_changes_nothing_else() {
        let path = tmp_path("disable-write");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), true).unwrap(),
            DisabledWrite::Updated
        );

        let mut after = read_json(&path);
        assert_eq!(after["accounts"][0]["disabled"], json!(true));
        after["accounts"][0]
            .as_object_mut()
            .expect("the entry is an object")
            .remove("disabled");
        let before: Value = serde_json::from_str(DISABLE_SAMPLE).unwrap();
        assert_eq!(
            after, before,
            "the write touched something other than the disabled flag"
        );
        fs::remove_file(&path).ok();
    }

    /// Re-enabling DROPS the key rather than writing `false`, matching the CLI
    /// contract pinned by `cli::tests::set_enabled_false_drops_the_disabled_key`.
    /// A full disable→enable round trip must leave the file as it started.
    #[test]
    fn re_enable_drops_the_key_and_round_trips_the_document() {
        let path = tmp_path("disable-roundtrip");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        save_disabled(&path, &by_name("acct-a"), true).unwrap();
        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), false).unwrap(),
            DisabledWrite::Updated
        );

        let after = read_json(&path);
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "re-enable must DROP the disabled key, not write false: {after}"
        );
        let before: Value = serde_json::from_str(DISABLE_SAMPLE).unwrap();
        assert_eq!(
            after, before,
            "a disable→enable round trip must leave the document as it started"
        );
        fs::remove_file(&path).ok();
    }

    /// The unrelated account and the unmodelled keys (`warmupSeconds`, `pacing`,
    /// `routes`, per-account `models`) survive the write untouched. Named
    /// separately from the equality check above so weakening that one still trips
    /// a gate on the preserve-unknown-keys guarantee.
    #[test]
    fn disable_leaves_other_accounts_and_unmodelled_keys_untouched() {
        let path = tmp_path("disable-collateral");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        save_disabled(&path, &by_name("acct-a"), true).unwrap();

        let after = read_json(&path);
        assert_eq!(after["warmupSeconds"], json!(900));
        assert_eq!(after["pacing"]["maxInFlightPerAccount"], json!(3));
        assert!(after["routes"].is_array());
        assert_eq!(after["accounts"][0]["models"], json!(["claude-fable-5"]));
        // The account we did NOT name keeps its credentials and gains no flag.
        assert_eq!(after["accounts"][1]["name"], json!("acct-b"));
        assert_eq!(after["accounts"][1]["accessToken"], json!("at-b"));
        assert_eq!(after["accounts"][1]["refreshToken"], json!("rt-b"));
        assert!(
            after["accounts"][1].get("disabled").is_none(),
            "the unrelated account was flagged too: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// A redundant write — the document already says exactly this — reports
    /// `Unchanged` and does not rewrite the file. A file holding single-use
    /// refresh tokens is not rewritten to say what it already said.
    #[test]
    fn redundant_write_reports_unchanged_and_leaves_the_file_alone() {
        let path = tmp_path("disable-noop");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        // Already enabled (no key at all) → re-enabling is a no-op.
        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), false).unwrap(),
            DisabledWrite::Unchanged
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            DISABLE_SAMPLE,
            "an Unchanged write must leave the file byte-identical, not reformat it"
        );

        // And once disabled, disabling again is a no-op too.
        save_disabled(&path, &by_name("acct-a"), true).unwrap();
        let disabled_text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), true).unwrap(),
            DisabledWrite::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), disabled_text);
        fs::remove_file(&path).ok();
    }

    // --- save_control_account -----------------------------------------------

    /// Setting `controlAccount` writes the top-level key and changes nothing
    /// else — same "raw document, not a `Config` round trip" contract as
    /// `save_disabled`. This is also `control_persist_preserves_unmodelled_top_level_keys`
    /// from the bridge: a key nothing here models (`routes`) survives the write.
    #[test]
    fn set_control_writes_the_key_and_changes_nothing_else() {
        let path = tmp_path("control-write");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        assert_eq!(
            save_control_account(&path, Some("acct-a")).unwrap(),
            ControlWrite::Updated
        );

        let mut after = read_json(&path);
        assert_eq!(after["controlAccount"], json!("acct-a"));
        after
            .as_object_mut()
            .expect("document is an object")
            .remove("controlAccount");
        let before: Value = serde_json::from_str(DISABLE_SAMPLE).unwrap();
        assert_eq!(
            after, before,
            "the write touched something other than controlAccount"
        );
        fs::remove_file(&path).ok();
    }

    /// Clearing (`name: None`) REMOVES the key rather than writing `null` —
    /// matching `#[serde(skip_serializing_if = "Option::is_none")]` on
    /// `Config::control_account`, and a set→clear round trip leaves the file
    /// exactly as it started.
    #[test]
    fn clear_control_drops_the_key_and_round_trips_the_document() {
        let path = tmp_path("control-roundtrip");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        save_control_account(&path, Some("acct-a")).unwrap();
        assert_eq!(
            save_control_account(&path, None).unwrap(),
            ControlWrite::Updated
        );

        let after = read_json(&path);
        assert!(
            after.get("controlAccount").is_none(),
            "clearing must DROP the key, not write null: {after}"
        );
        let before: Value = serde_json::from_str(DISABLE_SAMPLE).unwrap();
        assert_eq!(
            after, before,
            "a set→clear round trip must leave the document as it started"
        );
        fs::remove_file(&path).ok();
    }

    /// A redundant write — the document already names this control account —
    /// reports `Unchanged` and leaves the file byte-identical.
    #[test]
    fn redundant_control_write_reports_unchanged_and_leaves_the_file_alone() {
        let path = tmp_path("control-noop");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        // Already unset → clearing again is a no-op.
        assert_eq!(
            save_control_account(&path, None).unwrap(),
            ControlWrite::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), DISABLE_SAMPLE);

        save_control_account(&path, Some("acct-a")).unwrap();
        let with_control = fs::read_to_string(&path).unwrap();
        assert_eq!(
            save_control_account(&path, Some("acct-a")).unwrap(),
            ControlWrite::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), with_control);
        fs::remove_file(&path).ok();
    }

    /// A `"disabled": false` already on disk is normalized away on re-enable
    /// rather than left sitting there — same end state the CLI produces.
    #[test]
    fn stale_disabled_false_on_disk_is_dropped() {
        let path = tmp_path("disable-stale-false");
        fs::write(
            &path,
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a", "disabled": false } ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), false).unwrap(),
            DisabledWrite::Updated
        );
        let after = read_json(&path);
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "a stale false must be dropped, not kept: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// No on-disk entry carries that identity (deleted or renamed while the proxy
    /// ran): report `NoEntry` and write NOTHING, so the caller can warn that the
    /// flag will not survive a restart instead of silently believing it landed.
    #[test]
    fn disable_with_no_matching_entry_writes_nothing() {
        let path = tmp_path("disable-no-entry");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-gone"), true).unwrap(),
            DisabledWrite::NoEntry
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            DISABLE_SAMPLE,
            "a no-match write must leave the file byte-identical"
        );
        fs::remove_file(&path).ok();
    }

    /// A document with no usable `accounts` list is `NoEntry`, never a fallback
    /// that writes a whole config over it — the clobber `save_tokens` accepts for
    /// a single-use token is NOT worth it for a flag that can be re-set by hand.
    #[test]
    fn disable_with_no_usable_accounts_list_writes_nothing() {
        for document in [r#"{ "upstream": "x" }"#, r#"{ "accounts": "nope" }"#] {
            let path = tmp_path("disable-unusable");
            fs::write(&path, document).unwrap();
            assert_eq!(
                save_disabled(&path, &by_name("acct-a"), true).unwrap(),
                DisabledWrite::NoEntry
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), document);
            fs::remove_file(&path).ok();
        }
    }

    /// An unreadable or malformed file surfaces the ERROR rather than taking
    /// `save_tokens`' whole-config fallback. Writing a boot-time snapshot over a
    /// file we could not parse is the clobber this module exists to prevent.
    #[test]
    fn disable_surfaces_errors_instead_of_clobbering() {
        let missing = tmp_path("disable-missing");
        assert!(!missing.exists());
        assert!(matches!(
            save_disabled(&missing, &by_name("acct-a"), true),
            Err(ConfigError::Io(_))
        ));
        assert!(
            !missing.exists(),
            "a missing config must not be created by a flag write"
        );

        let malformed = tmp_path("disable-malformed");
        fs::write(&malformed, "{ not json").unwrap();
        assert!(matches!(
            save_disabled(&malformed, &by_name("acct-a"), true),
            Err(ConfigError::Parse(_))
        ));
        assert_eq!(
            fs::read_to_string(&malformed).unwrap(),
            "{ not json",
            "a malformed config must be left exactly as found"
        );
        fs::remove_file(&malformed).ok();
    }

    /// The flag lands on the right ORG when one email is logged into two — the
    /// same identity matching `merge_tokens` uses, so a rotated credential and a
    /// disabled flag can never land on two different entries.
    #[test]
    fn disable_picks_the_right_entry_when_one_email_has_two_orgs() {
        let path = tmp_path("disable-two-orgs");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "me@example.com", "accessToken": "at-a", "accountUuid": "uuid-person",
                  "orgUuid": "org-a", "orgName": "Org A" },
                { "name": "me@example.com", "accessToken": "at-b", "accountUuid": "uuid-person",
                  "orgUuid": "org-b", "orgName": "Org B" }
            ] }"#,
        )
        .unwrap();

        let target = crate::identity::probe(
            "me@example.com",
            Some("uuid-person".to_string()),
            Some("org-b".to_string()),
            Some("Org B".to_string()),
        );
        assert_eq!(
            save_disabled(&path, &target, true).unwrap(),
            DisabledWrite::Updated
        );

        let after = read_json(&path);
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "the flag landed on the wrong org: {after}"
        );
        assert_eq!(after["accounts"][1]["disabled"], json!(true));
        fs::remove_file(&path).ok();
    }

    /// An AMBIGUOUS identity is refused, never resolved to the first match. Two
    /// entries sharing a name both satisfy `same_identity` (it falls back to name
    /// equality when either side lacks a uuid), and the TUI selects by ROW INDEX —
    /// so guessing lands the flag on whichever entry is earlier in the file,
    /// benching a healthy account while the TUI renders the other one disabled.
    /// Nothing is written and the ambiguity is reported distinctly.
    #[test]
    fn disable_refuses_an_ambiguous_identity_and_writes_nothing() {
        let path = tmp_path("disable-ambiguous");
        let document = r#"{ "accounts": [
            { "name": "acct-dup", "accessToken": "at-first" },
            { "name": "acct-dup", "accessToken": "at-second" }
        ] }"#;
        fs::write(&path, document).unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-dup"), true).unwrap(),
            DisabledWrite::Ambiguous
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            document,
            "an ambiguous identity must leave the file byte-identical"
        );
        fs::remove_file(&path).ok();
    }

    /// The refusal is scoped to the AMBIGUOUS identity, not to the file: an
    /// unambiguous account in the same document still writes. Without this pair,
    /// "nothing was written" above would be satisfied by a save_disabled that had
    /// simply stopped working.
    #[test]
    fn a_duplicate_elsewhere_does_not_block_an_unambiguous_write() {
        let path = tmp_path("disable-ambiguous-neighbour");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "acct-dup", "accessToken": "at-first" },
                { "name": "acct-dup", "accessToken": "at-second" },
                { "name": "acct-unique", "accessToken": "at-unique" }
            ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-unique"), true).unwrap(),
            DisabledWrite::Updated
        );
        let after = read_json(&path);
        assert_eq!(after["accounts"][2]["disabled"], json!(true));
        assert!(
            after["accounts"][0].get("disabled").is_none()
                && after["accounts"][1].get("disabled").is_none(),
            "only the unambiguous entry may be touched: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// Two entries that share a NAME but are separated by org are NOT ambiguous —
    /// `same_identity` tells them apart, so the write still lands. The refusal must
    /// bite on genuinely indistinguishable entries only, or the two-org config the
    /// identity work exists to support would stop being writable.
    #[test]
    fn two_orgs_under_one_name_are_not_ambiguous() {
        let path = tmp_path("disable-two-orgs-unambiguous");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "me@example.com", "accessToken": "at-a", "accountUuid": "uuid-person",
                  "orgUuid": "org-a", "orgName": "Org A" },
                { "name": "me@example.com", "accessToken": "at-b", "accountUuid": "uuid-person",
                  "orgUuid": "org-b", "orgName": "Org B" }
            ] }"#,
        )
        .unwrap();

        let target = crate::identity::probe(
            "me@example.com",
            Some("uuid-person".to_string()),
            Some("org-a".to_string()),
            Some("Org A".to_string()),
        );
        assert_eq!(
            save_disabled(&path, &target, true).unwrap(),
            DisabledWrite::Updated
        );
        let after = read_json(&path);
        assert_eq!(after["accounts"][0]["disabled"], json!(true));
        assert!(after["accounts"][1].get("disabled").is_none());
        fs::remove_file(&path).ok();
    }

    /// The shape the test above does NOT cover, and the one the refusal actually
    /// broke: the older entry predates org UUIDs and carries none, so its org key
    /// is `(None)` while its sibling's is `Some("org-a")`. `same_identity`
    /// deliberately treats an unknown org as a match — that is what lets a
    /// freshly-profiled login backfill a legacy entry — which also means the
    /// pre-org entry matches BOTH runtime rows. These are two real accounts, one
    /// person in two orgs, and refusing on the second `same_identity` hit left
    /// NEITHER of them durably benchable.
    ///
    /// Each row must reach its own entry: the org-carrying row by the exact match,
    /// and the pre-org row by name — it is the only entry with no org at all.
    #[test]
    fn a_pre_org_entry_beside_its_org_carrying_sibling_is_not_ambiguous() {
        let path = tmp_path("disable-legacy-backfill");
        let file = r#"{ "accounts": [
                { "name": "me@example.com", "accessToken": "at-a", "accountUuid": "uuid-person",
                  "orgUuid": "org-a", "orgName": "Org A" },
                { "name": "me@example.com", "accessToken": "at-legacy", "accountUuid": "uuid-person" }
            ] }"#;

        // The org-carrying row: both entries match it loosely, one matches exactly.
        fs::write(&path, file).unwrap();
        let with_org = crate::identity::probe(
            "me@example.com",
            Some("uuid-person".to_string()),
            Some("org-a".to_string()),
            Some("Org A".to_string()),
        );
        assert_eq!(
            save_disabled(&path, &with_org, true).unwrap(),
            DisabledWrite::Updated,
            "the fully-known identity resolves to the entry that carries the same org"
        );
        let after = read_json(&path);
        assert_eq!(after["accounts"][0]["disabled"], json!(true));
        assert!(
            after["accounts"][1].get("disabled").is_none(),
            "the flag must not land on the pre-org sibling: {after}"
        );

        // And the pre-org row, whose own org is still unknown, resolves to the one
        // entry that likewise has none.
        fs::write(&path, file).unwrap();
        let pre_org = crate::identity::probe(
            "me@example.com",
            Some("uuid-person".to_string()),
            None,
            None,
        );
        assert_eq!(
            save_disabled(&path, &pre_org, true).unwrap(),
            DisabledWrite::Updated
        );
        let after = read_json(&path);
        assert_eq!(after["accounts"][1]["disabled"], json!(true));
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "the flag must not land on the org-carrying sibling: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// The refusal still fires where it must. Two entries that share a name and
    /// carry no UUID at all are genuinely indistinguishable — there is no stricter
    /// fact to prefer one by — so nothing is written and the caller is told.
    #[test]
    fn two_entries_with_nothing_to_tell_them_apart_are_still_refused() {
        let path = tmp_path("disable-truly-ambiguous");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "me@example.com", "accessToken": "at-one" },
                { "name": "me@example.com", "accessToken": "at-two" }
            ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("me@example.com"), true).unwrap(),
            DisabledWrite::Ambiguous
        );
        let after = read_json(&path);
        assert!(after["accounts"][0].get("disabled").is_none());
        assert!(after["accounts"][1].get("disabled").is_none());
        fs::remove_file(&path).ok();
    }

    /// The flag write goes through the same atomic 0600 path as every other
    /// write, so it can never leave the token file world-readable.
    #[test]
    fn save_disabled_writes_owner_only_permissions() {
        let path = tmp_path("disable-perm");
        fs::write(&path, DISABLE_SAMPLE).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        save_disabled(&path, &by_name("acct-a"), true).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(&path).ok();
    }

    // --- save_account (the durable half of live account-add) ----------------

    /// A full, valid account record — the shape `Manager::add_or_update_account`
    /// always hands `save_account`, whether it is a brand-new login or an
    /// existing account's own identity with fresh credentials merged in.
    fn full_account(name: &str, access_token: &str) -> Account {
        Account {
            name: name.to_string(),
            account_type: "oauth".to_string(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: access_token.to_string(),
            refresh_token: Some(format!("rt-{access_token}")),
            expires_at: Some(1_800_000_000_000),
            priority: Some(2),
            switch_threshold: None,
            disabled: None,
            groups: None,
            extra: serde_json::Map::new(),
        }
    }

    /// A miss appends a brand-new entry carrying every field, and leaves the
    /// rest of the document untouched.
    #[test]
    fn save_account_appends_a_new_identity() {
        let path = tmp_path("add-append");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        assert_eq!(
            save_account(&path, &full_account("carol@example.com", "at-carol")).unwrap(),
            AccountWrite::Added
        );

        let after = read_json(&path);
        let accounts = after["accounts"].as_array().expect("accounts array");
        assert_eq!(accounts.len(), 3, "appended, not replacing anything");
        assert_eq!(accounts[2]["name"], json!("carol@example.com"));
        assert_eq!(accounts[2]["accessToken"], json!("at-carol"));
        assert_eq!(accounts[2]["refreshToken"], json!("rt-at-carol"));
        assert_eq!(accounts[2]["priority"], json!(2));
        // The pre-existing rows and unmodelled top-level keys are untouched.
        assert_eq!(accounts[0]["name"], json!("acct-a"));
        assert_eq!(accounts[1]["name"], json!("acct-b"));
        assert_eq!(after["warmupSeconds"], json!(900));
        fs::remove_file(&path).ok();
    }

    /// Two pre-existing entries with real, distinct priorities — the shape
    /// needed to prove the Added path computes MAX+1 rather than reading an
    /// absent priority as 0.
    const PRIORITY_SAMPLE: &str = r#"{
      "accounts": [
        { "name": "acct-a", "type": "oauth", "accessToken": "at-a", "priority": 0 },
        { "name": "acct-b", "type": "oauth", "accessToken": "at-b", "priority": 1 }
      ]
    }"#;

    /// An appended account with no explicit priority joins the BACK of the
    /// fleet — `max(existing priorities) + 1` — never left absent. An absent
    /// `priority` reads as 0 at runtime (`AccountRuntime::from_config`'s
    /// `unwrap_or(0)`), which silently promotes a freshly added account to the
    /// PRIMARY tier ahead of the established fleet. Mirrors
    /// `oauth::upsert_account`'s historical default, which this route did not
    /// share before this fix.
    #[test]
    fn save_account_added_path_assigns_max_plus_one_priority_when_none_submitted() {
        let path = tmp_path("add-default-priority");
        fs::write(&path, PRIORITY_SAMPLE).unwrap();

        let new_account = Account {
            priority: None,
            ..full_account("carol@example.com", "at-carol")
        };
        assert_eq!(
            save_account(&path, &new_account).unwrap(),
            AccountWrite::Added
        );

        let after = read_json(&path);
        let accounts = after["accounts"].as_array().expect("accounts array");
        assert_eq!(accounts.len(), 3);
        assert_eq!(
            accounts[2]["priority"],
            json!(2),
            "an added account with no explicit priority must join the back of \
             the fleet, not read as 0: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// The max+1 default only fills an ABSENT priority — a caller that submits
    /// one explicitly (the documented new-account case, see
    /// `AddAccountRequest`'s doc-comment in `proxy.rs`) is never overridden.
    #[test]
    fn save_account_added_path_keeps_an_explicit_priority() {
        let path = tmp_path("add-explicit-priority");
        fs::write(&path, PRIORITY_SAMPLE).unwrap();

        let new_account = Account {
            priority: Some(99),
            ..full_account("carol@example.com", "at-carol")
        };
        assert_eq!(
            save_account(&path, &new_account).unwrap(),
            AccountWrite::Added
        );

        let after = read_json(&path);
        assert_eq!(after["accounts"][2]["priority"], json!(99));
        fs::remove_file(&path).ok();
    }

    /// A hit replaces ONLY the credential triple on the matching entry — name,
    /// type, and every unmodelled key (here `models`) survive untouched, exactly
    /// as `merge_tokens` leaves them.
    #[test]
    fn save_account_updates_an_existing_identity_in_place() {
        let path = tmp_path("add-update");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        let fresh = Account {
            expires_at: Some(999),
            priority: Some(99), // deliberately different — must NOT land on disk
            ..full_account("acct-a", "at-a-fresh")
        };
        assert_eq!(save_account(&path, &fresh).unwrap(), AccountWrite::Updated);

        let after = read_json(&path);
        let accounts = after["accounts"].as_array().expect("accounts array");
        assert_eq!(accounts.len(), 2, "no duplicate row");
        assert_eq!(accounts[0]["name"], json!("acct-a"), "identity untouched");
        assert_eq!(accounts[0]["accessToken"], json!("at-a-fresh"));
        assert_eq!(accounts[0]["refreshToken"], json!("rt-at-a-fresh"));
        assert_eq!(accounts[0]["expiresAt"], json!(999));
        assert!(
            accounts[0].get("priority").is_none(),
            "priority was never on this entry and the update must not add it: {after}"
        );
        assert_eq!(
            accounts[0]["models"],
            json!(["claude-fable-5"]),
            "an unmodelled per-account key survives the credential-only merge"
        );
        // The account we did NOT name keeps its own credentials.
        assert_eq!(accounts[1]["accessToken"], json!("at-b"));
        fs::remove_file(&path).ok();
    }

    /// An identity matching more than one on-disk entry refuses rather than
    /// guesses which one to overwrite — same posture as `save_disabled`.
    #[test]
    fn save_account_refuses_an_ambiguous_identity() {
        const TWINS: &str = r#"{
          "accounts": [
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-1" },
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-2" }
          ]
        }"#;
        let path = tmp_path("add-ambiguous");
        fs::write(&path, TWINS).unwrap();

        assert_eq!(
            save_account(&path, &full_account("dup@example.com", "at-new")).unwrap(),
            AccountWrite::Ambiguous
        );

        let after = read_json(&path);
        assert_eq!(after["accounts"][0]["accessToken"], json!("at-1"));
        assert_eq!(after["accounts"][1]["accessToken"], json!("at-2"));
        fs::remove_file(&path).ok();
    }

    /// A miss on a document with NO `accounts` key at all still succeeds — the
    /// key is created — because (unlike `save_disabled`'s target) `account` here
    /// is always a complete, real record worth creating a home for.
    #[test]
    fn save_account_creates_the_accounts_array_when_absent() {
        let path = tmp_path("add-no-accounts-key");
        fs::write(&path, r#"{"warmupSeconds": 900}"#).unwrap();

        assert_eq!(
            save_account(&path, &full_account("carol@example.com", "at-carol")).unwrap(),
            AccountWrite::Added
        );

        let after = read_json(&path);
        assert_eq!(after["accounts"][0]["name"], json!("carol@example.com"));
        assert_eq!(after["warmupSeconds"], json!(900));
        fs::remove_file(&path).ok();
    }

    /// An `accounts` key that is present but NOT an array is too corrupt a shape
    /// to append into blindly — refused, and the document is left byte-identical.
    #[test]
    fn save_account_refuses_when_accounts_key_is_not_an_array() {
        const MALFORMED: &str = r#"{"accounts": "not-an-array"}"#;
        let path = tmp_path("add-not-array");
        fs::write(&path, MALFORMED).unwrap();

        assert_eq!(
            save_account(&path, &full_account("carol@example.com", "at-carol")).unwrap(),
            AccountWrite::Unwritable
        );

        let after: Value = serde_json::from_str(MALFORMED).unwrap();
        assert_eq!(
            read_json(&path),
            after,
            "an unwritable shape is left untouched"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn throttle_zero_spacing_is_inert() {
        // `Some(0)` spacing normalizes to unset (footgun parity with pacing).
        let config: Config =
            serde_json::from_str(r#"{ "accounts": [], "throttle": { "minSpacingMs": 0 } }"#)
                .unwrap();
        assert_eq!(config.throttle.effective_min_spacing(), None);
        assert!(!config.throttle.is_active());
    }
}
