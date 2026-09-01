-- Active DuckDB schema. Durable memory and workspace data live outside DuckDB.

CREATE TABLE IF NOT EXISTS model (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    provider TEXT NOT NULL,
    api_url TEXT NOT NULL,
    api_type TEXT NOT NULL DEFAULT 'OpenAI',
    -- 协议版本路由 (2026-07-26 llm-system-param-fix): openai-v1 | anthropic-messages | openai-compatible | responses-v1
    api_protocol TEXT NOT NULL DEFAULT 'openai-v1',
    api_key TEXT NOT NULL DEFAULT '',
    model_id TEXT NOT NULL,
    config JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS agent (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    mode TEXT NOT NULL,
    prompt TEXT,
    capability_allowlist JSON NOT NULL DEFAULT '[]',
    config JSON,
    display_name TEXT,
    is_default BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS base_capability (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    type TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    schema_in JSON NOT NULL,
    schema_out JSON NOT NULL,
    executor TEXT NOT NULL,
    version TEXT NOT NULL DEFAULT '',
    enabled BOOLEAN NOT NULL DEFAULT FALSE,
    tombstoned_at TIMESTAMP,
    metadata JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS composite_capability (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    schema_in JSON,
    schema_out JSON,
    executor TEXT,
    dag JSON NOT NULL,
    version TEXT NOT NULL DEFAULT '',
    enabled BOOLEAN NOT NULL DEFAULT FALSE,
    tombstoned_at TIMESTAMP,
    metadata JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS usage_method (
    id TEXT PRIMARY KEY,
    capability_id TEXT NOT NULL,
    name TEXT NOT NULL,
    prompt TEXT NOT NULL,
    examples JSON,
    metadata JSON,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- 运行时授权审计表 (v0.4.4): permission.grant/revoke 全量落库。
CREATE TABLE IF NOT EXISTS permission_grants (
    id TEXT PRIMARY KEY,
    granted_at TEXT NOT NULL,
    granter_agent TEXT NOT NULL,
    target_agent TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    mode TEXT NOT NULL,
    ttl_secs INTEGER,
    expires_at TEXT,
    used_at TEXT,
    revoked_at TEXT,
    status TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- web.fetch.public 网络抓取审计表 (v0.4.6): 每次调用全量落库（纯审计，不进 Registry）。
-- error 为空字符串 = 成功；非空 = 结构化错误 code（如 domain_not_allowed / redirect_rejected / size_limit_exceeded）。
CREATE TABLE IF NOT EXISTS web_fetch_audit (
    id TEXT PRIMARY KEY,
    called_at TEXT NOT NULL,
    called_by TEXT NOT NULL,
    url TEXT NOT NULL,
    http_code INTEGER,
    bytes INTEGER,
    extracted_chars INTEGER,
    error TEXT
);
