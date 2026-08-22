// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// FLATLAND DIVERGENCE: `tail -f` is dropped, and with it the `notify`
// dependency.
//
// Following a file means watching a *path* and being told when it changes.
// `notify` holds paths and reports on them independently of any namespace, and
// on some backends recurses on its own; `deny.toml` listed it as knowingly
// unrouted for exactly that reason. There is nothing to route it onto -- a
// watch is not an open, `cap-std` has no equivalent, and the descriptor the
// namespace can hand out is not what the OS watch APIs take.
//
// D4's disposition for a capability the namespace cannot express is to drop it
// and keep the utility -- it names `tail` without `-f` as the example. So the
// flag still parses, `tail` still reads and prints, and following reports that
// it is unavailable rather than watching paths the namespace never saw.
//
// The stub below is upstream's own: it already existed for WASI, where
// `notify` is equally unavailable. Making it the only implementation is a
// smaller divergence than writing one.

mod stub;

pub use stub::{Observer, follow};
