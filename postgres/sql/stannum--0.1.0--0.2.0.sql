-- Copyright (C) 2026 Ben Weis <ben@springbird.app>
--
-- See LICENSE in the repository root for license terms.

-- Dictionary governance. Functions intentionally remain invoker-run. Rust
-- checks administrators before performing narrowly scoped internal table DML.
CREATE TABLE @extschema@.jieba_words (
    word text PRIMARY KEY,
    freq integer NOT NULL DEFAULT 0 CHECK (freq >= 0),
    tag text
);
REVOKE ALL ON TABLE @extschema@.jieba_words FROM PUBLIC;

CREATE FUNCTION @extschema@.jieba_add_word(word text, freq integer DEFAULT 0,
    tag text DEFAULT NULL) RETURNS void
    LANGUAGE c VOLATILE PARALLEL UNSAFE
    AS 'MODULE_PATHNAME', 'jieba_add_word_wrapper';
CREATE FUNCTION @extschema@.jieba_delete_word(word text) RETURNS void
    LANGUAGE c VOLATILE PARALLEL UNSAFE STRICT
    AS 'MODULE_PATHNAME', 'jieba_delete_word_wrapper';
CREATE FUNCTION @extschema@.jieba_dict_version() RETURNS bigint
    LANGUAGE c STABLE PARALLEL UNSAFE STRICT
    AS 'MODULE_PATHNAME', 'jieba_dict_version_wrapper';
CREATE FUNCTION @extschema@.jieba_reload_dict() RETURNS void
    LANGUAGE c VOLATILE PARALLEL UNSAFE STRICT
    AS 'MODULE_PATHNAME', 'jieba_reload_dict_wrapper';
CREATE FUNCTION @extschema@.builtin_stop_words(preset text) RETURNS SETOF text
    LANGUAGE c IMMUTABLE PARALLEL SAFE STRICT
    AS 'MODULE_PATHNAME', 'builtin_stop_words_wrapper';
CREATE FUNCTION @extschema@.index_analysis("index" regclass)
RETURNS TABLE (index_name text, recorded_jieba_version integer,
    recorded_dict_fingerprint bigint, runtime_jieba_version integer,
    runtime_dict_fingerprint bigint, matches boolean, status text)
    LANGUAGE c STABLE PARALLEL UNSAFE STRICT
    AS 'MODULE_PATHNAME', 'index_analysis_wrapper';

-- These existing functions now consult the mutable dictionary for jieba.
ALTER FUNCTION @extschema@.tokenize(text, text, text, text, text, integer, text, text)
    STABLE PARALLEL UNSAFE;
ALTER FUNCTION @extschema@.ql_parse(text, boolean, text, text, text, text, integer, text, text)
    STABLE PARALLEL UNSAFE;
