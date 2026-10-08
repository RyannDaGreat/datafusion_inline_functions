#[cfg(feature = "python")]
use pyo3::exceptions::PyValueError;
#[cfg(feature = "python")]
use pyo3::prelude::*;

mod compiler;
pub use compiler::{Error, expand};

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
///     'SELECT ((3) * 2)'
#[cfg(feature = "python")]
#[pyfunction]
fn expand_sql(py: Python<'_>, sql: &str) -> PyResult<String> {
    py.detach(|| compiler::expand(sql))
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

/// Command. Register the Python extension's callable in the module namespace.
/// Args: module is the new Python module. Returns: success or a Python error.
#[cfg(feature = "python")]
#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(expand_sql, module)?)
}
