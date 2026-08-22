//! What the user has consented to grant (D29).
//!
//! # Keyed by the granted set, not by the path or the manifest
//!
//! D29 rejects both obvious keys. **Path-keyed** consent lets a different
//! repository cloned to the same path inherit the grant. **Hashing the
//! manifest** re-prompts on edits that changed nothing about what is granted.
//! Storing the granted set itself means a request that asks for *less* than
//! something already granted is accepted without asking — so narrowing never
//! costs a prompt, which is what keeps prompts rare enough to still mean
//! something when one appears.
//!
//! # There is no approve-all flag
//!
//! Deliberately, and D29 says why: it would become a copy-pasted line in every
//! pipeline, on exactly the machines where the boundary matters most. A
//! non-interactive run that needs more than it was granted fails, and the
//! failure names the flag that would grant it.

use std::path::{Path, PathBuf};

use brush_vfs::Access;

/// One mount, as consent records it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct GrantedMount {
    /// Where it appears in the namespace.
    pub at: String,
    /// The host directory behind it.
    pub host: PathBuf,
    /// Whether it is writable.
    pub writable: bool,
}

impl GrantedMount {
    /// The `--mount` flag that would grant exactly this.
    #[must_use]
    pub fn as_flag(&self) -> String {
        let mode = if self.writable { "rw" } else { "ro" };
        format!("--mount {}:{}:{mode}", self.at, self.host.display())
    }
}

/// A set of mounts, as consent records it.
///
/// Always sorted, so two requests that differ only in order are one set. A
/// subset check that depended on ordering would prompt on a reshuffle, which is
/// the re-prompting D29 is trying to avoid.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GrantedSet {
    mounts: Vec<GrantedMount>,
}

impl GrantedSet {
    /// Builds a set from mounts in any order.
    #[must_use]
    pub fn new(mounts: impl IntoIterator<Item = GrantedMount>) -> Self {
        let mut mounts: Vec<_> = mounts.into_iter().collect();
        mounts.sort();
        mounts.dedup();
        Self { mounts }
    }

    /// The mounts, sorted.
    #[must_use]
    pub fn mounts(&self) -> &[GrantedMount] {
        &self.mounts
    }

    /// Whether everything here was already granted by `other`.
    ///
    /// A read-only mount is covered by a read-write grant of the same
    /// directory: asking for *less* access than was granted is narrowing, and
    /// narrowing never re-asks.
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        self.mounts.iter().all(|wanted| {
            other.mounts.iter().any(|granted| {
                granted.at == wanted.at
                    && granted.host == wanted.host
                    && (granted.writable || !wanted.writable)
            })
        })
    }

    /// What this set asks for beyond `other`.
    #[must_use]
    pub fn beyond(&self, other: &Self) -> Vec<&GrantedMount> {
        self.mounts
            .iter()
            .filter(|wanted| !Self::new([(*wanted).clone()]).is_subset_of(other))
            .collect()
    }
}

/// What to do about a request.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Already granted, or a narrowing of something already granted.
    Accept,
    /// Asks for more than anything on record. The mounts listed are the excess.
    Ask(Vec<GrantedMount>),
}

/// Which execution tier a grant is consent for (D17, D19).
///
/// The two tiers run the same Roc code with the same capability (D19), and
/// differ only in the trust they demand: [`Wasm`](Tier::Wasm) runs the guest
/// behind a real boundary, so the Roc compiler is not in the trusted base;
/// [`Native`](Tier::Native) runs it as host code in the same address space,
/// with the whole compiler -- and D17's documented miscompile -- inside the TCB.
///
/// So consent is **not symmetric**. Granting native trust covers a later wasm
/// run of the same mounts -- the user already accepted the riskier tier. The
/// reverse is refused: consent to a sandboxed wasm run is not consent to run the
/// same code natively, because that is the more dangerous thing D19 exists to
/// keep from becoming the silent default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Tier {
    /// Sandboxed: the guest runs behind a wasm boundary. The least-trust tier,
    /// and the default for an unlabelled record -- an older store, written
    /// before tiers existed, grants only this, so a native run re-prompts
    /// rather than inheriting a consent that predates the distinction.
    #[default]
    Wasm,
    /// Trusted: the guest runs as native host code. Requires its own consent.
    Native,
}

impl Tier {
    /// Whether consent at this tier covers a request at `other`.
    ///
    /// Native covers both; wasm covers only wasm. The asymmetry is the whole
    /// point -- see the type docs.
    #[must_use]
    pub const fn covers(self, other: Self) -> bool {
        matches!((self, other), (Self::Native, _) | (Self::Wasm, Self::Wasm))
    }
}

/// A granted set together with the tier it was consented for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GrantedRecord {
    /// The mounts consented to.
    #[serde(flatten)]
    pub set: GrantedSet,
    /// The tier the consent was for. Defaults to the least-trust tier for a
    /// record written before tiers were recorded.
    #[serde(default)]
    pub tier: Tier,
}

/// Everything the user has consented to on this machine.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TrustStore {
    #[serde(default)]
    granted: Vec<GrantedRecord>,
}

impl TrustStore {
    /// Reads the store, or an empty one if it has never been written.
    ///
    /// A store that cannot be parsed is treated as empty rather than as an
    /// error: the failure direction is an extra prompt, where treating it as an
    /// error would make a corrupt file lock the user out of their own projects.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        // The launcher's own state, read before there is a namespace to ask --
        // the same exemption discovery and config loading carry.
        #[expect(
            clippy::disallowed_methods,
            reason = "the trust store is read before any namespace exists"
        )]
        let text = std::fs::read_to_string(path).unwrap_or_default();
        toml::from_str(&text).unwrap_or_default()
    }

    /// Writes the store.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the file cannot be written.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        #[expect(
            clippy::disallowed_methods,
            reason = "the trust store is written before any namespace exists"
        )]
        {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, text)
        }
    }

    /// Whether `request` at `tier` is covered by something already granted.
    ///
    /// A stored record covers the request only if its mounts are a superset
    /// *and* its tier covers the requested tier ([`Tier::covers`]). So a native
    /// request is never satisfied by a wasm-only grant, however broad the
    /// mounts -- the tier is part of what was consented to, not a mode on top of
    /// it.
    #[must_use]
    pub fn decide(&self, request: &GrantedSet, tier: Tier) -> Decision {
        if self
            .granted
            .iter()
            .any(|g| g.tier.covers(tier) && request.is_subset_of(&g.set))
        {
            return Decision::Accept;
        }
        // The excess is reported against the closest stored set *of a tier that
        // would cover this request*, so the message names what is actually new.
        // A run refused only because its tier escalates reports its whole set,
        // since no covering-tier record exists to diff against.
        let closest = self
            .granted
            .iter()
            .filter(|g| g.tier.covers(tier))
            .min_by_key(|g| request.beyond(&g.set).len());
        let excess = closest.map_or_else(
            || request.mounts().to_vec(),
            |g| request.beyond(&g.set).into_iter().cloned().collect(),
        );
        Decision::Ask(excess)
    }

    /// Records a set as granted at `tier`.
    ///
    /// A record already covering this one -- superset mounts at a covering tier
    /// -- means nothing is added. Records this one now covers are dropped, so
    /// recording a native grant supersedes a narrower wasm one but not the other
    /// way around.
    pub fn record(&mut self, set: GrantedSet, tier: Tier) {
        if self
            .granted
            .iter()
            .any(|g| g.tier.covers(tier) && set.is_subset_of(&g.set))
        {
            return;
        }
        self.granted
            .retain(|g| !(tier.covers(g.tier) && g.set.is_subset_of(&set)));
        self.granted.push(GrantedRecord { set, tier });
    }
}

/// The message a non-interactive run fails with.
///
/// Names the exact flags rather than describing them, because the person
/// reading it is looking at CI output and cannot be asked anything.
#[must_use]
pub fn refusal(excess: &[GrantedMount]) -> String {
    let flags: Vec<String> = excess.iter().map(GrantedMount::as_flag).collect();
    format!(
        "this project asks for access that has not been granted, and there is no \
         terminal to ask on. Grant it explicitly with:\n    {}",
        flags.join("\n    ")
    )
}

/// Converts a mount table into the set consent records.
#[must_use]
pub fn granted_set(mounts: &brush_vfs::MountTable) -> GrantedSet {
    GrantedSet::new(mounts.mounts().filter_map(|m| {
        Some(GrantedMount {
            at: m.mount_point().as_str().to_owned(),
            host: m.host_path()?.to_path_buf(),
            writable: m.access() == Access::ReadWrite,
        })
    }))
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]
mod tests {
    use super::*;

    // The mount-subset tests predate tiers and are tier-agnostic, so they run
    // at one tier. The tier *asymmetry* has its own cases below.
    fn record_native(store: &mut TrustStore, set: GrantedSet) {
        store.record(set, Tier::Native);
    }
    fn decide_native(store: &TrustStore, set: &GrantedSet) -> Decision {
        store.decide(set, Tier::Native)
    }

    fn mount(at: &str, host: &str, writable: bool) -> GrantedMount {
        GrantedMount {
            at: at.to_owned(),
            host: PathBuf::from(host),
            writable,
        }
    }

    #[test]
    fn narrowing_never_re_asks() {
        // The property the whole keying choice exists for.
        let mut store = TrustStore::default();
        record_native(
            &mut store,
            GrantedSet::new([mount("/work", "/p", true), mount("/home/user", "/h", true)]),
        );

        let narrower = GrantedSet::new([mount("/work", "/p", true)]);
        assert_eq!(decide_native(&store, &narrower), Decision::Accept);

        // Less *access*, not just fewer mounts, is also narrowing.
        let read_only = GrantedSet::new([mount("/work", "/p", false)]);
        assert_eq!(decide_native(&store, &read_only), Decision::Accept);
    }

    #[test]
    fn a_superset_asks_and_names_only_what_is_new() {
        let mut store = TrustStore::default();
        record_native(&mut store, GrantedSet::new([mount("/work", "/p", true)]));

        let wider = GrantedSet::new([mount("/work", "/p", true), mount("/extra", "/e", true)]);
        match decide_native(&store, &wider) {
            Decision::Ask(excess) => assert_eq!(excess, vec![mount("/extra", "/e", true)]),
            Decision::Accept => unreachable!("a superset must ask"),
        }
    }

    #[test]
    fn more_access_to_the_same_directory_is_a_superset() {
        // The case a naive set comparison gets wrong: same mount point, same
        // host directory, and strictly more authority.
        let mut store = TrustStore::default();
        record_native(&mut store, GrantedSet::new([mount("/work", "/p", false)]));

        let writable = GrantedSet::new([mount("/work", "/p", true)]);
        assert!(matches!(decide_native(&store, &writable), Decision::Ask(_)));
    }

    #[test]
    fn a_different_repository_at_the_same_mount_point_asks() {
        // Why consent is not path-keyed: a different checkout behind the same
        // virtual path is a different grant.
        let mut store = TrustStore::default();
        record_native(&mut store, GrantedSet::new([mount("/work", "/one", true)]));
        let other = GrantedSet::new([mount("/work", "/two", true)]);
        assert!(matches!(decide_native(&store, &other), Decision::Ask(_)));
    }

    #[test]
    fn order_does_not_make_two_requests_different() {
        let a = GrantedSet::new([mount("/a", "/a", true), mount("/b", "/b", true)]);
        let b = GrantedSet::new([mount("/b", "/b", true), mount("/a", "/a", true)]);
        assert_eq!(a, b);
    }

    #[test]
    fn the_store_does_not_grow_on_repeated_narrowing() {
        let mut store = TrustStore::default();
        let wide = GrantedSet::new([mount("/work", "/p", true), mount("/extra", "/e", true)]);
        record_native(&mut store, wide);
        record_native(&mut store, GrantedSet::new([mount("/work", "/p", true)]));
        assert_eq!(store.granted.len(), 1);
    }

    #[test]
    fn recording_a_superset_replaces_what_it_covers() {
        let mut store = TrustStore::default();
        record_native(&mut store, GrantedSet::new([mount("/work", "/p", true)]));
        record_native(
            &mut store,
            GrantedSet::new([mount("/work", "/p", true), mount("/extra", "/e", true)]),
        );
        assert_eq!(store.granted.len(), 1, "the narrower set is subsumed");
    }

    #[test]
    fn the_refusal_names_the_flag_that_would_grant_it() {
        // A CI log is the only thing the reader has, so the message has to be
        // copy-pasteable rather than descriptive.
        let message = refusal(&[mount("/extra", "/e", false)]);
        assert!(message.contains("--mount /extra:/e:ro"), "{message}");
    }

    #[test]
    fn a_corrupt_store_reads_as_empty_rather_than_failing() {
        // The failure direction is an extra prompt. Treating it as an error
        // would let a corrupt file lock a user out of their own projects.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("trust.toml");
        std::fs::write(&path, b"this is not toml {{{").expect("write");
        assert_eq!(TrustStore::load(&path).granted.len(), 0);
    }

    #[test]
    fn a_saved_store_reads_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("sub").join("trust.toml");
        let mut store = TrustStore::default();
        record_native(&mut store, GrantedSet::new([mount("/work", "/p", true)]));
        store.save(&path).expect("saves");

        let loaded = TrustStore::load(&path);
        assert_eq!(
            decide_native(&loaded, &GrantedSet::new([mount("/work", "/p", true)])),
            Decision::Accept
        );
    }

    #[test]
    fn native_consent_covers_a_later_wasm_run() {
        // The user accepted the riskier tier, so the safer one is already
        // covered -- no second prompt for running the same code sandboxed.
        let mut store = TrustStore::default();
        let set = GrantedSet::new([mount("/work", "/p", true)]);
        store.record(set.clone(), Tier::Native);
        assert_eq!(store.decide(&set, Tier::Wasm), Decision::Accept);
        assert_eq!(store.decide(&set, Tier::Native), Decision::Accept);
    }

    #[test]
    fn wasm_consent_does_not_escalate_to_native() {
        // The asymmetry that is the point of D19: consenting to a sandboxed run
        // is not consenting to run the same code natively, in the same address
        // space as the vfs. A native request re-prompts even though the mounts
        // are identical.
        let mut store = TrustStore::default();
        let set = GrantedSet::new([mount("/work", "/p", true)]);
        store.record(set.clone(), Tier::Wasm);
        assert_eq!(store.decide(&set, Tier::Wasm), Decision::Accept);
        assert!(
            matches!(store.decide(&set, Tier::Native), Decision::Ask(_)),
            "wasm consent must not grant a native run"
        );
    }

    #[test]
    fn an_unlabelled_stored_record_grants_only_wasm() {
        // A store written before tiers existed has no tier field; it must
        // default to the least-trust tier, so a native run re-prompts rather
        // than inheriting a consent that predates the distinction.
        let toml = "[[granted]]\nmounts = [{ at = \"/work\", host = \"/p\", writable = true }]\n";
        let store: TrustStore = toml::from_str(toml).expect("parses");
        let set = GrantedSet::new([mount("/work", "/p", true)]);
        assert_eq!(store.decide(&set, Tier::Wasm), Decision::Accept);
        assert!(
            matches!(store.decide(&set, Tier::Native), Decision::Ask(_)),
            "an untiered record must not silently grant native"
        );
    }

    #[test]
    fn a_native_grant_supersedes_a_narrower_wasm_one() {
        let mut store = TrustStore::default();
        record_native(&mut store, GrantedSet::new([mount("/work", "/p", true)]));
        // A wasm grant that a native record already covers is not added.
        store.record(GrantedSet::new([mount("/work", "/p", true)]), Tier::Wasm);
        assert_eq!(
            store.granted.len(),
            1,
            "the native record already covers it"
        );
    }
}
