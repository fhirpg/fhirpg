//! Search execution: compiled search targets → SQL.
//!
//! Semantics per FHIR search: AND across parameters, OR across a
//! parameter's comma-separated values and across its targets. Every user
//! value is bound as a parameter — nothing user-supplied is interpolated
//! into SQL text.

use fhirpg_map::model::{ResourceMap, SearchDef, TargetKind};

use crate::StoreError;

pub struct CompiledQuery {
    pub sql: String,
    /// Same WHERE clause, counting instead of selecting (for _total).
    pub count_sql: String,
    pub binds: Vec<String>,
}

/// One _sort key: parameter code (or _id/_lastUpdated) and direction.
#[derive(Debug, Clone)]
pub struct SortKey {
    pub code: String,
    pub descending: bool,
}

/// Resolve a sort key to a base-table column. Sorting on child-table
/// parameters is not supported (kept honest with an error, not a wrong
/// order).
fn sort_col(rm: &ResourceMap, key: &SortKey) -> Result<String, StoreError> {
    if key.code == "_id" {
        return Ok("p.\"id\"".to_string());
    }
    if key.code == "_lastUpdated" {
        return Ok("p.\"last_updated\"".to_string());
    }
    let Some(def) = rm.search.iter().find(|d| d.code == key.code) else {
        return Err(StoreError::Other(format!(
            "unsupported sort parameter {:?}",
            key.code
        )));
    };
    let Some(t) = def.targets.iter().find(|t| t.table == 0) else {
        return Err(StoreError::Other(format!(
            "cannot sort by {:?}: not a base-table parameter",
            key.code
        )));
    };
    let col = match &t.kind {
        TargetKind::Str { col }
        | TargetKind::Number { col }
        | TargetKind::Uri { col }
        | TargetKind::Token { code: col, .. }
        | TargetKind::Date { lo: col, .. }
        | TargetKind::Quantity { value: col, .. } => col,
        TargetKind::Reference { c_id, .. } => c_id,
    };
    Ok(format!("p.\"{col}\""))
}

/// Build `SELECT id FROM base WHERE …` for raw query parameters
/// (`name` or `name:modifier` → comma-separated values).
pub fn build_search_sql(
    schema: &str,
    rm: &ResourceMap,
    params: &[(String, String)],
    count: i64,
    offset: i64,
    sort: &[SortKey],
) -> Result<CompiledQuery, StoreError> {
    let base = &rm.base_table().name;
    let from = format!("FROM \"{schema}\".\"{base}\" p");
    let mut sql = format!("SELECT p.\"id\" {from}");
    let mut wheres: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    for (rawname, value) in params {
        let (name, modifier) = match rawname.split_once(':') {
            Some((n, m)) => (n, Some(m)),
            None => (rawname.as_str(), None),
        };
        if name == "_id" {
            let ors: Vec<String> = value
                .split(',')
                .map(|v| {
                    binds.push(v.to_string());
                    format!("p.\"id\" = ${}", binds.len())
                })
                .collect();
            wheres.push(format!("({})", ors.join(" OR ")));
            continue;
        }
        if name == "_lastUpdated" {
            let mut ors = Vec::new();
            for v in value.split(',') {
                ors.push(date_pred("p.\"last_updated\"", None, v, &mut binds)?);
            }
            wheres.push(format!("({})", ors.join(" OR ")));
            continue;
        }
        let Some(def) = rm.search.iter().find(|d| d.code == name) else {
            return Err(StoreError::Other(format!(
                "unsupported search parameter {name:?}"
            )));
        };
        if def.targets.is_empty() {
            return Err(StoreError::Other(format!(
                "search parameter {name:?} is not supported: {}",
                def.note.as_deref().unwrap_or("no targets")
            )));
        }
        let mut ors: Vec<String> = Vec::new();
        for v in value.split(',') {
            for t in &def.targets {
                let pred = target_pred(def, t, v, modifier, &mut binds)?;
                let tname = &rm.tables[t.table as usize].name;
                if t.table == 0 {
                    ors.push(pred.replace("«c»", "p"));
                } else {
                    ors.push(format!(
                        "EXISTS (SELECT 1 FROM \"{schema}\".\"{tname}\" c WHERE c.\"rid\" = p.\"id\" AND {})",
                        pred.replace("«c»", "c")
                    ));
                }
            }
        }
        wheres.push(format!("({})", ors.join(" OR ")));
    }

    let mut where_clause = String::new();
    if !wheres.is_empty() {
        where_clause = format!(" WHERE {}", wheres.join(" AND "));
        sql.push_str(&where_clause);
    }
    let count_sql = format!("SELECT count(*) {from}{where_clause}");
    let mut order: Vec<String> = Vec::new();
    for key in sort {
        let col = sort_col(rm, key)?;
        order.push(format!(
            "{col} {} NULLS LAST",
            if key.descending { "DESC" } else { "ASC" }
        ));
    }
    order.push("p.\"id\" ASC".to_string());
    sql.push_str(&format!(" ORDER BY {}", order.join(", ")));
    binds.push(count.to_string());
    sql.push_str(&format!(" LIMIT (${}::text)::bigint", binds.len()));
    binds.push(offset.to_string());
    sql.push_str(&format!(" OFFSET (${}::text)::bigint", binds.len()));
    Ok(CompiledQuery {
        sql,
        count_sql,
        binds,
    })
}

/// One predicate for one target and one value. Column references use the
/// placeholder «c» for the table alias.
fn target_pred(
    def: &SearchDef,
    t: &fhirpg_map::model::SearchTarget,
    value: &str,
    modifier: Option<&str>,
    binds: &mut Vec<String>,
) -> Result<String, StoreError> {
    let b = |binds: &mut Vec<String>, v: &str| {
        binds.push(v.to_string());
        format!("${}", binds.len())
    };
    match &t.kind {
        TargetKind::Str { col } => {
            let c = format!("«c».\"{col}\"");
            match modifier {
                Some("exact") => Ok(format!("{c} = {}", b(binds, value))),
                Some("contains") => Ok(format!(
                    "{c} ILIKE {}",
                    b(binds, &format!("%{}%", like_escape(value)))
                )),
                None | Some("text") => Ok(format!(
                    "{c} ILIKE {}",
                    b(binds, &format!("{}%", like_escape(value)))
                )),
                Some(m) => Err(StoreError::Other(format!("unsupported modifier :{m}"))),
            }
        }
        TargetKind::Token { system, code } => {
            let code_c = format!("(«c».\"{code}\")::text");
            match value.split_once('|') {
                None => Ok(format!("{code_c} = {}", b(binds, value))),
                Some((sys, cv)) => {
                    let Some(sys_col) = system else {
                        // Bare-code element cannot match a system-qualified
                        // token unless the system part is empty.
                        return if sys.is_empty() {
                            Ok(format!("{code_c} = {}", b(binds, cv)))
                        } else {
                            Ok("FALSE".to_string())
                        };
                    };
                    let sys_c = format!("«c».\"{sys_col}\"");
                    if sys.is_empty() {
                        Ok(format!("({sys_c} IS NULL AND {code_c} = {})", b(binds, cv)))
                    } else if cv.is_empty() {
                        Ok(format!("{sys_c} = {}", b(binds, sys)))
                    } else {
                        Ok(format!(
                            "({sys_c} = {} AND {code_c} = {})",
                            b(binds, sys),
                            b(binds, cv)
                        ))
                    }
                }
            }
        }
        TargetKind::Date { lo, hi } => {
            let lo_c = format!("«c».\"{lo}\"");
            let hi_c = hi.as_ref().map(|h| format!("«c».\"{h}\""));
            date_pred(&lo_c, hi_c.as_deref(), value, binds)
        }
        TargetKind::Number { col } => {
            let (op, num) = num_prefix(value);
            Ok(format!(
                "«c».\"{col}\" {op} ({}::text)::numeric",
                b(binds, num)
            ))
        }
        TargetKind::Quantity {
            value: vcol,
            system,
            code,
        } => {
            let mut parts = value.splitn(3, '|');
            let num = parts.next().unwrap_or("");
            let sys = parts.next();
            let unit = parts.next();
            let (op, num) = num_prefix(num);
            let mut pred = format!("«c».\"{vcol}\" {op} ({}::text)::numeric", b(binds, num));
            if let (Some(sys), Some(sc)) = (sys.filter(|s| !s.is_empty()), system) {
                pred = format!("({pred} AND «c».\"{sc}\" = {})", b(binds, sys));
            }
            if let (Some(u), Some(cc)) = (unit.filter(|s| !s.is_empty()), code) {
                pred = format!("({pred} AND «c».\"{cc}\" = {})", b(binds, u));
            }
            Ok(pred)
        }
        TargetKind::Reference {
            c_type,
            c_id,
            c_url,
        } => {
            let ty_c = format!("«c».\"{c_type}\"");
            let id_c = format!("«c».\"{c_id}\"");
            let url_c = format!("«c».\"{c_url}\"");
            if value.contains("://") || value.starts_with("urn:") || value.starts_with('#') {
                Ok(format!("{url_c} = {}", b(binds, value)))
            } else if let Some((ty, id)) = value.split_once('/') {
                Ok(format!(
                    "({ty_c} = {} AND {id_c} = {})",
                    b(binds, ty),
                    b(binds, id)
                ))
            } else {
                let tymod = modifier.filter(|m| m.chars().next().is_some_and(char::is_uppercase));
                match tymod {
                    Some(ty) => Ok(format!(
                        "({ty_c} = {} AND {id_c} = {})",
                        b(binds, ty),
                        b(binds, value)
                    )),
                    None => Ok(format!("{id_c} = {}", b(binds, value))),
                }
            }
        }
        TargetKind::Uri { col } => {
            let _ = def;
            Ok(format!("«c».\"{col}\" = {}", b(binds, value)))
        }
    }
}

/// FHIR date comparison against a derived sort column (plus an end column
/// for Periods, where equality means overlap).
fn date_pred(
    lo_col: &str,
    hi_col: Option<&str>,
    value: &str,
    binds: &mut Vec<String>,
) -> Result<String, StoreError> {
    let (prefix, v) = date_prefix(value);
    let Some((lo, hi)) = date_bounds(v) else {
        return Err(StoreError::Other(format!("invalid date value {v:?}")));
    };
    // Bind lazily: a parameter PostgreSQL never sees referenced is an error,
    // so each comparison binds only the bound(s) it actually uses.
    let bind = |binds: &mut Vec<String>, v: &str| {
        binds.push(v.to_string());
        format!("(${}::text)::timestamptz", binds.len())
    };
    let lo_val = lo;
    let hi_of = |binds: &mut Vec<String>| match &hi {
        // Full timestamps: exclusive upper bound one second later, computed
        // in SQL to sidestep calendar arithmetic.
        None => {
            let lo_b = bind(binds, &lo_val);
            format!("({lo_b} + interval '1 second')")
        }
        Some(h) => bind(binds, h),
    };
    let end = hi_col.map(|h| h.to_string());
    Ok(match prefix {
        "eq" | "ne" => {
            let inner = match &end {
                // Period overlap: starts before the range ends, ends after
                // it begins (open-ended periods overlap everything later).
                Some(e) => {
                    let hi_b = hi_of(binds);
                    let lo_b = bind(binds, &lo_val);
                    format!(
                        "({lo_col} < {hi_b} AND coalesce({e}, 'infinity'::timestamptz) >= {lo_b})"
                    )
                }
                None => {
                    let lo_b = bind(binds, &lo_val);
                    let hi_b = hi_of(binds);
                    format!("({lo_col} >= {lo_b} AND {lo_col} < {hi_b})")
                }
            };
            if prefix == "eq" {
                inner
            } else {
                format!("NOT {inner}")
            }
        }
        "lt" | "eb" => {
            let lo_b = bind(binds, &lo_val);
            format!("{lo_col} < {lo_b}")
        }
        "gt" | "sa" => {
            let hi_b = hi_of(binds);
            match &end {
                Some(e) => format!("coalesce({e}, 'infinity'::timestamptz) >= {hi_b}"),
                None => format!("{lo_col} >= {hi_b}"),
            }
        }
        "ge" => {
            let lo_b = bind(binds, &lo_val);
            match &end {
                Some(e) => format!("coalesce({e}, 'infinity'::timestamptz) >= {lo_b}"),
                None => format!("{lo_col} >= {lo_b}"),
            }
        }
        "le" => {
            let hi_b = hi_of(binds);
            format!("{lo_col} < {hi_b}")
        }
        other => {
            return Err(StoreError::Other(format!(
                "unsupported date prefix {other:?}"
            )));
        }
    })
}

fn date_prefix(v: &str) -> (&str, &str) {
    match v.get(..2) {
        Some(p @ ("eq" | "ne" | "lt" | "gt" | "ge" | "le" | "sa" | "eb" | "ap")) => (p, &v[2..]),
        _ => ("eq", v),
    }
}

fn num_prefix(v: &str) -> (&'static str, &str) {
    match v.get(..2) {
        Some("eq") => ("=", &v[2..]),
        Some("ne") => ("<>", &v[2..]),
        Some("lt") => ("<", &v[2..]),
        Some("gt") => (">", &v[2..]),
        Some("ge") => (">=", &v[2..]),
        Some("le") => ("<=", &v[2..]),
        _ => ("=", v),
    }
}

/// [start, end) instants for a FHIR date/dateTime of any precision.
/// A None end means "one second after start", computed in SQL.
fn date_bounds(v: &str) -> Option<(String, Option<String>)> {
    fn leap(y: i64) -> bool {
        (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
    }
    fn days_in(y: i64, m: i64) -> i64 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if leap(y) {
                    29
                } else {
                    28
                }
            }
            _ => 0,
        }
    }
    let b = v.as_bytes();
    let year: i64 = v.get(..4)?.parse().ok()?;
    match b.len() {
        4 => Some((
            format!("{year:04}-01-01T00:00:00Z"),
            Some(format!("{:04}-01-01T00:00:00Z", year + 1)),
        )),
        7 => {
            let m: i64 = v.get(5..7)?.parse().ok()?;
            let (ny, nm) = if m == 12 {
                (year + 1, 1)
            } else {
                (year, m + 1)
            };
            Some((
                format!("{year:04}-{m:02}-01T00:00:00Z"),
                Some(format!("{ny:04}-{nm:02}-01T00:00:00Z")),
            ))
        }
        10 => {
            let m: i64 = v.get(5..7)?.parse().ok()?;
            let d: i64 = v.get(8..10)?.parse().ok()?;
            let (ny, nm, nd) = if d < days_in(year, m) {
                (year, m, d + 1)
            } else if m == 12 {
                (year + 1, 1, 1)
            } else {
                (year, m + 1, 1)
            };
            Some((
                format!("{year:04}-{m:02}-{d:02}T00:00:00Z"),
                Some(format!("{ny:04}-{nm:02}-{nd:02}T00:00:00Z")),
            ))
        }
        _ => {
            // Full dateTime: [t, t + 1 second), upper bound left to SQL.
            if !v.contains('T') {
                return None;
            }
            Some((v.to_string(), None))
        }
    }
}

fn like_escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
