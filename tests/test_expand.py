"""Semantic comparisons against handwritten DataFusion SQL and explicit failure cases."""

from concurrent.futures import ThreadPoolExecutor
import doctest

import pyarrow as pa
import pytest
from datafusion import SessionContext, SQLOptions

from datafusion_inline_functions import expand_sql


@pytest.fixture
def context():
    """Command. Create an isolated session with synthetic clips and windows; return the session."""
    ctx = SessionContext()
    ctx.register_record_batches("clips", [[pa.record_batch({"id": [1, 2, 3], "n": [4, 0, None]})]])
    ctx.register_record_batches("windows", [[pa.record_batch({"id": [1, 1, 2]})]])
    return ctx


def execute(context, sql):
    """
    Query. Read the session's tables with all SQL mutation options disabled.

    Args: context: isolated test session; sql: query text.
    Returns: Arrow Table, including output schema. SELECT 1 AS n returns one row n=1.
    """
    options = SQLOptions().with_allow_ddl(False).with_allow_dml(False).with_allow_statements(False)
    frame = context.sql(sql, options=options)
    batches = frame.collect()
    return pa.Table.from_batches(batches) if batches else pa.Table.from_batches([], schema=frame.schema())


@pytest.mark.parametrize("source, expected", [
    ("WITH FUNCTION f(x) AS (x+1) SELECT 2*f(3) AS v", "SELECT 8 AS v"),
    ("WITH FUNCTION f(x) AS (x*2) SELECT f(3+1) AS v", "SELECT 8 AS v"),
    ("WITH FUNCTION g(x) AS (x*2), FUNCTION f(x) AS (g(x)+1) SELECT f(3+1) AS v", "SELECT 9 AS v"),
    ("WITH FUNCTION f(x) AS (x+1) SELECT f(f(2)) AS v", "SELECT 4 AS v"),
    ("WITH FUNCTION f() AS (42) SELECT f() AS v", "SELECT 42 AS v"),
    ("WITH FUNCTION f(x BIGINT) RETURNS BIGINT RETURN x*2 SELECT f('3') AS v", "SELECT CAST(6 AS BIGINT) AS v"),
    ("WITH FUNCTION f(x DOUBLE) RETURNS VARCHAR RETURN x/2 SELECT f(3) AS v", "SELECT CAST(1.5 AS VARCHAR) AS v"),
    ("WITH FUNCTION f(x) AS (x+1) SELECT f(n) AS v FROM clips ORDER BY id", "SELECT n+1 AS v FROM clips ORDER BY id"),
    ("WITH FUNCTION f(x) AS (coalesce(x, 9)) SELECT f(n) AS v FROM clips ORDER BY id", "SELECT coalesce(n, 9) AS v FROM clips ORDER BY id"),
    ("WITH FUNCTION f(x) AS (CASE WHEN x=0 THEN NULL ELSE 10/x END) SELECT f(n) AS v FROM clips ORDER BY id", "SELECT CASE WHEN n=0 THEN NULL ELSE 10/n END AS v FROM clips ORDER BY id"),
    ("WITH FUNCTION f(x) AS (named_struct('value', x, 'text', CAST(x AS VARCHAR))) SELECT f(n) AS v FROM clips ORDER BY id", "SELECT named_struct('value', n, 'text', CAST(n AS VARCHAR)) AS v FROM clips ORDER BY id"),
    ("WITH FUNCTION f(x) AS (struct(x AS value)) SELECT f(n) AS v FROM clips ORDER BY id", "SELECT struct(n AS value) AS v FROM clips ORDER BY id"),
    ("WITH FUNCTION f(x) AS ((SELECT count(*) FROM windows w WHERE w.id=x)) SELECT f(c.id) AS v FROM clips c ORDER BY c.id", "SELECT (SELECT count(*) FROM windows w WHERE w.id=c.id) AS v FROM clips c ORDER BY c.id"),
    ("WITH FUNCTION f(x) AS ((SELECT max(w.id) FROM windows w WHERE w.id=x)) SELECT f(c.id) AS v FROM clips c ORDER BY c.id", "SELECT (SELECT max(w.id) FROM windows w WHERE w.id=c.id) AS v FROM clips c ORDER BY c.id"),
    ("WITH FUNCTION f(x) AS ((SELECT count(*) FROM windows w WHERE w.id=x)), FUNCTION g(y) AS (f(y)+1) SELECT g(c.id) AS v FROM clips c ORDER BY c.id", "SELECT (SELECT count(*) FROM windows w WHERE w.id=c.id)+1 AS v FROM clips c ORDER BY c.id"),
    ("WITH FUNCTION f(x) AS (x*2) WITH q AS (SELECT 5 AS n) SELECT f(n) AS v FROM q", "WITH q AS (SELECT 5 AS n) SELECT n*2 AS v FROM q"),
    ('WITH FUNCTION "Twice"("X") AS ("X"*2) SELECT "Twice"(3) AS v', "SELECT 6 AS v"),
    ("with function F(X) as (x+1) select f(2) AS v;", "SELECT 3 AS v"),
    ("/* outer /* nested */ comment */ WITH -- line\n FUNCTION f(x) AS (x+1) SELECT f(2) AS v", "SELECT 3 AS v"),
    ("WITH FUNCTION f(x) AS (x || '; FUNCTION f(0)') SELECT f('a') AS v", "SELECT 'a; FUNCTION f(0)' AS v"),
])
def test_semantics(context, source, expected):
    """Query. Compare the source's expanded values and schema with expected handwritten SQL."""
    actual = execute(context, expand_sql(source))
    reference = execute(context, expected)
    assert actual.schema == reference.schema
    assert actual.to_pylist() == reference.to_pylist()


@pytest.mark.parametrize("sql", [
    "", "SELECT 1;", " \n-- note\n SELECT 'WITH FUNCTION f(x)' AS text; ",
    "WITH q AS (SELECT 1) SELECT * FROM q", "SELECT 'unterminated", "DROP TABLE t;",
    'SELECT "function" FROM t', "/* WITH FUNCTION fake */ SELECT 4",
    "SELECT 1; SELECT 2", "-- unterminated comment without newline", "/* unfinished",
])
def test_passthrough(sql):
    """Pure function. Assert exact pass-through of sql without leading declarations; returns None."""
    assert expand_sql(sql) == sql


@pytest.mark.parametrize("sql, message", [
    ("WITH FUNCTION f(x) AS (x) SELECT f()", "expects 1 arguments"),
    ("WITH FUNCTION f(x) AS (x) SELECT f(1,2)", "expects 1 arguments"),
    ("WITH FUNCTION f(x,x) AS (x) SELECT f(1,2)", "duplicate parameter"),
    ("WITH FUNCTION f(x) AS (x), FUNCTION F(y) AS (y) SELECT f(1)", "duplicate local function"),
    ("WITH FUNCTION f(x) AS (f(x)) SELECT f(1)", "recursive"),
    ("WITH FUNCTION f(x) AS (g(x)), FUNCTION g(x) AS (f(x)) SELECT f(1)", "recursive"),
    ("WITH FUNCTION f(x) AS (x+y) SELECT f(1)", "unbound identifier"),
    ("WITH FUNCTION f(x) AS (t.x) SELECT f(1)", "unbound qualifier"),
    ("WITH FUNCTION f(x) AS ((SELECT count(*) FROM windows w WHERE w.id=x)) SELECT f(id) FROM clips", "must be qualified"),
    ("WITH FUNCTION f(x) AS ((SELECT count(*) FROM windows c WHERE c.id=x)) SELECT f(c.id) FROM clips c", "collides"),
    ("WITH FUNCTION f(x) AS ((SELECT max(id) FROM windows)) SELECT f(1)", "unbound identifier"),
    ("WITH FUNCTION f(x) AS (x) SELECT f(DISTINCT 1)", "DISTINCT"),
    ("WITH FUNCTION f(x) AS (x) SELECT f(*)", "positional"),
    ("WITH FUNCTION f(x) AS (x) SELECT f(x => 1)", "positional"),
    ("WITH FUNCTION f(x) AS (x) SELECT f(1) OVER ()", "modifiers"),
    ("WITH FUNCTION f(x) AS (x) SELECT f(1) FILTER (WHERE true)", "modifiers"),
    ("WITH FUNCTION f(x) AS (x) SELECT f(1); DROP TABLE clips", "Expected"),
    ("WITH FUNCTION f(x) AS (x) SELECT f(1) INTO out", "SELECT INTO"),
    ("WITH FUNCTION f(x) AS (x) SELECT f(1) FOR UPDATE", "Expected|locking"),
    ("WITH FUNCTION f(x) AS (x) DELETE FROM clips", "mutation"),
    ("WITH FUNCTION f(x) AS (x", "Expected"),
    ("WITH FUNCTION f(x) AS (array_transform([1], x -> x+1)) SELECT f(2)", "lambda"),
    ("WITH FUNCTION f(x) AS (sum(x) OVER w) SELECT f(2)", "named window"),
    ("WITH FUNCTION unused() AS ((SELECT 1 INTO hidden)) SELECT 1", "SELECT INTO"),
])
def test_explicit_errors(sql, message):
    """Command. Assert sql raises a ValueError whose message identifies the rejected condition."""
    with pytest.raises(ValueError, match=message):
        expand_sql(sql)


def test_no_cross_query_state(context):
    """Query. Assert local f definitions do not modify a session or later expansion calls."""
    assert execute(context, expand_sql("WITH FUNCTION f(x) AS(x+1) SELECT f(2) AS v")).to_pylist() == [{"v": 3}]
    assert expand_sql("SELECT f(2)") == "SELECT f(2)"
    with pytest.raises(Exception, match="function|Function"):
        context.sql("SELECT f(2)")


def test_concurrent_expansion():
    """Command. Run independent definitions on worker threads and assert no shared function state."""
    sources = [f"WITH FUNCTION f(x) AS (x+{n}) SELECT f(2)" for n in range(32)]
    expected = [expand_sql(sql) for sql in sources]
    with ThreadPoolExecutor(max_workers=8) as executor:
        assert list(executor.map(expand_sql, sources)) == expected


def test_expansion_budget():
    """Command. Assert a branching definition chain raises instead of growing without a bound."""
    definitions = ["FUNCTION f0(x) AS (x)"]
    definitions += [f"FUNCTION f{n}(x) AS (f{n-1}(x)+f{n-1}(x))" for n in range(1, 12)]
    with pytest.raises(ValueError, match="limit"):
        expand_sql("WITH " + ", ".join(definitions) + " SELECT f11(1)")


def test_macro_evaluation_contract():
    """Pure function. Assert repeated parameters copy argument SQL; returns None. f(random()) yields two calls."""
    sql = expand_sql("WITH FUNCTION f(x) AS (x+x) SELECT f(random())")
    assert sql.upper().count("RANDOM()") == 2


def test_widget_helpers(context):
    """Query. Compare count/hour widget values and field types across zero, null, tiny, and dimmed values."""
    count_body = """struct('colored_text' AS widget, value AS sort,
        CAST(value AS VARCHAR) AS text,
        CASE WHEN dead OR value=0 THEN '#777' ELSE color END AS color,
        NOT dead AND value>0 AS bold)"""
    hours_body = """struct('colored_text' AS widget, value AS sort,
        CASE WHEN value>0 AND value<0.05 THEN '<0.1' ELSE CAST(round(value,1) AS VARCHAR) END AS text,
        CASE WHEN dead OR value=0 THEN '#777' ELSE 'inherit' END AS color,
        false AS bold)"""
    input_sql = """WITH facts AS (SELECT * FROM
        (VALUES (0.0, false), (0.001, false), (0.049, false), (0.05, false),
                (0.051, false), (12.25, true), (NULL, false)) AS t(value,dead))"""
    source = ("WITH FUNCTION count_cell(value,dead,color) AS (" + count_body + "), "
              "FUNCTION hours_cell(value,dead) AS (" + hours_body + ") " + input_sql +
              " SELECT count_cell(value,dead,'green') AS count, hours_cell(value,dead) AS hours FROM facts")
    reference = input_sql + " SELECT " + count_body.replace("ELSE color", "ELSE 'green'") + " AS count, " + hours_body + " AS hours FROM facts"
    actual = execute(context, expand_sql(source))
    expected = execute(context, reference)
    assert actual.schema == expected.schema
    assert actual.to_pylist() == expected.to_pylist()


def test_query_composition(context):
    """Query. Expand two independently scoped named queries, then reuse each inside one CTE query."""
    first = expand_sql("WITH FUNCTION f(x) AS(x+1) SELECT f(2) AS v")
    second = expand_sql("WITH FUNCTION f(x) AS(x*2) SELECT f(2) AS v")
    sql = f"WITH first AS ({first}), second AS ({second}) SELECT first.v AS a, second.v AS b FROM first CROSS JOIN second"
    assert execute(context, sql).to_pylist() == [{"a": 3, "b": 4}]


def test_api_doc_examples():
    """Command. Execute the native Python API's documented examples and assert every example succeeds."""
    test = doctest.DocTestParser().get_doctest(expand_sql.__doc__, {"expand_sql": expand_sql}, "expand_sql", None, None)
    result = doctest.DocTestRunner().run(test)
    assert result.failed == 0


def test_parameter_does_not_rewrite_alias(context):
    """Query. Assert an alias matching a parameter stays a label while expression references are substituted."""
    sql = """WITH FUNCTION f(x) AS (
        (SELECT count(*) AS x FROM windows w WHERE w.id=x)
    ) SELECT f(c.id) AS v FROM clips c ORDER BY c.id"""
    assert execute(context, expand_sql(sql)).to_pylist() == [{"v": 2}, {"v": 1}, {"v": 0}]
