-- 账号表:多账号凭证与状态。
-- 注意:jwt / refresh_token 计划在实现录入时用 Windows DPAPI 加密后落库,当前为骨架占位列。
CREATE TABLE accounts (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    label         TEXT    NOT NULL,
    user_id       TEXT,
    jwt           TEXT    NOT NULL DEFAULT '',
    refresh_token TEXT    NOT NULL DEFAULT '',
    device_mid    TEXT    NOT NULL,
    status        TEXT    NOT NULL DEFAULT 'stored',
    is_current    INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
