"""Expand query-local SQL expression functions without database registration."""

from ._native import expand_sql, expand_sql_with_offset

__all__ = ["expand_sql", "expand_sql_with_offset"]
