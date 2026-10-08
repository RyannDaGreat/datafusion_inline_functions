"""Expand query-local SQL expression functions without database registration."""

from ._native import expand_sql

__all__ = ["expand_sql"]
