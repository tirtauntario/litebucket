//! Command-line interface.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};

use crate::config::{Config, Overrides};
use crate::credentials::{CredentialSet, CredentialStore};
use crate::error::{Error, Result};
use crate::store::Store;

#[derive(Parser, Debug)]
#[command(name = "storlite", version, about = "Compact single-host S3-compatible object storage")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Debug, Clone)]
struct ConfigArg {
    /// Path to the TOML configuration file.
    #[arg(long, default_value = "./config.toml")]
    config: PathBuf,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create a new store in an empty data directory.
    Init(ConfigArg),
    /// Run the S3 and management listeners.
    Serve {
        #[command(flatten)]
        cfg: ConfigArg,
        /// Override http.listen.
        #[arg(long)]
        listen: Option<String>,
        /// Override management.listen.
        #[arg(long)]
        management_listen: Option<String>,
        /// Override logging.level.
        #[arg(long)]
        log_level: Option<String>,
    },
    /// Configuration commands.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Credential file commands (never touch the object store).
    Credentials {
        #[command(subcommand)]
        command: CredentialsCommand,
    },
    /// Query the management readiness endpoint (exit 0 when ready).
    Healthcheck {
        #[arg(long, default_value = "http://127.0.0.1:9001/readyz")]
        url: String,
    },
    /// Offline diagnostics: schema, settings, references, counters, filesystem.
    Doctor(ConfigArg),
    /// Offline verification; `--full` hashes every referenced file and reports untracked files.
    Check {
        #[command(flatten)]
        cfg: ConfigArg,
        #[arg(long)]
        full: bool,
    },
    /// Offline garbage collection of tracked garbage (dry run by default).
    Gc {
        #[command(flatten)]
        cfg: ConfigArg,
        /// Only report (default).
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// Delete eligible tracked garbage.
        #[arg(long)]
        apply: bool,
    },
    /// Offline bucket administration.
    Bucket {
        #[command(subcommand)]
        command: BucketCommand,
    },
    /// Offline consistent backup into a new directory.
    Backup {
        #[command(flatten)]
        cfg: ConfigArg,
        #[arg(long)]
        destination: PathBuf,
    },
    /// Restore a verified backup into a new, empty data directory.
    Restore {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        data_dir: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Validate configuration structure without starting listeners.
    Check(ConfigArg),
}

#[derive(Subcommand, Debug)]
enum CredentialsCommand {
    /// Write a new disabled credential with a 256-bit secret to a new 0600 file.
    Generate {
        #[arg(long)]
        id: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Validate a credentials file (permissions, syntax, grants).
    Check {
        #[arg(long)]
        file: PathBuf,
        /// Accept a group-readable file.
        #[arg(long)]
        allow_group_read: bool,
    },
}

#[derive(Subcommand, Debug)]
enum BucketCommand {
    /// Set or clear a bucket's logical byte quota.
    SetQuota {
        #[command(flatten)]
        cfg: ConfigArg,
        #[arg(long)]
        name: String,
        /// Quota in bytes; accepts suffixes K/M/G/T (powers of 1024).
        #[arg(long, conflicts_with = "clear")]
        bytes: Option<String>,
        #[arg(long)]
        clear: bool,
    },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("storlite: {e}");
            ExitCode::from(1)
        }
    }
}

fn load_config(path: &std::path::Path, overrides: &Overrides) -> Result<Config> {
    Config::load(path, overrides)
}

fn execute(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Init(a) => {
            let cfg = load_config(&a.config, &Overrides::default())?;
            let meta = crate::store::initialize(&cfg)?;
            println!(
                "initialized store {} (region {}) at {}",
                meta.store_id,
                meta.region,
                cfg.data_dir.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Serve {
            cfg,
            listen,
            management_listen,
            log_level,
        } => {
            let overrides = Overrides {
                listen,
                management_listen,
                log_level,
            };
            let cfg = load_config(&cfg.config, &overrides)?;
            serve(cfg)
        }
        Command::Config {
            command: ConfigCommand::Check(a),
        } => {
            let cfg = load_config(&a.config, &Overrides::default())?;
            println!("configuration OK");
            println!("{}", serde_json::to_string_pretty(&cfg.summary()).unwrap_or_default());
            Ok(ExitCode::SUCCESS)
        }
        Command::Credentials {
            command: CredentialsCommand::Generate { id, output },
        } => {
            crate::credentials::generate(&id, &output)?;
            println!(
                "wrote disabled credential '{id}' to {} (mode 0600); merge it into the credentials file, add grants, and enable it",
                output.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Credentials {
            command: CredentialsCommand::Check { file, allow_group_read },
        } => {
            let set = CredentialSet::load(&file, allow_group_read)?;
            println!(
                "credentials OK: {} enabled ({}), {} disabled",
                set.enabled_count(),
                set.ids().join(", "),
                set.disabled_count
            );
            if set.enabled_count() == 0 {
                println!("warning: no enabled credentials; `serve` will refuse to start");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Healthcheck { url } => healthcheck(&url),
        Command::Doctor(a) => {
            let cfg = load_config(&a.config, &Overrides::default())?;
            let ok = crate::doctor::doctor(&cfg, false)?;
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::from(2) })
        }
        Command::Check { cfg, full } => {
            let cfg = load_config(&cfg.config, &Overrides::default())?;
            let ok = crate::doctor::doctor(&cfg, full)?;
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::from(2) })
        }
        Command::Gc { cfg, apply, .. } => {
            let cfg = load_config(&cfg.config, &Overrides::default())?;
            crate::doctor::gc(&cfg, apply)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Bucket {
            command: BucketCommand::SetQuota { cfg, name, bytes, clear },
        } => {
            let cfg = load_config(&cfg.config, &Overrides::default())?;
            let quota = match (bytes, clear) {
                (Some(b), false) => Some(parse_size(&b)?),
                (None, true) => None,
                _ => return Err(Error::config("specify exactly one of --bytes or --clear")),
            };
            crate::doctor::set_quota(&cfg, &name, quota)?;
            match quota {
                Some(q) => println!("bucket {name}: quota set to {q} bytes"),
                None => println!("bucket {name}: quota cleared"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Backup { cfg, destination } => {
            let cfg = load_config(&cfg.config, &Overrides::default())?;
            crate::backup::backup(&cfg, &destination)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Restore { source, data_dir } => {
            crate::backup::restore(&source, &data_dir)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Parse `123`, `10G`, `512M` (binary multiples); prints are exact bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        Some('T' | 't') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(|| Error::config(format!("invalid size: {s}")))
}

fn healthcheck(url: &str) -> Result<ExitCode> {
    use std::io::{Read, Write};
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| Error::config("healthcheck supports http:// management URLs only"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/readyz"),
    };
    let mut stream = std::net::TcpStream::connect(authority)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    let ok = buf.starts_with("HTTP/1.1 200");
    println!("{}", buf.split("\r\n\r\n").nth(1).unwrap_or("").trim());
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

fn serve(cfg: Config) -> Result<ExitCode> {
    crate::telemetry::init_logging(&cfg.logging);
    crate::metadata::check_sqlite_runtime()?;
    tracing::info!(
        event = "startup",
        version = env!("CARGO_PKG_VERSION"),
        sqlite_version = %crate::metadata::sqlite_version(),
        sqlite_source_id = %crate::metadata::sqlite_source_id(),
        storage_format = crate::metadata::migrations::FORMAT_VERSION,
        config = %cfg.summary(),
        "starting storlite"
    );
    let creds = CredentialSet::load(&cfg.credentials_file, cfg.credentials_allow_group_read)?;
    if creds.enabled_count() == 0 {
        return Err(Error::config(
            "no enabled credentials; refusing to start an S3 endpoint without authentication",
        ));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("storlite")
        .build()?;
    runtime.block_on(async move {
        let creds_path = cfg.credentials_file.clone();
        let allow_group = cfg.credentials_allow_group_read;
        let store = Store::open(cfg, CredentialStore::new(creds))?;
        let running = crate::server::start(store.clone()).await?;
        wait_for_signals(&store, &creds_path, allow_group).await?;
        let drained = running.shutdown().await;
        Ok(if drained { ExitCode::SUCCESS } else { ExitCode::from(3) })
    })
}

async fn wait_for_signals(store: &Arc<Store>, creds_path: &std::path::Path, allow_group: bool) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut hup = signal(SignalKind::hangup())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            _ = hup.recv() => reload_credentials(store, creds_path, allow_group),
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }
    tracing::info!(event = "shutdown_requested", "stopping");
    Ok(())
}

/// Validate the complete new credential set before swapping it in.
pub fn reload_credentials(store: &Arc<Store>, path: &std::path::Path, allow_group: bool) {
    match CredentialSet::load(path, allow_group) {
        Ok(set) if set.enabled_count() > 0 => {
            let n = set.enabled_count();
            store.credentials.replace(set);
            tracing::info!(event = "credentials_reloaded", enabled = n, "credentials reloaded");
        }
        Ok(_) => {
            store
                .metrics
                .credential_reload_failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(event = "credential_reload_failed", "reload rejected: no enabled credentials; keeping previous set");
        }
        Err(e) => {
            store
                .metrics
                .credential_reload_failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(event = "credential_reload_failed", error = %e, "reload rejected; keeping previous set");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("10G").unwrap(), 10_737_418_240);
        assert_eq!(parse_size("512").unwrap(), 512);
        assert!(parse_size("x").is_err());
        assert!(parse_size("99999999999T").is_err());
    }
}
