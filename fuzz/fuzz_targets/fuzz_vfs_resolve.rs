#![no_main]
#![allow(missing_docs)]
#![allow(clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "libfuzzer_sys::fuzz_target! opens a host file in its own expansion"
)]

use brush_vfs::VirtualPath;
use libfuzzer_sys::fuzz_target;

/// Properties every accepted path must hold, checked against arbitrary input.
///
/// The grammar is the one artifact in the vfs whose whole job is to reject
/// hostile strings, so asserting invariants over random input is worth more here
/// than anywhere else in the crate. The escape property in particular is what
/// the sandbox rests on: if a path is accepted, no spelling of `..` inside it
/// climbed out of the root.
fn check(base: &str, input: &str) {
    let Ok(base) = VirtualPath::new(base) else {
        return;
    };

    match base.resolve(input) {
        Err(_) => {}
        Ok(resolved) => {
            let s = resolved.as_str();

            // Absolute, and free of the constructs the grammar forbids.
            assert!(s.starts_with('/'), "not absolute: {s:?}");
            assert!(!s.contains('\\'), "backslash survived: {s:?}");
            assert!(!s.contains('\0'), "NUL survived: {s:?}");
            assert!(!s.contains("//"), "empty component survived: {s:?}");
            assert!(s == "/" || !s.ends_with('/'), "trailing slash: {s:?}");

            // No component is `.`, `..`, or ends in a character Windows strips.
            for c in resolved.components() {
                assert!(!c.is_empty(), "empty component: {s:?}");
                assert_ne!(c, ".", "dot component survived: {s:?}");
                assert_ne!(c, "..", "dotdot survived: {s:?}");
                assert!(!c.contains(':'), "colon survived: {s:?}");
                assert!(!c.ends_with('.') && !c.ends_with(' '), "strippable: {s:?}");
            }

            // Re-resolving an accepted path is a no-op. Anything that fails this
            // is a normalization the grammar did not reach a fixed point on, and
            // therefore two spellings of one file.
            let again = VirtualPath::new(s).expect("accepted path must re-parse");
            assert_eq!(again, resolved, "not idempotent: {s:?}");

            // Resolution may drop components but must never invent one, so the
            // result cannot be longer than what went in.
            //
            // This replaces `assert!(resolved.starts_with(&VirtualPath::root()))`,
            // which cannot fail -- every virtual path starts with the root by
            // construction -- and `starts_with(&base)`, which is false for the
            // ordinary case of a `..` that stays inside: `/work/sub` plus
            // `../file.txt` is `/work/file.txt`, beneath the root but not
            // beneath the base.
            //
            // The escape property itself is carried by `resolve` returning
            // `Err`, which is why the interesting corpus seeds are the ones
            // that never reach this block.
            assert!(
                resolved.components().count()
                    <= base.components().count() + input.split('/').count(),
                "resolution invented components: {base} + {input:?} -> {s:?}"
            );
        }
    }
}

fuzz_target!(|data: (String, String)| {
    let (base, input) = data;
    check(&base, &input);

    // An absolute input ignores the base entirely, so resolving it against the
    // root must agree with resolving it against a deep base. A difference would
    // mean a path's meaning depended on where it was evaluated from.
    if input.starts_with('/') {
        let deep = VirtualPath::new("/some/deep/base").expect("fixed base parses");
        assert_eq!(
            VirtualPath::root().resolve(&input).ok(),
            deep.resolve(&input).ok(),
            "absolute input depended on its base: {input:?}"
        );
    }
});
