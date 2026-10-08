# Using local SQL functions

## Syntax

An inferred-type function uses a parenthesized expression:

```sql
WITH FUNCTION count_cell(value, dead, color) AS (
    struct('colored_text' AS widget, value AS sort,
           CAST(value AS VARCHAR) AS text,
           CASE WHEN dead OR value = 0 THEN '#777' ELSE color END AS color,
           NOT dead AND value > 0 AS bold)
)
SELECT count_cell(ready, dead, '#3fb950') AS ready FROM ranked
```

A typed declaration uses `RETURNS` and `RETURN`. Arguments and the result are explicitly cast to the declared types in the expanded SQL; DataFusion checks and executes those casts:

```sql
WITH FUNCTION twice(x BIGINT) RETURNS BIGINT RETURN x * 2
SELECT twice('3') AS answer
```

Separate declarations with commas, repeating `FUNCTION`. No semicolon is needed between declarations and the final query. Calls can reference any declaration in the same prefix, including later ones. Recursion is rejected when expanded.

```sql
WITH FUNCTION twice(x) AS (x * 2),
     FUNCTION plus_one(x) AS (twice(x) + 1)
WITH inputs AS (SELECT 3 AS n)
SELECT plus_one(n) FROM inputs
```

The second `WITH` introduces ordinary CTEs (named query expressions). Definitions must be at the top level. Calls accept positional scalar expressions; table-valued calls, named arguments, `DISTINCT`, `OVER`, and `FILTER` on local calls are unsupported. Ordinary aggregate/window functions can appear in a body when the resulting SQL is valid. SQL lambdas and references to named windows inside bodies are rejected because they introduce additional binding scopes. The `->` operator is also rejected inside bodies because the parser represents both lambda and JSON arrow syntax that way; use JSON helper function calls instead.

## SELECT bodies and binding rules

Scalar subqueries are supported within DataFusion's own subquery capabilities:

```sql
WITH FUNCTION window_count(clip_id) AS (
    (SELECT count(*) FROM windows AS w WHERE w.id = clip_id)
)
SELECT c.id, window_count(c.id) FROM clips AS c
```

The body is inserted into the query; it does not execute a separate database request during expansion. A scalar subquery must return one column and at most one row. Expansion does not overcome DataFusion's unsupported correlation forms.

Binding is deliberately conservative and does not require a database schema:

- Unqualified names in a body are parameters. Other body columns must be qualified by a relation introduced in that body.
- A caller's column argument must be qualified when the expanded body contains a subquery: `window_count(c.id)`, not `window_count(id)`.
- Caller argument qualifiers must not collide with relation names/aliases in the expanded body. Rename one alias if rejected.
- Subqueries in bodies support named tables and aliased derived tables. More complex relation forms fail explicitly.
- Aliases and field labels are not parameter substitutions. Quoted identifiers retain case; unquoted identifiers compare case-insensitively.

These rules reject some otherwise valid SQL to avoid silently capturing names from the wrong scope. Bare parameter names are reserved throughout their body, including its subqueries. Refer to an actual same-named column with its relation qualifier.

## API and behavior

`expand_sql(sql: str) -> str` is deterministic, thread-safe, and has no process-global function state. Expansion errors are Python `ValueError`s; errors when executing the returned SQL come from DataFusion.

If the first two non-comment tokens are not `WITH FUNCTION`, the input is returned **byte-for-byte unchanged**, even if it is malformed SQL. Such queries remain the engine's responsibility. For opted-in queries, declarations are removed and the parsed SQL is formatted; comments and original whitespace are not preserved.

The functions have **expression-macro semantics**: an argument used twice is inserted twice. `f(random())` may therefore contain multiple random calls. There is no exactly-once evaluation promise. Types are checked by DataFusion after expansion, and unused definitions do not trigger execution-time type errors.

Expansion is limited to 32 nested local calls, 1,024 local-function expansions, and 8 MiB of generated SQL. There is no silent truncation. Leading function declarations must be followed by one query; mutation bodies, `SELECT INTO`, and additional statements are rejected. **This is not a read-only security gate**: ordinary SQL is passed through, and externally registered functions can have their own behavior. Keep DataFusion's `SQLOptions` checks.

## Reusable queries and dynamic SQL

Expand each stored query independently before discovering its table/query dependencies or embedding it in another query's `WITH` clause. Expand incoming SQL from program arguments or a UI at the same boundary. Local definitions must not be concatenated into a CTE body before expansion.

```python
expanded_queries = {name: expand_sql(text) for name, text in queries.items()}
expanded_input = expand_sql(user_sql)
# Pass these to the application's existing named-query preparation step.
```

This package does not implement named-query lookup, CTE dependency resolution, or a global function catalog. No Bluejay integration is included.
