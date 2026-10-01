// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Backend-local dictionary governance. Never do work in an invalidation callback.

use std::cell::Cell;
use std::ffi::CStr;
use std::hash::Hasher;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::storage::layout::AnalysisStamp;
use pgrx::iter::TableIterator;
use pgrx::prelude::*;
use pgrx::{PgRelation, PgTryBuilder};
use siphasher::sip::SipHasher13;

const EMPTY_FINGERPRINT: u64 = 0x6855_a073_6155_f3dd;

static DIRTY: AtomicBool = AtomicBool::new(true);
static WORDS_OID: AtomicU32 = AtomicU32::new(0);
thread_local! {
    static CALLBACKS: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn init() {
    unsafe {
        pg_sys::CacheRegisterRelcacheCallback(Some(invalidate), pg_sys::Datum::from(0usize));
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn invalidate(_arg: pg_sys::Datum, oid: pg_sys::Oid) {
    if oid == pg_sys::InvalidOid || oid.to_u32() == WORDS_OID.load(Ordering::Relaxed) {
        DIRTY.store(true, Ordering::Relaxed);
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn xact(event: pg_sys::XactEvent::Type, _arg: *mut std::ffi::c_void) {
    if matches!(
        event,
        pg_sys::XactEvent::XACT_EVENT_COMMIT
            | pg_sys::XactEvent::XACT_EVENT_ABORT
            | pg_sys::XactEvent::XACT_EVENT_PREPARE
    ) {
        DIRTY.store(true, Ordering::Relaxed);
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn subxact(
    event: pg_sys::SubXactEvent::Type,
    _subid: pg_sys::SubTransactionId,
    _parent: pg_sys::SubTransactionId,
    _arg: *mut std::ffi::c_void,
) {
    if event == pg_sys::SubXactEvent::SUBXACT_EVENT_ABORT_SUB {
        DIRTY.store(true, Ordering::Relaxed);
    }
}

fn register_xact() {
    if !CALLBACKS.replace(true) {
        unsafe {
            pg_sys::RegisterXactCallback(Some(xact), std::ptr::null_mut());
            pg_sys::RegisterSubXactCallback(Some(subxact), std::ptr::null_mut());
        }
    }
}

/// Resolve by extension OID, never by search_path. Keep a relation lock across SPI.
fn words_relation() -> PgRelation {
    unsafe {
        let extension = pg_sys::get_extension_oid(c"stannum".as_ptr(), true);
        if extension == pg_sys::InvalidOid {
            error!("stannum dictionary unavailable; CREATE EXTENSION stannum or upgrade it");
        }
        let schema = pg_sys::get_extension_schema(extension);
        let oid = pg_sys::get_relname_relid(c"jieba_words".as_ptr(), schema);
        if oid == pg_sys::InvalidOid {
            error!("stannum.jieba_words missing; CREATE EXTENSION stannum or upgrade it");
        }
        WORDS_OID.store(oid.to_u32(), Ordering::Relaxed);
        PgRelation::with_lock(oid, pg_sys::AccessShareLock as _)
    }
}

fn qualified(relation: &PgRelation) -> String {
    unsafe {
        let schema = pg_sys::get_namespace_name((*(*relation.as_ptr()).rd_rel).relnamespace);
        CStr::from_ptr(pg_sys::quote_qualified_identifier(
            schema,
            c"jieba_words".as_ptr(),
        ))
        .to_string_lossy()
        .into_owned()
    }
}

/// Only fixed, qualified internal queries run with table-owner access. SQL UDFs
/// remain invoker-run; writers must pass `require_admin` before entering here.
/// The owner is restored even on cancellation or PostgreSQL ERROR.
fn table_access<R>(relation: &PgRelation, work: impl FnOnce() -> R + std::panic::UnwindSafe) -> R {
    unsafe {
        let mut user = pg_sys::InvalidOid;
        let mut context = 0;
        pg_sys::GetUserIdAndSecContext(&mut user, &mut context);
        let owner = (*(*relation.as_ptr()).rd_rel).relowner;
        PgTryBuilder::new(|| {
            pg_sys::SetUserIdAndSecContext(
                owner,
                context
                    | pg_sys::SECURITY_LOCAL_USERID_CHANGE as i32
                    | pg_sys::SECURITY_RESTRICTED_OPERATION as i32,
            );
            work()
        })
        .finally(|| pg_sys::SetUserIdAndSecContext(user, context))
        .execute()
    }
}

type Word = (String, i32, Option<String>);

/// Frozen v1 wire identity: SipHash-1-3, fixed keys, raw UTF-8 tuple ordering.
/// Each row is independently domain separated and length framed (including tag).
fn fingerprint(rows: &mut [Word]) -> u64 {
    rows.sort();
    let mut hash = SipHasher13::new_with_keys(0x7374616e6e756d31, 0x6a69656261646963);
    hash.write(b"stannum.jieba.dict.v1\0");
    for (word, freq, tag) in rows {
        hash.write(b"row\0");
        hash.write(&(word.len() as u32).to_le_bytes());
        hash.write(word.as_bytes());
        hash.write(&(*freq as u32).to_le_bytes());
        hash.write(&[u8::from(tag.is_some())]);
        if let Some(tag) = tag {
            hash.write(&(tag.len() as u32).to_le_bytes());
            hash.write(tag.as_bytes());
        }
    }
    // Zero is WI-1's embedded-only sentinel, never a table identity.
    match hash.finish() {
        0 => 1,
        value => value,
    }
}

pub(crate) fn ensure_current() -> u64 {
    reload(false)
}

/// Clear the pending flag before loading so an invalidation delivered during
/// SPI remains pending for the next use. Failure always restores dirtiness.
struct ReloadGuard {
    complete: bool,
}

impl Drop for ReloadGuard {
    fn drop(&mut self) {
        if !self.complete {
            DIRTY.store(true, Ordering::Relaxed);
        }
    }
}

fn reload(force: bool) -> u64 {
    // Parallel workers and startup cannot safely enter SPI. Step 3.2 will make
    // the leader decline worker paths for nonempty custom dictionaries.
    if unsafe { pg_sys::ParallelWorkerNumber >= 0 } {
        if tokenizer::jieba_current_fingerprint() == 0 {
            tokenizer::jieba_install_with_fingerprint(&[], EMPTY_FINGERPRINT);
            crate::storage::evict_jieba_tokenizers_except(EMPTY_FINGERPRINT);
        }
        return tokenizer::jieba_current_fingerprint();
    }
    if unsafe { !pg_sys::IsTransactionState() } {
        return tokenizer::jieba_current_fingerprint();
    }
    register_xact();
    if !force && !DIRTY.load(Ordering::Relaxed) {
        return tokenizer::jieba_current_fingerprint();
    }

    let mut guard = ReloadGuard { complete: false };
    DIRTY.store(false, Ordering::Relaxed);
    let relation = words_relation();
    let query = format!(
        "SELECT word, freq, tag FROM {} ORDER BY word",
        qualified(&relation)
    );
    let mut rows = table_access(&relation, || {
        Spi::connect(|client| {
            client
                .select(&query, None, &[])
                .expect("read jieba_words")
                .map(|row| {
                    pgrx::check_for_interrupts!();
                    (
                        row["word"].value::<String>().unwrap().unwrap(),
                        row["freq"].value::<i32>().unwrap().unwrap(),
                        row["tag"].value::<String>().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        })
    });
    let fingerprint = fingerprint(&mut rows);
    if force || fingerprint != tokenizer::jieba_current_fingerprint() {
        let words: Vec<_> = rows
            .iter()
            .map(|(word, freq, tag)| {
                (
                    word.as_str(),
                    (*freq > 0).then_some(*freq as usize),
                    tag.as_deref(),
                )
            })
            .collect();
        pgrx::check_for_interrupts!();
        tokenizer::jieba_install_with_fingerprint(&words, fingerprint);
        crate::storage::evict_jieba_tokenizers_except(fingerprint);
    }
    guard.complete = true;
    fingerprint
}

fn require_admin() {
    unsafe {
        if !pg_sys::superuser()
            && !pg_sys::has_privs_of_role(
                pg_sys::GetUserId(),
                pg_sys::get_role_oid(c"pg_database_owner".as_ptr(), false),
            )
        {
            error!("stannum dictionary changes require superuser or pg_database_owner membership");
        }
    }
}

fn validate_word(word: &str) {
    if word.trim().is_empty() || word.len() > 256 || word.chars().any(char::is_whitespace) {
        error!(
            "dictionary word must be nonempty, at most 256 UTF-8 bytes, and contain no Unicode whitespace"
        );
    }
}

fn changed(oid: pg_sys::Oid) {
    DIRTY.store(true, Ordering::Relaxed);
    unsafe {
        pg_sys::CommandCounterIncrement();
        pg_sys::CacheInvalidateRelcacheByRelid(oid);
    }
    ensure_current();
}

pgrx::extension_sql!(
    r#"
CREATE TABLE @extschema@.jieba_words (
    word text PRIMARY KEY,
    freq integer NOT NULL DEFAULT 0 CHECK (freq >= 0),
    tag text
);
REVOKE ALL ON TABLE @extschema@.jieba_words FROM PUBLIC;
"#,
    name = "jieba_words"
);

#[pg_extern(volatile, parallel_unsafe)]
fn jieba_add_word(word: &str, freq: default!(i32, 0), tag: default!(Option<&str>, "NULL")) {
    require_admin();
    validate_word(word);
    if freq < 0 {
        error!("dictionary frequency must be non-negative");
    }
    register_xact();
    let relation = words_relation();
    let query = format!(
        "INSERT INTO {} (word, freq, tag) VALUES ($1, $2, $3) ON CONFLICT (word) DO UPDATE SET freq = EXCLUDED.freq, tag = EXCLUDED.tag",
        qualified(&relation)
    );
    table_access(&relation, || {
        Spi::connect_mut(|client| {
            client
                .update(&query, None, &[word.into(), freq.into(), tag.into()])
                .expect("update jieba_words");
        })
    });
    changed(relation.oid());
}

#[pg_extern(volatile, parallel_unsafe)]
fn jieba_delete_word(word: &str) {
    require_admin();
    validate_word(word);
    register_xact();
    let relation = words_relation();
    let query = format!(
        "DELETE FROM {} WHERE word OPERATOR(pg_catalog.=) $1",
        qualified(&relation)
    );
    table_access(&relation, || {
        Spi::connect_mut(|client| {
            client
                .update(&query, None, &[word.into()])
                .expect("delete jieba_words");
        })
    });
    changed(relation.oid());
}

#[pg_extern(stable, parallel_unsafe)]
fn jieba_dict_version() -> i64 {
    ensure_current() as i64
}

#[pg_extern(volatile, parallel_unsafe)]
fn jieba_reload_dict() {
    require_admin();
    reload(true);
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AnalysisStatus {
    NotApplicable,
    Match,
    MissingStamp,
    JiebaVersionDrift,
    DictionaryDrift,
    BothDrift,
}

impl AnalysisStatus {
    fn text(&self) -> &'static str {
        match self {
            Self::NotApplicable => "not applicable",
            Self::Match => "matches",
            Self::MissingStamp => "missing analysis stamp; REINDEX required",
            Self::JiebaVersionDrift => "jieba version drift; REINDEX required",
            Self::DictionaryDrift => "dictionary drift; REINDEX required",
            Self::BothDrift => "jieba version and dictionary drift; REINDEX required",
        }
    }
}

fn status(recorded: Option<AnalysisStamp>, runtime: Option<AnalysisStamp>) -> AnalysisStatus {
    let Some(runtime) = runtime else {
        return AnalysisStatus::NotApplicable;
    };
    let Some(recorded) = recorded else {
        return AnalysisStatus::MissingStamp;
    };
    match (
        recorded.jieba_rs_version == runtime.jieba_rs_version,
        recorded.dict_fingerprint == runtime.dict_fingerprint,
    ) {
        (true, true) => AnalysisStatus::Match,
        (true, false) => AnalysisStatus::DictionaryDrift,
        (false, true) => AnalysisStatus::JiebaVersionDrift,
        (false, false) => AnalysisStatus::BothDrift,
    }
}

pub(crate) fn stamp(spec: &[u8; crate::options::SPEC_BYTES]) -> Option<AnalysisStamp> {
    crate::options::decode_spec(spec)
        .is_some_and(|spec| spec.tokenizer == tokenizer::TokenizerSpec::Jieba)
        .then(|| AnalysisStamp {
            jieba_rs_version: tokenizer::JIEBA_RS_VERSION,
            dict_fingerprint: ensure_current(),
        })
}

#[allow(clippy::type_complexity)] // the SRF row shape is fixed public SQL surface
pub(crate) fn index_analysis(
    index: PgRelation,
) -> TableIterator<
    'static,
    (
        name!(index_name, String),
        name!(recorded_jieba_version, Option<i32>),
        name!(recorded_dict_fingerprint, Option<i64>),
        name!(runtime_jieba_version, Option<i32>),
        name!(runtime_dict_fingerprint, Option<i64>),
        name!(matches, Option<bool>),
        name!(status, String),
    ),
> {
    crate::udfs::require_stannum_index(&index, "index_analysis");
    let spec = unsafe { crate::storage::index_spec(index.as_ptr()) };
    let runtime = stamp(&spec);
    let recorded = if runtime.is_some() && unsafe { crate::storage::present(index.as_ptr()) } {
        unsafe { crate::storage::analysis_meta(index.as_ptr()) }.analysis
    } else {
        None
    };
    let state = status(recorded, runtime);
    TableIterator::once((
        index.name().to_string(),
        recorded.map(|s| s.jieba_rs_version as i32),
        recorded.map(|s| s.dict_fingerprint as i64),
        runtime.map(|s| s.jieba_rs_version as i32),
        runtime.map(|s| s.dict_fingerprint as i64),
        runtime.map(|_| state == AnalysisStatus::Match),
        state.text().to_owned(),
    ))
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    #[test]
    fn stable_framed_fingerprint() {
        let a = ("词".into(), 0, None);
        let b = ("词语".into(), 42, Some("n".into()));
        assert_eq!(
            fingerprint(&mut [a.clone(), b.clone()]),
            fingerprint(&mut [b.clone(), a.clone()])
        );
        // Frozen v1 vector; changing keys/framing requires an identity version.
        assert_eq!(fingerprint(&mut []), EMPTY_FINGERPRINT);
        assert_ne!(
            fingerprint(&mut [a.clone()]),
            fingerprint(&mut [(a.0, 0, Some("".into()))])
        );
    }
    #[test]
    fn drift_matrix() {
        let s = AnalysisStamp {
            jieba_rs_version: 7,
            dict_fingerprint: 42,
        };
        assert_eq!(status(None, None), AnalysisStatus::NotApplicable);
        assert_eq!(status(Some(s), None), AnalysisStatus::NotApplicable);
        assert_eq!(status(None, Some(s)), AnalysisStatus::MissingStamp);
        assert_eq!(status(Some(s), Some(s)), AnalysisStatus::Match);
        assert_eq!(
            status(
                Some(s),
                Some(AnalysisStamp {
                    dict_fingerprint: 43,
                    ..s
                })
            ),
            AnalysisStatus::DictionaryDrift
        );
        assert_eq!(
            status(
                Some(s),
                Some(AnalysisStamp {
                    jieba_rs_version: 8,
                    ..s
                })
            ),
            AnalysisStatus::JiebaVersionDrift
        );
        assert_eq!(
            status(
                Some(s),
                Some(AnalysisStamp {
                    jieba_rs_version: 8,
                    dict_fingerprint: 43
                })
            ),
            AnalysisStatus::BothDrift
        );
    }
}
