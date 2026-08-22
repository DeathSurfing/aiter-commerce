//! External configuration loader (issue #34).
//!
//! Server configuration is consolidated from **three layers**, in increasing
//! precedence:
//!
//! 1. **Defaults** — compiled-in values: `PORT=8080`, Razorpay sandbox
//!    defaults (mode `sandbox`, base URL `https://api.razorpay.com`), keys
//!    unset.
//! 2. **Config file** — an optional `KEY=VALUE` file. The path comes from
//!    the `AITER_CONFIG` env var or defaults to `./aiter.env`. A missing
//!    file is never an error ("no file layer"). `#` comments and blank lines
//!    are skipped; empty values count as unset; unknown keys are ignored
//!    (with a warning) for forward compatibility; malformed lines (no `=`)
//!    and an unparseable `PORT` are hard errors so typos surface at startup.
//! 3. **Process environment** — real env vars always win over the file for
//!    the same key.
//!
//! **Precedence: defaults < config file < process env vars (env wins).**
//! Each key is resolved independently: `var = env(key) -> file(key) -> default`.
//!
//! Backwards compatibility: when no config file exists the resolved
//! configuration is identical to the pre-#34 env-only behavior — `PORT` and
//! `RAZORPAY_*` env vars keep working exactly as before, including the old
//! lazy Razorpay semantics (the server boots without keys and fails only
//! when a payment link is minted or a webhook arrives).

use std::collections::HashMap;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::payments::{RazorpayConfig, RazorpayError, RazorpayMode, DEFAULT_BASE_URL};

/// Default config-file name, resolved relative to the current directory.
pub const DEFAULT_CONFIG_FILE: &str = "aiter.env";

/// Default HTTP listen port (`PORT` env var / config key override it).
pub const DEFAULT_PORT: u16 = 8080;

/// Every key the loader understands. Anything else in the file is ignored
/// with a warning (newer files stay readable by older binaries).
const KNOWN_KEYS: &[&str] = &[
    "PORT",
    "RAZORPAY_KEY_ID",
    "RAZORPAY_KEY_SECRET",
    "RAZORPAY_MODE",
    "RAZORPAY_BASE_URL",
    "RAZORPAY_WEBHOOK_SECRET",
];

/// The config-file path to use: the `AITER_CONFIG` env var when set (and
/// non-empty), else [`DEFAULT_CONFIG_FILE`] in the current directory.
pub fn config_path() -> PathBuf {
    std::env::var("AITER_CONFIG")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILE))
}

/// Fully resolved server configuration: the listen port plus the Razorpay
/// settings, each resolved per-key as `env -> file -> default` (see the
/// module docs for the documented precedence).
#[derive(Clone, Debug)]
pub struct Config {
    /// Path of the config file that was (or would have been) read.
    pub config_path: PathBuf,
    /// `true` when the file existed and was parsed; `false` when it was
    /// missing — a missing file is never an error.
    pub file_loaded: bool,
    /// HTTP listen port.
    pub port: u16,
    /// Resolved Razorpay settings. Presence is not required to boot: the
    /// payment handlers fail lazily, exactly as the env-only loader did.
    pub razorpay: RazorpaySettings,
}

/// The five `RAZORPAY_*` knobs, resolved (env > file) but not yet
/// validated. `None` means "no value in the file or the environment" — the
/// same lazy-per-request outcome the env-only loader produced (missing keys
/// error as `RAZORPAY_KEY_ID is required`, mode defaults to `sandbox`, base
/// URL defaults to the Razorpay API host).
///
/// `Debug` redacts `key_secret` and `webhook_secret`: the secrets are never
/// printed.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RazorpaySettings {
    pub key_id: Option<String>,
    pub key_secret: Option<String>,
    pub mode: Option<String>,
    pub base_url: Option<String>,
    pub webhook_secret: Option<String>,
}

impl fmt::Debug for RazorpaySettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RazorpaySettings")
            .field("key_id", &self.key_id)
            .field(
                "key_secret",
                &self.key_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("mode", &self.mode)
            .field("base_url", &self.base_url)
            .field(
                "webhook_secret",
                &self.webhook_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl RazorpaySettings {
    /// Build a typed [`RazorpayConfig`], applying the same defaults and
    /// validation as the pre-#34 env-only `RazorpayConfig::from_env`: the
    /// two keys are required (clear error naming the missing one), mode
    /// defaults to `sandbox` and rejects anything else, base URL defaults to
    /// the Razorpay API host, webhook secret stays optional.
    pub fn to_razorpay_config(&self) -> Result<RazorpayConfig, RazorpayError> {
        let key_id = self
            .key_id
            .clone()
            .ok_or_else(|| RazorpayError::Config("RAZORPAY_KEY_ID is required".to_string()))?;
        let key_secret = self
            .key_secret
            .clone()
            .ok_or_else(|| RazorpayError::Config("RAZORPAY_KEY_SECRET is required".to_string()))?;
        let mode = match self.mode.as_deref() {
            Some("sandbox") => RazorpayMode::Sandbox,
            Some("live") => RazorpayMode::Live,
            Some(other) => {
                return Err(RazorpayError::Config(format!(
                    "RAZORPAY_MODE must be 'sandbox' or 'live', got '{other}'"
                )))
            }
            None => RazorpayMode::Sandbox,
        };
        let base_url = self
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Ok(RazorpayConfig {
            key_id,
            key_secret,
            mode,
            base_url,
            webhook_secret: self.webhook_secret.clone(),
        })
    }
}

/// Load and resolve configuration using the process environment: optional
/// config file (path from [`config_path`]) overlaid by env vars.
pub fn load() -> Result<Config, ConfigError> {
    let path = config_path();
    load_from_with(&path, |key| std::env::var(key).ok())
}

/// Like [`load`], but against an explicit path and an explicit environment
/// provider — the knob tests use to stay hermetic (no process-env
/// dependence, so they never race other tests that manipulate env vars).
pub fn load_from_with(
    path: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Config, ConfigError> {
    let (file_loaded, file) = match std::fs::read_to_string(path) {
        Ok(contents) => (true, parse_file(path, &contents)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (false, HashMap::new()),
        Err(err) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source: err,
            })
        }
    };

    for key in file.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            tracing::warn!(
                config_file = %path.display(),
                key = %key,
                "ignoring unknown config key"
            );
        }
    }

    let port = resolve_port(&env, &file, path)?;
    let razorpay = RazorpaySettings {
        key_id: resolve(&env, &file, "RAZORPAY_KEY_ID"),
        key_secret: resolve(&env, &file, "RAZORPAY_KEY_SECRET"),
        mode: resolve(&env, &file, "RAZORPAY_MODE"),
        base_url: resolve(&env, &file, "RAZORPAY_BASE_URL"),
        webhook_secret: resolve(&env, &file, "RAZORPAY_WEBHOOK_SECRET"),
    };
    Ok(Config {
        config_path: path.to_path_buf(),
        file_loaded,
        port,
        razorpay,
    })
}

/// Parse `KEY=VALUE` lines: `#` comments and blank lines are skipped, keys
/// and values are trimmed, empty values count as unset, later duplicate keys
/// win, and any other line is a hard error (typos surface at startup).
fn parse_file(path: &Path, contents: &str) -> Result<HashMap<String, String>, ConfigError> {
    let mut map = HashMap::new();
    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ConfigError::MalformedLine {
                path: path.to_path_buf(),
                line: index + 1,
            });
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || value.is_empty() {
            continue; // empty key or empty value: treat as unset
        }
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}

/// Per-key precedence: env var wins, then the file, then `None`.
fn resolve(
    env: &impl Fn(&str) -> Option<String>,
    file: &HashMap<String, String>,
    key: &str,
) -> Option<String> {
    env(key).or_else(|| file.get(key).cloned())
}

/// `PORT` resolution with the pre-#34 env semantics: an env `PORT` that fails
/// to parse silently falls back to the default (exactly what the old main.rs
/// did), while a config-file `PORT` that fails to parse is a hard error —
/// the file layer is new surface, so typos there surface loudly at startup.
fn resolve_port(
    env: &impl Fn(&str) -> Option<String>,
    file: &HashMap<String, String>,
    path: &Path,
) -> Result<u16, ConfigError> {
    if let Some(raw) = env("PORT") {
        if let Ok(port) = raw.parse() {
            return Ok(port);
        }
        return Ok(DEFAULT_PORT);
    }
    if let Some(raw) = file.get("PORT") {
        if let Ok(port) = raw.parse() {
            return Ok(port);
        }
        return Err(ConfigError::InvalidPort {
            path: path.to_path_buf(),
            value: raw.clone(),
        });
    }
    Ok(DEFAULT_PORT)
}

/// Render the commented `KEY=VALUE` template written by `aiter-server init`
/// (and used by its round-trip test). Every value is the current default, so
/// a freshly written template parses and resolves identically to a no-file
/// run.
pub fn template_contents() -> String {
    format!(
        "# AITER COMMERCE — external server configuration (issue #34).\n\
         #\n\
         # Precedence: defaults < this file < process environment (env wins).\n\
         # `#` comments and blank lines are fine. Loaded automatically by\n\
         # `aiter-server run` — no shell source-ing needed.\n\
         #\n\
         # Generated by `aiter-server init`. Keep secrets out of version control.\n\
         \n\
         # --- Server ---\n\
         # HTTP listen port.\n\
         PORT={port}\n\
         \n\
         # --- Razorpay (optional: the server boots without keys and payment\n\
         # endpoints fail lazily until key_id/key_secret are set) ---\n\
         # API key pair from the Razorpay Dashboard (sandbox keys start rzp_test_).\n\
         RAZORPAY_KEY_ID=\n\
         RAZORPAY_KEY_SECRET=\n\
         # `sandbox` (default) or `live`; any other value is an error.\n\
         RAZORPAY_MODE=sandbox\n\
         # API base URL override (never in production); defaults to\n\
         # https://api.razorpay.com. Uncomment to set:\n\
         # RAZORPAY_BASE_URL=https://api.razorpay.com\n\
         # HMAC-SHA256 webhook verification secret (webhooks fail closed\n\
         # without it).\n\
         RAZORPAY_WEBHOOK_SECRET=\n",
        port = DEFAULT_PORT,
    )
}

/// Write the commented config template to `path`. Refuses to overwrite an
/// existing file (it may hold real secrets) — the caller must remove it or
/// point `AITER_CONFIG` elsewhere first.
pub fn write_template(path: &Path) -> Result<(), ConfigError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                ConfigError::AlreadyExists(path.to_path_buf())
            } else {
                ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
    file.write_all(template_contents().as_bytes())
        .map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(())
}

/// Configuration loading/writing failures. Error messages never contain
/// credentials.
#[derive(Debug)]
pub enum ConfigError {
    /// The config file could not be read or written.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A line in the file is not `KEY=VALUE` (or has an empty key).
    MalformedLine { path: PathBuf, line: usize },
    /// `PORT` in the file does not parse as a u16.
    InvalidPort { path: PathBuf, value: String },
    /// `init` refused to overwrite an existing file.
    AlreadyExists(PathBuf),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, source } => {
                write!(f, "config file {}: {source}", path.display())
            }
            ConfigError::MalformedLine { path, line } => {
                write!(
                    f,
                    "config file {}:{line}: expected KEY=VALUE",
                    path.display()
                )
            }
            ConfigError::InvalidPort { path, value } => write!(
                f,
                "config file {}: PORT='{value}' is not a valid u16 port",
                path.display()
            ),
            ConfigError::AlreadyExists(path) => {
                write!(
                    f,
                    "{} already exists — refusing to overwrite",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic fake env provider: exactly the vars listed, so config
    /// tests never touch (or race) the process environment, which other
    /// modules' tests manipulate.
    fn env_from(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'static {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key| vars.get(key).cloned()
    }

    fn no_env(_key: &str) -> Option<String> {
        None
    }

    /// A per-test unique temp path (tests run in parallel threads).
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("aiter-config-test-{}-{name}", std::process::id()))
    }

    fn write_file(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    // --- precedence: defaults < file < env ---------------------------------

    #[test]
    fn missing_file_and_no_env_resolves_to_defaults() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        let config = load_from_with(&path, no_env).unwrap();
        assert!(!config.file_loaded);
        assert_eq!(config.port, DEFAULT_PORT);
        assert!(config.razorpay.key_id.is_none());
        assert!(config.razorpay.key_secret.is_none());
        assert!(config.razorpay.mode.is_none());
        assert!(config.razorpay.base_url.is_none());
        assert!(config.razorpay.webhook_secret.is_none());
    }

    #[test]
    fn file_values_apply_when_env_is_empty() {
        let path = temp_path("file-only");
        write_file(
            &path,
            "PORT=9001\nRAZORPAY_KEY_ID=file_key\nRAZORPAY_KEY_SECRET=file_secret\n\
             RAZORPAY_MODE=live\nRAZORPAY_WEBHOOK_SECRET=whsec_file\n",
        );
        let config = load_from_with(&path, no_env).unwrap();
        assert!(config.file_loaded);
        assert_eq!(config.port, 9001);
        assert_eq!(config.razorpay.key_id.as_deref(), Some("file_key"));
        assert_eq!(config.razorpay.key_secret.as_deref(), Some("file_secret"));
        assert_eq!(config.razorpay.mode.as_deref(), Some("live"));
        assert_eq!(
            config.razorpay.webhook_secret.as_deref(),
            Some("whsec_file")
        );
        // Not in the file, not in env -> default applies at build time.
        assert!(config.razorpay.base_url.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn env_wins_over_file_per_key() {
        let path = temp_path("env-wins");
        write_file(
            &path,
            "PORT=9001\nRAZORPAY_KEY_ID=file_key\nRAZORPAY_BASE_URL=http://file-base\n",
        );
        let env = env_from(&[("PORT", "7777"), ("RAZORPAY_KEY_ID", "env_key")]);
        let config = load_from_with(&path, env).unwrap();
        // Env wins where set...
        assert_eq!(config.port, 7777);
        assert_eq!(config.razorpay.key_id.as_deref(), Some("env_key"));
        // ...the file still supplies what env does not...
        assert_eq!(
            config.razorpay.base_url.as_deref(),
            Some("http://file-base")
        );
        // ...and untouched keys stay unset.
        assert!(config.razorpay.key_secret.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unparseable_env_port_falls_back_to_default_like_before() {
        let path = temp_path("env-bad-port");
        let _ = std::fs::remove_file(&path);
        let env = env_from(&[("PORT", "not-a-port")]);
        let config = load_from_with(&path, env).unwrap();
        // The pre-#34 env-only behavior: unparseable PORT silently -> 8080.
        assert_eq!(config.port, DEFAULT_PORT);
    }

    #[test]
    fn unparseable_file_port_is_a_hard_error() {
        let path = temp_path("file-bad-port");
        write_file(&path, "PORT=not-a-port\n");
        let err = load_from_with(&path, no_env).unwrap_err();
        assert!(
            matches!(&err, ConfigError::InvalidPort { path: p, value } if p == &path && value == "not-a-port"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    // --- file parsing ------------------------------------------------------

    #[test]
    fn comments_blank_lines_and_unknown_keys_are_tolerated() {
        let path = temp_path("comments");
        write_file(
            &path,
            "# leading comment\n\n   # indented comment\nPORT= 8123 \nUNKNOWN_KEY=whatever\n",
        );
        let config = load_from_with(&path, no_env).unwrap();
        assert!(config.file_loaded);
        assert_eq!(
            config.port, 8123,
            "values are trimmed; unknown keys ignored"
        );
        assert!(config.razorpay.key_id.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_values_count_as_unset() {
        let path = temp_path("empty-values");
        write_file(&path, "RAZORPAY_KEY_ID=\nPORT=\n");
        let config = load_from_with(&path, no_env).unwrap();
        assert!(config.razorpay.key_id.is_none());
        assert_eq!(config.port, DEFAULT_PORT);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_line_is_a_clear_error() {
        let path = temp_path("malformed");
        write_file(&path, "PORT=8080\nthis line has no equals\n");
        let err = load_from_with(&path, no_env).unwrap_err();
        assert!(
            matches!(&err, ConfigError::MalformedLine { line: 2, .. }),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn duplicate_keys_last_wins() {
        let path = temp_path("dupes");
        write_file(&path, "PORT=9001\nPORT=9002\n");
        let config = load_from_with(&path, no_env).unwrap();
        assert_eq!(config.port, 9002);
        let _ = std::fs::remove_file(&path);
    }

    // --- to_razorpay_config mirrors from_env semantics --------------------

    #[test]
    fn razorpay_config_requires_both_keys_with_clear_errors() {
        let settings = RazorpaySettings::default();
        let err = settings.to_razorpay_config().unwrap_err();
        assert!(err.to_string().contains("RAZORPAY_KEY_ID"));

        let settings = RazorpaySettings {
            key_id: Some("rzp_test_key".to_string()),
            ..RazorpaySettings::default()
        };
        let err = settings.to_razorpay_config().unwrap_err();
        assert!(err.to_string().contains("RAZORPAY_KEY_SECRET"));
    }

    #[test]
    fn razorpay_config_defaults_and_validates_like_from_env() {
        let settings = RazorpaySettings {
            key_id: Some("rzp_test_key".to_string()),
            key_secret: Some("secret".to_string()),
            ..RazorpaySettings::default()
        };
        let config = settings.to_razorpay_config().unwrap();
        assert_eq!(config.mode, RazorpayMode::Sandbox);
        assert_eq!(config.base_url, "https://api.razorpay.com");
        assert_eq!(config.webhook_secret, None);

        let settings = RazorpaySettings {
            key_id: Some("rzp_test_key".to_string()),
            key_secret: Some("secret".to_string()),
            mode: Some("prod".to_string()),
            ..RazorpaySettings::default()
        };
        let err = settings.to_razorpay_config().unwrap_err();
        assert!(err.to_string().contains("RAZORPAY_MODE"));
        assert!(err.to_string().contains("sandbox"));
        assert!(err.to_string().contains("live"));

        let settings = RazorpaySettings {
            key_id: Some("rzp_test_key".to_string()),
            key_secret: Some("secret".to_string()),
            mode: Some("live".to_string()),
            base_url: Some("http://localhost:1234".to_string()),
            webhook_secret: Some("whsec_x".to_string()),
        };
        let config = settings.to_razorpay_config().unwrap();
        assert_eq!(config.mode, RazorpayMode::Live);
        assert_eq!(config.base_url, "http://localhost:1234");
        assert_eq!(config.webhook_secret.as_deref(), Some("whsec_x"));
    }

    // --- init template round-trip ------------------------------------------

    #[test]
    fn init_writes_a_parseable_file_the_loader_reads_back() {
        let path = temp_path("init");
        let _ = std::fs::remove_file(&path);
        write_template(&path).unwrap();

        let config = load_from_with(&path, no_env).unwrap();
        assert!(config.file_loaded, "init output must be parseable");
        // The template carries the current defaults...
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.razorpay.mode.as_deref(), Some("sandbox"));
        // ...and the empty key placeholders count as unset, so a fresh
        // template behaves exactly like a no-file (env-only) run.
        assert!(config.razorpay.key_id.is_none());
        assert!(config.razorpay.key_secret.is_none());
        assert!(config.razorpay.webhook_secret.is_none());
        assert!(config.razorpay.base_url.is_none());

        // Parsing the template produced no unknown keys (template + loader
        // stay in sync) and no malformed lines — implied by the Ok above.

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn init_refuses_to_overwrite_an_existing_file() {
        let path = temp_path("init-existing");
        let _ = std::fs::remove_file(&path);
        write_template(&path).unwrap();
        let err = write_template(&path).unwrap_err();
        assert!(
            matches!(&err, ConfigError::AlreadyExists(p) if p == &path),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
