use crate::common::AgentError;

pub fn create_all_tables(conn: &duckdb::Connection) -> Result<(), AgentError> {
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| AgentError::Bootstrap(format!("create_all_tables: {}", e)))?;

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
    fn creates_exactly_seven_tables() {
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
                "model",
                "permission_grants",
                "usage_method",
                "web_fetch_audit",
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
                "extracted_chars",
                "http_code",
                "id",
                "url",
            ]
        );
    }
}
