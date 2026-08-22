//! Session runtime facts: the signal queue, the clock, and the RNG (D36, D15, D14).
//!
//! These three are what a session *is* beyond its namespace and environment.
//! D15's session tuple is `(mounts, cwd, env, stdio, clock/RNG, locale)`; the
//! clock and the RNG are two of its members, and D14's hermetic mode is exactly
//! the mode that pins them. So they are **injectable**: a normal run reads real
//! time and real entropy, and a hermetic run (when D14 builds it) swaps in fixed
//! sources without the effects changing.

use std::collections::VecDeque;

/// Signals a guest can receive, as numbers — matching what rocjust's `Signal`
/// package names. The host delivers these; the guest reads them.
pub mod signal {
    /// Hangup.
    pub const SIGHUP: i64 = 1;
    /// Interrupt (Ctrl-C).
    pub const SIGINT: i64 = 2;
    /// Quit.
    pub const SIGQUIT: i64 = 3;
    /// Termination request.
    pub const SIGTERM: i64 = 15;
}

/// The signals a guest has caught but not yet read (D36).
///
/// # The queue is fed by the sandbox, not by the host's signals
///
/// A confined guest does **not** receive the host's signal disposition. D36
/// removes the terminal, so `SIGWINCH` and `SIGTSTP` have nothing to mean, and
/// forwarding `SIGINT` from the host would put a host-controlled event across
/// the boundary — a decision with its own argument, not made here. What feeds
/// this queue instead is the *sandbox*: a job completing, D35's deadline
/// expiring, the launcher asking for shutdown. None of those mechanisms exists
/// yet, so in an ordinary run [`take`](Self::take) returns `0` — which is
/// exactly what the plan says it does.
///
/// The host pushes with [`deliver`](Self::deliver); the guest polls with
/// [`take`](Self::take). Delivery before [`install`](Self::install) is dropped,
/// matching "install handlers, then poll": a signal with no handler installed
/// has nowhere to land.
#[derive(Debug, Default)]
pub struct SignalQueue {
    installed: bool,
    pending: VecDeque<i64>,
}

impl SignalQueue {
    /// An empty, un-armed queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            installed: false,
            pending: VecDeque::new(),
        }
    }

    /// Arms the queue. Idempotent — `basic-cli`'s `signal_install_handler!`.
    pub const fn install(&mut self) {
        self.installed = true;
    }

    /// The oldest caught signal, or `0` if none — and reading it clears it, so a
    /// poll loop sees each arrival exactly once (`basic-cli`'s `signal_take!`).
    pub fn take(&mut self) -> i64 {
        self.pending.pop_front().unwrap_or(0)
    }

    /// Delivers a signal into the queue, if a handler is installed.
    ///
    /// The host's path, not the guest's: the sandbox calls this when a job
    /// finishes or the launcher wants shutdown. Dropped when no handler is
    /// installed, because there is nothing listening.
    pub fn deliver(&mut self, sig: i64) {
        if self.installed {
            self.pending.push_back(sig);
        }
    }
}

/// A session's clock (D15). `now_nanos` is nanoseconds since the Unix epoch.
///
/// Injectable so D14's hermetic mode can pin it; the default reads real time.
pub trait Clock {
    /// Nanoseconds since the Unix epoch, or `None` if the clock is before it
    /// (`basic-cli`'s `ClockBeforeEpoch`).
    fn now_nanos(&self) -> Option<u128>;

    /// The local timezone's offset from UTC in seconds, right now — negative
    /// west of Greenwich (`basic-cli`'s `env_tz_offset!`).
    fn tz_offset_seconds(&self) -> i64;
}

/// The real clock: `SystemTime`, and a UTC offset of zero.
///
/// The offset is `0` rather than the host's, because the host's is a fact about
/// the machine (D33 puts `TZ` in the session's synthesized class). A launcher
/// that wants local time supplies a clock that reports the offset it chose; the
/// default does not read it from the host.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_nanos(&self) -> Option<u128> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_nanos())
    }

    fn tz_offset_seconds(&self) -> i64 {
        0
    }
}

/// A session's random source (D15). Injectable so D14's hermetic mode can seed
/// it; the default is real entropy.
pub trait Rng {
    /// The next random `u64`.
    fn next_u64(&mut self) -> u64;
}

/// The real RNG: fresh entropy per value.
///
/// Unpredictable by default, and that is load-bearing rather than incidental:
/// rocjust names a temp directory from `Random.seed_u64!()`, so a predictable
/// value there is a path collision and a predictable-path hazard, not a cosmetic
/// difference. Hermetic mode (D14) is the *only* place a fixed seed belongs, and
/// it opts in by supplying its own [`Rng`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRng;

impl Rng for SystemRng {
    fn next_u64(&mut self) -> u64 {
        rand::random()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_is_zero_until_a_signal_is_delivered_after_install() {
        let mut q = SignalQueue::new();
        assert_eq!(q.take(), 0, "an un-armed queue is quiet");

        // Delivery before install is dropped: nothing is listening.
        q.deliver(signal::SIGINT);
        q.install();
        assert_eq!(q.take(), 0, "a signal before install does not survive it");

        q.deliver(signal::SIGINT);
        q.deliver(signal::SIGTERM);
        assert_eq!(q.take(), signal::SIGINT, "oldest first");
        assert_eq!(q.take(), signal::SIGTERM);
        assert_eq!(q.take(), 0, "and reading clears");
    }

    #[test]
    fn the_system_clock_is_after_the_epoch() {
        assert!(SystemClock.now_nanos().is_some());
        assert_eq!(SystemClock.tz_offset_seconds(), 0);
    }

    #[test]
    fn the_system_rng_is_not_a_constant() {
        let mut rng = SystemRng;
        let a = rng.next_u64();
        let b = rng.next_u64();
        // Two draws being equal is a 1-in-2^64 event; treat it as the RNG being
        // stuck rather than as bad luck.
        assert_ne!(a, b, "the default RNG must not repeat");
    }
}
