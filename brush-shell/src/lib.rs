//! Create for brush, an executable bash-compatible shell.

#![allow(dead_code)]

pub mod args;
mod brushctl;
mod builtinallowlist;
pub mod bundled;
pub mod config;
pub mod discovery;
pub mod entry;
mod error_formatter;
pub mod events;
pub mod grant;
mod productinfo;
pub mod trust;
