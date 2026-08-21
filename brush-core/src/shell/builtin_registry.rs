//! Builtin command management for shell instances.

use std::collections::HashMap;

use crate::{builtins, extensions};

impl<SE: extensions::ShellExtensions> crate::Shell<SE> {
    /// Register a builtin to the shell's environment, replacing any existing
    /// registration with the same name.
    ///
    /// A name the shell's builtin policy does not admit (D11) is silently not
    /// registered. Silence is deliberate: the caller is a bulk registration
    /// loop offering every compiled-in builtin, and a policy denying most of
    /// them is the expected case, not an error to report once per name.
    ///
    /// # Arguments
    ///
    /// * `name` - The in-shell name of the builtin.
    /// * `registration` - The registration handle for the builtin.
    pub fn register_builtin<S: Into<String>>(
        &mut self,
        name: S,
        registration: builtins::Registration<SE>,
    ) {
        let name = name.into();
        if !self.builtin_policy().admits(&name) {
            return;
        }
        self.builtins.insert(name, registration);
    }

    /// Register a builtin only if no builtin with that name is already registered.
    ///
    /// Subject to the builtin policy, as [`register_builtin`](Self::register_builtin)
    /// is.
    ///
    /// # Arguments
    ///
    /// * `name` - The in-shell name of the builtin.
    /// * `registration` - The registration handle for the builtin.
    pub fn register_builtin_if_unset<S: Into<String>>(
        &mut self,
        name: S,
        registration: builtins::Registration<SE>,
    ) {
        let name = name.into();
        if !self.builtin_policy().admits(&name) {
            return;
        }
        self.builtins.entry(name).or_insert(registration);
    }

    /// Tries to retrieve a mutable reference to an existing builtin registration.
    /// Returns `None` if no such registration exists.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the builtin to lookup.
    pub fn builtin_mut(&mut self, name: &str) -> Option<&mut builtins::Registration<SE>> {
        self.builtins.get_mut(name)
    }

    /// Returns the registered builtins for the shell.
    pub const fn builtins(&self) -> &HashMap<String, builtins::Registration<SE>> {
        &self.builtins
    }
}
