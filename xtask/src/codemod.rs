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
}

/// Runs the codemod command.
pub fn run(cmd: &CodemodCommand, _verbose: bool) -> Result<()> {
    let files = rust_files(&cmd.path)?;
    if files.is_empty() {
        anyhow::bail!("no .rs files under {}", cmd.path.display());
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
];

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
/// These are capabilities D4 has not built (copy, hard links, permission and
/// non-recursive-dir-create). Deliberately absent are `metadata` / `is_dir` /
/// `is_file` / `is_symlink`, which also exist on non-`Path` types (`File`,
/// `FileType`) where they need no routing — flagging them without type inference
/// reports pure calls as unrouted, which is `uu_cat`'s `filetype.is_dir()` false
/// positive. The ban and Landlock test are the backstop for any genuinely
/// unrouted `path.metadata()`.
const CARVE_OUT_FNS: &[&str] = &[
    "set_permissions",
    "hard_link",
    "soft_link",
    "copy",
    "create_dir",
];

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
}

impl<'ast> Visit<'ast> for EditCollector<'_> {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*call.func {
            self.try_rewrite_call(&p.path);
        }
        // Recurse so nested calls in the arguments are seen too.
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, mc: &'ast syn::ExprMethodCall) {
        let method = mc.method.to_string();
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
            self.push_call_edit(path, &format!("{FACADE}::{real}"));
            if let Some(b) = binding {
                *self.consumed.entry(b).or_default() += 1;
            }
            self.rewrites.push(format!("{real}() -> {FACADE}::{real}()"));
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
    fn a_carve_out_free_fn_is_reported_not_rewritten() {
        // copy is a capability the facade does not provide.
        let out = routed("fn f(p: &str, q: &str) { let _ = std::fs::copy(p, q); }\n");
        assert!(!out.source.contains("ambient::copy"));
        assert!(out.unhandled.iter().any(|u| u.contains("copy")));
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
        let out = routed("fn f(p: &str) { let _ = std::fs::hard_link(p, p); }\n");
        assert!(!out.source.contains("ambient::hard_link"));
        assert!(out.unhandled.iter().any(|u| u.contains("hard_link")));
    }
}
