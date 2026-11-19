//! rusqlite 的薄封装：行以 `Map<String, Value>` 返回（与 node:sqlite 的对象同形），参数用 JSON 值绑定。

use super::mapper::Row;
use rusqlite::types::{Value as SqlVal, ValueRef};
use rusqlite::{Connection, ToSql};
use serde_json::{Map, Value};

/// JSON → SQLite 参数（布尔按 0/1，对象/数组不会出现）。
pub fn bind(v: &Value) -> SqlVal {
    match v {
        Value::Null => SqlVal::Null,
        Value::Bool(b) => SqlVal::Integer(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlVal::Integer(i),
            None => {
                let f = n.as_f64().unwrap_or(0.0);
                // JS 的数字不分整数 / 浮点：5.0 绑定成整数（node:sqlite 同）。
                if f.fract() == 0.0 && f.abs() < 9.007e15 { SqlVal::Integer(f as i64) } else { SqlVal::Real(f) }
            }
        },
        Value::String(s) => SqlVal::Text(s.clone()),
        other => SqlVal::Text(other.to_string()),
    }
}

fn from_ref(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => {
            // JS 读出 REAL 0 / 1.0 就是整数 0 / 1，序列化后没有小数点。
            if f.fract() == 0.0 && f.abs() < 9.007e15 { Value::from(f as i64) } else { serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null) }
        }
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(_) => Value::Null,
    }
}

pub fn all(c: &Connection, sql: &str, params: &[Value]) -> Result<Vec<Row>, String> {
    let mut st = c.prepare_cached(sql).map_err(|e| e.to_string())?;
    let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
    let vals: Vec<SqlVal> = params.iter().map(bind).collect();
    let refs: Vec<&dyn ToSql> = vals.iter().map(|v| v as &dyn ToSql).collect();
    let mut rows = st.query(refs.as_slice()).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let mut m = Map::new();
        for (i, n) in names.iter().enumerate() {
            m.insert(n.clone(), from_ref(r.get_ref(i).map_err(|e| e.to_string())?));
        }
        out.push(m);
    }
    Ok(out)
}

pub fn get(c: &Connection, sql: &str, params: &[Value]) -> Result<Option<Row>, String> {
    Ok(all(c, sql, params)?.into_iter().next())
}

/// 返回受影响行数。
pub fn run(c: &Connection, sql: &str, params: &[Value]) -> Result<usize, String> {
    let mut st = c.prepare_cached(sql).map_err(|e| e.to_string())?;
    let vals: Vec<SqlVal> = params.iter().map(bind).collect();
    let refs: Vec<&dyn ToSql> = vals.iter().map(|v| v as &dyn ToSql).collect();
    st.execute(refs.as_slice()).map_err(|e| e.to_string())
}
