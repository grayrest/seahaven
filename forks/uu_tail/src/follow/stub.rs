// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! The no-follow `Observer`, upstream's WASI stub made universal.
//!
//! See `mod.rs` for why `tail -f` is dropped. The API is unchanged so
//! `tail.rs` compiles against it untouched.

use crate::args::Settings;
use std::io::BufRead;
use std::path::Path;
use uucore::error::{UResult, USimpleError};

pub struct Observer {
    pub use_polling: bool,
}

impl Observer {
    pub fn from(_settings: &Settings) -> Self {
        Self { use_polling: false }
    }

    #[allow(clippy::unnecessary_wraps)]
    pub fn start(&mut self, _settings: &Settings) -> UResult<()> {
        Ok(())
    }

    #[allow(clippy::unnecessary_wraps)]
    pub fn add_path(
        &mut self,
        _path: &Path,
        _display_name: &str,
        _reader: Option<Box<dyn BufRead>>,
        _update_last: bool,
    ) -> UResult<()> {
        Ok(())
    }

    #[allow(clippy::unnecessary_wraps)]
    pub fn add_bad_path(
        &mut self,
        _path: &Path,
        _display_name: &str,
        _update_last: bool,
    ) -> UResult<()> {
        Ok(())
    }

    pub fn follow_name_retry(&self) -> bool {
        false
    }
}

pub fn follow(_observer: Observer, _settings: &Settings) -> UResult<()> {
    Err(USimpleError::new(
        1,
        "follow mode is not supported: watching a path cannot be expressed in \
         the shell's namespace, so this build has no follow support at all",
    ))
}
