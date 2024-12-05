use pyo3::{intern, prelude::*};

/// Collect pytest items
#[pyfunction]
#[pyo3(signature = (session))]
fn pytest_collection<'py>(
    py: Python<'py>,
    session: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let items = session.getattr(intern!(py, "items"))?;
    let retval = items;
    Ok(retval)
}

/// A Python module implemented in Rust.
#[pymodule]
fn pytest_testsearch(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(pytest_collection, m)?)?;
    Ok(())
}
