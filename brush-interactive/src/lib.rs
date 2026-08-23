//! Library implementing interactive command input and completion for the brush shell.

mod error;
pub use error::ShellError;

mod interactive_shell;
pub use interactive_shell::{InteractiveExecutionResult, InteractiveOptions, InteractiveShell};

mod input_backend;
pub use input_backend::{InputBackend, InteractivePrompt, ReadResult};

mod options;
pub use options::UIOptions;

mod refs;
pub use refs::ShellRef;

mod completeness;
mod term_detection;
mod term_integration;
mod trace_categories;

#[cfg(feature = "highlighting")]
pub mod highlighting;

#[cfg(feature = "completion")]
mod completion;

// Reedline-based shell
#[cfg(feature = "reedline")]
mod reedline;
#[cfg(feature = "reedline")]
pub use reedline::ReedlineInputBackend;

// Basic shell
#[cfg(feature = "basic")]
mod basic;
#[cfg(feature = "basic")]
pub use basic::BasicInputBackend;

// Minimal shell. Always available: it is a std-only, no-op-UI backend that reads
// a program from stdin, so it needs no feature-gated dependency. A non-interactive
// or headless run (e.g. a confined `-c` recipe shell) can use it without opting
// into any UI backend, and the default backend for a non-terminal run is Minimal.
mod minimal;
pub use minimal::MinimalInputBackend;
