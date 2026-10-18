use crate::common::AgentError;

pub fn create_all_tables(conn: &duckdb::Connection) -> Result<(), AgentError> {
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| AgentError::Bootstrap(format!("create_all_tables: {}", e)))?;

    // v0.5.11 B3：完成状态台账（第 9 表）。DDL 单源于
    // migration::ensure_deliverable_ledger_table（schema.sql 未版本化追加，不重复抄写）。
    crate::data::migration::ensure_deliverable_ledger_table(conn)
        .map_err(|e| AgentError::Bootstrap(format!("create_all_tables ledger: {}", e)))?;

    conn.execute_batch(
        "ALTER TABLE model ADD COLUMN IF NOT EXISTS api_protocol TEXT;\n\
         UPDATE model SET api_protocol = 'openai-v1' WHERE api_protocol IS NULL;",
    )
    .map_err(|e| AgentError::Bootstrap(format!("migrate model.api_protocol: {}", e)))?;
    Ok(())
}

const SCHEMA_SQL: &str = include_str!("../../../data/schema.sql");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_exactly_nine_tables() {
        let connection = duckdb::Connection::open_in_memory().expect("open DuckDB");
        create_all_tables(&connection).expect("create schema");

        let mut statement = connection
            .prepare(
                "SELECT table_name FROM information_schema.tables \
                 WHERE table_schema = 'main' ORDER BY table_name",
            )
            .expect("prepare table query");
        let tables: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .expect("query tables")
            .collect::<duckdb::Result<_>>()
            .expect("read tables");

        assert_eq!(
            tables,
            [
                "agent",
                "base_capability",
                "composite_capability",
                "deliverable_ledger",
                "method_call_audit",
                "model",
                "permission_grants",
                "usage_method",
                "web_fetch_audit",
            ]
        );
    }

    /// v0.5.11 B3：deliverable_ledger 列集（与 migration DDL 契约一致）。
    #[test]
    fn deliverable_ledger_table_has_expected_columns() {
        let connection = duckdb::Connection::open_in_memory().expect("open DuckDB");
        create_all_tables(&connection).expect("create schema");
        let mut statement = connection
            .prepare(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_schema = 'main' AND table_name = 'deliverable_ledger' ORDER BY column_name",
            )
            .expect("prepare column query");
        let columns: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .expect("query columns")
            .collect::<duckdb::Result<_>>()
            .expect("read columns");
        assert_eq!(
            columns,
            [
                "created_at",
                "evidence_refs",
                "id",
                "item",
                "note",
                "owner_agent",
                "state",
                "updated_at",
            ]
        );
    }

    #[test]
    fn web_fetch_audit_table_has_expected_columns() {
        let connection = duckdb::Connection::open_in_memory().expect("open DuckDB");
        create_all_tables(&connection).expect("create schema");
        let mut statement = connection
            .prepare(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_schema = 'main' AND table_name = 'web_fetch_audit' ORDER BY column_name",
            )
            .expect("prepare column query");
        let columns: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .expect("query columns")
            .collect::<duckdb::Result<_>>()
            .expect("read columns");
        assert_eq!(
            columns,
            [
                "bytes",
                "called_at",
                "called_by",
                "error",
                "execut",
                "extracted_chars",
                "http_code",
                "id",
                "url",
            ]
        );
    }

    #[test]
    fn method_call_audit_table_has_expected_columns() {
        let connection = duckdb::Connection::open_in_memory().expect("open DuckDB");
        create_all_tables(&connection).expect("create schema");
        let mut statement = connection
            .prepare(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_schema = 'main' AND table_name = 'method_call_audit' ORDER BY column_name",
            )
            .expect("prepare column query");
        let columns: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .expect("query columns")
            .collect::<duckdb::Result<_>>()
            .expect("read columns");
        assert_eq!(
            columns,
            [
                "called_at",
                "called_by",
                "created_at",
                "error",
                "executed_atoms",
                "granted_capabilities",
                "id",
                "method_id",
                "result",
                "state_machine_state",
                "status",
                "updated_at",
            ]
        );
    }
}
