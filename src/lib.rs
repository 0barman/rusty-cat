//! `rusty-cat` public SDK crate.
//!
//! This crate exposes high-level APIs to enqueue and control upload/download
//! tasks with breakpoint resume support.
//!
//! For beginners, start from [`api`] or directly use [`meow_client::MeowClient`]
//! and [`meow_config::MeowConfig`].
pub mod api;
pub mod chunk_outcome;
pub(crate) mod dflt;
pub mod direction;
pub mod down_pounce_builder;
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
pub mod transfer_executor_trait;
pub mod transfer_snapshot;
pub mod transfer_status;
pub mod transfer_task;
pub mod up_pounce_builder;
pub mod upload_trait;
mod download_trait;

pub use api::*;
