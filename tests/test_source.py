"""Original SQL source positions, local-name grammar and parser isolation."""

from concurrent.futures import ThreadPoolExecutor

import pytest
from sqlglot import exp, parse_one
from sqlglot.dialects.postgres import Postgres
from sqlglot.errors import ParseError

from datafusion_inline_functions.source import parse_source, utf16_spans


@pytest.mark.parametrize('name', ['normalize', 'date_part', 'left', 'right', 'abs', 'upper', 'sum', 'json_extract'])
def test_local_builtin_names_retain_every_argument(name):
    """Command. Assert local calls retain every column and original character span."""
    sql = f'-- 🐦\r\nWITH FUNCTION {name}(x,y) AS(x+y) SELECT {name}(id,score) AS n FROM items'
    expanded, tree, start = parse_source(sql)
    assert expanded == 'SELECT ((id) + (score)) AS n FROM items'
    assert {column.name for column in tree.find_all(exp.Column)} == {'id', 'score'}
    for column in tree.find_all(exp.Column):
        assert sql[start + column.this.meta['start']:start + column.this.meta['end'] + 1] == column.name


@pytest.mark.parametrize('expression', [
    'CAST(score AS VARCHAR)', "substring(CAST(score AS VARCHAR) FROM 1 FOR 2)",
    "normalize(CAST(score AS VARCHAR), NFC)", "date_part('year', DATE '2020-01-01')",
    'sum(score) OVER(PARTITION BY id ORDER BY score)',
    "trim(BOTH '0' FROM CAST(score AS VARCHAR))", 'count(*)',
])
def test_unused_local_name_preserves_builtin_parser(expression):
    """Command. Assert builtins parse identically when a different local helper exists."""
    source = f'SELECT {expression} AS n FROM items'
    _, tree, _ = parse_source('WITH FUNCTION f(x) AS(x) ' + source)
    assert tree == parse_one(source, dialect='postgres')


def test_nested_local_and_builtin_grammars():
    """Command. Assert nesting resets no shared state and keeps builtin argument rules."""
    sql = "WITH FUNCTION normalize(x,y) AS(x+y) SELECT normalize(abs(id), normalize(score, CAST(id AS DOUBLE))) AS n FROM items"
    _, tree, _ = parse_source(sql)
    assert len(list(tree.find_all(exp.Anonymous))) == 2
    assert len(list(tree.find_all(exp.Abs))) == 1
    assert len(list(tree.find_all(exp.Cast))) == 1
    assert {column.name for column in tree.find_all(exp.Column)} == {'id', 'score'}


def test_quoted_case_and_unqualified_names():
    """Command. Assert uppercase quoted local names do not shadow ordinary builtins."""
    source = "SELECT \"NORMALIZE\"(id,score), normalize('text', NFC) FROM items"
    _, tree, _ = parse_source('WITH FUNCTION "NORMALIZE"(x,y) AS(x+y) ' + source)
    assert {column.name for column in tree.find_all(exp.Column)} == {'id', 'score'}
    assert len(list(tree.find_all(exp.Anonymous))) == 1
    assert tree.expressions[1] == parse_one("SELECT normalize('text', NFC)", dialect="postgres").expressions[0]
    qualified = "SELECT public.normalize('text', NFC) FROM items"
    _, actual, _ = parse_source('WITH FUNCTION normalize(x,y) AS(x+y) ' + qualified)
    assert actual == parse_one(qualified, dialect="postgres")


def test_concurrent_parser_isolation():
    """Command. Assert independent calls never alter global SQLGlot function maps."""
    maps = (Postgres.Parser.FUNCTIONS.copy(), Postgres.Parser.FUNCTION_PARSERS.copy())
    local = 'WITH FUNCTION normalize(x,y) AS(x+y) SELECT normalize(id,score) FROM items'
    ordinary = "SELECT normalize('text', NFC)"
    with ThreadPoolExecutor(max_workers=4) as pool:
        results = list(pool.map(parse_source, [local, ordinary] * 20))
    for index, (_, tree, _) in enumerate(results):
        assert len(list(tree.find_all(exp.Anonymous))) == (index % 2 == 0)
    assert maps == (Postgres.Parser.FUNCTIONS, Postgres.Parser.FUNCTION_PARSERS)


@pytest.mark.parametrize('sql', ['', '-- comment', '/* comment */'])
def test_empty_query_is_an_explicit_error(sql):
    """Command. Assert empty input produces a caller-visible validation error."""
    with pytest.raises((ValueError, ParseError), match='Expected a SQL query|No expression was parsed'):
        parse_source(sql)


@pytest.mark.parametrize('name', ['and', 'or', 'apply', 'into', 'vector', 'select', 'where', 'over', 'join'])
def test_keyword_local_calls_keep_argument_spans(name):
    """Command. Assert keyword calls are identified by native spans before SQLGlot dispatch."""
    sql = f'-- 🐦\r\nWITH FUNCTION {name}(x,y) AS(x+y)\nSELECT {name}(id,score) AS n FROM items'
    _, tree, start = parse_source(sql)
    assert {column.name for column in tree.find_all(exp.Column)} == {'id', 'score'}
    for column in tree.find_all(exp.Column):
        assert sql[start + column.this.meta['start']:start + column.this.meta['end'] + 1] == column.name


@pytest.mark.parametrize('name, main', [
    ('and', 'SELECT id > 0 AND (score > 0) AS n FROM items'),
    ('or', 'SELECT id > 0 OR (score > 0) AS n FROM items'),
    ('in', 'SELECT id IN (1,2) AS n FROM items'),
    ('where', 'SELECT id FROM items WHERE (score > 0)'),
    ('over', 'SELECT sum(score) OVER (PARTITION BY id) AS n FROM items'),
    ('join', 'SELECT a.id FROM items a JOIN (SELECT id FROM items) b ON a.id=b.id'),
    ('select', 'SELECT (SELECT max(score) FROM items) AS n'),
])
def test_keyword_operators_and_clauses_are_not_local_calls(name, main):
    """Command. Assert keyword spelling alone never rewrites ordinary SQL grammar."""
    _, tree, _ = parse_source(f'WITH FUNCTION {name}(x,y) AS(x+y) ' + main)
    assert tree == parse_one(main, dialect='postgres')


def test_keyword_calls_and_operators_coexist():
    """Command. Assert real AND calls and the ordinary AND operator keep separate roles."""
    sql = 'WITH FUNCTION and(x,y) AS(x+y) SELECT and(id,score)>0 AND (score>0) AS n FROM items'
    _, tree, _ = parse_source(sql)
    assert len(list(tree.find_all(exp.Anonymous))) == 1
    assert len(list(tree.find_all(exp.And))) == 1
    assert {column.name for column in tree.find_all(exp.Column)} == {'id', 'score'}


@pytest.mark.parametrize('sql', ['SELECT 1; SELECT 2', 'SELECT id FROM items; DELETE FROM items', 'SELECT 1; -- trailing comment'])
def test_ordinary_sql_preserves_all_parser_statements(sql):
    """Command. Preserve SQLGlot's complete ordinary tree, including multi-statement blocks."""
    expanded, tree, start = parse_source(sql)
    assert expanded == sql
    assert start == 0
    assert tree == parse_one(sql, dialect='postgres')


@pytest.mark.parametrize('text', ['ASCII', 'λ🦜x💡', '🐦 SELECT "🦜" FROM t', 'line1\r\n🐦line2'])
def test_utf16_spans_match_javascript_slicing(text):
    """Command. Every converted source-character span selects the same UTF-16 encoded text."""
    spans = [(start, end) for start in range(len(text) + 1) for end in range(start, len(text) + 1)]
    converted = utf16_spans(text, spans)
    encoded = text.encode('utf-16-le')
    for (start, end), (left, right) in zip(spans, converted):
        assert encoded[2 * left:2 * right].decode('utf-16-le') == text[start:end]
