#[cfg(feature = "python")]
use pyo3::exceptions::PyValueError;
#[cfg(feature = "python")]
use pyo3::prelude::*;

mod compiler;
pub use compiler::{Error, expand, expand_with_metadata, expand_with_offset};

/// Pure function. Expand leading WITH FUNCTION declarations into DataFusion SQL.
///
/// Args:
///     sql: One query, optionally preceded by local function declarations.
/// Returns:
///     Expanded SQL, or byte-for-byte original SQL when there are no declarations.
/// Raises:
///     ValueError: Invalid declarations, calls, or unsafe parameter binding.
/// Examples:
///     >>> expand_sql('SELECT 1')
///     'SELECT 1'
///     >>> expand_sql('WITH FUNCTION twice(x) AS (x * 2) SELECT twice(3)')
///     'SELECT (CASE WHEN true THEN 3 ELSE 3 END * 2)'
#[cfg(feature = "python")]
#[pyfunction]
fn expand_sql(py: Python<'_>, sql: &str) -> PyResult<String> {
    py.detach(|| compiler::expand(sql))
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

/// Pure function. Expand SQL and locate the original main query for editor tools.
///
/// Args:
///     sql: Original SQL text, optionally with leading local definitions.
/// Returns:
///     (expanded_sql, query_start), where query_start counts Unicode characters
///     in the original string. This locates the main query, not every expanded token.
/// Examples:
///     >>> expand_sql_with_offset('SELECT 1')
///     ('SELECT 1', 0)
///     >>> source = 'WITH FUNCTION f(x) AS (x+1) SELECT f(2) AS n'
///     >>> expanded, start = expand_sql_with_offset(source)
///     >>> source[start:]
///     'SELECT f(2) AS n'
#[cfg(feature = "python")]
#[pyfunction]
fn expand_sql_with_offset(py: Python<'_>, sql: &str) -> PyResult<(String, usize)> {
    py.detach(|| compiler::expand_with_offset(sql))
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

/// Pure function. Supply expanded SQL and exact original-query/local-call positions.
/// Args: SQL text. Returns: (expanded SQL, query offset, absolute Unicode call-name offsets).
/// Example: SELECT 1 -> ("SELECT 1", 0, []); operators sharing a local name are not calls.
#[cfg(feature = "python")]
#[pyfunction]
fn _expand_sql_with_metadata(py: Python<'_>, sql: &str) -> PyResult<(String, usize, Vec<usize>)> {
    py.detach(|| compiler::expand_with_metadata(sql))
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

/// Command. Register the Python extension's callable in the module namespace.
/// Args: module is the new Python module. Returns: success or a Python error.
#[cfg(feature = "python")]
#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(expand_sql, module)?)?;
    module.add_function(wrap_pyfunction!(expand_sql_with_offset, module)?)?;
    module.add_function(wrap_pyfunction!(_expand_sql_with_metadata, module)?)
}
