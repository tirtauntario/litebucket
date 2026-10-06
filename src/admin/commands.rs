//! `litebucket admin ...`: a client of the admin socket, plus the offline
//! `admin recover`.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;

use super::client::AdminClient;
use super::{
    BackupRequest, BucketInfo, CorsRequest, CreateBucketRequest, CreateKeyRequest, GrantJson,
    IssuedKey, KeyInfo, QuotaRequest, RemoveGrantRequest, RotateKeyRequest, StatusInfo,
    UpdateKeyRequest,
};
use crate::cli::{load_config, parse_duration_secs, parse_size};
use crate::config::Overrides;
use crate::credentials::{Grant, split_target};
use crate::error::{Error, Result};
use crate::metadata::queries::AuditRow;
use crate::s3::cors::CorsRule;

#[derive(Args, Debug)]
pub struct AdminArgs {
    /// Configuration file; the admin socket path is read from it.
    #[arg(
        long,
        env = "LITEBUCKET_CONFIG",
        default_value = "./config.toml",
        global = true
    )]
    config: PathBuf,
    /// Admin socket path (overrides the one in the configuration).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Print JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: AdminCommand,
}

#[derive(Subcommand, Debug)]
enum AdminCommand {
    /// Server version, store identity, secret protection, key and bucket counts.
    Status,
    /// Access keys.
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Bucket and prefix grants of an access key.
    Grant {
        #[command(subcommand)]
        command: GrantCommand,
    },
    /// Global grants (admin, list_buckets, create_bucket) of an access key.
    GlobalGrant {
        #[command(subcommand)]
        command: GlobalGrantCommand,
    },
    /// Buckets, quotas, and CORS.
    Bucket {
        #[command(subcommand)]
        command: BucketCommand,
    },
    /// Recent admin changes, newest first.
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Online backup into a new directory on the server's file system, while
    /// the server keeps serving. Waits until the backup is complete.
    Backup {
        /// Absolute path of the new backup directory, as the server sees it.
        destination: PathBuf,
    },
    /// OFFLINE (server stopped): create a new admin key directly in the
    /// database, for when every admin key is lost.
    Recover {
        /// Delete every existing access key first (needed when the master
        /// key is lost: the stored secrets can no longer be decrypted).
        #[arg(long)]
        reset_keys: bool,
        /// Access key id for the new admin key (generated when omitted).
        #[arg(long)]
        id: Option<String>,
        /// Write the new key to this new file (mode 0600) instead of printing it.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum SecretFormat {
    /// Labelled lines.
    Text,
    /// AWS_ACCESS_KEY_ID=... / AWS_SECRET_ACCESS_KEY=... lines.
    Env,
}

#[derive(Subcommand, Debug)]
enum KeyCommand {
    /// Create an access key. The secret is shown once.
    Create {
        /// Access key id (default: generated, `SL` + 18 characters).
        #[arg(long)]
        id: Option<String>,
        #[arg(long, default_value = "")]
        description: String,
        /// Grant `bucket[/prefix]:action[,action...]`; repeatable.
        /// Actions: read, list, write, delete, manage_bucket.
        #[arg(long = "grant", value_name = "SPEC")]
        grants: Vec<String>,
        /// admin, list_buckets, or create_bucket; repeatable.
        #[arg(long = "global-grant", value_name = "GRANT")]
        global_grants: Vec<String>,
        /// RFC 3339 expiry, e.g. 2027-01-01T00:00:00Z.
        #[arg(long)]
        expires_at: Option<String>,
        /// Create the key disabled.
        #[arg(long)]
        disabled: bool,
        #[arg(long, value_enum, default_value_t = SecretFormat::Text)]
        format: SecretFormat,
        /// Write the key to this new file (mode 0600, env format) instead of printing the secret.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// List access keys (never shows secrets).
    List,
    /// Show one access key.
    Show {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    /// Change the description or expiry.
    Update {
        id: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, conflicts_with = "no_expiry")]
        expires_at: Option<String>,
        #[arg(long)]
        no_expiry: bool,
    },
    /// Delete an access key and its grants.
    Delete {
        id: String,
    },
    /// Issue a new secret. The old one keeps working for --grace (default: 0, revoked now).
    Rotate {
        id: String,
        /// e.g. 15m, 24h, 7d (max 30d).
        #[arg(long, default_value = "0")]
        grace: String,
        #[arg(long, value_enum, default_value_t = SecretFormat::Text)]
        format: SecretFormat,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum GrantCommand {
    /// Add or replace the grant for bucket[/prefix]: `bucket[/prefix]:action[,action...]`.
    Add { id: String, spec: String },
    /// Remove the grant for `bucket[/prefix]`.
    Remove { id: String, target: String },
}

#[derive(Subcommand, Debug)]
enum GlobalGrantCommand {
    Add { id: String, grant: String },
    Remove { id: String, grant: String },
}

#[derive(Subcommand, Debug)]
enum BucketCommand {
    Create {
        name: String,
        /// Logical byte quota, e.g. 10G.
        #[arg(long)]
        quota: Option<String>,
        /// CORS rules: a JSON array of rules, or an S3 CORSConfiguration XML file.
        #[arg(long)]
        cors_file: Option<PathBuf>,
    },
    List,
    Show {
        name: String,
    },
    /// Delete an empty bucket.
    Delete {
        name: String,
    },
    /// Set (`--bytes 10G`) or remove (`--clear`) the logical byte quota.
    SetQuota {
        name: String,
        #[arg(long, conflicts_with = "clear")]
        bytes: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Replace the bucket's CORS rules from a JSON or S3 XML file.
    SetCors {
        name: String,
        #[arg(long)]
        file: PathBuf,
    },
    ClearCors {
        name: String,
    },
}

pub fn run(args: AdminArgs) -> Result<ExitCode> {
    if let AdminCommand::Recover {
        reset_keys,
        id,
        output,
    } = &args.command
    {
        return recover(&args.config, *reset_keys, id.clone(), output.as_deref());
    }
    let socket = match &args.socket {
        Some(s) => s.clone(),
        None => {
            load_config(&args.config, &Overrides::default())?
                .admin
                .socket
        }
    };
    let c = AdminClient::new(&socket);
    let json = args.json;
    match args.command {
        AdminCommand::Status => {
            let s: StatusInfo = c.get("/v1/status")?;
            print_or_json(json, &s, |s| {
                println!("litebucket {}", s.version);
                println!("store id: {}  region: {}", s.store_id, s.region);
                println!("secret protection: {}", s.secret_protection);
                println!(
                    "access keys: {} enabled, {} disabled",
                    s.keys_enabled, s.keys_disabled
                );
                println!("buckets: {}", s.buckets);
            })
        }
        AdminCommand::Key { command } => key(&c, json, command),
        AdminCommand::Grant { command } => {
            let k: KeyInfo = match command {
                GrantCommand::Add { id, spec } => {
                    let g = GrantJson::from_grant(&Grant::parse_spec(&spec)?);
                    c.send("POST", &key_path(&id, "/grants"), &g)?
                }
                GrantCommand::Remove { id, target } => {
                    let (bucket, prefix) = split_target(&target);
                    c.send(
                        "POST",
                        &key_path(&id, "/grants/remove"),
                        &RemoveGrantRequest {
                            bucket: bucket.into(),
                            prefix: prefix.into(),
                        },
                    )?
                }
            };
            print_or_json(json, &k, print_key)
        }
        AdminCommand::GlobalGrant { command } => {
            let k: KeyInfo = match command {
                GlobalGrantCommand::Add { id, grant } => c.send(
                    "PUT",
                    &key_path(&id, &format!("/global-grants/{}", seg(&grant))),
                    &serde_json::json!({}),
                )?,
                GlobalGrantCommand::Remove { id, grant } => {
                    c.delete(&key_path(&id, &format!("/global-grants/{}", seg(&grant))))?
                }
            };
            print_or_json(json, &k, print_key)
        }
        AdminCommand::Bucket { command } => bucket(&c, json, command),
        AdminCommand::Audit { limit } => {
            let rows: Vec<AuditRow> = c.get(&format!("/v1/audit?limit={limit}"))?;
            print_or_json(json, &rows, |rows| {
                for r in rows {
                    println!(
                        "{}  {:<12} {:<20} {:<24} {}",
                        crate::credentials::format_rfc3339_ms(r.at_ms),
                        r.actor,
                        r.action,
                        r.target,
                        r.detail
                    );
                }
            })
        }
        AdminCommand::Backup { destination } => {
            let destination = if destination.is_absolute() {
                destination
            } else {
                std::env::current_dir()?.join(destination)
            };
            let s: crate::backup::Summary = c.without_read_timeout().send(
                "POST",
                "/v1/backup",
                &BackupRequest { destination },
            )?;
            print_or_json(json, &s, crate::backup::Summary::print)
        }
        AdminCommand::Recover { .. } => unreachable!("handled above"),
    }
}

/// Percent-encode one path segment.
fn seg(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn key_path(id: &str, rest: &str) -> String {
    format!("/v1/keys/{}{rest}", seg(id))
}

fn print_or_json<T: Serialize>(json: bool, v: &T, text: impl FnOnce(&T)) -> Result<ExitCode> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(v).map_err(|e| Error::other(e.to_string()))?
        );
    } else {
        text(v);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_key(k: &KeyInfo) {
    println!("access_key_id: {}", k.access_key_id);
    println!("enabled:       {}", k.enabled);
    if !k.description.is_empty() {
        println!("description:   {}", k.description);
    }
    println!("created:       {}", k.created_at);
    println!("updated:       {}", k.updated_at);
    println!(
        "expires:       {}",
        k.expires_at.as_deref().unwrap_or("never")
    );
    if let Some(u) = &k.previous_secret_valid_until {
        println!("previous secret valid until: {u}");
    }
    println!(
        "global grants: {}",
        if k.global_grants.is_empty() {
            "-".into()
        } else {
            k.global_grants.join(", ")
        }
    );
    if k.grants.is_empty() {
        println!("grants:        -");
    } else {
        println!("grants:");
        for g in &k.grants {
            println!("  {}", grant_spec(g));
        }
    }
}

fn grant_spec(g: &GrantJson) -> String {
    let target = if g.prefix.is_empty() {
        g.bucket.clone()
    } else {
        format!("{}/{}", g.bucket, g.prefix)
    };
    format!("{target}:{}", g.actions.join(","))
}

/// Print a new secret once, or write it to a new 0600 file.
pub fn emit_issued(k: &IssuedKey, output: Option<&Path>, what: &str) -> Result<()> {
    emit_issued_as(k, output, what, SecretFormatOut::Text, false)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SecretFormatOut {
    Text,
    Env,
}

fn emit_issued_as(
    k: &IssuedKey,
    output: Option<&Path>,
    what: &str,
    format: SecretFormatOut,
    json: bool,
) -> Result<()> {
    let env = format!(
        "AWS_ACCESS_KEY_ID={}\nAWS_SECRET_ACCESS_KEY={}\n",
        k.access_key_id, k.secret_access_key
    );
    if let Some(path) = output {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)
            .map_err(|e| Error::config(format!("cannot create {}: {e}", path.display())))?;
        f.write_all(env.as_bytes())?;
        f.sync_all()?;
        println!(
            "{what}: {} (secret written to {}, mode 0600)",
            k.access_key_id,
            path.display()
        );
    } else if json {
        println!(
            "{}",
            serde_json::to_string_pretty(k).map_err(|e| Error::other(e.to_string()))?
        );
    } else if format == SecretFormatOut::Env {
        print!("{env}");
    } else {
        println!("{what}. The secret is shown only once; store it now.");
        println!("access_key_id:     {}", k.access_key_id);
        println!("secret_access_key: {}", k.secret_access_key);
    }
    if let Some(u) = &k.previous_secret_valid_until
        && format != SecretFormatOut::Env
        && !json
    {
        println!("the previous secret keeps working until {u}");
    }
    Ok(())
}

fn key(c: &AdminClient, json: bool, command: KeyCommand) -> Result<ExitCode> {
    match command {
        KeyCommand::Create {
            id,
            description,
            grants,
            global_grants,
            expires_at,
            disabled,
            format,
            output,
        } => {
            let grants = grants
                .iter()
                .map(|s| Grant::parse_spec(s).map(|g| GrantJson::from_grant(&g)))
                .collect::<Result<Vec<_>>>()?;
            let req = CreateKeyRequest {
                access_key_id: id,
                description,
                enabled: Some(!disabled),
                expires_at,
                global_grants,
                grants,
            };
            let k: IssuedKey = c.send("POST", "/v1/keys", &req)?;
            emit_issued_as(
                &k,
                output.as_deref(),
                "access key created",
                out(format),
                json,
            )?;
            Ok(ExitCode::SUCCESS)
        }
        KeyCommand::List => {
            let keys: Vec<KeyInfo> = c.get("/v1/keys")?;
            print_or_json(json, &keys, |keys| {
                println!(
                    "{:<22} {:<8} {:<21} {:<28} GRANTS / DESCRIPTION",
                    "ACCESS KEY ID", "ENABLED", "EXPIRES", "GLOBAL"
                );
                for k in keys {
                    let grants: Vec<String> = k.grants.iter().map(grant_spec).collect();
                    let mut tail = grants.join(" ");
                    if !k.description.is_empty() {
                        tail = if tail.is_empty() {
                            k.description.clone()
                        } else {
                            format!("{tail}  # {}", k.description)
                        };
                    }
                    println!(
                        "{:<22} {:<8} {:<21} {:<28} {}",
                        k.access_key_id,
                        k.enabled,
                        k.expires_at.as_deref().unwrap_or("never"),
                        if k.global_grants.is_empty() {
                            "-".into()
                        } else {
                            k.global_grants.join(",")
                        },
                        tail
                    );
                }
            })
        }
        KeyCommand::Show { id } => {
            let k: KeyInfo = c.get(&key_path(&id, ""))?;
            print_or_json(json, &k, print_key)
        }
        KeyCommand::Enable { id } => set_enabled(c, json, &id, true),
        KeyCommand::Disable { id } => set_enabled(c, json, &id, false),
        KeyCommand::Update {
            id,
            description,
            expires_at,
            no_expiry,
        } => {
            let k: KeyInfo = c.send(
                "PATCH",
                &key_path(&id, ""),
                &UpdateKeyRequest {
                    enabled: None,
                    description,
                    expires_at,
                    clear_expiry: no_expiry,
                },
            )?;
            print_or_json(json, &k, print_key)
        }
        KeyCommand::Delete { id } => {
            let _: serde_json::Value = c.delete(&key_path(&id, ""))?;
            if !json {
                println!("deleted access key {id}");
            } else {
                println!("{{\"deleted\": true}}");
            }
            Ok(ExitCode::SUCCESS)
        }
        KeyCommand::Rotate {
            id,
            grace,
            format,
            output,
        } => {
            let k: IssuedKey = c.send(
                "POST",
                &key_path(&id, "/rotate"),
                &RotateKeyRequest {
                    grace_seconds: parse_duration_secs(&grace)?,
                },
            )?;
            emit_issued_as(&k, output.as_deref(), "secret rotated", out(format), json)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn out(f: SecretFormat) -> SecretFormatOut {
    match f {
        SecretFormat::Text => SecretFormatOut::Text,
        SecretFormat::Env => SecretFormatOut::Env,
    }
}

fn set_enabled(c: &AdminClient, json: bool, id: &str, enabled: bool) -> Result<ExitCode> {
    let k: KeyInfo = c.send(
        "PATCH",
        &key_path(id, ""),
        &UpdateKeyRequest {
            enabled: Some(enabled),
            ..Default::default()
        },
    )?;
    print_or_json(json, &k, print_key)
}

fn read_cors_file(path: &Path) -> Result<Vec<CorsRule>> {
    let bytes = std::fs::read(path)
        .map_err(|e| Error::config(format!("cannot read {}: {e}", path.display())))?;
    let text = String::from_utf8_lossy(&bytes);
    if text.trim_start().starts_with('<') {
        return crate::s3::cors::parse_config(&bytes)
            .map_err(|e| Error::config(format!("{}: {}", path.display(), e.message)));
    }
    if let Ok(rules) = serde_json::from_slice::<Vec<CorsRule>>(&bytes) {
        return Ok(rules);
    }
    serde_json::from_slice::<CorsRequest>(&bytes)
        .map(|r| r.rules)
        .map_err(|e| {
            Error::config(format!(
                "{}: expected a JSON array of CORS rules, {{\"rules\": [...]}}, or S3 CORSConfiguration XML ({e})",
                path.display()
            ))
        })
}

fn print_bucket(b: &BucketInfo) {
    println!("bucket:   {}", b.name);
    println!("created:  {}", b.created_at);
    println!("objects:  {}", b.object_count);
    println!("bytes:    {}", b.logical_bytes);
    println!(
        "quota:    {}",
        b.quota_bytes
            .map(|q| q.to_string())
            .unwrap_or_else(|| "none".into())
    );
    match &b.cors {
        None => println!("cors:     none"),
        Some(rules) => println!(
            "cors:     {}",
            serde_json::to_string(rules).unwrap_or_default()
        ),
    }
}

fn bucket(c: &AdminClient, json: bool, command: BucketCommand) -> Result<ExitCode> {
    let b: BucketInfo = match command {
        BucketCommand::Create {
            name,
            quota,
            cors_file,
        } => c.send(
            "POST",
            "/v1/buckets",
            &CreateBucketRequest {
                name,
                quota_bytes: quota.as_deref().map(parse_size).transpose()?,
                cors: cors_file.as_deref().map(read_cors_file).transpose()?,
            },
        )?,
        BucketCommand::List => {
            let list: Vec<BucketInfo> = c.get("/v1/buckets")?;
            return print_or_json(json, &list, |list| {
                println!(
                    "{:<40} {:>10} {:>16} {:>16} CORS",
                    "BUCKET", "OBJECTS", "BYTES", "QUOTA"
                );
                for b in list {
                    println!(
                        "{:<40} {:>10} {:>16} {:>16} {}",
                        b.name,
                        b.object_count,
                        b.logical_bytes,
                        b.quota_bytes
                            .map(|q| q.to_string())
                            .unwrap_or_else(|| "-".into()),
                        b.cors.as_ref().map(|r| r.len()).unwrap_or(0)
                    );
                }
            });
        }
        BucketCommand::Show { name } => c.get(&format!("/v1/buckets/{}", seg(&name)))?,
        BucketCommand::Delete { name } => {
            let _: serde_json::Value = c.delete(&format!("/v1/buckets/{}", seg(&name)))?;
            if json {
                println!("{{\"deleted\": true}}");
            } else {
                println!("deleted bucket {name}");
            }
            return Ok(ExitCode::SUCCESS);
        }
        BucketCommand::SetQuota { name, bytes, clear } => {
            let quota = match (bytes, clear) {
                (Some(b), false) => Some(parse_size(&b)?),
                (None, true) => None,
                _ => return Err(Error::config("specify exactly one of --bytes or --clear")),
            };
            c.send(
                "PUT",
                &format!("/v1/buckets/{}/quota", seg(&name)),
                &QuotaRequest { quota_bytes: quota },
            )?
        }
        BucketCommand::SetCors { name, file } => c.send(
            "PUT",
            &format!("/v1/buckets/{}/cors", seg(&name)),
            &CorsRequest {
                rules: read_cors_file(&file)?,
            },
        )?,
        BucketCommand::ClearCors { name } => {
            c.delete(&format!("/v1/buckets/{}/cors", seg(&name)))?
        }
    };
    print_or_json(json, &b, print_bucket)
}

/// Offline admin-key recovery. Holds the store lock, so the server must be
/// stopped.
fn recover(
    config: &Path,
    reset_keys: bool,
    id: Option<String>,
    output: Option<&Path>,
) -> Result<ExitCode> {
    let cfg = load_config(config, &Overrides::default())?;
    if let Some(out) = output
        && out.exists()
    {
        return Err(Error::config(format!("{} already exists", out.display())));
    }
    let codec = crate::store::secret_codec(&cfg).map_err(|e| {
        Error::config(format!(
            "{e}\n  If the master key is lost, set secrets.protection = \"plaintext\" (or point master_key_file at a new key) and run `litebucket admin recover --reset-keys`."
        ))
    })?;
    let (_data, mut conn, meta, _) = crate::store::open_offline(&cfg, true)?;
    let now = crate::metadata::now_ms();
    if !reset_keys {
        // Never mix keys sealed under different master keys.
        super::load_credential_set(&conn, &codec, &meta.store_id, now).map_err(|e| {
            Error::config(format!(
                "existing access keys cannot be read ({e}); rerun with --reset-keys to delete them"
            ))
        })?;
    }
    let issued = crate::metadata::with_named_write_tx(&mut conn, "admin", |tx| {
        let cx = super::Ctx {
            codec: &codec,
            store_id: &meta.store_id,
            actor: "recover",
            now_ms: now,
            max_buckets: cfg.limits.max_buckets,
        };
        if reset_keys {
            let n = crate::metadata::queries::delete_all_credentials(tx)?;
            crate::metadata::queries::insert_audit(
                tx,
                now,
                "recover",
                "keys.reset",
                "*",
                &serde_json::json!({ "deleted": n }),
            )?;
        }
        super::create_admin_key(tx, &cx, id)
    })?;
    emit_issued(&issued, output, "admin access key created")?;
    Ok(ExitCode::SUCCESS)
}
