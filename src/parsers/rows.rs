//! SQLite rows as JSON objects, for parsers whose tables carry JSON columns.

use anyhow::Result;
use rusqlite::{types::ValueRef, Connection, ToSql};
use serde_json::{Map, Number, Value};

/// Run `sql` and hand each row to `each` as a JSON object keyed by column name.
/// A text cell longer than `max_cell` bytes is replaced by `null` and flagged
/// under `<column>__oversize` so memory stays bounded and the caller can fail
/// that one record. Blobs are not read (`null`).
pub fn query_maps(
    conn: &Connection,
    sql: &str,
    params: &[&dyn ToSql],
    max_cell: usize,
    mut each: impl FnMut(Map<String, Value>) -> Result<()>,
) -> Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(params)?;
    while let Some(row) = rows.next()? {
        let mut m = Map::new();
        for (i, n) in names.iter().enumerate() {
            let v = match row.get_ref(i)? {
                ValueRef::Integer(x) => Value::from(x),
                ValueRef::Real(x) => Number::from_f64(x).map_or(Value::Null, Value::Number),
                ValueRef::Text(t) if t.len() > max_cell => {
                    m.insert(format!("{n}__oversize"), Value::from(t.len() as u64));
                    Value::Null
                }
                ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Null | ValueRef::Blob(_) => Value::Null,
            };
            m.insert(n.clone(), v);
        }
        each(m)?;
    }
    Ok(())
}

/// Names of cells that [`query_maps`] refused to load.
pub fn oversize_cells(m: &Map<String, Value>) -> Vec<&str> {
    m.keys()
        .filter_map(|k| k.strip_suffix("__oversize"))
        .collect()
}
