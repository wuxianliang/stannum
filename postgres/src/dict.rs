// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Backend-local dictionary governance. Never do work in an invalidation callback.
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::CStr;
use std::hash::Hasher;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::storage::layout::{AnalysisStamp, Meta};
use pgrx::prelude::*;
use pgrx::{PgRelation, PgTryBuilder};
use siphasher::sip::SipHasher13;

const EMPTY_FINGERPRINT: u64 = 0x6855_a073_6155_f3dd;

static DIRTY: AtomicBool = AtomicBool::new(true);
static WORDS_OID: AtomicU32 = AtomicU32::new(0);
thread_local! {
    static INTERNAL_SPI: Cell<bool> = const { Cell::new(false) };
    static CALLBACKS: Cell<bool> = const { Cell::new(false) };
    static SEEN: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
    static NONEMPTY: Cell<bool> = const { Cell::new(false) };
    static WARNED: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
}

pub(crate) fn internal_spi() -> bool {
    INTERNAL_SPI.get()
}
pub(crate) fn note_executor_start() {
    SEEN.set(None);
    WARNED.with_borrow_mut(HashSet::clear);
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
        // Also discard snapshot-specific reads (e.g. a REPEATABLE READ reader).
        DIRTY.store(true, Ordering::Relaxed);
        SEEN.set(None);
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
        SEEN.set(None);
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
            error!("stannum.jieba_words missing; CREATE EXTENSION stannum or upgrade to 0.2.0");
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
/// remain invoker-run; writers must pass require_admin BEFORE entering here.
/// The owner and hook guard are restored even on cancellation / PostgreSQL ERROR.
fn table_access<R>(relation: &PgRelation, work: impl FnOnce() -> R + std::panic::UnwindSafe) -> R {
    unsafe {
        let mut user = pg_sys::InvalidOid;
        let mut context = 0;
        pg_sys::GetUserIdAndSecContext(&mut user, &mut context);
        let owner = (*(*relation.as_ptr()).rd_rel).relowner;
        let internal = INTERNAL_SPI.replace(true);
        PgTryBuilder::new(|| {
            pg_sys::SetUserIdAndSecContext(
                owner,
                context
                    | pg_sys::SECURITY_LOCAL_USERID_CHANGE as i32
                    | pg_sys::SECURITY_RESTRICTED_OPERATION as i32,
            );
            work()
        })
        .finally(|| {
            pg_sys::SetUserIdAndSecContext(user, context);
            INTERNAL_SPI.set(internal);
        })
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

/// Clear the pending flag before loading so an invalidation delivered DURING
/// SPI is retained for the next statement. Failure always restores dirtiness.
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
    // Workers/startup cannot safely enter SPI; custom paths with custom words
    // are declined by the leader. Standbys DO load their WAL-visible table.
    if unsafe { pg_sys::ParallelWorkerNumber >= 0 } {
        // Only empty-custom-dictionary paths may reach a worker. Give its
        // embedded snapshot the empty TABLE identity, not the startup sentinel,
        // so a stamped empty index also matches under strict_analysis.
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
    let statement = crate::score::current_statement();
    if !force {
        if let Some((seen, fp)) = SEEN.get()
            && seen == statement
        {
            return fp;
        }
        if !DIRTY.load(Ordering::Relaxed) {
            let fp = tokenizer::jieba_current_fingerprint();
            SEEN.set(Some((statement, fp)));
            return fp;
        }
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
    let fp = fingerprint(&mut rows);
    if force || fp != tokenizer::jieba_current_fingerprint() {
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
        tokenizer::jieba_install_with_fingerprint(&words, fp);
        crate::storage::evict_jieba_tokenizers_except(fp);
    }
    NONEMPTY.set(!rows.is_empty());
    SEEN.set(Some((statement, fp)));
    guard.complete = true;
    fp
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
    SEEN.set(None);
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

pub(crate) fn stamp(spec: &[u8; crate::options::SPEC_BYTES]) -> Option<AnalysisStamp> {
    is_jieba(spec).then(|| AnalysisStamp {
        jieba_rs_version: tokenizer::JIEBA_RS_VERSION,
        dict_fingerprint: ensure_current(),
    })
}
fn is_jieba(spec: &[u8; crate::options::SPEC_BYTES]) -> bool {
    crate::options::decode_spec(spec)
        .is_some_and(|s| s.tokenizer == tokenizer::TokenizerSpec::Jieba)
}
pub(crate) unsafe fn parallel_safe(index: pg_sys::Oid, root: *mut pg_sys::PlannerInfo) -> bool {
    let spec = unsafe { crate::storage::spec_by_oid(index) };
    if !is_jieba(&spec) {
        return true;
    }
    ensure_current();
    if !root.is_null() {
        // Cached plans must reconsider worker eligibility after dictionary DML.
        // The SPI query's dependencies belong to its own plan, not this one.
        unsafe {
            let global = (*root).glob;
            (*global).relationOids = pg_sys::list_append_unique_oid(
                (*global).relationOids,
                pg_sys::Oid::from(WORDS_OID.load(Ordering::Relaxed)),
            );
        }
    }
    !NONEMPTY.get()
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
/// The one-line analysis identity for EXPLAIN properties and
/// `index_stats`: the recorded jieba version and dictionary fingerprint
/// with their drift status, as `(matches, text)`. `None` when the index
/// has no analysis stamp or does not use the jieba tokenizer. Unlike
/// [`check_analysis`] this never warns and never consults
/// `strict_analysis`, so diagnostics can state drift without policing it.
pub(crate) fn analysis_summary(meta: &Meta) -> Option<(bool, String)> {
    let recorded = meta.analysis?;
    let runtime = stamp(&meta.spec)?;
    let matches = recorded.jieba_rs_version == runtime.jieba_rs_version
        && recorded.dict_fingerprint == runtime.dict_fingerprint;
    let version = recorded.jieba_rs_version;
    let text = format!(
        "jieba {}.{}.{} / dict {:016x} / {}",
        version >> 16,
        (version >> 8) & 0xff,
        version & 0xff,
        recorded.dict_fingerprint,
        if matches { "matches" } else { "drift" }
    );
    Some((matches, text))
}

pub(crate) fn check_analysis(index_oid: pg_sys::Oid, meta: &Meta) {
    let state = status(meta.analysis, stamp(&meta.spec));
    if matches!(state, AnalysisStatus::NotApplicable | AnalysisStatus::Match) {
        return;
    }
    let name = unsafe { CStr::from_ptr(pg_sys::get_rel_name(index_oid)).to_string_lossy() };
    if state != AnalysisStatus::MissingStamp && crate::storage::STRICT_ANALYSIS.get() {
        error!("stannum index {name}: {}", state.text());
    }
    if WARNED.with_borrow_mut(|set| set.insert(index_oid.to_u32())) {
        warning!("stannum index {name}: {}", state.text());
    }
}
#[pg_extern(stable, parallel_unsafe, strict)]
#[allow(clippy::type_complexity)] // the SRF row shape is fixed public SQL surface
fn index_analysis(
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

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use super::*;

    #[pg_test]
    fn word_byte_boundaries_and_validation_errors_leave_rows_unchanged() {
        // Both words are exactly 256 UTF-8 bytes but very different char counts.
        let ascii = "x".repeat(256);
        let unicode = format!("{}x", "词".repeat(85));
        assert_eq!(unicode.len(), 256);
        jieba_add_word(&ascii, 0, None);
        jieba_add_word(&unicode, i32::MAX, Some("boundary"));
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM stannum.jieba_words WHERE octet_length(word) = 256"
            )
            .unwrap(),
            Some(2)
        );
        let before = ensure_current();
        for (word, freq, expected) in [
            (
                format!("{unicode}x"),
                0,
                "dictionary word must be nonempty, at most 256 UTF-8 bytes, and contain no Unicode whitespace",
            ),
            (
                "\u{2003}\t\n".to_owned(),
                0,
                "dictionary word must be nonempty, at most 256 UTF-8 bytes, and contain no Unicode whitespace",
            ),
            (
                "valid\u{00a0}word".to_owned(),
                0,
                "dictionary word must be nonempty, at most 256 UTF-8 bytes, and contain no Unicode whitespace",
            ),
            // Updating an existing word must not clobber its old frequency/tag.
            (
                unicode.clone(),
                -1,
                "dictionary frequency must be non-negative",
            ),
        ] {
            Spi::run(&format!(
                "DO $test$ DECLARE before_rows jsonb; caught text; BEGIN
                   SELECT jsonb_agg(to_jsonb(w) ORDER BY word) INTO before_rows
                     FROM stannum.jieba_words w;
                   BEGIN PERFORM stannum.jieba_add_word('{word}', {freq}, 'changed');
                   EXCEPTION WHEN OTHERS THEN caught := SQLERRM; END;
                   IF caught IS DISTINCT FROM '{expected}' THEN
                     RAISE EXCEPTION 'unexpected validation result: %', caught;
                   END IF;
                   IF before_rows IS DISTINCT FROM
                     (SELECT jsonb_agg(to_jsonb(w) ORDER BY word) FROM stannum.jieba_words w) THEN
                     RAISE EXCEPTION 'failed validation mutated dictionary';
                   END IF;
                 END $test$;"
            ))
            .unwrap();
            assert_eq!(ensure_current(), before);
        }
    }

    #[pg_test]
    fn dictionary_admin_check_precedes_private_relation_access() {
        // Removing the expected relation name makes any premature dictionary
        // lookup fail distinctly. Even malformed arguments must hit admin first.
        Spi::run(
            "CREATE ROLE hardening_dict_reader;
             GRANT USAGE ON SCHEMA stannum TO hardening_dict_reader;
             ALTER TABLE stannum.jieba_words RENAME TO hardening_saved_words;
             SET LOCAL ROLE hardening_dict_reader;",
        )
        .unwrap();
        for call in [
            "jieba_add_word('', -1)",
            "jieba_delete_word('')",
            "jieba_reload_dict()",
        ] {
            Spi::run(&format!(
                "DO $test$ DECLARE caught text; BEGIN
                   BEGIN PERFORM stannum.{call};
                   EXCEPTION WHEN OTHERS THEN caught := SQLERRM; END;
                   IF caught IS DISTINCT FROM
                     'stannum dictionary changes require superuser or pg_database_owner membership' THEN
                     RAISE EXCEPTION 'admin check did not run first: %', caught;
                   END IF;
                 END $test$;"
            )).unwrap();
        }
        Spi::run(
            "RESET ROLE;
             ALTER TABLE stannum.hardening_saved_words RENAME TO jieba_words;",
        )
        .unwrap();
        assert!(!internal_spi());
    }

    #[pg_test]
    fn drift_warning_set_is_per_index_and_resets_per_statement() {
        Spi::run(
            "CREATE TABLE hardening_drift(body text);
             CREATE INDEX hardening_drift_a ON hardening_drift USING stannum(body) WITH(tokenizer='jieba');
             CREATE INDEX hardening_drift_b ON hardening_drift USING stannum(body) WITH(tokenizer='jieba');"
        ).unwrap();
        let indexes: Vec<_> = ["hardening_drift_a", "hardening_drift_b"]
            .into_iter()
            .map(|name| {
                let oid = Spi::get_one::<pg_sys::Oid>(&format!("SELECT '{name}'::regclass::oid"))
                    .unwrap()
                    .unwrap();
                unsafe { PgRelation::with_lock(oid, pg_sys::AccessShareLock as _) }
            })
            .collect();
        let metas: Vec<_> = indexes
            .iter()
            .map(|index| unsafe { crate::storage::analysis_meta(index.as_ptr()) })
            .collect();
        jieba_add_word("硬化覆盖测试词", 12345, None);
        note_executor_start();
        for (index, meta) in indexes.iter().zip(&metas) {
            check_analysis(index.oid(), meta);
            check_analysis(index.oid(), meta);
        }
        assert_eq!(WARNED.with_borrow(|set| set.len()), 2);
        note_executor_start();
        assert!(WARNED.with_borrow(|set| set.is_empty()));
        check_analysis(indexes[0].oid(), &metas[0]);
        assert_eq!(WARNED.with_borrow(|set| set.len()), 1);
        // Diagnostics must report drift without enforcing strict scan policy.
        Spi::run("SET LOCAL stannum.strict_analysis=on").unwrap();
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT NOT analysis_matches AND analysis_detail LIKE '%drift%'
               FROM stannum.index_stats('hardening_drift_a')"
            )
            .unwrap(),
            Some(true)
        );
    }

    #[pg_test]
    fn parallel_dictionary_policy_is_narrow() {
        Spi::run("CREATE TABLE dict_parallel(body text, plain text); CREATE INDEX dict_parallel_idx ON dict_parallel USING stannum(body) WITH(tokenizer='jieba'); CREATE INDEX dict_plain_idx ON dict_parallel USING stannum(plain);").unwrap();
        let jieba = Spi::get_one::<pg_sys::Oid>("SELECT 'dict_parallel_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        let plain = Spi::get_one::<pg_sys::Oid>("SELECT 'dict_plain_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        assert!(unsafe { parallel_safe(jieba, std::ptr::null_mut()) });
        jieba_add_word("星河数据库协议", 1000000, None);
        assert!(!unsafe { parallel_safe(jieba, std::ptr::null_mut()) });
        assert!(unsafe { parallel_safe(plain, std::ptr::null_mut()) });
        jieba_delete_word("星河数据库协议");
        assert!(unsafe { parallel_safe(jieba, std::ptr::null_mut()) });
    }

    #[pg_test]
    fn legacy_analysis_stays_warning_in_strict_mode() {
        Spi::run("CREATE TABLE dict_legacy(body text); CREATE INDEX dict_legacy_idx ON dict_legacy USING stannum(body) WITH(tokenizer='jieba'); SET LOCAL stannum.strict_analysis=on;").unwrap();
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'dict_legacy_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        let index = unsafe { PgRelation::with_lock(oid, pg_sys::AccessShareLock as _) };
        let mut meta = unsafe { crate::storage::analysis_meta(index.as_ptr()) };
        meta.analysis = None;
        note_executor_start();
        check_analysis(oid, &meta);
        check_analysis(oid, &meta);
        assert_eq!(WARNED.with_borrow(|s| s.len()), 1);
    }

    #[pg_test]
    fn reload_identity_and_generation() {
        let empty = ensure_current();
        let generation = tokenizer::jieba_current_generation();
        jieba_reload_dict();
        assert_eq!(ensure_current(), empty);
        assert!(tokenizer::jieba_current_generation() > generation);
        let generation = tokenizer::jieba_current_generation();
        jieba_add_word("星河数据库协议", 1000000, Some("n"));
        let custom = ensure_current();
        assert_ne!(custom, empty);
        assert!(tokenizer::jieba_current_generation() > generation);
        let generation = tokenizer::jieba_current_generation();
        jieba_add_word("星河数据库协议", 1000000, Some("n"));
        assert_eq!(ensure_current(), custom);
        assert_eq!(tokenizer::jieba_current_generation(), generation);
        jieba_delete_word("星河数据库协议");
        assert_eq!(ensure_current(), empty);
    }

    #[pg_test]
    fn analysis_and_stop_word_scoring() {
        Spi::run("CREATE TABLE dict_scoring(body text); INSERT INTO dict_scoring VALUES ('的 数据库'), ('的'), ('数据库'); CREATE INDEX dict_scoring_idx ON dict_scoring USING stannum(body) WITH(tokenizer='jieba', score_stop_words='auto:zh'); SET LOCAL enable_seqscan=off;").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT matches FROM stannum.index_analysis('dict_scoring_idx')")
                .unwrap(),
            Some(true)
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM stannum.score_inspect('dict_scoring_idx', '的', 1.1)"
            )
            .unwrap(),
            Some(0)
        );
        assert_eq!(Spi::get_one::<bool>("SELECT bool_and(stannum.score(ctid, dense_ratio => 1.1) = 0 AND stannum.full_score(ctid) > 0) FROM dict_scoring WHERE body ==> '的'").unwrap(), Some(true));
        jieba_add_word("星河数据库协议", 1000000, None);
        assert_eq!(
            Spi::get_one::<bool>("SELECT matches FROM stannum.index_analysis('dict_scoring_idx')")
                .unwrap(),
            Some(false)
        );
        Spi::run("REINDEX INDEX dict_scoring_idx").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT matches FROM stannum.index_analysis('dict_scoring_idx')")
                .unwrap(),
            Some(true)
        );
    }
}
