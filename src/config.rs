//! `~/.warden/config.toml` (MVP §7). Every field is optional and has a default.
//!
//! Pricing lives here and only here — the binary never hardcodes a rate, and an
//! unpriced model yields `None` rather than a misleading `0.0`.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Loaded configuration, with defaults already applied.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    /// Source adapters keyed by adapter name, e.g. `claude-code`.
    pub sources: BTreeMap<String, Source>,
    /// Per-provider price tables keyed by provider, e.g. `anthropic`.
    pub pricing: BTreeMap<String, BTreeMap<String, ModelPrice>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct General {
    /// Store location. `None` means "use the built-in default" (`~/.warden`).
    pub data_dir: Option<PathBuf>,
    /// Store prompt text alongside `text_hash` (MVP §6).
    pub index_prompt_text: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            data_dir: None,
            index_prompt_text: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Source {
    pub enabled: bool,
    /// Log root for this adapter; `None` means the adapter's own default.
    pub path: Option<PathBuf>,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
        }
    }
}

/// Per-million-token rates for one model. `cache_write` is optional because not
/// every provider bills it separately.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ModelPrice {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: Option<f64>,
}

/// Token counts to price. Absent counts are `None`, never `0`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCounts {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

impl Config {
    /// Read `config.toml` from `dir`. A missing file is not an error — it means
    /// "all defaults".
    pub fn load_from_dir(dir: &Path) -> Result<Self, ConfigError> {
        Self::load_file(&dir.join("config.toml"))
    }

    /// Read a specific config file. A missing file yields the defaults.
    pub fn load_file(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(ConfigError::Read(path.to_path_buf(), err)),
        };
        toml::from_str(&text).map_err(|err| ConfigError::Parse(path.to_path_buf(), err))
    }

    /// Configured source settings, or the defaults for an unconfigured adapter.
    pub fn source(&self, adapter: &str) -> Source {
        self.sources.get(adapter).cloned().unwrap_or_default()
    }

    /// Configured price for a model, if any.
    pub fn price(&self, provider: &str, model: &str) -> Option<ModelPrice> {
        self.pricing.get(provider)?.get(model).copied()
    }

    /// Estimate cost in whole currency units for the given token counts.
    ///
    /// Returns `None` when the model has no configured price — callers must
    /// propagate the absence rather than substituting `0.0` (MVP §2.5).
    pub fn estimate_cost(&self, provider: &str, model: &str, tokens: TokenCounts) -> Option<f64> {
        self.pricing().estimate_cost(provider, model, tokens)
    }

    /// The price table on its own, detached from the rest of the config.
    ///
    /// Reports price at *read* time, so they carry this rather than a borrow of
    /// the whole config: editing `config.toml` re-prices events that are already
    /// in the store, without a re-ingest (MVP §2.5).
    pub fn pricing(&self) -> Pricing {
        Pricing {
            table: self.pricing.clone(),
        }
    }
}

/// A price table, owned. Empty by default, and an empty table prices nothing.
#[derive(Debug, Clone, Default)]
pub struct Pricing {
    table: BTreeMap<String, BTreeMap<String, ModelPrice>>,
}

impl Pricing {
    /// Configured price for a model, if any.
    pub fn price(&self, provider: &str, model: &str) -> Option<ModelPrice> {
        self.table.get(provider)?.get(model).copied()
    }

    /// `None` when this model has no configured price — never `0.0`.
    pub fn estimate_cost(&self, provider: &str, model: &str, tokens: TokenCounts) -> Option<f64> {
        let price = self.price(provider, model)?;
        let per_million = |count: Option<u64>, rate: f64| count.unwrap_or(0) as f64 * rate / 1e6;
        // An unset cache_write rate falls back to the input rate, matching how
        // providers that do not bill writes separately behave.
        let cache_write_rate = price.cache_write.unwrap_or(price.input);
        Some(
            per_million(tokens.input, price.input)
                + per_million(tokens.output, price.output)
                + per_million(tokens.cache_read, price.cache_read)
                + per_million(tokens.cache_write, cache_write_rate),
        )
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read(PathBuf, io::Error),
    Parse(PathBuf, toml::de::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read(path, err) => write!(f, "reading {}: {err}", path.display()),
            ConfigError::Parse(path, err) => write!(f, "parsing {}: {err}", path.display()),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[general]
index_prompt_text = false

[sources.claude-code]
enabled = true
path = "/logs/claude"

[pricing.anthropic]
"claude-sonnet-4-6" = { input = 3.0, output = 15.0, cache_read = 0.3 }
"#;

    fn write(dir: &Path, text: &str) {
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from_dir(dir.path()).unwrap();
        assert!(cfg.general.index_prompt_text);
        assert!(cfg.general.data_dir.is_none());
        assert!(cfg.pricing.is_empty());
        // An unconfigured source is enabled with the adapter's own default path.
        let source = cfg.source("claude-code");
        assert!(source.enabled);
        assert!(source.path.is_none());
    }

    #[test]
    fn parses_all_sections() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), SAMPLE);
        let cfg = Config::load_from_dir(dir.path()).unwrap();
        assert!(!cfg.general.index_prompt_text);
        assert_eq!(
            cfg.source("claude-code").path.as_deref(),
            Some(Path::new("/logs/claude"))
        );
        let price = cfg.price("anthropic", "claude-sonnet-4-6").unwrap();
        assert_eq!(price.input, 3.0);
        assert_eq!(price.cache_write, None);
    }

    #[test]
    fn estimates_cost_from_config() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), SAMPLE);
        let cfg = Config::load_from_dir(dir.path()).unwrap();
        let tokens = TokenCounts {
            input: Some(1_000_000),
            output: Some(1_000_000),
            cache_read: Some(1_000_000),
            cache_write: None,
        };
        let cost = cfg
            .estimate_cost("anthropic", "claude-sonnet-4-6", tokens)
            .unwrap();
        assert!((cost - 18.3).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn unpriced_model_returns_none_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), SAMPLE);
        let cfg = Config::load_from_dir(dir.path()).unwrap();
        let tokens = TokenCounts {
            input: Some(1_000),
            ..TokenCounts::default()
        };
        assert!(cfg
            .estimate_cost("anthropic", "some-unlisted-model", tokens)
            .is_none());
        assert!(cfg.estimate_cost("openai", "gpt-x", tokens).is_none());
    }

    #[test]
    fn empty_config_prices_nothing() {
        let cfg = Config::default();
        assert!(cfg
            .estimate_cost("anthropic", "claude-sonnet-4-6", TokenCounts::default())
            .is_none());
    }
}
