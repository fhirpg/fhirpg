//! DDL emission: the map, rendered as PostgreSQL CREATE statements.
//! Deterministic — same map, same statements, same order.

use std::fmt::Write as _;

use crate::model::{ColTy, RelMap, ResourceMap, Table, TableKind};

pub fn col_sql(ty: ColTy) -> &'static str {
    match ty {
        ColTy::Bool => "boolean",
        ColTy::Int => "integer",
        ColTy::BigInt => "bigint",
        ColTy::Numeric => "numeric",
        ColTy::Text => "text",
        ColTy::Date => "date",
        ColTy::Timestamptz => "timestamptz",
        ColTy::Jsonb => "jsonb",
    }
}

/// All statements to install one version's schema, in application order.
pub fn ddl(map: &RelMap) -> Vec<String> {
    ddl_in(map, &map.schema)
}

/// The same statements, targeting an explicit schema name (used to stage an
/// install under a temporary schema and rename it into place atomically).
pub fn ddl_in(map: &RelMap, schema: &str) -> Vec<String> {
    let mut out = Vec::new();
    let s = schema;
    out.push(format!("CREATE SCHEMA IF NOT EXISTS \"{s}\""));
    out.push(format!(
        "CREATE TABLE \"{s}\".\"fhirpg_meta\" (\"key\" text PRIMARY KEY, \"value\" text NOT NULL)"
    ));
    for rm in map.resources.values() {
        for t in &rm.tables {
            out.push(create_table(s, rm, t));
        }
        out.extend(search_indexes(s, rm));
    }
    out
}

/// One index per distinct search-target column set (P6.4). Index names share
/// the relation namespace with tables, so they get a `_ix` suffix and the
/// same 63-byte discipline.
fn search_indexes(schema: &str, rm: &ResourceMap) -> Vec<String> {
    use crate::model::TargetKind;
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for def in &rm.search {
        for t in &def.targets {
            let cols: Vec<&str> = match &t.kind {
                TargetKind::Str { col } | TargetKind::Number { col } | TargetKind::Uri { col } => {
                    vec![col]
                }
                TargetKind::Token { system, code } => match system {
                    Some(s) => vec![s, code],
                    None => vec![code],
                },
                TargetKind::Date { lo, hi } => match hi {
                    Some(h) => vec![lo, h],
                    None => vec![lo],
                },
                TargetKind::Quantity { value, .. } => vec![value],
                TargetKind::Reference { c_type, c_id, .. } => vec![c_type, c_id],
            };
            let table = &rm.tables[t.table as usize].name;
            let key = format!("{table}:{}", cols.join(","));
            if !seen.insert(key) {
                continue;
            }
            let name = index_name(table, &cols);
            let collist: Vec<String> = cols.iter().map(|c| format!("\"{c}\"")).collect();
            out.push(format!(
                "CREATE INDEX \"{name}\" ON \"{schema}\".\"{table}\" ({})",
                collist.join(", ")
            ));
        }
    }
    out
}

fn index_name(table: &str, cols: &[&str]) -> String {
    let full = format!("{table}_{}_ix", cols.join("_"));
    if full.len() <= 63 {
        return full;
    }
    // FNV-1a of the full name keeps truncated names unique and stable.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in full.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    let hex = format!("{h:016x}");
    let keep: String = full.chars().take(63 - 17).collect();
    format!("{keep}_{hex}")
}

fn create_table(schema: &str, rm: &ResourceMap, t: &Table) -> String {
    let base = &rm.base_table().name;
    let mut sql = format!("CREATE TABLE \"{schema}\".\"{}\" (\n", t.name);
    match t.kind {
        TableKind::Base => {
            sql.push_str("  \"id\" text PRIMARY KEY,\n");
            sql.push_str("  \"version_id\" bigint NOT NULL,\n");
            sql.push_str("  \"last_updated\" timestamptz NOT NULL");
            push_data_cols(&mut sql, t);
        }
        TableKind::Elem => {
            let _ = write!(
                sql,
                "  \"rid\" text NOT NULL REFERENCES \"{schema}\".\"{base}\" (\"id\") ON DELETE CASCADE,\n  \"ords\" smallint[] NOT NULL"
            );
            push_data_cols(&mut sql, t);
            sql.push_str(",\n  PRIMARY KEY (\"rid\", \"ords\")");
        }
        TableKind::Ext => {
            let _ = write!(
                sql,
                "  \"rid\" text NOT NULL REFERENCES \"{schema}\".\"{base}\" (\"id\") ON DELETE CASCADE,\n\
                 \x20 \"path\" text NOT NULL,\n\
                 \x20 \"ords\" smallint[] NOT NULL,\n\
                 \x20 \"modifier\" boolean NOT NULL,\n\
                 \x20 \"ext_ord\" smallint NOT NULL,\n\
                 \x20 \"url\" text,\n\
                 \x20 \"leaf\" text NOT NULL,\n\
                 \x20 \"v_kind\" char(1) NOT NULL,\n\
                 \x20 \"v_text\" text,\n\
                 \x20 \"v_num\" numeric,\n\
                 \x20 \"v_bool\" boolean,\n\
                 \x20 PRIMARY KEY (\"rid\", \"path\", \"ords\", \"modifier\", \"ext_ord\", \"leaf\")"
            );
        }
        TableKind::Deep => {
            let _ = write!(
                sql,
                "  \"rid\" text NOT NULL REFERENCES \"{schema}\".\"{base}\" (\"id\") ON DELETE CASCADE,\n\
                 \x20 \"path\" text NOT NULL,\n\
                 \x20 \"ords\" smallint[] NOT NULL,\n\
                 \x20 \"leaf\" text NOT NULL,\n\
                 \x20 \"v_kind\" char(1) NOT NULL,\n\
                 \x20 \"v_text\" text,\n\
                 \x20 \"v_num\" numeric,\n\
                 \x20 \"v_bool\" boolean,\n\
                 \x20 PRIMARY KEY (\"rid\", \"path\", \"ords\", \"leaf\")"
            );
        }
        TableKind::Contained => {
            let _ = write!(
                sql,
                "  \"rid\" text NOT NULL REFERENCES \"{schema}\".\"{base}\" (\"id\") ON DELETE CASCADE,\n\
                 \x20 \"ord\" smallint NOT NULL,\n\
                 \x20 \"resource\" jsonb NOT NULL,\n\
                 \x20 PRIMARY KEY (\"rid\", \"ord\")"
            );
        }
        TableKind::History => {
            let _ = write!(
                sql,
                "  \"id\" text NOT NULL,\n\
                 \x20 \"version_id\" bigint NOT NULL,\n\
                 \x20 \"last_updated\" timestamptz NOT NULL,\n\
                 \x20 \"op\" char(1) NOT NULL,\n\
                 \x20 \"resource\" jsonb,\n\
                 \x20 PRIMARY KEY (\"id\", \"version_id\")"
            );
        }
    }
    sql.push_str("\n)");
    sql
}

fn push_data_cols(sql: &mut String, t: &Table) {
    for c in &t.cols {
        let _ = write!(sql, ",\n  \"{}\" {}", c.name, col_sql(c.ty));
    }
}
