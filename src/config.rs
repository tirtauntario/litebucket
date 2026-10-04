//! Non-secret TOML configuration: parsing, defaults, overrides, and validation.
//!
//! Unknown keys are errors. Paths resolve relative to the configuration file.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

const GIB: u64 = 1024 * 1024 * 1024;
/// S3 hard limits that configuration cannot exceed.
pub const HARD_MAX_SINGLE_PUT_BYTES: u64 = 5 * GIB;
pub const HARD_MAX_PART_BYTES: u64 = 5 * GIB;
pub const HARD_MAX_OBJECT_BYTES: u64 = 5 * 1024 * GIB;
pub const MIN_PART_BYTES: u64 = 5 * 1024 * 1024;
pub const HARD_MAX_PARTS: u32 = 10_000;
pub const HARD_MAX_USER_METADATA_BYTES: usize = 2048;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub data_dir: PathBuf,
    #[serde(default = "default_region")]
    pub region: String,
    pub credentials_file: PathBuf,
    /// Accept a group-readable credentials file (for example a secret mount with a
    /// dedicated group). World-readable files are always rejected.
    #[serde(default)]
    pub credentials_allow_group_read: bool,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub management: ManagementConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub multipart: MultipartConfig,
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Directory containing the configuration file; used to resolve relative paths.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

fn default_region() -> String {
    "us-east-1".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HttpConfig {
    pub listen: String,
    pub allow_insecure_loopback_http: bool,
    pub trusted_proxy_mode: bool,
    /// Peer addresses allowed to connect in trusted-proxy mode.
    pub trusted_proxy_addresses: Vec<String>,
    pub max_header_bytes: usize,
    pub max_header_count: usize,
    pub max_request_target_bytes: usize,
    pub header_timeout_seconds: u64,
    pub body_idle_timeout_seconds: u64,
    pub tls_certificate_file: Option<PathBuf>,
    pub tls_private_key_file: Option<PathBuf>,
    /// Allowed clock skew for header-signed requests.
    pub max_clock_skew_seconds: u64,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:9000".into(),
            allow_insecure_loopback_http: false,
            trusted_proxy_mode: false,
            trusted_proxy_addresses: Vec::new(),
            max_header_bytes: 32 * 1024,
            max_header_count: 128,
            max_request_target_bytes: 16 * 1024,
            header_timeout_seconds: 10,
            body_idle_timeout_seconds: 60,
            tls_certificate_file: None,
            tls_private_key_file: None,
            max_clock_skew_seconds: 15 * 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ManagementConfig {
    pub listen: String,
    pub metrics_enabled: bool,
}

impl Default for ManagementConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:9001".into(),
            metrics_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DatabaseConfig {
    pub reader_connections: usize,
    pub writer_queue_capacity: usize,
    pub reader_queue_capacity: usize,
    pub busy_timeout_ms: u64,
    /// How long a request may wait for metadata queue space before a retryable error.
    pub queue_wait_ms: u64,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            reader_connections: 2,
            writer_queue_capacity: 256,
            reader_queue_capacity: 128,
            busy_timeout_ms: 5000,
            queue_wait_ms: 5000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsConfig {
    pub active_uploads: usize,
    pub active_downloads: usize,
    pub active_copies: usize,
    pub active_multipart_assemblies: usize,
    pub max_single_put_bytes: u64,
    pub max_part_bytes: u64,
    pub max_object_bytes: u64,
    pub max_temporary_bytes: u64,
    pub min_disk_free_bytes: u64,
    pub min_disk_free_percent: u64,
    pub min_free_inodes: u64,
    pub max_buckets: u64,
    pub max_listing_entries: usize,
    pub max_delete_entries: usize,
    pub max_xml_body_bytes: usize,
    pub max_cors_body_bytes: usize,
    pub max_user_metadata_bytes: usize,
    pub transfer_buffer_bytes: usize,
    /// How long a request may wait for a transfer permit before a retryable error.
    pub admission_timeout_ms: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            active_uploads: 16,
            active_downloads: 64,
            active_copies: 2,
            active_multipart_assemblies: 2,
            max_single_put_bytes: 5 * GIB,
            max_part_bytes: 5 * GIB,
            max_object_bytes: 100 * GIB,
            max_temporary_bytes: 200 * GIB,
            min_disk_free_bytes: GIB,
            min_disk_free_percent: 5,
            min_free_inodes: 10_000,
            max_buckets: 1000,
            max_listing_entries: 1000,
            max_delete_entries: 1000,
            max_xml_body_bytes: 4 * 1024 * 1024,
            max_cors_body_bytes: 64 * 1024,
            max_user_metadata_bytes: 2048,
            transfer_buffer_bytes: 256 * 1024,
            admission_timeout_ms: 5000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MultipartConfig {
    pub max_active_uploads: u64,
    pub max_parts: u32,
    pub inactive_expiration_seconds: u64,
    pub receipt_retention_seconds: u64,
}

impl Default for MultipartConfig {
    fn default() -> Self {
        Self {
            max_active_uploads: 1024,
            max_parts: 10_000,
            inactive_expiration_seconds: 7 * 24 * 3600,
            receipt_retention_seconds: 24 * 3600,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MaintenanceConfig {
    pub garbage_batch_size: usize,
    pub garbage_grace_seconds: u64,
    pub garbage_interval_seconds: u64,
    pub shutdown_grace_seconds: u64,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            garbage_batch_size: 100,
            garbage_grace_seconds: 60,
            garbage_interval_seconds: 60,
            shutdown_grace_seconds: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LoggingConfig {
    pub format: String,
    pub level: String,
    pub log_object_keys: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            format: "json".into(),
            level: "info".into(),
            log_object_keys: false,
        }
    }
}

/// Explicit command-line overrides (highest precedence).
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub listen: Option<String>,
    pub management_listen: Option<String>,
    pub log_level: Option<String>,
}

/// Commented configuration templates printed by `storlite config template`.
pub const TEMPLATE_STANDALONE: &str = include_str!("../docs/examples/config.example.toml");
pub const TEMPLATE_DOCKER: &str = include_str!("../deploy/config.toml");

/// Documented non-secret environment overrides.
pub const ENV_OVERRIDES: &[&str] = &[
    "STORLITE_HTTP_LISTEN",
    "STORLITE_MANAGEMENT_LISTEN",
    "STORLITE_LOG_LEVEL",
    "STORLITE_LOG_FORMAT",
];

impl Config {
    /// Load, apply environment and CLI overrides, resolve paths, and validate.
    pub fn load(path: &Path, overrides: &Overrides) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::config(format!("cannot read {}: {e}", path.display())))?;
        let mut cfg = Self::parse(&text)?;
        let base = path
            .parent()
            .map(|p| {
                if p.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    p
                }
            })
            .unwrap_or(Path::new("."));
        cfg.base_dir = std::fs::canonicalize(base)
            .map_err(|e| Error::config(format!("cannot resolve config directory: {e}")))?;
        cfg.apply_env(|k| std::env::var(k).ok());
        cfg.apply_overrides(overrides);
        cfg.resolve_paths();
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| Error::config(format!("invalid configuration: {e}")))
    }

    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) {
        if let Some(v) = get("STORLITE_HTTP_LISTEN") {
            self.http.listen = v;
        }
        if let Some(v) = get("STORLITE_MANAGEMENT_LISTEN") {
            self.management.listen = v;
        }
        if let Some(v) = get("STORLITE_LOG_LEVEL") {
            self.logging.level = v;
        }
        if let Some(v) = get("STORLITE_LOG_FORMAT") {
            self.logging.format = v;
        }
    }

    pub fn apply_overrides(&mut self, o: &Overrides) {
        if let Some(v) = &o.listen {
            self.http.listen = v.clone();
        }
        if let Some(v) = &o.management_listen {
            self.management.listen = v.clone();
        }
        if let Some(v) = &o.log_level {
            self.logging.level = v.clone();
        }
    }

    pub fn resolve_paths(&mut self) {
        let base = self.base_dir.clone();
        let resolve = |p: &mut PathBuf| {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        };
        resolve(&mut self.data_dir);
        resolve(&mut self.credentials_file);
        if let Some(p) = self.http.tls_certificate_file.as_mut() {
            resolve(p);
        }
        if let Some(p) = self.http.tls_private_key_file.as_mut() {
            resolve(p);
        }
    }

    pub fn http_listen(&self) -> Result<SocketAddr> {
        self.http.listen.parse().map_err(|_| {
            Error::config(format!(
                "http.listen is not a socket address: {}",
                self.http.listen
            ))
        })
    }

    pub fn management_listen(&self) -> Result<SocketAddr> {
        self.management.listen.parse().map_err(|_| {
            Error::config(format!(
                "management.listen is not a socket address: {}",
                self.management.listen
            ))
        })
    }

    pub fn tls_enabled(&self) -> bool {
        self.http.tls_certificate_file.is_some()
    }

    pub fn trusted_proxy_peers(&self) -> Result<Vec<std::net::IpAddr>> {
        self.http
            .trusted_proxy_addresses
            .iter()
            .map(|s| {
                s.parse()
                    .map_err(|_| Error::config(format!("invalid trusted proxy address: {s}")))
            })
            .collect()
    }

    pub fn validate(&self) -> Result<()> {
        validate_region(&self.region)?;
        let listen = self.http_listen()?;
        let mgmt = self.management_listen()?;
        if listen.port() != 0 && listen == mgmt {
            return Err(Error::config(
                "management.listen must differ from http.listen",
            ));
        }

        let h = &self.http;
        match (&h.tls_certificate_file, &h.tls_private_key_file) {
            (Some(_), None) | (None, Some(_)) => {
                return Err(Error::config(
                    "tls_certificate_file and tls_private_key_file must be set together",
                ));
            }
            _ => {}
        }
        if h.trusted_proxy_mode {
            if h.trusted_proxy_addresses.is_empty() {
                return Err(Error::config(
                    "trusted_proxy_mode requires http.trusted_proxy_addresses (the proxy peers allowed to connect)",
                ));
            }
            self.trusted_proxy_peers()?;
        }
        if !self.tls_enabled() && !h.trusted_proxy_mode {
            if !listen.ip().is_loopback() {
                return Err(Error::config(
                    "refusing plaintext HTTP on a non-loopback address; configure TLS or trusted_proxy_mode",
                ));
            }
            if !h.allow_insecure_loopback_http {
                return Err(Error::config(
                    "plaintext loopback HTTP requires http.allow_insecure_loopback_http = true",
                ));
            }
        }
        range(
            "http.max_header_bytes",
            h.max_header_bytes as u64,
            8 * 1024,
            1024 * 1024,
        )?;
        range("http.max_header_count", h.max_header_count as u64, 16, 1024)?;
        range(
            "http.max_request_target_bytes",
            h.max_request_target_bytes as u64,
            1024,
            64 * 1024,
        )?;
        range(
            "http.header_timeout_seconds",
            h.header_timeout_seconds,
            1,
            600,
        )?;
        range(
            "http.body_idle_timeout_seconds",
            h.body_idle_timeout_seconds,
            1,
            3600,
        )?;
        range(
            "http.max_clock_skew_seconds",
            h.max_clock_skew_seconds,
            60,
            3600,
        )?;

        let d = &self.database;
        range(
            "database.reader_connections",
            d.reader_connections as u64,
            1,
            32,
        )?;
        range(
            "database.writer_queue_capacity",
            d.writer_queue_capacity as u64,
            1,
            65_536,
        )?;
        range(
            "database.reader_queue_capacity",
            d.reader_queue_capacity as u64,
            1,
            65_536,
        )?;
        range("database.busy_timeout_ms", d.busy_timeout_ms, 100, 60_000)?;
        range("database.queue_wait_ms", d.queue_wait_ms, 10, 60_000)?;

        let l = &self.limits;
        range("limits.active_uploads", l.active_uploads as u64, 1, 4096)?;
        range(
            "limits.active_downloads",
            l.active_downloads as u64,
            1,
            16_384,
        )?;
        range("limits.active_copies", l.active_copies as u64, 1, 1024)?;
        range(
            "limits.active_multipart_assemblies",
            l.active_multipart_assemblies as u64,
            1,
            1024,
        )?;
        range(
            "limits.max_single_put_bytes",
            l.max_single_put_bytes,
            1,
            HARD_MAX_SINGLE_PUT_BYTES,
        )?;
        range(
            "limits.max_part_bytes",
            l.max_part_bytes,
            MIN_PART_BYTES,
            HARD_MAX_PART_BYTES,
        )?;
        range(
            "limits.max_object_bytes",
            l.max_object_bytes,
            1,
            HARD_MAX_OBJECT_BYTES,
        )?;
        if l.max_object_bytes < l.max_single_put_bytes {
            return Err(Error::config(
                "limits.max_object_bytes must be at least limits.max_single_put_bytes",
            ));
        }
        let part_capacity = l
            .max_part_bytes
            .checked_mul(u64::from(self.multipart.max_parts));
        if part_capacity.is_some_and(|c| c < l.max_object_bytes) {
            return Err(Error::config(
                "limits.max_object_bytes exceeds multipart.max_parts * limits.max_part_bytes",
            ));
        }
        if l.max_temporary_bytes < l.max_part_bytes.max(l.max_single_put_bytes) {
            return Err(Error::config(
                "limits.max_temporary_bytes must hold at least one maximum part or single PUT",
            ));
        }
        range(
            "limits.min_disk_free_percent",
            l.min_disk_free_percent,
            0,
            50,
        )?;
        range("limits.max_buckets", l.max_buckets, 1, 1_000_000)?;
        range(
            "limits.max_listing_entries",
            l.max_listing_entries as u64,
            1,
            1000,
        )?;
        range(
            "limits.max_delete_entries",
            l.max_delete_entries as u64,
            1,
            1000,
        )?;
        range(
            "limits.max_xml_body_bytes",
            l.max_xml_body_bytes as u64,
            64 * 1024,
            64 * 1024 * 1024,
        )?;
        range(
            "limits.max_cors_body_bytes",
            l.max_cors_body_bytes as u64,
            1024,
            1024 * 1024,
        )?;
        range(
            "limits.max_user_metadata_bytes",
            l.max_user_metadata_bytes as u64,
            0,
            HARD_MAX_USER_METADATA_BYTES as u64,
        )?;
        range(
            "limits.transfer_buffer_bytes",
            l.transfer_buffer_bytes as u64,
            4 * 1024,
            8 * 1024 * 1024,
        )?;
        range(
            "limits.admission_timeout_ms",
            l.admission_timeout_ms,
            0,
            120_000,
        )?;

        let m = &self.multipart;
        range(
            "multipart.max_active_uploads",
            m.max_active_uploads,
            1,
            1_000_000,
        )?;
        range(
            "multipart.max_parts",
            u64::from(m.max_parts),
            1,
            u64::from(HARD_MAX_PARTS),
        )?;
        range(
            "multipart.inactive_expiration_seconds",
            m.inactive_expiration_seconds,
            60,
            365 * 24 * 3600,
        )?;
        range(
            "multipart.receipt_retention_seconds",
            m.receipt_retention_seconds,
            0,
            30 * 24 * 3600,
        )?;

        let mt = &self.maintenance;
        range(
            "maintenance.garbage_batch_size",
            mt.garbage_batch_size as u64,
            1,
            10_000,
        )?;
        range(
            "maintenance.garbage_grace_seconds",
            mt.garbage_grace_seconds,
            0,
            7 * 24 * 3600,
        )?;
        range(
            "maintenance.garbage_interval_seconds",
            mt.garbage_interval_seconds,
            1,
            24 * 3600,
        )?;
        range(
            "maintenance.shutdown_grace_seconds",
            mt.shutdown_grace_seconds,
            1,
            3600,
        )?;

        match self.logging.format.as_str() {
            "json" | "text" => {}
            other => {
                return Err(Error::config(format!(
                    "logging.format must be json or text, not {other}"
                )));
            }
        }
        tracing_subscriber::EnvFilter::try_new(&self.logging.level)
            .map_err(|e| Error::config(format!("invalid logging.level: {e}")))?;
        Ok(())
    }

    /// Effective non-secret settings for startup diagnostics.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "data_dir": self.data_dir,
            "region": self.region,
            "http_listen": self.http.listen,
            "tls": self.tls_enabled(),
            "trusted_proxy_mode": self.http.trusted_proxy_mode,
            "management_listen": self.management.listen,
            "active_uploads": self.limits.active_uploads,
            "active_downloads": self.limits.active_downloads,
            "active_copies": self.limits.active_copies,
            "active_multipart_assemblies": self.limits.active_multipart_assemblies,
            "max_single_put_bytes": self.limits.max_single_put_bytes,
            "max_part_bytes": self.limits.max_part_bytes,
            "max_object_bytes": self.limits.max_object_bytes,
            "max_temporary_bytes": self.limits.max_temporary_bytes,
            "reader_connections": self.database.reader_connections,
        })
    }
}

fn range(name: &str, value: u64, min: u64, max: u64) -> Result<()> {
    if value < min || value > max {
        return Err(Error::config(format!(
            "{name} = {value} is outside the allowed range {min}..={max}"
        )));
    }
    Ok(())
}

pub fn validate_region(region: &str) -> Result<()> {
    let ok = (1..=32).contains(&region.len())
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !region.starts_with('-')
        && !region.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::config(format!("invalid region name: {region}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = TEMPLATE_STANDALONE;

    fn example() -> Config {
        let mut c = Config::parse(EXAMPLE).unwrap();
        c.base_dir = PathBuf::from("/srv/storlite");
        c.resolve_paths();
        c
    }

    #[test]
    fn example_config_parses_and_validates() {
        let c = example();
        c.validate().unwrap();
        assert_eq!(c.data_dir, PathBuf::from("/srv/storlite/./data"));
        assert_eq!(c.limits.max_object_bytes, 100 * GIB);
        assert_eq!(c.multipart.receipt_retention_seconds, 86_400);
    }

    #[test]
    fn unknown_keys_are_errors() {
        let text = format!("{EXAMPLE}\n[extra]\nfoo = 1\n");
        assert!(Config::parse(&text).is_err());
        let text = EXAMPLE.replace("max_buckets = 1000", "max_buckets = 1000\nmax_bukets = 2");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn plaintext_non_loopback_is_refused() {
        let mut c = example();
        c.http.listen = "0.0.0.0:9000".into();
        assert!(c.validate().is_err());
        c.http.trusted_proxy_mode = true;
        assert!(c.validate().is_err(), "proxy mode needs peers");
        c.http.trusted_proxy_addresses = vec!["10.0.0.5".into()];
        c.validate().unwrap();
    }

    #[test]
    fn loopback_http_requires_explicit_opt_in() {
        let mut c = example();
        c.http.allow_insecure_loopback_http = false;
        assert!(c.validate().is_err());
    }

    #[test]
    fn contradictory_limits_are_rejected() {
        let mut c = example();
        c.limits.max_single_put_bytes = 6 * GIB;
        assert!(c.validate().is_err());
        let mut c = example();
        c.limits.max_object_bytes = 1024;
        assert!(c.validate().is_err());
        let mut c = example();
        c.limits.max_temporary_bytes = 1024;
        assert!(c.validate().is_err());
        let mut c = example();
        c.multipart.max_parts = 10;
        assert!(c.validate().is_err(), "10 parts * 5 GiB < 100 GiB");
    }

    #[test]
    fn env_and_cli_override_precedence() {
        let mut c = example();
        c.apply_env(|k| (k == "STORLITE_HTTP_LISTEN").then(|| "127.0.0.1:9100".to_string()));
        assert_eq!(c.http.listen, "127.0.0.1:9100");
        c.apply_overrides(&Overrides {
            listen: Some("127.0.0.1:9200".into()),
            ..Default::default()
        });
        assert_eq!(c.http.listen, "127.0.0.1:9200");
    }

    /// Uncomment every `# key = value` default line.
    fn uncomment_defaults(text: &str) -> String {
        text.lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest)
                    if rest.split_once(" = ").is_some_and(|(k, _)| {
                        k.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                    }) =>
                {
                    rest
                }
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn templates_validate_and_document_real_defaults() {
        for (name, text) in [
            ("standalone", TEMPLATE_STANDALONE),
            ("docker", TEMPLATE_DOCKER),
        ] {
            let mut c = Config::parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            c.base_dir = PathBuf::from("/srv/storlite");
            c.resolve_paths();
            c.validate().unwrap_or_else(|e| panic!("{name}: {e}"));

            // Every commented key exists, and its documented value is the default.
            let full = uncomment_defaults(text);
            assert_ne!(full, text, "{name}: no commented defaults found");
            let mut u = Config::parse(&full).unwrap_or_else(|e| panic!("{name} uncommented: {e}"));
            u.http.tls_certificate_file = c.http.tls_certificate_file.clone();
            u.http.tls_private_key_file = c.http.tls_private_key_file.clone();
            u.base_dir = c.base_dir.clone();
            u.resolve_paths();
            assert_eq!(format!("{:?}", u), format!("{:?}", c), "{name}");
        }
    }
}
