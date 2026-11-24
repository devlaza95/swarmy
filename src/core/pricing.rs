//! API-equivalent prices as versioned data.
//!
//! A price is never a constant in code: every entry carries the date it takes effect from and the
//! source it came from. Three layers are merged, the later winning: the built-in table below, the
//! price table Claude Code itself caches in `.claude.json` (`additionalModelCostsCache`), and the
//! user's overrides in `config.toml`.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::ids::SeatId;
use super::units::Usd;
use super::usage::Usage;

mod builtin;
use builtin::{BUILTIN, LONG_CONTEXT, writes_at_input};

/// The most a web search can cost, in USD per request: no provider charges near it. A
/// `[[price]]` above it was written per 1000 searches (`config::Config::validate`).
pub const MAX_WEB_SEARCH_USD: f64 = 1.0;

/// USD per million tokens; `web_search` is USD per request.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    #[serde(default)]
    pub web_search: f64,
    /// Fast mode (`usage.speed == "fast"`) multiplies every rate.
    #[serde(default)]
    pub fast_multiplier: Option<f64>,
    /// Higher rates when the prompt exceeds a threshold.
    #[serde(default)]
    pub long_context: Option<LongContext>,
}

/// The rates a price source may leave out; `Rates::derive` fills them in.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RateOverrides {
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    pub cache_read: Option<f64>,
    pub web_search: Option<f64>,
    pub fast_multiplier: Option<f64>,
}

impl Rates {
    /// The one place derived rates are defined. A rate not given comes from `base`, the entry
    /// being overridden: cache rates keep their ratio to the input rate, the web-search price,
    /// the fast-mode multiplier and the long-context rule are kept as they are. Without a base,
    /// Anthropic's general ratios apply: cache writes 1.25× (5 min) and 2× (1 h) the input rate,
    /// cache reads 0.1×, web search $10 per 1000 requests.
    pub fn derive(input: f64, output: f64, over: &RateOverrides, base: Option<&Rates>) -> Rates {
        let base = base.filter(|b| b.input > 0.0);
        let ratio =
            |pick: fn(&Rates) -> f64, general: f64| base.map_or(general, |b| pick(b) / b.input);
        Rates {
            input,
            output,
            cache_write_5m: over
                .cache_write_5m
                .unwrap_or_else(|| input * ratio(|b| b.cache_write_5m, 1.25)),
            cache_write_1h: over
                .cache_write_1h
                .unwrap_or_else(|| input * ratio(|b| b.cache_write_1h, 2.0)),
            cache_read: over
                .cache_read
                .unwrap_or_else(|| input * ratio(|b| b.cache_read, 0.1)),
            web_search: over
                .web_search
                .or(base.map(|b| b.web_search))
                .unwrap_or(0.01),
            fast_multiplier: over
                .fast_multiplier
                .or(base.and_then(|b| b.fast_multiplier)),
            long_context: base.and_then(|b| b.long_context),
        }
    }

    /// Rates are finite and not negative; multipliers are above zero (TOML accepts `nan`, `inf`
    /// and negative numbers, so this is checked, not assumed).
    pub fn validate(&self) -> Result<(), String> {
        let rate = |name: &str, x: f64| {
            if x.is_finite() && x >= 0.0 {
                Ok(())
            } else {
                Err(format!("{name} must be a finite number ≥ 0 (got {x})"))
            }
        };
        let factor = |name: &str, x: f64| {
            if x.is_finite() && x > 0.0 {
                Ok(())
            } else {
                Err(format!("{name} must be a finite number > 0 (got {x})"))
            }
        };
        rate("input", self.input)?;
        rate("output", self.output)?;
        rate("cache_write_5m", self.cache_write_5m)?;
        rate("cache_write_1h", self.cache_write_1h)?;
        rate("cache_read", self.cache_read)?;
        rate("web_search", self.web_search)?;
        if let Some(f) = self.fast_multiplier {
            factor("fast_multiplier", f)?;
        }
        if let Some(lc) = self.long_context {
            factor("long_context input multiplier", lc.input_multiplier)?;
            factor("long_context output multiplier", lc.output_multiplier)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LongContext {
    /// Prompt tokens (input + cache writes + cache reads) above which the multipliers apply.
    pub threshold: u64,
    /// Applies to input, cache writes and cache reads.
    pub input_multiplier: f64,
    pub output_multiplier: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum PriceSource {
    Builtin,
    /// `additionalModelCostsCache` of a Claude Code config dir (the seat it was read from).
    ClaudeCode(SeatId),
    User,
}

impl PriceSource {
    /// How `swarmy prices` names the source; `seat_label` names a seat id.
    pub fn label(&self, seat_label: impl Fn(&SeatId) -> String) -> String {
        match self {
            PriceSource::Builtin => "built-in".into(),
            PriceSource::ClaudeCode(seat) => format!("claude code ({})", seat_label(seat)),
            PriceSource::User => "config.toml".into(),
        }
    }

    /// The source without the seat it was read from: which seat carried a cache changes no
    /// price, so it must not change the fingerprint (no reprice on a seat rename).
    fn kind(&self) -> &'static str {
        match self {
            PriceSource::Builtin => "builtin",
            PriceSource::ClaudeCode(_) => "claude_code",
            PriceSource::User => "user",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PriceEntry {
    /// Exact model id, or a prefix ending in `*`.
    pub model: String,
    /// The first day the entry applies to; `None` = every day (a user override without `from`,
    /// or a Claude Code price the index has not dated yet).
    pub from: Option<NaiveDate>,
    pub rates: Rates,
    pub source: PriceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Cost {
    pub usd: Usd,
    /// False when no price is known for the model: the cost is then 0, flagged, never silent.
    pub priced: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PriceBook {
    entries: Vec<PriceEntry>,
}

impl PriceBook {
    pub fn builtin() -> Self {
        let entries = BUILTIN
            .iter()
            .map(|&(model, from, input, output, cache_read, fast)| {
                let flat = writes_at_input(model);
                let over = RateOverrides {
                    cache_write_5m: flat.then_some(input),
                    cache_write_1h: flat.then_some(input),
                    cache_read: Some(cache_read),
                    fast_multiplier: fast,
                    ..Default::default()
                };
                let mut rates = Rates::derive(input, output, &over, None);
                rates.long_context = LONG_CONTEXT.iter().find(|(m, ..)| *m == model).map(
                    |&(_, threshold, input_multiplier, output_multiplier)| LongContext {
                        threshold,
                        input_multiplier,
                        output_multiplier,
                    },
                );
                PriceEntry {
                    model: model.to_string(),
                    from: Some(from),
                    rates,
                    source: PriceSource::Builtin,
                }
            })
            .collect();
        PriceBook { entries }
    }

    #[cfg(test)]
    pub fn push(&mut self, e: PriceEntry) {
        self.entries.push(e);
    }

    pub fn entries(&self) -> &[PriceEntry] {
        &self.entries
    }

    /// Merges the `additionalModelCostsCache` object of a `.claude.json`: per model
    /// `{inputTokens, outputTokens, promptCacheWriteTokens, promptCacheWrite1hTokens,
    /// promptCacheReadTokens, webSearchRequests}` in USD per MTok (web search per request).
    /// What the cache leaves out (fast mode, long context, a missing cache rate) is derived from
    /// the built-in entry of the same model. Entries are undated (`from: None`): the index dates
    /// them by when it first saw them (`Index::dated_prices`). Invalid rates are skipped.
    pub fn merge_claude_code_cache(&mut self, seat: &SeatId, cache: &serde_json::Value) {
        let Some(map) = cache.as_object() else { return };
        for (model, v) in map {
            let f = |k: &str| v.get(k).and_then(|x| x.as_f64());
            let (Some(input), Some(output)) = (f("inputTokens"), f("outputTokens")) else {
                continue;
            };
            let base = self
                .lookup_where(model, None, |s| *s == PriceSource::Builtin)
                .map(|e| e.rates);
            let over = RateOverrides {
                cache_write_5m: f("promptCacheWriteTokens"),
                cache_write_1h: f("promptCacheWrite1hTokens"),
                cache_read: f("promptCacheReadTokens"),
                web_search: f("webSearchRequests"),
                fast_multiplier: None,
            };
            let rates = Rates::derive(input, output, &over, base.as_ref());
            if rates.validate().is_err() {
                continue;
            }
            // Several seats carry the same cache; keep one copy per model and rates.
            if self.entries.iter().any(|e| {
                e.model == *model
                    && e.rates == rates
                    && matches!(e.source, PriceSource::ClaudeCode(_))
            }) {
                continue;
            }
            self.entries.push(PriceEntry {
                model: model.clone(),
                from: None,
                rates,
                source: PriceSource::ClaudeCode(seat.clone()),
            });
        }
    }

    /// Adds a user override (`[[price]]` in `config.toml`). Rates it does not give are derived
    /// from the entry it overrides: for an exact model id, the entry that prices the model on
    /// `from` (today when the override has no date); a pattern has no single base.
    pub fn push_override(
        &mut self,
        model: &str,
        from: Option<NaiveDate>,
        input: f64,
        output: f64,
        over: &RateOverrides,
        today: NaiveDate,
    ) -> Result<(), String> {
        let base = (!model.ends_with('*'))
            .then(|| {
                self.lookup_where(model, Some(from.unwrap_or(today)), |s| {
                    *s != PriceSource::User
                })
            })
            .flatten()
            .map(|e| e.rates);
        let rates = Rates::derive(input, output, over, base.as_ref());
        rates.validate()?;
        self.entries.push(PriceEntry {
            model: model.to_string(),
            from,
            rates,
            source: PriceSource::User,
        });
        Ok(())
    }

    /// The same book with Claude Code's entries replaced by `observed` (dated versions).
    pub fn with_observed(&self, observed: Vec<PriceEntry>) -> PriceBook {
        let mut entries: Vec<PriceEntry> = self
            .entries
            .iter()
            .filter(|e| !matches!(e.source, PriceSource::ClaudeCode(_)))
            .cloned()
            .collect();
        entries.extend(observed);
        PriceBook { entries }
    }

    /// Whether a source other than Claude Code's cache prices `model` on `day`.
    pub fn priced_without_cache(&self, model: &str, day: NaiveDate) -> bool {
        self.lookup_where(model, Some(day), |s| {
            !matches!(s, PriceSource::ClaudeCode(_))
        })
        .is_some()
    }

    /// The entry that prices `model` on `day`: a user entry beats Claude Code's cache, which beats
    /// the built-in table; then the most specific pattern; then the latest `from` not after `day`.
    /// Dated ids (`claude-haiku-4-5-20251001`) fall back to the id without the date.
    pub fn lookup(&self, model: &str, day: NaiveDate) -> Option<&PriceEntry> {
        self.lookup_where(model, Some(day), |_| true)
    }

    /// `lookup` over the entries of the sources `keep` accepts; `day: None` ignores dates and
    /// takes the latest version.
    fn lookup_where(
        &self,
        model: &str,
        day: Option<NaiveDate>,
        keep: impl Fn(&PriceSource) -> bool,
    ) -> Option<&PriceEntry> {
        let undated = strip_date_suffix(model);
        // Source priority, pattern specificity, effective date: the largest wins.
        type Rank = (u8, usize, Option<NaiveDate>);
        let mut best: Option<(&PriceEntry, Rank)> = None;
        for e in &self.entries {
            if !keep(&e.source) {
                continue;
            }
            if let (Some(day), Some(from)) = (day, e.from)
                && from > day
            {
                continue;
            }
            let specificity = match_score(&e.model, model)
                .or_else(|| undated.and_then(|u| match_score(&e.model, u)));
            let Some(spec) = specificity else { continue };
            let prio = match e.source {
                PriceSource::User => 2,
                PriceSource::ClaudeCode(_) => 1,
                PriceSource::Builtin => 0,
            };
            let key = (prio, spec, e.from);
            if best.as_ref().is_none_or(|(_, k)| key > *k) {
                best = Some((e, key));
            }
        }
        best.map(|(e, _)| e)
    }

    pub fn cost(&self, model: &str, day: NaiveDate, usage: &Usage, fast: bool) -> Cost {
        let parts = self.cost_parts(model, day, usage, fast);
        Cost {
            usd: parts.map_or(Usd::ZERO, |p| p.total()),
            priced: parts.is_some(),
        }
    }

    /// [`cost_parts`] at the prices in force for `model` on `day`; `None` when no price is known
    /// (a request without tokens needs none: all parts zero).
    pub fn cost_parts(
        &self,
        model: &str,
        day: NaiveDate,
        usage: &Usage,
        fast: bool,
    ) -> Option<CostParts> {
        match self.lookup(model, day) {
            Some(e) => Some(cost_parts(&e.rates, usage, fast)),
            None => usage.is_empty().then(CostParts::default),
        }
    }

    /// [`cost_parts_in_band`] at the prices in force for `model` on `day`: the cost of token sums
    /// whose requests were all inside (`long`) or all outside the long-context band (the
    /// archive's rollups, repriced from their sums); `None` when no price is known.
    pub fn cost_parts_in_band(
        &self,
        model: &str,
        day: NaiveDate,
        usage: &Usage,
        fast: bool,
        long: bool,
    ) -> Option<CostParts> {
        match self.lookup(model, day) {
            Some(e) => Some(cost_parts_in_band(&e.rates, usage, fast, long)),
            None => usage.is_empty().then(CostParts::default),
        }
    }

    /// Changes whenever any price changes, so stored costs know when to be recomputed. Which
    /// seat a cached price came from is left out: renaming or re-ordering seats reprices nothing.
    pub fn fingerprint(&self) -> String {
        let mut h = Sha256::new();
        for e in &self.entries {
            h.update(e.model.as_bytes());
            h.update([0]);
            h.update(e.from.map(|d| d.to_string()).unwrap_or_default().as_bytes());
            h.update([0]);
            h.update(e.source.kind().as_bytes());
            h.update([0]);
            // Rates are plain numbers: serializing them cannot fail.
            h.update(serde_json::to_vec(&e.rates).unwrap_or_default());
            h.update([0]);
        }
        let mut s = hex::encode(h.finalize());
        s.truncate(16);
        s
    }
}

/// USD of one request by kind of token. The parts add up to the request's cost: the efficiency
/// panel's split and the totals come from the same numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct CostParts {
    pub input: Usd,
    /// 5-minute and 1-hour cache writes.
    pub cache_write: Usd,
    pub cache_read: Usd,
    pub output: Usd,
    pub web: Usd,
    /// What the cache reads saved against paying the input rate for them (not part of the cost).
    pub cache_saved: Usd,
}

impl CostParts {
    pub fn total(&self) -> Usd {
        self.input + self.cache_write + self.cache_read + self.output + self.web
    }
}

/// One request at the given rates, by part. The only place the multipliers apply: long context
/// (above its threshold, on the prompt and the output), then fast mode (on everything).
pub fn cost_parts(r: &Rates, u: &Usage, fast: bool) -> CostParts {
    let long = r.long_context.is_some_and(|lc| u.prompt() > lc.threshold);
    cost_parts_in_band(r, u, fast, long)
}

/// [`cost_parts`] with the long-context band given instead of taken from the prompt: `long`
/// applies the multipliers (when the rates have a long-context rule). Linear in the tokens for a
/// fixed band, so the sums of many requests of one band cost exactly what the requests do.
pub fn cost_parts_in_band(r: &Rates, u: &Usage, fast: bool, long: bool) -> CostParts {
    let (mut pin, mut pout) = (1.0, 1.0);
    if let Some(lc) = r.long_context.filter(|_| long) {
        pin = lc.input_multiplier;
        pout = lc.output_multiplier;
    }
    let speed = if fast {
        r.fast_multiplier.unwrap_or(1.0)
    } else {
        1.0
    };
    let tok = |n: u64, rate: f64, mult: f64| Usd::new(n as f64 * rate * mult / 1_000_000.0 * speed);
    CostParts {
        input: tok(u.input, r.input, pin),
        cache_write: tok(u.cache_write_5m, r.cache_write_5m, pin)
            + tok(u.cache_write_1h, r.cache_write_1h, pin),
        cache_read: tok(u.cache_read, r.cache_read, pin),
        output: tok(u.output, r.output, pout),
        web: Usd::new(u.web_search as f64 * r.web_search * speed),
        cache_saved: tok(u.cache_read, (r.input - r.cache_read).max(0.0), pin),
    }
}

/// USD for one request at the given rates.
pub fn cost_with(r: &Rates, u: &Usage, fast: bool) -> Usd {
    cost_parts(r, u, fast).total()
}

fn match_score(pattern: &str, model: &str) -> Option<usize> {
    match pattern.strip_suffix('*') {
        Some(prefix) => model.starts_with(prefix).then_some(prefix.len()),
        None => (pattern == model).then_some(usize::MAX),
    }
}

/// `claude-3-5-sonnet-20241022` → `claude-3-5-sonnet`.
pub fn strip_date_suffix(model: &str) -> Option<&str> {
    let (head, tail) = model.rsplit_once('-')?;
    (tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit())).then_some(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 22).unwrap()
    }

    fn close(a: impl Into<f64>, b: impl Into<f64>) -> bool {
        let (a, b): (f64, f64) = (a.into(), b.into());
        (a - b).abs() < 1e-12
    }

    #[test]
    fn opus_5_5_request_by_hand() {
        // A real request shape: 2 input, 409 written to the 1-hour cache, 155 527 read, 192 out.
        // 2*4 + 409*8 + 155527*0.2 + 192*20 = 8 + 3272 + 31105.4 + 3840 = 38225.4 µ$.
        let u = Usage {
            input: 2,
            cache_write_1h: 409,
            cache_read: 155_527,
            output: 192,
            ..Default::default()
        };
        let c = PriceBook::builtin().cost("claude-opus-5-5", day(), &u, false);
        assert!(c.priced);
        assert!(close(c.usd, 0.0382254), "{}", c.usd);
    }

    #[test]
    fn five_minute_and_one_hour_writes_are_priced_apart() {
        // Sonnet 5.5: 1000 tokens to the 5-min cache at 2.5, 1000 to the 1-h cache at 4.0.
        let u = Usage {
            cache_write_5m: 1000,
            cache_write_1h: 1000,
            ..Default::default()
        };
        let c = PriceBook::builtin().cost("claude-sonnet-5-5", day(), &u, false);
        assert!(close(c.usd, (2500.0 + 4000.0) / 1e6), "{}", c.usd);
    }

    #[test]
    fn fast_mode_doubles_opus_5_5() {
        // $8 / $40 in fast mode: 1M in + 1M out = 48.
        let u = Usage {
            input: 1_000_000,
            output: 1_000_000,
            ..Default::default()
        };
        let c = PriceBook::builtin().cost("claude-opus-5-5", day(), &u, true);
        assert!(close(c.usd, 48.0), "{}", c.usd);
    }

    #[test]
    fn haiku_5_5_long_context() {
        // 150 000 prompt tokens > 100K: input at 0.50, output at 2.50.
        // 150000*0.5 + 1000*2.5 = 75000 + 2500 = 77500 µ$.
        let u = Usage {
            input: 150_000,
            output: 1000,
            ..Default::default()
        };
        let c = PriceBook::builtin().cost("claude-haiku-5-5", day(), &u, false);
        assert!(close(c.usd, 0.0775), "{}", c.usd);
        // At the threshold itself the base rate applies: 100000*0.1 = 10000 µ$.
        let u = Usage {
            input: 100_000,
            ..Default::default()
        };
        assert!(close(
            PriceBook::builtin()
                .cost("claude-haiku-5-5", day(), &u, false)
                .usd,
            0.01
        ));
    }

    #[test]
    fn dated_ids_fall_back_and_unknown_models_are_flagged() {
        let b = PriceBook::builtin();
        assert_eq!(
            b.lookup("claude-haiku-4-5-20251001", day()).unwrap().model,
            "claude-haiku-4-5"
        );
        assert_eq!(
            b.lookup("claude-opus-4-20250514", day()).unwrap().model,
            "claude-opus-4"
        );
        let u = Usage {
            input: 10,
            ..Default::default()
        };
        assert!(!b.cost("claude-unknown-9", day(), &u, false).priced);
        // A synthetic error message with no tokens is not "unpriced".
        assert!(
            b.cost("<synthetic>", day(), &Usage::default(), false)
                .priced
        );
    }

    #[test]
    fn claude_code_cache_beats_builtin_and_user_beats_both() {
        let mut b = PriceBook::builtin();
        let cache = serde_json::json!({"claude-opus-5-5": {"inputTokens": 3, "outputTokens": 15,
            "promptCacheWriteTokens": 3.75, "promptCacheWrite1hTokens": 6, "promptCacheReadTokens": 0.3,
            "webSearchRequests": 0.01}});
        b.merge_claude_code_cache(&"default".into(), &cache);
        let e = b.lookup("claude-opus-5-5", day()).unwrap();
        assert_eq!(e.rates.input, 3.0);
        assert_eq!(
            e.rates.fast_multiplier,
            Some(2.0),
            "fast rule kept from the built-in entry"
        );
        b.push(PriceEntry {
            model: "claude-opus-*".into(),
            from: None,
            rates: Rates {
                input: 1.0,
                ..e.rates
            },
            source: PriceSource::User,
        });
        assert_eq!(b.lookup("claude-opus-5-5", day()).unwrap().rates.input, 1.0);
    }

    #[test]
    fn effective_dates_pick_the_version_in_force() {
        let mut b = PriceBook::default();
        let r = PriceBook::builtin()
            .lookup("claude-opus-5-5", day())
            .unwrap()
            .rates;
        let d = |m, dd| NaiveDate::from_ymd_opt(2026, m, dd).unwrap();
        b.push(PriceEntry {
            model: "m".into(),
            from: Some(d(1, 1)),
            rates: Rates { input: 1.0, ..r },
            source: PriceSource::User,
        });
        b.push(PriceEntry {
            model: "m".into(),
            from: Some(d(6, 1)),
            rates: Rates { input: 2.0, ..r },
            source: PriceSource::User,
        });
        assert_eq!(b.lookup("m", d(5, 31)).unwrap().rates.input, 1.0);
        assert_eq!(b.lookup("m", d(6, 1)).unwrap().rates.input, 2.0);
        assert!(
            b.lookup("m", NaiveDate::from_ymd_opt(2025, 12, 31).unwrap())
                .is_none()
        );
    }

    #[test]
    fn a_user_override_keeps_the_fast_rule_and_the_cache_ratio() {
        // `[[price]] model = "claude-opus-5-5" input = 3 output = 15`: no fast multiplier and no
        // cache rates given, so they come from the built-in entry (fast ×2, reads 0.20/4 = 5%).
        let mut b = PriceBook::builtin();
        b.push_override(
            "claude-opus-5-5",
            None,
            3.0,
            15.0,
            &RateOverrides::default(),
            day(),
        )
        .unwrap();
        let e = b.lookup("claude-opus-5-5", day()).unwrap();
        assert_eq!(e.source, PriceSource::User);
        assert_eq!(e.rates.fast_multiplier, Some(2.0));
        // Reads at 5% of $3 = $0.15; writes 1.25× and 2× = $3.75 and $6.
        assert!(close(e.rates.cache_read, 0.15), "{}", e.rates.cache_read);
        assert!(close(e.rates.cache_write_5m, 3.75));
        assert!(close(e.rates.cache_write_1h, 6.0));
        // Fast mode doubles: 1M in + 1M out = (3 + 15) × 2 = $36.
        let u = Usage {
            input: 1_000_000,
            output: 1_000_000,
            ..Default::default()
        };
        assert!(close(b.cost("claude-opus-5-5", day(), &u, true).usd, 36.0));
        // Haiku 5.5 keeps its long-context rule under an override.
        b.push_override(
            "claude-haiku-5-5",
            None,
            0.2,
            1.0,
            &RateOverrides::default(),
            day(),
        )
        .unwrap();
        assert!(
            b.lookup("claude-haiku-5-5", day())
                .unwrap()
                .rates
                .long_context
                .is_some()
        );
    }

    #[test]
    fn invalid_rates_are_refused() {
        let mut b = PriceBook::builtin();
        for (input, over) in [
            (-1.0, RateOverrides::default()),
            (f64::NAN, RateOverrides::default()),
            (
                1.0,
                RateOverrides {
                    cache_read: Some(f64::INFINITY),
                    ..Default::default()
                },
            ),
            (
                1.0,
                RateOverrides {
                    fast_multiplier: Some(0.0),
                    ..Default::default()
                },
            ),
        ] {
            let r = b.push_override("claude-opus-5-5", None, input, 5.0, &over, day());
            assert!(r.is_err(), "{input} {over:?}");
        }
        assert_eq!(b.entries().len(), PriceBook::builtin().entries().len());
    }

    #[test]
    fn a_cache_entry_without_a_read_price_uses_the_models_own_ratio() {
        // Claude Code's cache gives Fable 5.2 at $8 / $40 with no cache-read price: the built-in
        // ratio (0.20 / 8 = 2.5%) applies, so reads cost $0.20, not 10% = $0.80.
        let mut b = PriceBook::builtin();
        let cache = serde_json::json!({"claude-fable-5-2": {"inputTokens": 8, "outputTokens": 40}});
        b.merge_claude_code_cache(&"work".into(), &cache);
        let e = b.lookup("claude-fable-5-2", day()).unwrap();
        assert_eq!(e.source, PriceSource::ClaudeCode("work".into()));
        assert!(close(e.rates.cache_read, 0.2), "{}", e.rates.cache_read);
        // A model nobody else prices falls back to the general 10%.
        let cache = serde_json::json!({"claude-new-9": {"inputTokens": 2, "outputTokens": 10}});
        b.merge_claude_code_cache(&"work".into(), &cache);
        let e = b.lookup("claude-new-9", day()).unwrap();
        assert!(close(e.rates.cache_read, 0.2), "{}", e.rates.cache_read);
        // Negative prices in a cache are skipped.
        let cache = serde_json::json!({"claude-bad-1": {"inputTokens": -2, "outputTokens": 10}});
        b.merge_claude_code_cache(&"work".into(), &cache);
        assert!(b.lookup("claude-bad-1", day()).is_none());
    }

    #[test]
    fn built_in_prices_start_on_their_models_date() {
        let b = PriceBook::builtin();
        // Claude Sonnet 4.5's first snapshot is 2025-09-29.
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert!(b.lookup("claude-sonnet-4-5", d(2025, 9, 28)).is_none());
        assert!(b.lookup("claude-sonnet-4-5", d(2025, 9, 29)).is_some());
        assert!(
            b.entries()
                .iter()
                .all(|e| e.from.is_some_and(|f| f >= d(2024, 1, 1)))
        );
    }

    #[test]
    fn the_parts_add_up_to_the_cost_with_fast_mode_and_long_context() {
        let book = PriceBook::builtin();
        let u = Usage {
            input: 300_000,
            cache_write_5m: 20_000,
            cache_write_1h: 10_000,
            cache_read: 100_000,
            output: 4_000,
            web_search: 3,
            ..Default::default()
        };
        for (model, fast) in [
            ("claude-opus-5-5", true),
            ("claude-opus-5-5", false),
            ("claude-haiku-5-5", false),
            ("claude-sonnet-5-5", true),
        ] {
            let p = book.cost_parts(model, day(), &u, fast).unwrap();
            let c = book.cost(model, day(), &u, fast);
            assert!(c.priced);
            assert!(close(p.total(), c.usd), "{model} fast={fast}");
            assert!(p.cache_saved > Usd::ZERO);
        }
        // Fast-mode Opus 5.5 by hand ($4 input, $20 output per MTok, $0.01 per search, fast ×2):
        // input 300k × 4 = $1.20 → $2.40; output 4k × 20 = $0.08 → $0.16; web 3 × 0.01 = $0.03
        // → $0.06; cache reads 100k × 0.20 = $0.02 → $0.04, saving 100k × (4 − 0.2) → $0.76.
        let p = book.cost_parts("claude-opus-5-5", day(), &u, true).unwrap();
        assert!(close(p.input, 2.4) && close(p.output, 0.16) && close(p.web, 0.06));
        assert!(close(p.cache_read, 0.04) && close(p.cache_saved, 0.76));
        // Long context: the prompt (300k + 20k + 10k + 100k = 430k) is above Haiku 5.5's 100k
        // threshold, so every part is ×5: input 300k × 0.10 × 5 = $0.15; output 4k × 0.50 × 5 =
        // $0.01. The old split used the plain rates, five times too low.
        let p = book
            .cost_parts("claude-haiku-5-5", day(), &u, false)
            .unwrap();
        assert!(close(p.input, 0.15) && close(p.output, 0.01), "{p:?}");
        // In a band given, sums cost what their requests do: two Haiku prompts above the
        // threshold, priced one by one and as one sum (the archive reprices its rollups so).
        let half = Usage {
            input: 150_000,
            cache_write_5m: 10_000,
            cache_write_1h: 5_000,
            cache_read: 50_000,
            output: 2_000,
            web_search: 0,
            ..Default::default()
        };
        let one = book
            .cost_parts("claude-haiku-5-5", day(), &half, false)
            .unwrap();
        let both = book
            .cost_parts_in_band("claude-haiku-5-5", day(), &u, false, true)
            .unwrap();
        assert!(close(one.input * 2.0, both.input) && close(one.output * 2.0, both.output));
        // Outside the band the same sums take the plain rates: input 300k × 0.10 = $0.03.
        let plain = book
            .cost_parts_in_band("claude-haiku-5-5", day(), &u, false, false)
            .unwrap();
        assert!(close(plain.input, 0.03), "{plain:?}");
        // No price: no parts; no tokens: zero parts, priced.
        assert_eq!(book.cost_parts("claude-unknown-9", day(), &u, false), None);
        assert_eq!(
            book.cost_parts("claude-unknown-9", day(), &Usage::default(), false),
            Some(CostParts::default())
        );
    }

    #[test]
    fn gemini_builtin_prices_and_long_context_by_hand() {
        let book = PriceBook::builtin();
        let d = NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        // gemini-2.5-pro (≤ 200k prompt): $1.25 input, $0.125 cached, $10.00 output per MTok.
        // 100k input = $0.125; 80k cached = $0.010; 20k output = $0.200 -> $0.335.
        // Cache saved: 80k × ($1.25 - $0.125) / 1M = $0.090.
        let normal = Usage {
            input: 100_000,
            cache_read: 80_000,
            output: 20_000,
            ..Default::default()
        };
        let p = book
            .cost_parts("gemini-2.5-pro", d, &normal, false)
            .unwrap();
        assert!(close(p.input, 0.125), "{p:?}");
        assert!(close(p.cache_read, 0.010), "{p:?}");
        assert!(close(p.output, 0.200), "{p:?}");
        assert!(close(p.total(), 0.335), "{p:?}");
        assert!(close(p.cache_saved, 0.090), "{p:?}");

        // gemini-2.5-pro (> 200k prompt: 150k + 100k = 250k): input ×2 ($2.50), cached ×2
        // ($0.25), output ×1.5 ($15.00).
        // 150k × 2.50 = $0.375; 100k × 0.25 = $0.025; 20k × 15.00 = $0.300 -> $0.700.
        let long = Usage {
            input: 150_000,
            cache_read: 100_000,
            output: 20_000,
            ..Default::default()
        };
        let lp = book.cost_parts("gemini-2.5-pro", d, &long, false).unwrap();
        assert!(close(lp.input, 0.375), "{lp:?}");
        assert!(close(lp.cache_read, 0.025), "{lp:?}");
        assert!(close(lp.output, 0.300), "{lp:?}");
        assert!(close(lp.total(), 0.700), "{lp:?}");

        // A preview id matches its pattern (gemini-3-pro-preview -> $2 / $0.20 / $12).
        // 50k input = $0.10; 50k cached = $0.01; 10k output = $0.12 -> $0.23.
        let g3 = Usage {
            input: 50_000,
            cache_read: 50_000,
            output: 10_000,
            ..Default::default()
        };
        let c3 = book.cost("gemini-3-pro-preview", d, &g3, false);
        assert!(c3.priced);
        assert!(close(c3.usd.get(), 0.23), "{c3:?}");

        // gemini-4-neon, what Antigravity runs ($3.00 / $0.15 cached / $15.00):
        // 10k input = $0.030; 100k cached = $0.015; 2k output = $0.030 -> $0.075.
        let neon = Usage {
            input: 10_000,
            cache_read: 100_000,
            output: 2_000,
            ..Default::default()
        };
        let cn = book.cost("gemini-4-neon", d, &neon, false);
        assert!(cn.priced);
        assert!(close(cn.usd.get(), 0.075), "{cn:?}");

        // gemini-4-carbon doubles on 2026-12-01: 100k input + 10k output
        // through 2026-11-30: 100k × $2 = $0.20 + 10k × $10 = $0.10 -> $0.30;
        // from 2026-12-01:    100k × $4 = $0.40 + 10k × $20 = $0.20 -> $0.60.
        let u = Usage {
            input: 100_000,
            output: 10_000,
            ..Default::default()
        };
        let nov30 = NaiveDate::from_ymd_opt(2026, 11, 30).unwrap();
        let dec1 = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        assert!(close(
            book.cost("gemini-4-carbon", nov30, &u, false).usd.get(),
            0.30
        ));
        assert!(close(
            book.cost("gemini-4-carbon", dec1, &u, false).usd.get(),
            0.60
        ));

        // A model with no list price stays unpriced, never guessed.
        let unknown = book.cost("gemini-9-unknown", d, &g3, false);
        assert!(!unknown.priced);
        assert_eq!(unknown.usd, Usd::ZERO);
    }

    #[test]
    fn openai_builtin_prices_long_context_and_priority_by_hand() {
        let book = PriceBook::builtin();
        let d = NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        // gpt-6-luna, Codex's default ($0.10 input, $0.01 cached, $0.125 cache write = 1.25x,
        // $0.50 output per MTok). Prompt 100k + 10k + 150k = 260k, under 272k:
        // 100k × 0.10 = $0.010; 10k × 0.125 = $0.00125; 150k × 0.01 = $0.0015;
        // 20k × 0.50 = $0.010 -> $0.02275.
        let normal = Usage {
            input: 100_000,
            cache_write_5m: 10_000,
            cache_read: 150_000,
            output: 20_000,
            ..Default::default()
        };
        let p = book.cost_parts("gpt-6-luna", d, &normal, false).unwrap();
        assert!(close(p.input, 0.010), "{p:?}");
        assert!(close(p.cache_write, 0.00125), "{p:?}");
        assert!(close(p.cache_read, 0.0015), "{p:?}");
        assert!(close(p.output, 0.010), "{p:?}");
        assert!(close(p.total(), 0.02275), "{p:?}");
        // The priority tier (fast) doubles it: $0.0455.
        let fast = book.cost("gpt-6-luna", d, &normal, true);
        assert!(close(fast.usd.get(), 0.0455), "{fast:?}");

        // Above 272k (200k + 100k = 300k): input and cached x2, output x1.5:
        // 200k × 0.20 = $0.040; 100k × 0.02 = $0.002; 10k × 0.75 = $0.0075 -> $0.0495.
        let long = Usage {
            input: 200_000,
            cache_read: 100_000,
            output: 10_000,
            ..Default::default()
        };
        let lp = book.cost_parts("gpt-6-luna", d, &long, false).unwrap();
        assert!(close(lp.total(), 0.0495), "{lp:?}");

        // gpt-5.1-codex: no cache-write premium before GPT-5.6, so writes cost the input rate.
        // 100k × 1.25 = $0.125; 10k × 1.25 = $0.0125; 100k × 0.125 = $0.0125;
        // 10k × 10.00 = $0.100 -> $0.25.
        let u = Usage {
            input: 100_000,
            cache_write_5m: 10_000,
            cache_read: 100_000,
            output: 10_000,
            ..Default::default()
        };
        let c = book.cost("gpt-5.1-codex", d, &u, false);
        assert!(close(c.usd.get(), 0.25), "{c:?}");

        // Before its snapshot (2026-09-22) gpt-6-luna has no price.
        let before = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        assert!(!book.cost("gpt-6-luna", before, &normal, false).priced);
    }
}
