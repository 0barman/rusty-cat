//! `rusty-cat` public SDK crate.
//!
//! This crate exposes high-level APIs to enqueue and control upload/download
//! tasks with breakpoint resume support.
//!
//! For beginners, start from [`api`] or directly use [`meow_client::MeowClient`]
//! and [`meow_config::MeowConfig`].
//!
//! # Application-selected TLS provider
//!
//! The SDK does not select or install a Rustls crypto provider. Before using an
//! internally constructed HTTP client, the application must enable its chosen
//! provider and install it once during startup. For example, an application
//! choosing ring can depend on
//! `rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }`:
//!
//! ```
//! use rusty_cat::api::{MeowClient, MeowConfig};
//!
//! rustls::crypto::ring::default_provider()
//!     .install_default()
//!     .expect("install the application's TLS provider once at startup");
//! let client = MeowClient::new(MeowConfig::default());
//! let _http = client.http_client()?;
//! # Ok::<(), rusty_cat::api::MeowError>(())
//! ```
//!
//! Missing a provider returns `HttpClientBuildFailed` (119) when an internal
//! client is needed. An injected, already configured Reqwest client can serve
//! ordinary transfers without a global provider; Binary downloads use their
//! own isolated client and still require the application's global provider.
//!
//! # Quick example
//!
//! ```no_run
//! use rusty_cat::api::{MeowClient, MeowConfig, UploadPounceBuilder};
//!
//! rustls::crypto::ring::default_provider()
//!     .install_default()
//!     .expect("install the application's TLS provider once at startup");
//! let config = MeowConfig::builder()
//!     .max_upload_concurrency(2)
//!     .max_download_concurrency(2)
//!     .build()?;
//! let client = MeowClient::new(config);
//! let _task = UploadPounceBuilder::new("file.bin", "./file.bin", 1024 * 1024)
//!     .with_url("https://example.com/upload")
//!     .build();
//! let _ = client;
//! # Ok::<(), rusty_cat::api::MeowError>(())
//! ```
//!
//! # Panic policy (enforced)
//!
//! Production SDK code must never panic: any fallible path returns
//! `Result<_, error::MeowError>` and logs the failure at [`log::LogLevel::Error`]
//! (see [`meow_error_log`]). The lint block below makes a panic-trigger in
//! non-test code a compile-time error under `cargo clippy`, freezing this
//! guarantee. Tests, doctests and examples are exempt.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable
    )
)]
#[cfg(feature = "aliyun-oss-direct")]
#[path = "aliyun-oss-direct/mod.rs"]
pub mod aliyun_oss_direct;
#[cfg(feature = "aliyun-oss-presigned")]
#[path = "aliyun-oss-presigned/mod.rs"]
pub mod aliyun_oss_presigned;
pub mod api;
#[cfg(feature = "azure-blob-direct")]
#[path = "azure-blob-direct/mod.rs"]
pub mod azure_blob_direct;
#[cfg(feature = "azure-blob-sas")]
#[path = "azure-blob-sas/mod.rs"]
pub mod azure_blob_sas;
pub mod binary;
pub mod chunk_outcome;
pub(crate) mod dflt;
pub mod direction;
pub mod down_pounce_builder;
mod download_trait;
pub mod error;
pub mod file_transfer_record;
pub mod http_breakpoint;
pub mod ids;
pub(crate) mod inner;
pub mod log;
pub mod meow_client;
pub mod meow_config;
pub mod pounce_task;
pub mod prepare_outcome;
#[cfg(feature = "presigned")]
pub mod presigned;
#[cfg(any(feature = "aliyun-oss-direct", feature = "azure-blob-direct"))]
mod provider_body;
#[cfg(any(feature = "aliyun-oss-direct", feature = "azure-blob-direct"))]
mod provider_xml;
pub(crate) mod target_lease;
#[cfg(test)]
mod test_support;
mod tls_provider;
pub mod transfer_executor_trait;
pub mod transfer_snapshot;
pub mod transfer_status;
pub mod transfer_task;
pub mod up_pounce_builder;
pub(crate) mod upload_file;
pub(crate) mod upload_source;
pub mod upload_trait;
pub use api::*;
