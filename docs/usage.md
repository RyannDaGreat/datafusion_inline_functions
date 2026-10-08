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

The second `WITH` introduces ordinary CTEs (named query expressions). Definitions must be at the top level. Calls accept positional scalar expressions; table-valued calls, named arguments, `DISTINCT`, `OVER`, and `FILTER` on local calls are unsupported. Ordinary aggregate/window functions can appear in a body when the resulting SQL is valid. Names with dedicated SQL syntax, such as `trim`, `substring`, `cast`, `struct`, `all`, `distinct`, `top`, `any`, `some`, `rollup`, and `cube`, cannot be local function names. SQL lambdas and references to named windows inside bodies are rejected because they introduce additional binding scopes. The `->` operator is also rejected inside bodies because the parser represents both lambda and JSON arrow syntax that way; use JSON helper function calls instead.

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

- Parameter names must parse as identifiers; SQL expressions such as `current_date` and `true` require double quotes in both the declaration and body, or a different parameter name.
- Unqualified names in a body are parameters. Other body columns must be qualified by a relation in their own query or an enclosing body-local query. Aliases from sibling or descendant queries are not visible.
- A caller's column argument must be qualified when the expanded body contains a subquery: `window_count(c.id)`, not `window_count(id)`.
- Window expressions cannot be passed into a function whose expanded body contains a subquery: their row/window context would move. Compute the window result in a caller CTE, then pass its qualified column. Named-window references in bodies are rejected in both `OVER w` and `OVER (w)` forms.
- Caller query relation names/aliases and argument qualifiers must not collide with relation names/aliases in the expanded body, including nested local calls. Rename one alias if rejected. This prevents DataFusion from resolving a missing inner column against an identically named caller relation.
- Subqueries in bodies support named tables and aliased derived tables. More complex relation forms fail explicitly.
- Qualified wildcards (`table.*`), set operations (`UNION`, `INTERSECT`, `EXCEPT`), and extra parenthesized query-body forms inside function definitions are rejected. Ordinary `COUNT(*)` remains supported.
- Aliases and field labels are not parameter substitutions. Quoted identifiers retain case; unquoted identifiers compare case-insensitively.

These rules reject some otherwise valid SQL to avoid silently capturing names from the wrong scope. Bare parameter names are reserved throughout their body, including its subqueries. Refer to an actual same-named column with its relation qualifier.

Function-call arguments (including scalar, aggregate, and window calls) cannot be passed directly into a function containing a subquery: expansion could change their query scope, and the expander has no function catalog to distinguish aggregate UDFs. Compute the argument in a caller CTE and pass its qualified result column instead. Calls remain allowed as arguments to functions without subqueries.

A subquery supplied as an argument can also be rejected by the conservative alias checks, even when it would be safe. Compute it in a caller CTE and pass the qualified result column instead.

Untyped numeric literal arguments and results expand as type-preserving `CASE` expressions. This prevents `GROUP BY f(1)` and `ORDER BY f(1)` from becoming positional column references and gives projections, grouping keys, and `GROUPING(...)` the same expression. Existing numeric ordinals such as `GROUP BY 1` and `ORDER BY 1` remain unchanged. String arguments remain literals, including patterns passed to application helpers such as `fzf`.

Window-frame offsets require literal SQL in DataFusion. In that specific position, the compiler restores its own numeric wrappers to literals, so `ROWS f(1) PRECEDING` works. User-written `CASE`, casts, and other nonliteral frame expressions remain subject to DataFusion's restrictions.

## API and behavior

`expand_sql(sql: str) -> str` is deterministic, thread-safe, and has no process-global function state. Expansion errors are Python `ValueError`s; errors when executing the returned SQL come from DataFusion.

Editor integrations can use `expand_sql_with_offset(sql) -> tuple[str, int]`. The first value is the same expanded SQL; the second is the original main query's starting index in Python Unicode characters. `sql[start:]` retains the main query's original formatting for source spans. Queries without declarations return offset zero. This is not a mapping for tokens copied from function bodies.

For SQLGlot-based editors, install `datafusion_inline_functions[editors]` and use `from datafusion_inline_functions.source import parse_source`. `parse_source(sql)` returns `(expanded_sql, caller_tree, query_start)`: a SQLGlot PostgreSQL tree of the original main query, whose identifier positions are relative to `sql[query_start:]`. Local calls retain all argument expressions even when they shadow builtins such as `normalize` or `date_part`; other builtin grammar remains unchanged. The expanded SQL remains authoritative for execution and dependency tracing. The optional adapter pins SQLGlot 30.18.0; `expand_sql` itself has no SQLGlot dependency. For JavaScript editors, `utf16_spans(text, spans)` converts Python character spans to UTF-16 code-unit offsets, including emoji.

The editor adapter identifies local calls from the native parser's exact source positions. This also supports keyword-shaped call names such as `and(x,y)` without treating the ordinary `AND (condition)` operator as a function call.

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
