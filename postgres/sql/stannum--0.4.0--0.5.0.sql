-- Copyright (C) 2026 Ben Weis <ben@springbird.app>
-- Based on Lead, copyright (C) 2026 PlanetScale
--
-- See LICENSE in the repository root for license terms.

-- Catalog delta vs 0.4.0:
--   Created: capabilities()
--   Altered: none
--   Dropped: none
-- jieba_add_word, jieba_delete_word, jieba_dict_version, and jieba_reload_dict
-- remain; 0.5.0 keep-surface stubs emit SQL byte-identical to the 0.4.0
-- snapshot. No segment conversion. Invoker rights only.

-- Keep this definition identical to the 0.5.0 fresh-install snapshot so
-- extension fingerprints remain equal.
CREATE  FUNCTION "capabilities"() RETURNS jsonb /* JsonB */
IMMUTABLE STRICT PARALLEL SAFE
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'capabilities_wrapper';
