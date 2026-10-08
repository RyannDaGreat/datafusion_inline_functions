# datafusion_inline_functions

Define a function inside one SQL query, then reuse it throughout that query. This Python package uses Rust to expand those calls into ordinary **Apache DataFusion 54.x SQL** before execution.

DataFusion does not accept this package's `WITH FUNCTION` syntax directly. Without it, you might repeat a formatting expression:

```sql
SELECT
    CASE WHEN ready_hours < 0.1 THEN '<0.1' ELSE CAST(ready_hours AS VARCHAR) END AS ready,
    CASE WHEN done_hours < 0.1 THEN '<0.1' ELSE CAST(done_hours AS VARCHAR) END AS done
FROM production
```

With `expand_sql`, define that expression once:

```sql
WITH FUNCTION hours_text(hours) AS (
    CASE WHEN hours < 0.1 THEN '<0.1' ELSE CAST(hours AS VARCHAR) END
)
SELECT hours_text(ready_hours) AS ready, hours_text(done_hours) AS done
FROM production
```

The result is still one SQL query. There is no database function registration, extra database request, or function state shared between queries. The Rust code runs during SQL preparation; DataFusion executes the resulting expressions.

## Install and try it

Requires Python 3.11+. Install the package and DataFusion:

```sh
python -m pip install datafusion_inline_functions 'datafusion>=54,<55'
```

Wheels are provided for macOS Apple Silicon and Linux x86-64/ARM64. Other platforms build from source and require [Rust](https://rustup.rs/).

The distribution name and import name are both `datafusion_inline_functions`. DataFusion is installed separately to execute queries; the expander itself only transforms strings.

This complete Python example needs no input files or database setup:

```python
from datafusion import SessionContext, SQLOptions
from datafusion_inline_functions import expand_sql

sql = """
WITH FUNCTION twice(x) AS (x * 2)
SELECT twice(3 + 1) AS answer
"""
expanded = expand_sql(sql)
print(expanded)
# SELECT ((3 + 1) * 2) AS answer

options = (
    SQLOptions()
    .with_allow_ddl(False)
    .with_allow_dml(False)
    .with_allow_statements(False)
)
print(SessionContext().sql(expanded, options=options).to_pydict())
# {'answer': [8]}
```

`expand_sql(sql: str) -> str` returns SQL text; it does not execute it. Invalid declarations or unsupported calls raise `ValueError`.

## Multiple functions

Use one `WITH`, separate declarations with commas, and repeat `FUNCTION` for each definition. Local functions can call other local functions:

```sql
WITH FUNCTION twice(x) AS (x * 2),
     FUNCTION twice_plus_one(x) AS (twice(x) + 1)
SELECT twice(3) AS a, twice_plus_one(3) AS b
```

After expansion and execution, `.to_pylist()[0]` returns `{'a': 6, 'b': 7}`. Recursive calls, including functions calling each other in a cycle, are rejected during expansion.

## More than arithmetic

Functions can return structs, for example to keep a numeric sort value beside display text:

```sql
WITH FUNCTION cell(value) AS (
    struct(value AS sort, CAST(value AS VARCHAR) AS text)
)
SELECT cell(12) AS ready, cell(3) AS done
```

After expansion and execution, `.to_pylist()[0]` returns this row:

```python
{'ready': {'sort': 12, 'text': '12'}, 'done': {'sort': 3, 'text': '3'}}
```

You can also use `CASE`, nested calls to other local functions, typed parameters/results, ordinary `WITH` queries, and scalar `SELECT` subqueries. See [syntax and binding rules](https://github.com/RyannDaGreat/datafusion_inline_functions/blob/main/docs/usage.md) for examples. Definitions belong to one query; this package does not provide a global function catalog or resolve reusable named queries for your application.

## Boundaries to know

- Queries without a leading `WITH FUNCTION` pass through **byte-for-byte unchanged**. Queries with definitions are parsed and rewritten; their original comments and formatting are not retained.
- These are expression macros: an argument used twice is inserted twice. A call such as `f(random())` can evaluate `random()` more than once.
- Subquery bodies require qualified column references and reject alias collisions. Unsupported binding forms fail explicitly; table-returning functions and procedural statements are not supported.
- Expansion is **not a read-only SQL validator**. Keep DataFusion's `SQLOptions` checks when read-only execution matters.

The supported engine target is DataFusion 54.x, not arbitrary SQL dialects. The implementation uses Apache's `sqlparser` Rust crate; the core expander has no SQLGlot or SQLMesh dependency. An optional `editors` extra provides a SQLGlot adapter for original source positions.

For application integration, see [reusable queries and dynamic SQL](https://github.com/RyannDaGreat/datafusion_inline_functions/blob/main/docs/usage.md#reusable-queries-and-dynamic-sql). For building, testing, and the `pypi` upload command, see [development and publishing](https://github.com/RyannDaGreat/datafusion_inline_functions/blob/main/docs/development.md).
