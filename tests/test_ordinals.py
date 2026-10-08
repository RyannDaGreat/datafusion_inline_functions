"""Regression tests preventing numeric macro results from becoming SQL column ordinals."""

import pytest
from datafusion import SessionContext, SQLOptions

from datafusion_inline_functions import expand_sql


def assert_equivalent(source, reference):
    """
    Query. Compare expanded SQL against explicit non-positional reference SQL.

    Args: source: local-function SQL; reference: equivalent ordinary SQL.
    Returns: expanded SQL after asserting equal row multisets and field types.
    Example: grouping by an identity function of 1 returns one count of three.
    """
    options = SQLOptions().with_allow_ddl(False).with_allow_dml(False).with_allow_statements(False)
    context = SessionContext()
    expanded = expand_sql(source)
    actual = context.sql(expanded, options=options)
    expected = context.sql(reference, options=options)
    assert actual.schema() == expected.schema()
    assert sorted(actual.to_pylist(), key=repr) == sorted(expected.to_pylist(), key=repr)
    return expanded


@pytest.mark.parametrize("literal", ["1", "2", "0", "-1", "+1", "(+1)", "-(-1)", "1.0", "1e0"])
def test_grouped_numeric_calls_keep_constant_semantics(literal):
    """Query. Identity calls returning numbers group all rows together, never by a SELECT position."""
    query = "SELECT count(*) AS n FROM (VALUES(3),(1),(2))t(v) GROUP BY {}"
    source = "WITH FUNCTION f(x) AS(x) " + query.format(f"f({literal})")
    reference = query.format(f"CASE WHEN TRUE THEN {literal} ELSE {literal} END")
    assert "CASE WHEN" in assert_equivalent(source, reference)


@pytest.mark.parametrize("call", ["f(1)", "(f(1))", "+f(1)", "-f(1)", "-(-f(1))", "f(f(1))"])
def test_ordered_numeric_calls_do_not_select_sort_columns(call):
    """Query. Constant ordering leaves the explicit secondary descending sort in control."""
    source = f"WITH FUNCTION f(x) AS(x) SELECT t.v AS n FROM (VALUES(1),(3),(2))t(v) ORDER BY {call}, t.v DESC LIMIT 1"
    reference = "SELECT t.v AS n FROM (VALUES(1),(3),(2))t(v) ORDER BY CASE WHEN TRUE THEN 1 ELSE 1 END, t.v DESC LIMIT 1"
    assert_equivalent(source, reference)


@pytest.mark.parametrize("position", ["x", "(x)", "+x", "-x", "-(-x)"])
def test_parameters_in_body_grouping_keep_constant_semantics(position):
    """Query. A numeric argument substituted into a function's own GROUP BY remains a constant."""
    source = f"WITH FUNCTION f(x) AS((SELECT count(*) FROM (VALUES(10),(20))d(v) GROUP BY {position})) SELECT f(1) AS n"
    reference = "SELECT (SELECT count(*) FROM (VALUES(10),(20))d(v) GROUP BY CASE WHEN TRUE THEN 1 ELSE 1 END) AS n"
    assert_equivalent(source, reference)


@pytest.mark.parametrize("position", ["x", "(x)", "+x", "-x", "-(-x)"])
def test_parameters_in_body_ordering_keep_constant_semantics(position):
    """Query. Substitution cannot turn a callee's sort parameter into a SELECT-list position."""
    source = f"WITH FUNCTION f(x) AS((SELECT d.v FROM (VALUES(10),(20))d(v) ORDER BY {position}, d.v DESC LIMIT 1)) SELECT f(1) AS n"
    reference = "SELECT (SELECT d.v FROM (VALUES(10),(20))d(v) ORDER BY CASE WHEN TRUE THEN 1 ELSE 1 END, d.v DESC LIMIT 1) AS n"
    assert_equivalent(source, reference)


@pytest.mark.parametrize("grouping", ["GROUPING SETS (({}), ())", "ROLLUP ({})", "CUBE ({})", "GROUPING SETS (({}, t.v), ())"])
def test_grouping_set_members_keep_constant_semantics(grouping):
    """Query. Numeric replacements inside grouping sets, rollups, and cubes remain values."""
    query = "SELECT count(*) AS n FROM (VALUES(3),(1),(2))t(v) GROUP BY "
    source = "WITH FUNCTION f(x) AS(x) " + query + grouping.format("f(1)")
    reference = query + grouping.format("CASE WHEN TRUE THEN 1 ELSE 1 END")
    assert_equivalent(source, reference)


@pytest.mark.parametrize("literal", ["1", "(1)", "+1"])
def test_existing_group_ordinals_remain_ordinals(literal):
    """Query. Original numeric GROUP BY syntax still refers to the first selected column."""
    query = f"SELECT t.v AS value, count(*) AS n FROM (VALUES(1),(2),(1))t(v) GROUP BY {literal} ORDER BY value"
    expanded = assert_equivalent("WITH FUNCTION f(x) AS(x) " + query, query)
    assert "CASE WHEN" not in expanded


def test_existing_order_ordinals_and_column_functions_are_unchanged():
    """Query. Ordinary ORDER BY positions and local functions of grouping columns preserve their meanings."""
    query = "SELECT t.v AS value, count(*) AS n FROM (VALUES(1),(2),(1))t(v) GROUP BY f(t.v), f(2) ORDER BY 1 DESC"
    reference = query.replace("f(t.v)", "t.v").replace("f(2)", "CASE WHEN TRUE THEN 2 ELSE 2 END")
    assert_equivalent("WITH FUNCTION f(x) AS(x) " + query, reference)


@pytest.mark.parametrize("declaration", ["f(x INT) AS(x)", "f(x) RETURNS INT RETURN x", "f(x INT) RETURNS INT RETURN x"])
def test_typed_numeric_functions_keep_cast_semantics(declaration):
    """Query. Typed numeric calls group by their cast values without becoming positional references."""
    query = "SELECT count(*) AS n FROM (VALUES(1),(2))t(v) GROUP BY {}"
    assert_equivalent("WITH FUNCTION " + declaration + " " + query.format("f(1)"), query.format("CAST(1 AS INT)"))


def test_grouping_call_does_not_accidentally_legalize_ungrouped_columns():
    """Command. Constant GROUP BY must still reject a selected column that is not grouped or aggregated."""
    sql = expand_sql("WITH FUNCTION f(x) AS(x) SELECT t.v FROM (VALUES(1),(2))t(v) GROUP BY f(1)")
    with pytest.raises(Exception, match="aggregate|GROUP BY|group by"):
        SessionContext().sql(sql).collect()


@pytest.mark.parametrize("declaration,call", [
    ("f(x) AS(x)", "f(1)"),
    ("f(x) AS(x)", "f(-1)"),
    ("f(x) AS(x)", "f(f(1))"),
    ("f() AS(1)", "f()"),
    ("f(x INT) AS(x)", "f(1)"),
    ("f(x) RETURNS INT RETURN x", "f(1)"),
])
@pytest.mark.parametrize("grouping", ["ROLLUP", "CUBE", "GROUPING SETS"])
def test_grouping_and_projected_subtotals_share_the_same_expression(declaration, call, grouping):
    """Query. Grouping metadata matches projections, and subtotal grouping keys become NULL."""
    groups = f"{grouping}({call})" if grouping != "GROUPING SETS" else f"GROUPING SETS(({call}),())"
    sql = f"WITH FUNCTION {declaration} SELECT {call} AS k, grouping({call}) AS g, count(*) AS n FROM (VALUES(1),(2),(3))t(v) GROUP BY {groups}"
    result = SessionContext().sql(expand_sql(sql)).to_pylist()
    value = -1 if call == "f(-1)" else 1
    assert sorted(result, key=repr) == sorted([{"k": value, "g": 0, "n": 3}, {"k": None, "g": 1, "n": 3}], key=repr)


def test_multiple_numeric_grouping_keys_preserve_subtotal_masks():
    """Query. Two numeric local-function keys remain distinct and produce normal ROLLUP masks."""
    sql = "WITH FUNCTION f(x) AS(x) SELECT f(1) AS a, f(2) AS b, grouping(f(1),f(2)) AS g, count(*) AS n FROM (VALUES(1),(2),(3))t(v) GROUP BY ROLLUP(f(1),f(2))"
    actual = SessionContext().sql(expand_sql(sql)).to_pylist()
    expected = [{"a": 1, "b": 2, "g": 0, "n": 3}, {"a": 1, "b": None, "g": 1, "n": 3}, {"a": None, "b": None, "g": 3, "n": 3}]
    assert sorted(actual, key=repr) == sorted(expected, key=repr)


@pytest.mark.parametrize("query", [
    "SELECT t.v AS n FROM (VALUES(1),(2),(3))t(v) ORDER BY t.v LIMIT {two} OFFSET {one}",
    "SELECT round(1.2345, {two}) AS n",
    "SELECT trunc(1.2345, {two}) AS n",
    "SELECT substr('abcd', {two}, {one}) AS n",
    "SELECT array_element([10,20,30], {two}) AS n",
    "SELECT array_slice([10,20,30], {one}, {two}) AS n",
    "SELECT ntile({two}) OVER(ORDER BY t.v) AS n FROM (VALUES(1),(2),(3))t(v)",
    "SELECT nth_value(t.v, {two}) OVER(ORDER BY t.v ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) AS n FROM (VALUES(1),(2),(3))t(v)",
    "SELECT lag(t.v,{one},0) OVER(ORDER BY t.v) AS n FROM (VALUES(1),(2),(3))t(v)",
])
def test_numeric_macro_arguments_in_engine_numeric_slots(query):
    """Query. Numeric expression binding remains compatible with LIMIT and numeric builtin arguments."""
    assert_equivalent("WITH FUNCTION f(x) AS(x) " + query.format(one="f(1)", two="f(2)"), query.format(one="1", two="2"))


@pytest.mark.parametrize("units", ["ROWS", "RANGE", "GROUPS"])
@pytest.mark.parametrize("bounds", ["BETWEEN {offset} PRECEDING AND CURRENT ROW", "BETWEEN CURRENT ROW AND {offset} FOLLOWING", "{offset} PRECEDING"])
@pytest.mark.parametrize("named", [False, True])
def test_numeric_macro_offsets_remain_frame_literals(units, bounds, named):
    """Query. Numeric macro offsets work in both inline and named window frames."""
    window = "ORDER BY t.v " + units + " " + bounds
    query = ("SELECT sum(t.v) OVER w AS n FROM (VALUES(1),(2),(3))t(v) WINDOW w AS (" + window + ")") if named else ("SELECT sum(t.v) OVER(" + window + ") AS n FROM (VALUES(1),(2),(3))t(v)")
    assert_equivalent("WITH FUNCTION f(x) AS(x) " + query.format(offset="f(f(1))"), query.format(offset="1"))


@pytest.mark.parametrize("units", ["ROWS", "RANGE", "GROUPS"])
def test_subquery_body_frame_parameter_remains_a_literal(units):
    """Query. A numeric parameter in a callee's own window frame stays a supported literal."""
    body = f"(SELECT sum(d.v) OVER(ORDER BY d.v {units} BETWEEN x PRECEDING AND CURRENT ROW) FROM (VALUES(1),(2),(3))d(v) ORDER BY d.v DESC LIMIT 1)"
    source = "WITH FUNCTION f(x) AS(" + body + ") SELECT f(1) AS n"
    reference = "SELECT " + body.replace("x PRECEDING", "1 PRECEDING") + " AS n"
    assert_equivalent(source, reference)


@pytest.mark.parametrize("offset", ["CASE WHEN TRUE THEN 1 ELSE 1 END", "CASE WHEN TRUE THEN f(1) ELSE f(1) END"])
def test_user_written_frame_case_is_not_rewritten(offset):
    """Command. Only generated wrappers collapse; user-written frame CASE retains DataFusion's rejection."""
    source = f"WITH FUNCTION f(x) AS(x) SELECT sum(t.v) OVER(ORDER BY t.v ROWS BETWEEN {offset} PRECEDING AND CURRENT ROW) FROM (VALUES(1),(2))t(v)"
    expanded = expand_sql(source)
    assert "CASE WHEN" in expanded
    with pytest.raises(Exception, match="frame offsets"):
        SessionContext().sql(expanded).collect()
