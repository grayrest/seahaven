//! The D4 codemod: route a forked utility's filesystem access through the vfs.
//!
//! The transformation is an *identifier swap at call sites*, not a type swap
//! (D34): the [`brush_vfs::ambient`] facade returns `std::fs` types, so
//! `File::open(p)` becomes `brush_vfs::ambient::open(p)` and the surrounding
//! code — the `File` the call yields, the `Metadata` a `metadata` call yields —
//! is untouched. Because the rewrite targets are written as absolute paths, no
//! `use` is added; the now-unused `std::fs` imports are pruned instead.
//!
//! It edits the original source by byte span rather than reprinting the parsed
//! AST, so the diff is confined to the lines that actually change — which is
//! what makes the residual patch set (D13's health metric) legible.
//!
//! ## Scope of this version
//!
//! Handled: `std::fs` free-function calls (`metadata(p)`, `fs::read(p)`) and the
//! `File::open` / `File::create` associated calls. Inherent `Path` methods
//! (`p.metadata()`, `p.exists()`) — the majority of a large utility's sites per
//! the spike — are *reported*, not yet rewritten: a utility that has them is
//! not fully routed, and the report says so rather than the tool pretending
//! otherwise. The remaining carve-outs the facade does not provide (`copy`,
//! hard links, permissions, non-recursive `create_dir`) are reported too.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use syn::spanned::Spanned;
use syn::visit::Visit;

/// The `codemod` subcommand.
#[derive(clap::Parser)]
pub struct CodemodCommand {
    /// A `.rs` file, or a directory whose `.rs` files are all rewritten.
    pub path: PathBuf,

    /// Report what would change without writing anything.
    #[clap(long)]
    pub check: bool,

    /// Leave a module alone: a path suffix, e.g. `features/fsext.rs`.
    ///
    /// For modules whose filesystem access is *host introspection* rather than
    /// namespace access -- `uucore`'s `fsext` reads `/etc/mtab`, `mods/os.rs`
    /// reads `/proc/sys/kernel/osrelease` -- where routing through the vfs would
    /// break the utility rather than confine it. Repeatable.
    #[clap(long = "skip", value_name = "PATH_SUFFIX")]
    pub skips: Vec<String>,
}

/// Runs the codemod command.
pub fn run(cmd: &CodemodCommand, _verbose: bool) -> Result<()> {
    let all = rust_files(&cmd.path)?;
    if all.is_empty() {
        anyhow::bail!("no .rs files under {}", cmd.path.display());
    }

    let (files, skipped): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|f| !is_skipped(f, &cmd.skips));
    for file in &skipped {
        eprintln!("{}: skipped (--skip)", file.display());
    }
    // A `--skip` that matches nothing is almost always a typo'd path, and
    // silently routing a module the operator meant to exempt is the failure
    // this flag exists to prevent.
    for pattern in &cmd.skips {
        anyhow::ensure!(
            skipped.iter().any(|f| path_has_suffix(f, pattern)),
            "--skip {pattern:?} matched no file under {}",
            cmd.path.display()
        );
    }

    let mut total_rewrites = 0usize;
    let mut total_unhandled = 0usize;
    for file in &files {
        let source = std::fs::read_to_string(file)
            .with_context(|| format!("reading {}", file.display()))?;
        let outcome = rewrite(&source)
            .with_context(|| format!("rewriting {}", file.display()))?;

        if !outcome.rewrites.is_empty() || !outcome.unhandled.is_empty() {
            eprintln!("{}:", file.display());
            for note in &outcome.rewrites {
                eprintln!("  rewrote  {note}");
            }
            for note in &outcome.unhandled {
                eprintln!("  UNROUTED {note}");
            }
        }
        total_rewrites += outcome.rewrites.len();
        total_unhandled += outcome.unhandled.len();

        if !cmd.check && outcome.changed {
            std::fs::write(file, &outcome.source)
                .with_context(|| format!("writing {}", file.display()))?;
        }
    }

    eprintln!(
        "\n{} site(s) routed, {} inherent/carve-out site(s) left unrouted{}.",
        total_rewrites,
        total_unhandled,
        if cmd.check { " (check only, nothing written)" } else { "" }
    );
    Ok(())
}

/// The free `std::fs` functions the [`brush_vfs::ambient`] facade provides.
/// `canonicalize` and `symlink_metadata` are absent on purpose — they are
/// owner-decision carve-outs (D34), so a call to them is reported unrouted
/// rather than pointed at a function that does not exist.
const FACADE_FREE_FNS: &[&str] = &[
    "metadata",
    "symlink_metadata",
    "read",
    "read_to_string",
    "write",
    "read_dir",
    "read_link",
    "canonicalize",
    "create_dir_all",
    "remove_file",
    "remove_dir",
    "remove_dir_all",
    "rename",
    "exists",
    "try_exists",
    "create_dir",
    "set_permissions",
    "copy",
    "hard_link",
];

/// Free functions whose facade equivalent has a *different name*, because
/// `std` gives the same name two different signatures.
///
/// `std::fs::exists(p)` returns `io::Result<bool>`; `Path::exists()` returns a
/// bare `bool`. The facade mirrors both -- `try_exists` and `exists`
/// respectively -- so the free call has to be pointed at `try_exists` or the
/// rewrite silently changes the type. `uu_cp` caught this: it writes
/// `std::fs::exists(t).is_ok_and(identity)`, which stops compiling when the
/// call starts returning `bool`. Signature preservation (D34) is about the
/// *call site* being unchanged, and that includes what it evaluates to.
const FREE_FN_RENAMES: &[(&str, &str)] = &[("exists", "try_exists")];

/// Inherent `Path`/`PathBuf` methods the visitor rewrites to a facade call.
///
/// Each is named *distinctively* — the name appears only on `Path`, not on
/// `File`, `FileType` or `DirEntry` — so a bare method name is a reliable
/// signal without type inference, and each is one the facade provides. The
/// facade's `impl AsRef<Path>` bound is a second guard: if a receiver somehow is
/// not path-like, `ambient::exists(&recv)` fails to *compile* rather than
/// mis-routing, so a wrong rewrite is caught at build time.
///
/// All take no arguments, which is what makes the receiver-only rewrite
/// `recv.m()` -> `ambient::m(&(recv))` uniform.
const REWRITTEN_PATH_METHODS: &[&str] = &[
    "exists",
    "try_exists",
    "read_link",
    "read_dir",
    "canonicalize",
    "symlink_metadata",
];

/// Filesystem functions the facade does not provide, reported when seen as a
/// free call or a distinctive method so the residual work is visible.
///
/// Down to one: `copy`, `hard_link`, `set_permissions` and `create_dir` all
/// moved into the facade once the survey showed they were 25 of the 26
/// unrouted sites across the unforked utilities, and `cap_std::fs::Dir`
/// provides all four. `soft_link` is deprecated in `std` itself in favour of
/// `os::unix::fs::symlink`, so a call to it is worth reporting rather than
/// quietly routing. Deliberately absent are `metadata` / `is_dir` /
/// `is_file` / `is_symlink`, which also exist on non-`Path` types (`File`,
/// `FileType`) where they need no routing — flagging them without type inference
/// reports pure calls as unrouted, which is `uu_cat`'s `filetype.is_dir()` false
/// positive. The ban and Landlock test are the backstop for any genuinely
/// unrouted `path.metadata()`.
const CARVE_OUT_FNS: &[&str] = &["soft_link"];

/// The facade path a rewrite points at.
const FACADE: &str = "brush_vfs::ambient";

/// What names `std::fs` items are bound to in a file.
#[derive(Default)]
struct Bindings {
    /// Names that refer to the `std::fs` *module* (`use std::fs;`, `use std::fs
    /// as f;`, `use std::fs::{self};`).
    module_aliases: BTreeSet<String>,
    /// Local binding name → real `std::fs` free-function name.
    free_fns: BTreeMap<String, String>,
    /// Local names bound to `std::fs::File`.
    file_names: BTreeSet<String>,
}

/// The result of rewriting one file.
struct Outcome {
    source: String,
    changed: bool,
    rewrites: Vec<String>,
    unhandled: Vec<String>,
}

/// A single byte-span replacement in the original source.
struct Edit {
    start: usize,
    end: usize,
    replacement: String,
}

/// Rewrites one file's source, returning the new text and a report.
fn rewrite(source: &str) -> Result<Outcome> {
    let ast: syn::File = syn::parse_file(source).context("parsing as Rust")?;

    let mut bindings = Bindings::default();
    let mut bv = BindingVisitor { b: &mut bindings };
    bv.visit_file(&ast);

    let mut collector = EditCollector {
        bindings: &bindings,
        source,
        edits: Vec::new(),
        rewrites: Vec::new(),
        unhandled: Vec::new(),
        // How many times each imported name was consumed by a rewrite, so the
        // import can be pruned only when nothing else still uses it.
        consumed: BTreeMap::new(),
        non_path: Vec::new(),
    };
    collector.visit_file(&ast);

    // Prune now-unused `std::fs` imports.
    let mut pruner = ReferenceCounter {
        targets: bindings
            .free_fns
            .keys()
            .chain(bindings.file_names.iter())
            .cloned()
            .collect(),
        counts: BTreeMap::new(),
    };
    pruner.visit_file(&ast);

    let import_edits = prune_imports(&ast, &bindings, &collector.consumed, &pruner.counts, source);

    let mut edits = collector.edits;
    edits.extend(import_edits);

    let rewrites = collector.rewrites;
    let unhandled = collector.unhandled;
    let changed = !edits.is_empty();
    let source = apply_edits(source, edits);

    Ok(Outcome {
        source,
        changed,
        rewrites,
        unhandled,
    })
}

/// Collects `std::fs` bindings from `use` items.
struct BindingVisitor<'a> {
    b: &'a mut Bindings,
}

impl<'ast> Visit<'ast> for BindingVisitor<'_> {
    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        collect_use(&i.tree, &mut Vec::new(), self.b);
    }
}

/// Walks a `use` tree, recording any `std::fs::...` leaves.
fn collect_use(tree: &syn::UseTree, prefix: &mut Vec<String>, b: &mut Bindings) {
    match tree {
        syn::UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            collect_use(&p.tree, prefix, b);
            prefix.pop();
        }
        syn::UseTree::Group(g) => {
            for item in &g.items {
                collect_use(item, prefix, b);
            }
        }
        syn::UseTree::Name(n) => handle_leaf(prefix, &n.ident.to_string(), &n.ident.to_string(), b),
        syn::UseTree::Rename(r) => {
            handle_leaf(prefix, &r.ident.to_string(), &r.rename.to_string(), b);
        }
        // `use std::fs::*` brings every fs name into scope unqualified; treat
        // each facade name as a free-fn binding under its own name.
        syn::UseTree::Glob(_) => {
            if prefix.as_slice() == ["std", "fs"] {
                for name in FACADE_FREE_FNS {
                    b.free_fns.insert((*name).to_string(), (*name).to_string());
                }
            }
        }
    }
}

/// Records one resolved `use` leaf. `real` is the upstream name; `local` is what
/// it is bound to here (differs under `as`).
fn handle_leaf(prefix: &[String], real: &str, local: &str, b: &mut Bindings) {
    if prefix == ["std"] && real == "fs" {
        // `use std::fs [as alias];`
        b.module_aliases.insert(local.to_string());
    } else if prefix == ["std", "fs"] {
        match real {
            "self" => {
                // `use std::fs::{self}` binds the module under its own last
                // segment name (`fs`); `self as alias` binds it under `alias`.
                // Without this, `fs::metadata(p)` in `uu_wc` went unrewritten
                // because the alias was recorded as "self".
                let bound = if local == "self" {
                    prefix.last().cloned().unwrap_or_else(|| local.to_string())
                } else {
                    local.to_string()
                };
                b.module_aliases.insert(bound);
            }
            "File" => {
                b.file_names.insert(local.to_string());
            }
            _ => {
                b.free_fns.insert(local.to_string(), real.to_string());
            }
        }
    }
}

/// Collects the call-site rewrites.
struct EditCollector<'a> {
    bindings: &'a Bindings,
    /// The original source, so a method rewrite can splice the receiver's text.
    source: &'a str,
    edits: Vec<Edit>,
    rewrites: Vec<String>,
    unhandled: Vec<String>,
    consumed: BTreeMap<String, usize>,
    /// Per-function sets of parameter names whose declared type is definitely
    /// not path-like, so a distinctive method on one is reported rather than
    /// rewritten. A stack, because functions nest.
    non_path: Vec<BTreeSet<String>>,
}

impl<'ast> Visit<'ast> for EditCollector<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if item_attrs(item).is_some_and(is_test_gated) {
            return;
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        if impl_item_attrs(item).is_some_and(is_test_gated) {
            return;
        }
        syn::visit::visit_impl_item(self, item);
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.non_path.push(non_path_params(&f.sig));
        syn::visit::visit_item_fn(self, f);
        self.non_path.pop();
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        self.non_path.push(non_path_params(&f.sig));
        syn::visit::visit_impl_item_fn(self, f);
        self.non_path.pop();
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let syn::Pat::Ident(pat) = &local.pat {
            let declared_non_path = match &local.pat {
                syn::Pat::Type(t) => named_base_type(&t.ty)
                    .is_some_and(|b| !PATH_LIKE_TYPES.contains(&b.as_str())),
                _ => false,
            };
            // Transitive, because the binding is usually several hops from the
            // constructor: `uu_du` writes `let open_result = ... DirFd::open(..)`
            // and then `let dir_fd = match open_result { Ok(fd) => fd, .. }`, so
            // only the *first* let mentions the type at all.
            let init_mentions_descriptor = local.init.as_ref().is_some_and(|i| {
                let text = quote_tokens(&i.expr);
                DESCRIPTOR_TYPES.iter().any(|t| text.contains(t))
                    || self
                        .non_path
                        .iter()
                        .flatten()
                        .any(|known| mentions_ident(&text, known))
            });
            if (declared_non_path || init_mentions_descriptor)
                && let Some(scope) = self.non_path.last_mut()
            {
                scope.insert(pat.ident.to_string());
            }
        }
        syn::visit::visit_local(self, local);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*call.func {
            self.try_rewrite_call(&p.path);
        }
        // Recurse so nested calls in the arguments are seen too.
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, mc: &'ast syn::ExprMethodCall) {
        let method = mc.method.to_string();
        if mc.args.is_empty()
            && REWRITTEN_PATH_METHODS.contains(&method.as_str())
            && self.receiver_is_known_non_path(&mc.receiver)
        {
            // `DirFd::read_dir` in `uucore`'s `perms.rs` is the motivating case:
            // it shares a name with `Path::read_dir` but is an `openat`-anchored
            // descriptor method that is already confined, and rewriting it to
            // `ambient::read_dir(&(dir_fd))` only fails the `AsRef<Path>` bound.
            let line = mc.method.span().start().line;
            self.unhandled.push(format!(
                "line {line}: `.{method}()` left alone -- receiver has a declared non-path type"
            ));
            syn::visit::visit_expr_method_call(self, mc);
            return;
        }
        if mc.args.is_empty() && REWRITTEN_PATH_METHODS.contains(&method.as_str()) {
            // This edit spans the whole call and copies the receiver verbatim,
            // so descending would emit edits inside a range about to be
            // replaced. Any rewritable call within such a receiver is rare and
            // left to the ban as backstop rather than risk overlapping edits.
            self.rewrite_method(mc, &method);
            return;
        }
        if CARVE_OUT_FNS.contains(&method.as_str()) {
            let line = mc.method.span().start().line;
            self.unhandled.push(format!(
                "line {line}: method `.{method}()` is a carve-out (D34), facade has no equivalent"
            ));
        }
        syn::visit::visit_expr_method_call(self, mc);
    }
}

impl EditCollector<'_> {
    /// Whether a method receiver is a bare parameter with a declared non-path
    /// type, and so must not be rewritten to a path-taking facade call.
    fn receiver_is_known_non_path(&self, receiver: &syn::Expr) -> bool {
        let syn::Expr::Path(p) = receiver else {
            return false;
        };
        let Some(ident) = p.path.get_ident() else {
            return false;
        };
        let name = ident.to_string();
        self.non_path.iter().any(|scope| scope.contains(&name))
    }

    /// Rewrites a call whose callee path names a routed `std::fs` operation.
    fn try_rewrite_call(&mut self, path: &syn::Path) {
        let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        let span = path.segments.last().map(|s| s.ident.span());

        // File::open / File::create, in any spelling that resolves to std::fs::File.
        let (file_base, assoc) = match segs.as_slice() {
            [a, m] if self.bindings.file_names.contains(a) => (Some(a.clone()), Some(m.clone())),
            [s, f, a, m] if s == "std" && f == "fs" && a == "File" => {
                (Some("File".to_string()), Some(m.clone()))
            }
            _ => (None, None),
        };
        if let (Some(base), Some(assoc)) = (file_base, assoc) {
            if assoc == "open" || assoc == "create" {
                self.push_call_edit(path, &format!("{FACADE}::{assoc}"));
                *self.consumed.entry(base).or_default() += 1;
                self.rewrites
                    .push(format!("File::{assoc} -> {FACADE}::{assoc}"));
            }
            return;
        }

        // A free function: bare `metadata(p)`, `fs::metadata(p)`, or
        // `std::fs::metadata(p)`.
        let (binding, real) = match segs.as_slice() {
            [only] => match self.bindings.free_fns.get(only) {
                Some(real) => (Some(only.clone()), real.clone()),
                None => (None, String::new()),
            },
            [module, f] if self.bindings.module_aliases.contains(module) => {
                (None, f.clone())
            }
            [s, fs, f] if s == "std" && fs == "fs" => (None, f.clone()),
            _ => (None, String::new()),
        };
        if real.is_empty() {
            return;
        }
        if FACADE_FREE_FNS.contains(&real.as_str()) {
            let target = FREE_FN_RENAMES
                .iter()
                .find(|(from, _)| *from == real)
                .map_or(real.as_str(), |(_, to)| *to);
            self.push_call_edit(path, &format!("{FACADE}::{target}"));
            if let Some(b) = binding {
                *self.consumed.entry(b).or_default() += 1;
            }
            self.rewrites
                .push(format!("{real}() -> {FACADE}::{target}()"));
        } else if CARVE_OUT_FNS.contains(&real.as_str()) {
            // A known fs function the facade does not provide (carve-out).
            let line = span.map_or(0, |s| s.start().line);
            self.unhandled
                .push(format!("line {line}: `{real}` is a carve-out (D34), facade has no equivalent"));
        }
    }

    /// Replaces a callee path's byte span with `replacement`.
    fn push_call_edit(&mut self, path: &syn::Path, replacement: &str) {
        let range = path_byte_range(path);
        self.edits.push(Edit {
            start: range.0,
            end: range.1,
            replacement: replacement.to_string(),
        });
    }

    /// Rewrites a no-argument `recv.method()` into `ambient::method(&(recv))`.
    ///
    /// The receiver is wrapped in `&(...)` so the borrow matches the `&self` the
    /// inherent method took, and the parentheses keep a complex receiver (a
    /// chain, a field access) grouped. `&(recv)` satisfies the facade's
    /// `impl AsRef<Path>` for any path-like receiver — including one already a
    /// reference, via the blanket `AsRef` on `&T`.
    fn rewrite_method(&mut self, mc: &syn::ExprMethodCall, method: &str) {
        let (whole_start, whole_end) = span_byte_range(mc.span());
        let (recv_start, recv_end) = span_byte_range(mc.receiver.span());
        // Spans land on char boundaries, so this range is valid; `get` avoids a
        // panicking index and skips the rewrite rather than corrupt on the
        // impossible case.
        let Some(recv_src) = self.source.get(recv_start..recv_end) else {
            return;
        };
        let replacement = format!("{FACADE}::{method}(&({recv_src}))");
        self.edits.push(Edit {
            start: whole_start,
            end: whole_end,
            replacement,
        });
        self.rewrites
            .push(format!(".{method}() -> {FACADE}::{method}(&(..))"));
    }
}

/// Counts non-`use` references to a set of names, so an import is pruned only
/// when nothing else in the file still needs it.
struct ReferenceCounter {
    targets: BTreeSet<String>,
    counts: BTreeMap<String, usize>,
}

impl<'ast> Visit<'ast> for ReferenceCounter {
    // Do not descend into `use` items: those references are the import itself.
    fn visit_item_use(&mut self, _i: &'ast syn::ItemUse) {}

    fn visit_path(&mut self, path: &'ast syn::Path) {
        if let Some(first) = path.segments.first() {
            let name = first.ident.to_string();
            if self.targets.contains(&name) {
                *self.counts.entry(name).or_default() += 1;
            }
        }
        syn::visit::visit_path(self, path);
    }
}

/// Produces edits removing the now-unused names from `use std::fs::...` items.
fn prune_imports(
    ast: &syn::File,
    bindings: &Bindings,
    consumed: &BTreeMap<String, usize>,
    references: &BTreeMap<String, usize>,
    source: &str,
) -> Vec<Edit> {
    // A name is prunable when every non-use reference to it was consumed by a
    // rewrite.
    let prunable = |name: &str| -> bool {
        let refs = references.get(name).copied().unwrap_or(0);
        let used = consumed.get(name).copied().unwrap_or(0);
        // Only prune names we actually bound from std::fs.
        (bindings.file_names.contains(name) || bindings.free_fns.contains_key(name))
            && refs == used
    };

    let mut edits = Vec::new();
    for item in &ast.items {
        if let syn::Item::Use(use_item) = item {
            if let Some(edit) = prune_use_item(use_item, &prunable, source) {
                edits.push(edit);
            }
        }
    }
    edits
}

/// Rebuilds a single `use` item with prunable `std::fs` leaves removed, if any.
///
/// Only the *pure* form `use std::fs::...` is touched. A grouped
/// `use std::{fs::File, ffi::OsString}` is left alone, because removing the fs
/// leaf would mean rebuilding the whole group and risk dropping the siblings —
/// the bug that deleted `OsString`/`Path` from `uu_wc`. An fs name left unused
/// inside a group is a harmless unused-import warning, not a broken build.
fn prune_use_item(
    use_item: &syn::ItemUse,
    prunable: &impl Fn(&str) -> bool,
    source: &str,
) -> Option<Edit> {
    // Match exactly `Path("std") -> Path("fs") -> tail`. Anything else (a group
    // directly under `std`, an aliased module) is not the pure form.
    let syn::UseTree::Path(std_p) = &use_item.tree else {
        return None;
    };
    if std_p.ident != "std" {
        return None;
    }
    let syn::UseTree::Path(fs_p) = &*std_p.tree else {
        return None;
    };
    if fs_p.ident != "fs" {
        return None;
    }

    let (kept, dropped) = match &*fs_p.tree {
        // `use std::fs::File;` — drop the whole statement if that name is unused.
        syn::UseTree::Name(n) => {
            if prunable(&n.ident.to_string()) {
                (Vec::new(), true)
            } else {
                return None;
            }
        }
        // `use std::fs::{a, b, c};` — keep the names still used. Bail if any leaf
        // is not a plain name (a rename or nested group), rather than risk
        // reconstructing it wrong.
        syn::UseTree::Group(g) => {
            let mut kept = Vec::new();
            let mut dropped = false;
            for item in &g.items {
                let syn::UseTree::Name(n) = item else {
                    return None;
                };
                let name = n.ident.to_string();
                if prunable(&name) {
                    dropped = true;
                } else {
                    kept.push(name);
                }
            }
            if !dropped {
                return None;
            }
            (kept, dropped)
        }
        // Glob or a direct rename: leave alone.
        _ => return None,
    };

    if !dropped {
        return None;
    }

    let (start, end) = span_byte_range(use_item.span());
    if kept.is_empty() {
        // Remove the whole statement, including a trailing newline if present.
        return Some(Edit {
            start,
            end: consume_trailing_newline(source, end),
            replacement: String::new(),
        });
    }
    let replacement = if kept.len() == 1 {
        format!("use std::fs::{};", kept[0])
    } else {
        format!("use std::fs::{{{}}};", kept.join(", "))
    };
    Some(Edit {
        start,
        end,
        replacement,
    })
}

/// The byte range a path's tokens occupy, from the first segment to the last.
fn path_byte_range(path: &syn::Path) -> (usize, usize) {
    let start = path
        .leading_colon
        .as_ref()
        .map_or_else(
            || {
                path.segments
                    .first()
                    .map_or(0, |s| span_byte_range(s.ident.span()).0)
            },
            |c| span_byte_range(c.spans[0]).0,
        );
    let end = path
        .segments
        .last()
        .map_or(start, |s| span_byte_range(s.ident.span()).1);
    (start, end)
}

/// A span's byte range in the parsed source.
fn span_byte_range(span: proc_macro2::Span) -> (usize, usize) {
    let r = span.byte_range();
    (r.start, r.end)
}

/// Extends `end` past a single trailing newline (and any preceding spaces), so
/// removing a statement does not leave a blank line.
fn consume_trailing_newline(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = end;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'\n' {
        i += 1;
    }
    i
}

/// Applies edits to the source, back to front so earlier offsets stay valid.
fn apply_edits(source: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    let mut out = source.to_string();
    for edit in edits {
        out.replace_range(edit.start..edit.end, &edit.replacement);
    }
    out
}

/// Every `.rs` file at or under `path`.
fn rust_files(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut out = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Whether `file` matches any `--skip` pattern.
fn is_skipped(file: &Path, skips: &[String]) -> bool {
    skips.iter().any(|s| path_has_suffix(file, s))
}

/// Whether `file`'s path ends with `suffix`, matched on whole components.
///
/// Component-wise rather than textual so `--skip fs.rs` cannot also match
/// `safe_fs.rs`, and separators are normalized so a pattern written with `/`
/// works on Windows.
fn path_has_suffix(file: &Path, suffix: &str) -> bool {
    let want: Vec<&str> = suffix
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .collect();
    if want.is_empty() {
        return false;
    }
    let have: Vec<String> = file
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    have.len() >= want.len()
        && have[have.len() - want.len()..]
            .iter()
            .zip(&want)
            .all(|(a, b)| a == b)
}

/// Whether an attribute list gates its item to test builds.
///
/// Upstream's own tests must not be routed: the facade fails closed with no
/// session installed, so a routed `#[cfg(test)]` body turns D13's health metric
/// -- "do upstream's tests still pass" -- into a guaranteed failure that reads
/// like a divergence. The five leaf forks escaped this only because none has a
/// filesystem call inside a test module.
///
/// The predicate is deliberately textual and deliberately biased. A `cfg`
/// containing `not` is never treated as a test gate, so `#[cfg(not(test))]` --
/// which marks *production* code -- is still routed. The cost is that
/// `#[cfg(all(test, not(windows)))]` is not recognized and its body gets routed;
/// that direction fails loudly in the upstream suite, whereas mistaking
/// production for test would silently leave a hole.
fn is_test_gated(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().is_ident("test") {
            return true;
        }
        if !attr.path().is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &attr.meta else {
            return false;
        };
        let tokens = list.tokens.to_string();
        !tokens.contains("not") && tokens.split(|c: char| !c.is_alphanumeric() && c != '_').any(|t| t == "test")
    })
}

/// Type names a receiver may have and still be routed as a path.
///
/// Everything else *named* is treated as definitely-not-a-path — but only when
/// the name is not one of the enclosing function's generic parameters, since a
/// bare `P` is very often `P: AsRef<Path>` and must keep its existing rewrite.
const PATH_LIKE_TYPES: &[&str] = &[
    "Path", "PathBuf", "OsStr", "OsString", "str", "String", "Cow",
];

/// Types that are `*at`-anchored directory descriptors and share method names
/// with `Path` without being path-like.
///
/// A local bound from one of these must not have `.read_dir()` rewritten. Unlike
/// [`non_path_params`], which reads a declared type, a local's type is usually
/// inferred -- `uu_du` writes `let dir_fd = match open_result { Ok(fd) => fd, .. }`
/// where `open_result` came from `DirFd::open` several statements earlier, and
/// following that properly means real type inference.
///
/// So the heuristic is deliberately crude: a `let` whose initializer *mentions*
/// one of these type names binds a non-path local. It over-approximates, and the
/// direction of that error is the safe one -- a missed rewrite is reported as
/// unrouted, whereas a wrong rewrite would be a mis-route. In practice even a
/// wrong rewrite fails to compile against the facade's `AsRef<Path>` bound,
/// which is how `uu_du` surfaced this in the first place.
const DESCRIPTOR_TYPES: &[&str] = &["DirFd"];

/// The parameter names of `sig` whose declared type is definitely not path-like.
///
/// Partial type inference on purpose: it reads only what the signature states.
/// A parameter whose type names one of the function's own generics, or is
/// `impl Trait`, or is anything other than a plain named path type, is left
/// *unknown* and keeps the existing receiver-blind rewrite. The set is
/// therefore small and every member is certain, which is the direction that
/// matters — a false "non-path" silently stops routing a real path call.
fn non_path_params(sig: &syn::Signature) -> BTreeSet<String> {
    let generics: BTreeSet<String> = sig
        .generics
        .params
        .iter()
        .filter_map(|p| match p {
            syn::GenericParam::Type(t) => Some(t.ident.to_string()),
            _ => None,
        })
        .collect();

    let mut out = BTreeSet::new();
    for arg in &sig.inputs {
        let syn::FnArg::Typed(pt) = arg else { continue };
        let syn::Pat::Ident(pat) = &*pt.pat else {
            continue;
        };
        if let Some(base) = named_base_type(&pt.ty)
            && !generics.contains(&base)
            && !PATH_LIKE_TYPES.contains(&base.as_str())
        {
            out.insert(pat.ident.to_string());
        }
    }
    out
}

/// The bare name of a type after peeling references, or `None` when the type is
/// not a plain named path (`impl Trait`, a tuple, a slice, a bare generic).
fn named_base_type(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Reference(r) => named_base_type(&r.elem),
        syn::Type::Paren(p) => named_base_type(&p.elem),
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

/// Whether tokenized text uses `ident` as a whole identifier.
///
/// Whole-token rather than substring, so a local named `dir` does not make every
/// later binding whose initializer mentions `directory` look descriptor-shaped.
fn mentions_ident(text: &str, ident: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|t| t == ident)
}

/// An expression's tokens as text, for the descriptor heuristic.
fn quote_tokens(expr: &syn::Expr) -> String {
    use quote::ToTokens as _;
    expr.to_token_stream().to_string()
}

/// An item's attributes, for the variants that can carry a `cfg`.
fn item_attrs(item: &syn::Item) -> Option<&[syn::Attribute]> {
    Some(match item {
        syn::Item::Mod(i) => &i.attrs,
        syn::Item::Fn(i) => &i.attrs,
        syn::Item::Impl(i) => &i.attrs,
        syn::Item::Use(i) => &i.attrs,
        syn::Item::Struct(i) => &i.attrs,
        syn::Item::Enum(i) => &i.attrs,
        syn::Item::Const(i) => &i.attrs,
        syn::Item::Static(i) => &i.attrs,
        syn::Item::Trait(i) => &i.attrs,
        syn::Item::Type(i) => &i.attrs,
        syn::Item::Macro(i) => &i.attrs,
        _ => return None,
    })
}

/// An impl member's attributes, so `#[cfg(test)] fn` inside an `impl` is seen.
fn impl_item_attrs(item: &syn::ImplItem) -> Option<&[syn::Attribute]> {
    Some(match item {
        syn::ImplItem::Fn(i) => &i.attrs,
        syn::ImplItem::Const(i) => &i.attrs,
        syn::ImplItem::Type(i) => &i.attrs,
        syn::ImplItem::Macro(i) => &i.attrs,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed(source: &str) -> Outcome {
        rewrite(source).expect("parse")
    }

    #[test]
    fn file_open_and_create_become_facade_calls() {
        let out = routed(
            "use std::fs::File;\nfn f(p: &str) { let _a = File::open(p); let _b = File::create(p); }\n",
        );
        assert!(out.source.contains("brush_vfs::ambient::open(p)"));
        assert!(out.source.contains("brush_vfs::ambient::create(p)"));
        // The now-unused import is pruned.
        assert!(!out.source.contains("use std::fs::File;"));
    }

    #[test]
    fn a_free_fn_becomes_a_facade_call_in_every_spelling() {
        let bare = routed("use std::fs::metadata;\nfn f(p: &str) { let _ = metadata(p); }\n");
        assert!(bare.source.contains("brush_vfs::ambient::metadata(p)"));
        assert!(!bare.source.contains("use std::fs::metadata;"));

        let module = routed("use std::fs;\nfn f(p: &str) { let _ = fs::read(p); }\n");
        assert!(module.source.contains("brush_vfs::ambient::read(p)"));

        let full = routed("fn f(p: &str) { let _ = std::fs::write(p, b\"x\"); }\n");
        assert!(full.source.contains("brush_vfs::ambient::write(p"));
    }

    #[test]
    fn a_partially_used_import_keeps_the_names_still_needed() {
        // `File` is routed away but `Metadata` is used as a type, so the import
        // shrinks rather than vanishing.
        let out = routed(
            "use std::fs::{File, Metadata};\nfn f(p: &str) -> Option<Metadata> { let _ = File::open(p); None }\n",
        );
        assert!(out.source.contains("brush_vfs::ambient::open(p)"));
        assert!(out.source.contains("use std::fs::Metadata;"));
        assert!(!out.source.contains("File"));
    }

    #[test]
    fn a_filetype_method_is_not_mistaken_for_a_path_method() {
        // The uu_cat false-positive: `ft` is a FileType, `is_dir` is pure, and
        // nothing here reads the filesystem, so there is nothing to route or
        // even report.
        let out = routed(
            "fn f(md: std::fs::Metadata) -> bool { let ft = md.file_type(); ft.is_dir() }\n",
        );
        assert!(out.unhandled.is_empty(), "is_dir on a FileType must not be flagged");
        assert!(!out.changed);
    }

    #[test]
    fn a_test_module_is_left_alone_while_production_is_routed() {
        // D13's health metric is "do upstream's tests still pass". The facade
        // fails closed with no session, so routing a test body guarantees the
        // metric reads failure. Exactly one of these two calls may be rewritten.
        let out = routed(
            "use std::fs::File;\n\
             fn prod(p: &str) { let _ = File::open(p); }\n\
             #[cfg(test)]\n\
             mod tests {\n\
             use std::fs::File;\n\
             #[test]\n\
             fn t() { let _ = File::open(\"fixture\"); }\n\
             }\n",
        );
        assert_eq!(
            out.source.matches("brush_vfs::ambient::open").count(),
            1,
            "production routed, test left alone"
        );
        assert!(
            out.source.contains("File::open(\"fixture\")"),
            "the test body keeps its own File::open"
        );
    }

    #[test]
    fn a_not_test_cfg_is_production_and_still_routes() {
        // `#[cfg(not(test))]` marks production code. Treating it as a test gate
        // would silently stop routing it -- the failure direction that matters.
        let out = routed(
            "#[cfg(not(test))]\nfn prod(p: &str) { let _ = std::fs::read(p); }\n",
        );
        assert!(out.source.contains("brush_vfs::ambient::read(p)"));
    }

    #[test]
    fn a_declared_non_path_receiver_is_reported_not_rewritten() {
        // uucore's perms.rs:499 `dir_fd.read_dir()`: a DirFd is an openat-anchored
        // descriptor that shares a method name with Path and is already confined.
        let out = routed("fn f(dir_fd: &DirFd) { let _ = dir_fd.read_dir(); }\n");
        assert!(
            !out.source.contains("ambient::read_dir"),
            "a DirFd receiver must not be routed as a path"
        );
        assert!(out.unhandled.iter().any(|u| u.contains("non-path")));
    }

    #[test]
    fn a_generic_receiver_keeps_its_rewrite() {
        // `P` is almost always `P: AsRef<Path>`; leaving it unknown preserves the
        // existing receiver-blind behaviour rather than silently under-routing.
        let out = routed("fn f<P: AsRef<Path>>(p: P) { let _ = p.exists(); }\n");
        assert!(out.source.contains("brush_vfs::ambient::exists(&(p))"));
    }

    #[test]
    fn skip_matches_whole_components_only() {
        assert!(path_has_suffix(Path::new("a/features/fsext.rs"), "features/fsext.rs"));
        assert!(path_has_suffix(Path::new("a/mods/os.rs"), "mods/os.rs"));
        // A suffix must not match a partial component: `fs.rs` is not `safe_fs.rs`.
        assert!(!path_has_suffix(Path::new("a/safe_fs.rs"), "fs.rs"));
        assert!(!path_has_suffix(Path::new("a/b.rs"), "z/b.rs"));
    }

    #[test]
    fn a_carve_out_free_fn_is_reported_not_rewritten() {
        // `soft_link` is the last one. `std` deprecated it in favour of
        // `os::unix::fs::symlink`, so a call is worth surfacing rather than
        // quietly routing.
        let out = routed("fn f(p: &str, q: &str) { let _ = std::fs::soft_link(p, q); }\n");
        assert!(!out.source.contains("ambient::soft_link"));
        assert!(out.unhandled.iter().any(|u| u.contains("soft_link")));
    }

    #[test]
    fn the_two_path_operations_are_routed() {
        // `copy` and `hard_link` were 15 of the 26 unrouted sites across the
        // unforked utilities, and `cap_std::fs::Dir` provides both, so they
        // stopped being carve-outs.
        let copy = routed("fn f(p: &str, q: &str) { let _ = std::fs::copy(p, q); }\n");
        assert!(copy.source.contains("brush_vfs::ambient::copy(p, q)"));
        assert!(copy.unhandled.is_empty());

        let link = routed("fn f(p: &str, q: &str) { let _ = std::fs::hard_link(p, q); }\n");
        assert!(link.source.contains("brush_vfs::ambient::hard_link(p, q)"));
        assert!(link.unhandled.is_empty());
    }

    #[test]
    fn the_free_exists_keeps_its_result_type() {
        // std::fs::exists returns io::Result<bool>; Path::exists() returns bool.
        // Pointing both at the same facade name silently changes what the call
        // evaluates to, which uu_cp's `.is_ok_and(identity)` caught by failing
        // to compile.
        let free = routed("fn f(p: &str) -> bool { std::fs::exists(p).is_ok_and(|b| b) }\n");
        assert!(free.source.contains("brush_vfs::ambient::try_exists(p)"));

        let method = routed("fn f(p: &std::path::Path) -> bool { p.exists() }\n");
        assert!(method.source.contains("brush_vfs::ambient::exists(&(p))"));
    }

    #[test]
    fn create_dir_and_set_permissions_are_routed() {
        let mkdir = routed("fn f(p: &str) { let _ = std::fs::create_dir(p); }\n");
        assert!(mkdir.source.contains("brush_vfs::ambient::create_dir(p)"));
        // Still distinct from create_dir_all: `mkdir` without -p must fail on a
        // missing parent, so the two must not collapse into one.
        assert!(!mkdir.source.contains("create_dir_all"));

        let perms = routed("fn f(p: &str, m: std::fs::Permissions) { let _ = std::fs::set_permissions(p, m); }\n");
        assert!(perms.source.contains("brush_vfs::ambient::set_permissions(p, m)"));
    }

    #[test]
    fn canonicalize_is_now_routed_not_a_carve_out() {
        // It moved into the facade as a virtual-path canonicalizer (D4).
        let out = routed("fn f(p: &str) { let _ = std::fs::canonicalize(p); }\n");
        assert!(out.source.contains("brush_vfs::ambient::canonicalize(p)"));
    }

    #[test]
    fn a_distinctive_path_method_is_rewritten_with_a_borrowed_receiver() {
        let out = routed("use std::path::Path;\nfn f(p: &Path) -> bool { p.exists() }\n");
        assert!(out.source.contains("brush_vfs::ambient::exists(&(p))"));
        assert!(out.unhandled.is_empty());
    }

    #[test]
    fn a_method_on_a_complex_receiver_stays_grouped() {
        let out = routed(
            "use std::path::Path;\nfn f(p: &Path) -> std::io::Result<std::path::PathBuf> { p.join(\"x\").canonicalize() }\n",
        );
        assert!(
            out.source.contains("brush_vfs::ambient::canonicalize(&(p.join(\"x\")))"),
            "got: {}",
            out.source
        );
    }

    #[test]
    fn read_dir_as_a_method_is_routed() {
        let out = routed(
            "use std::path::Path;\nfn f(p: &Path) { for _e in p.read_dir().unwrap() {} }\n",
        );
        assert!(out.source.contains("brush_vfs::ambient::read_dir(&(p))"));
    }

    #[test]
    fn a_self_import_binds_the_module_under_fs_not_self() {
        // The uu_wc bug: `use std::fs::{self, File}` binds the module as `fs`,
        // so `fs::metadata(p)` must still be recognized and routed.
        let out = routed(
            "use std::fs::{self, File};\nfn f(p: &str) { let _ = fs::metadata(p); let _ = File::open(p); }\n",
        );
        assert!(
            out.source.contains("brush_vfs::ambient::metadata(p)"),
            "fs::metadata must route, got: {}",
            out.source
        );
        assert!(out.source.contains("brush_vfs::ambient::open(p)"));
    }

    #[test]
    fn a_grouped_std_import_keeps_its_non_fs_siblings() {
        // The uu_wc bug: `File` is routed away, but the grouped import also
        // brings in OsString and PathBuf, which must survive.
        let out = routed(
            "use std::{fs::File, ffi::OsString, path::PathBuf};\nfn f(p: &OsString) -> Option<(PathBuf, File)> { let _ = File::open(p); None }\n",
        );
        assert!(out.source.contains("brush_vfs::ambient::open(p)"));
        assert!(
            out.source.contains("use std::{fs::File, ffi::OsString, path::PathBuf};"),
            "the grouped import must be untouched, got: {}",
            out.source
        );
    }

    #[test]
    fn symlink_metadata_is_routed_as_a_free_call_and_a_method() {
        // Once the facade grew a descriptor-based symlink_metadata, it stopped
        // being a carve-out.
        let free = routed("fn f(p: &str) { let _ = std::fs::symlink_metadata(p); }\n");
        assert!(free.source.contains("brush_vfs::ambient::symlink_metadata(p)"));

        let method = routed(
            "use std::path::Path;\nfn f(p: &Path) { let _ = p.symlink_metadata(); }\n",
        );
        assert!(method.source.contains("brush_vfs::ambient::symlink_metadata(&(p))"));
        assert!(method.unhandled.is_empty());
    }

    #[test]
    fn a_carve_out_free_call_is_still_reported() {
        let out = routed("fn f(p: &str) { let _ = std::fs::soft_link(p, p); }\n");
        assert!(!out.source.contains("ambient::soft_link"));
        assert!(out.unhandled.iter().any(|u| u.contains("soft_link")));
    }
}
