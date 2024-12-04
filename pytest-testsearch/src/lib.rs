use pyo3::{prelude::*, types::PyList};

/// Collect pytest items
#[pyfunction]
#[pyo3(signature = (session))]
fn pytest_collection<'py>(
    py: Python<'py>,
    session: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyList>> {
    let _ = session;
    let retval = PyList::empty(py);
    Ok(retval)
}

/// A Python module implemented in Rust.
#[pymodule]
fn pytest_testsearch(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(pytest_collection, m)?)?;
    Ok(())
}
