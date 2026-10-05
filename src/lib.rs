//! litebucket: a compact single-host S3-compatible object store.

pub mod admin;
pub mod backup;
pub mod capacity;
pub mod checksums;
pub mod cli;
pub mod config;
pub mod credentials;
pub mod doctor;
pub mod error;
pub mod failpoint;
pub mod fsutil;
pub mod ids;
pub mod keys;
pub mod locks;
pub mod maintenance;
pub mod metadata;
pub mod s3;
pub mod secrets;
pub mod server;
pub mod sigv4;
pub mod store;
pub mod telemetry;
