-- Copyright (C) 2026 Ben Weis <ben@springbird.app>
-- Based on Lead, copyright (C) 2026 PlanetScale
--
-- See LICENSE in the repository root for license terms.

-- Catalog delta vs 0.4.0:
--   Created: capabilities()
--   Altered: none
--   Dropped: none
-- jieba_add_word, jieba_delete_word, jieba_dict_version, and jieba_reload_dict
-- keep their 0.4.0 definitions; the upgrade is SQL byte-identical for them, so
-- no ALTER or DROP is needed here even though 0.5.0 implements them against a
-- real dictionary. No segment conversion. Invoker rights only.

-- Keep this definition identical to the 0.5.0 fresh-install snapshot so
-- extension fingerprints remain equal.
CREATE  FUNCTION "capabilities"() RETURNS jsonb /* JsonB */
IMMUTABLE STRICT PARALLEL SAFE
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'capabilities_wrapper';
