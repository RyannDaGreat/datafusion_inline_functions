"""Regression tests for independently discovered macro binding and special-name defects."""

import pytest
from datafusion import SessionContext, SQLOptions

from datafusion_inline_functions import expand_sql, expand_sql_with_offset


@pytest.mark.parametrize("body", [
    "(SELECT count(*) FROM (VALUES(1)) d(id) WHERE d.id=c.id) + (SELECT count(*) FROM (VALUES(0)) c(id))",
    "(SELECT count(*) FROM (VALUES(1)) d(id) WHERE d.id=c.id AND d.id>(SELECT min(c.id) FROM (VALUES(0)) c(id)))",
    "(WITH q AS(SELECT c.id) SELECT count(*) FROM (VALUES(1))c(id))",
    "(SELECT count(*) FROM (VALUES(1))d(id)) + c.id",
])
def test_qualifier_cannot_escape_its_query_scope(body):
    """Command. Assert a free c.id cannot borrow a sibling, descendant, or later relation alias."""
    sql = f"WITH FUNCTION f(x) AS({body}) SELECT f(0) FROM (VALUES(1),(8))c(id)"
    with pytest.raises(ValueError, match="unbound qualifier"):
        expand_sql(sql)


@pytest.mark.parametrize("body", ["count(c.*)", "(SELECT c.* FROM (VALUES(1))c(id))"])
def test_qualified_wildcards_are_explicitly_rejected(body):
    """Command. Assert wildcard forms that bypass ordinary identifier visitors fail explicitly."""
    with pytest.raises(ValueError, match="qualified wildcard"):
        expand_sql(f"WITH FUNCTION f() AS({body}) SELECT f()")


@pytest.mark.parametrize("source", [
    "WITH FUNCTION f() AS((SELECT count(*) FROM (VALUES(1))c(other) WHERE c.other=c.id)) SELECT f() FROM (VALUES(1),(8))c(id)",
    "WITH FUNCTION f() AS((SELECT count(*) FROM (VALUES(1))c(other) WHERE c.other=c.id)), FUNCTION g() AS((SELECT f() FROM (VALUES(1))c(id))) SELECT g()",
    "WITH FUNCTION f() AS((SELECT count(*) FROM (VALUES(1))c(other) WHERE c.other=c.id)) SELECT f() FROM UNNEST([1,8])c(id)",
])
def test_missing_local_column_cannot_fall_back_to_caller_alias(source):
    """Command. Reject alias overlap that lets DataFusion resolve a missing inner column outside the function."""
    with pytest.raises(ValueError, match="collides"):
        expand_sql(source)


@pytest.mark.parametrize("name", ["trim", "substring", "substr", "ceil", "floor", "position", "extract", "overlay", "struct", "cast", '"trim"', "all", "distinct", "top", "any", "some", "rollup", "cube"])
def test_special_sql_names_cannot_silently_bypass_expansion(name):
    """Command. Assert dedicated SQL expression names cannot silently invoke a builtin instead."""
    with pytest.raises(ValueError, match="reserved SQL syntax"):
        expand_sql(f"WITH FUNCTION {name}(x) AS('replacement') SELECT {name}('input')")


@pytest.mark.parametrize("body", ["(SELECT 1 UNION ALL SELECT 2)", "(SELECT 1 INTERSECT SELECT 1)"])
def test_unsupported_query_scope_forms_fail_explicitly(body):
    """Command. Assert query forms requiring extra binding scopes fail instead of guessing."""
    with pytest.raises(ValueError, match="set operations|parenthesized query"):
        expand_sql(f"WITH FUNCTION f() AS({body}) SELECT f()")


@pytest.mark.parametrize("body, expected", [
    ("(SELECT count(*) FROM (VALUES(1),(2))w(id) WHERE EXISTS(SELECT 1 FROM (VALUES(1))c(id) WHERE c.id=w.id))", 1),
    ("(SELECT count(*) FROM (VALUES(1))c(id)) + (SELECT count(*) FROM (VALUES(2))c(id))", 2),
    ("(SELECT w.id FROM (VALUES(2),(1))w(id) ORDER BY w.id LIMIT 1)", 1),
    ("(WITH q AS(SELECT 3 AS id) SELECT max(q.id) FROM q)", 3),
])
def test_valid_nested_correlated_and_ordered_scopes(body, expected):
    """Query. Verify body-local correlation, sibling aliases, ordering, and CTEs still execute."""
    options = SQLOptions().with_allow_ddl(False).with_allow_dml(False).with_allow_statements(False)
    sql = expand_sql(f"WITH FUNCTION f() AS({body}) SELECT f() AS result")
    assert SessionContext().sql(sql, options=options).to_pylist() == [{"result": expected}]


def test_literal_arguments_remain_literals_for_downstream_helpers():
    """Pure function. Verify fzf's literal pattern survives expansion; returns None on success."""
    assert expand_sql("WITH FUNCTION f(x,p) AS(fzf(x,p)) SELECT f(name,'blue') FROM items") == "SELECT (fzf((name), 'blue')) FROM items"
    assert expand_sql("WITH FUNCTION f(x) AS(x*2) SELECT f(3+1)") == "SELECT ((3 + 1) * 2)"


@pytest.mark.parametrize("separator", [" ", "\n", "\r\n\t", "\r", "\t/* λ💡 */\n"])
def test_python_offset_preserves_original_main_query(separator):
    """Pure function. Verify source[start:] selects the original SQL after Unicode declarations; returns None."""
    prefix = "-- λ💡\nWITH FUNCTION f() AS('λ💡')" + separator
    query = "WITH q AS(SELECT 1) SELECT f() FROM q"
    source = prefix + query
    expanded, start = expand_sql_with_offset(source)
    assert expanded == expand_sql(source)
    assert start == len(prefix)
    assert source[start:] == query
    assert expand_sql_with_offset("-- λ\r\nSELECT 1") == ("-- λ\r\nSELECT 1", 0)


@pytest.mark.parametrize("window", ["w", "(w)", "(w ORDER BY x)", '("w")'])
def test_parenthesized_named_windows_do_not_capture_callers(window):
    """Command. Reject every named-window spelling inside a local definition before it captures the caller."""
    with pytest.raises(ValueError, match="named window"):
        expand_sql(f"WITH FUNCTION f(x) AS(sum(x) OVER {window}) SELECT f(v) FROM (VALUES(1),(2)) t(v) WINDOW w AS ()")


@pytest.mark.parametrize("window", ["w", "(w)", "(w ORDER BY c.v)", "()", "(ORDER BY c.v)"])
def test_window_arguments_do_not_move_into_subquery_scope(window):
    """Command. Reject moving a caller window into a function's own SELECT or WINDOW scope."""
    sql = ("WITH FUNCTION f(x) AS((SELECT x FROM (VALUES(0)) d(a) WINDOW w AS ())) "
           f"SELECT c.v, f(row_number() OVER {window}) AS result FROM (VALUES(1),(2)) c(v) WINDOW w AS (ORDER BY c.v)")
    with pytest.raises(ValueError, match="window expressions"):
        expand_sql(sql)


def test_window_argument_in_row_local_expression_still_works():
    """Query. A body without a SELECT retains the caller window's row context and yields 2,4."""
    sql = "WITH FUNCTION twice(x) AS(x*2) SELECT twice(row_number() OVER (ORDER BY c.v)) AS n FROM (VALUES(1),(2))c(v) ORDER BY c.v"
    options = SQLOptions().with_allow_ddl(False).with_allow_dml(False).with_allow_statements(False)
    assert SessionContext().sql(expand_sql(sql), options=options).to_pylist() == [{"n": 2}, {"n": 4}]


@pytest.mark.parametrize("name", ["any", "some", "ANY", '"any"'])
def test_quantifier_names_cannot_bypass_expansion_in_comparisons(name):
    """Command. Reject names whose comparison syntax becomes a SQL quantifier instead of a function call."""
    with pytest.raises(ValueError, match="reserved SQL syntax"):
        expand_sql(f"WITH FUNCTION {name}(x) AS(103) SELECT 3 = {name}([3]) AS result")


@pytest.mark.parametrize("argument", ["count(*)", "count(1)", "sum(c.v)", "max(c.v)", "abs(c.v)", "custom_aggregate(c.v)", "1 + count(*)"])
def test_function_arguments_do_not_move_into_subquery_scope(argument):
    """Command. Reject catalog-dependent scalar/aggregate calls before they change their query context."""
    sql = f"WITH FUNCTION f(x) AS((SELECT x FROM (VALUES(10)) d(v))) SELECT f({argument}) AS result FROM (VALUES(2),(5)) c(v)"
    with pytest.raises(ValueError, match="function-call arguments"):
        expand_sql(sql)


def test_precomputed_aggregate_argument_retains_caller_context():
    """Query. Computing an aggregate in a caller CTE preserves one count of two rows."""
    sql = "WITH FUNCTION f(x) AS((SELECT max(d.v) FROM (VALUES(2),(5)) d(v) WHERE d.v=x)) WITH totals AS(SELECT count(*) AS n FROM (VALUES(2),(5)) c(v)) SELECT f(t.n) AS result FROM totals t"
    options = SQLOptions().with_allow_ddl(False).with_allow_dml(False).with_allow_statements(False)
    assert SessionContext().sql(expand_sql(sql), options=options).to_pylist() == [{"result": 2}]


@pytest.mark.parametrize("name", ["current_date", "current_time", "current_timestamp", "localtime", "localtimestamp", "current_user", "session_user", "user", "current_catalog", "true", "false", "null", "not"])
def test_parameter_names_cannot_be_builtin_expressions(name):
    """Command. Reject unquoted parameter names that SQL would interpret as literals or builtins."""
    with pytest.raises(ValueError, match="parameter .*reserved SQL syntax"):
        expand_sql(f"WITH FUNCTION f({name}) AS({name}) SELECT f(42) AS result")


@pytest.mark.parametrize("name", ["current_date", "current_user", "true", "null", "not"])
def test_quoted_keyword_parameters_are_unambiguous(name):
    """Query. Explicitly quoted keyword parameters retain normal identifier substitution."""
    sql = f'WITH FUNCTION f("{name}") AS("{name}"+1) SELECT f(42) AS result'
    assert SessionContext().sql(expand_sql(sql)).to_pylist() == [{"result": 43}]
