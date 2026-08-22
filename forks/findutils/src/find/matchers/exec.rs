// Copyright 2017 Google Inc.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! FLATLAND DIVERGENCE: `-exec`, `-execdir`, `-ok` and `-okdir` are dropped.
//!
//! These spawned a child with `std::process::Command` directly, which goes
//! around **both** boundaries this shell has:
//!
//! - D2's closed world is a *parent-side* predicate. `SessionPayload` carries
//!   `cwd` and `mounts` to a bundled child and nothing else, so `find` had no
//!   policy object to consult and no idea one existed. Measured: `/bin/echo` is
//!   refused directly with exit 127, and runs happily through
//!   `find . -exec /bin/echo …`. That is arbitrary host execution from inside a
//!   closed world.
//! - `-execdir` additionally `chdir`s to each file's parent before spawning, so
//!   the child ran in the *host's* working directory. `find . -execdir /bin/pwd`
//!   printed a path outside the mount.
//!
//! Dropped rather than routed, per D4's disposition. Routing is the better
//! answer and is recorded as a follow-up in `plans/2026-08-21-broker.md`: it
//! needs the handshake to carry the execution policy, which is work the broker
//! plan already lists as undone. Note that only `-exec` could be routed even
//! then — `-execdir` cannot be, because a host program has no session and so
//! cannot take a virtual working directory, which is exactly why brush deleted
//! `Command::current_dir` from its own bundled dispatch.
//!
//! The predicates still parse, so `find . -exec foo` still reports a missing
//! `;` rather than an unknown predicate; the constructors refuse.

use std::error::Error;

use super::Matcher;

/// The error every one of the four predicates now returns.
fn refused(predicate: &str) -> Box<dyn Error> {
    From::from(format!(
        "{predicate} is not supported: running another program from `find` \
         would bypass the shell's execution policy, which does not reach a \
         bundled utility"
    ))
}

pub struct SingleExecMatcher;

impl SingleExecMatcher {
    /// Always refuses; see the module docs.
    pub fn new(
        _executable: &str,
        _args: &[&str],
        exec_in_parent_dir: bool,
    ) -> Result<Self, Box<dyn Error>> {
        Err(refused(if exec_in_parent_dir {
            "-execdir"
        } else {
            "-exec"
        }))
    }

    /// Always refuses; see the module docs.
    pub fn new_interactive(
        _executable: &str,
        _args: &[&str],
        exec_in_parent_dir: bool,
    ) -> Result<Self, Box<dyn Error>> {
        Err(refused(if exec_in_parent_dir { "-okdir" } else { "-ok" }))
    }
}

impl Matcher for SingleExecMatcher {
    fn matches(&self, _file_info: &super::WalkEntry, _matcher_io: &mut super::MatcherIO) -> bool {
        // Unreachable: the constructors above never yield a value.
        false
    }

    fn has_side_effects(&self) -> bool {
        true
    }
}

pub struct MultiExecMatcher;

impl MultiExecMatcher {
    /// Always refuses; see the module docs.
    pub fn new(
        _executable: &str,
        _args: &[&str],
        exec_in_parent_dir: bool,
    ) -> Result<Self, Box<dyn Error>> {
        Err(refused(if exec_in_parent_dir {
            "-execdir"
        } else {
            "-exec"
        }))
    }
}

impl Matcher for MultiExecMatcher {
    fn matches(&self, _file_info: &super::WalkEntry, _matcher_io: &mut super::MatcherIO) -> bool {
        false
    }

    fn has_side_effects(&self) -> bool {
        true
    }
}
