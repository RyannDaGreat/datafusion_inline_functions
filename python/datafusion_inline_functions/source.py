"""Optional SQLGlot adapter for source positions in original local-function queries."""

from itertools import accumulate

from sqlglot import exp, parse_one
from sqlglot.dialects.postgres import Postgres
from sqlglot.tokens import TokenType

from ._native import _expand_sql_with_metadata


def parse_source(sql: str) -> tuple[str, exp.Expression, int]:
    """
    Pure function. Expand SQL and parse its original caller for editor source spans.

    Args:
        sql: One SQL query, optionally preceded by local function declarations.
    Returns:
        Expanded SQL, original-caller SQLGlot tree, and its Unicode character offset.
        Identifier positions in the tree are relative to ``sql[offset:]``.
    Examples:
        >>> text = 'WITH FUNCTION normalize(x,y) AS(x+y) SELECT normalize(a,b) FROM t'
        >>> expanded, tree, start = parse_source(text)
        >>> expanded
        'SELECT ((a) + (b)) FROM t'
        >>> sorted(column.name for column in tree.find_all(exp.Column))
        ['a', 'b']
        >>> text[start:]
        'SELECT normalize(a,b) FROM t'
    """
    expanded, start, offsets = _expand_sql_with_metadata(sql)
    if not start:
        return expanded, parse_one(sql, dialect="postgres"), start
    calls = {offset - start for offset in offsets}

    class SourceParser(Postgres.Parser):
        """Parser with query-local call names; no shared SQLGlot tables are changed."""

        def _parse_function_call(self, **kwargs):
            """
            Command. Advance this parser, treating exact local calls as anonymous.

            Args:
                kwargs: SQLGlot function-parsing options forwarded to its parser.
            Returns:
                Parsed expression, or None when the current token is not a call.
            """
            if self._curr and self._curr.start in calls:
                kwargs.update(anonymous=True, optional_parens=False)
            return super()._parse_function_call(**kwargs)

    source = sql[start:]
    tokens = Postgres().tokenize(source)
    for token in tokens:
        if token.start in calls and token.token_type != TokenType.IDENTIFIER:
            token.token_type = TokenType.VAR
    tree = SourceParser(dialect=Postgres()).parse(tokens, source)[0]
    if tree is None:
        raise ValueError("Expected a SQL query")
    return expanded, tree, start


def utf16_spans(text: str, spans: list[tuple[int, int]]) -> list[tuple[int, int]]:
    """
    Pure function. Convert Python character spans to JavaScript UTF-16 offsets.

    Args:
        text: Original source text.
        spans: Half-open spans measured in Python Unicode characters.
    Returns:
        The same spans measured in UTF-16 code units, for editors such as Monaco.
    Examples:
        >>> utf16_spans('🐦 SELECT x', [(9, 10), (0, 1)])
        [(10, 11), (0, 2)]
        >>> utf16_spans('λx', [(1, 2), (2, 2)])
        [(1, 2), (2, 2)]
    """
    offsets = [0, *accumulate(2 if ord(character) > 0xFFFF else 1 for character in text)]
    return [(offsets[start], offsets[end]) for start, end in spans]
