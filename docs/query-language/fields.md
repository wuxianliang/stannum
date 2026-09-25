<!--
Copyright (C) 2026 Ben Weis <ben@springbird.app>
Based on Lead, copyright (C) 2026 PlanetScale

See LICENSE in the repository root for license terms.
-->

# Fields

Stannum-specific; a multi-column index adds a field dimension to TINQL.

A multi-column Stannum index indexes every key column as its own field and
scores matches with [BM25F](../architecture/segmented-storage.md), blending
each field's term frequency with per-field weights. Field syntax restricts
any query — terms, boolean operators, phrases, proximity, span relations,
positional filters — to one named field.

```sql
CREATE INDEX docs_search ON docs USING stannum (title, body)
    WITH (field_weights = 'title:3.0,body:1.0');
```

## Syntax

```
title:(beer OR ale)
body:("craft beer")
```

A field group is `name:(` followed by any query, closed by `)`. The opening
parenthesis must be adjacent to the colon: `title: (foo)` with a space is
**not** field syntax — it parses as the word `title:` followed by a group,
exactly as in earlier releases.

A bare name is an ASCII letter followed by ASCII letters, digits, or
underscores, folded to lower case the way PostgreSQL folds an unquoted
identifier (`TITLE:(beer)` and `Title_2:(beer)` both scope to `title` /
`title_2`). Any other spelling is written as a quoted identifier using the
phrase escape rule and matches the recorded column name byte for byte:

```
"My Field":(beer)
"a\"b\\c":(beer)
```

`title:beer` without a parenthesis is unchanged from earlier releases: the
colon is a word character, so the whole thing is one term lookup. A field
group is the only field syntax.

## Scoping rules

- The scope applies to everything inside the group: `title:(beer AND ale)`
  requires both terms in the title.
- Groups nest and compose with the rest of the language:
  `title:(beer) AND body:(ale)` names two fields; `title:(beer OR ale)^2`
  boosts the scoped disjunction.
- An inner group wins over an outer one.
- An unknown field name is an error (`stannum: unknown field 'nope'`), and so
  is field syntax on a single-column index (`stannum: field syntax requires a
  multi-column index`).
- The `==>` operator carries an implicit field scope from its left operand's
  column: `title ==> 'beer'` scores and matches title hits only, without any
  group in the query. A group naming a *different* field inside an operator
  clause is rejected; use `stannum.search()` for all-fields queries.

## The same-field phrase rule

Positions are independent per field — each field's positions start at zero —
so a phrase, proximity, or span query matches only when **one field** holds
it in order. This is Lucene's rule: an unscoped phrase matches when any ONE
field contains it, never a spread across fields, and a field-scoped phrase
matches only within its field.

```
title:("甲 乙")        -- matches only when the TITLE holds 甲 immediately
                       -- followed by 乙
"甲 乙"                -- matches when any single field holds the phrase;
                       -- 甲 in the title plus 乙 in the body never matches
title:(a NEAR/5 b)     -- proximity is solved within the title's positions
x IN LAST 10%          -- positional filters use the owning field's length
```

Boolean operators compose at the document level, so `title:(甲) AND body:(乙)`
is fine — each *term* is independently scoped; it is the *positional* queries
that never cross fields.

## Snippets and highlights

`stannum.search()` returns one snippet per row. On a multi-column index it
renders the field named by a single top-level `Field` group
(`title:(needle)`), else the first field holding a match, else the first
non-NULL column plain. Marks never leave the matching field.

`stannum.highlight` has a field-aware overload whose fifth argument names the
field the passed text is; a field-scoped query part marks only when the text
is that field:

```sql
SELECT stannum.highlight(title, '<b>', '</b>', 'title:(needle)', 'title')
FROM docs WHERE title ==> 'needle';
```

`field => NULL` keeps the single-column behavior (field groups contribute no
marks there). The overload's trailing arguments take no defaults: shorter
calls stay unambiguous against the four-argument forms.
