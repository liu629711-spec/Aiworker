-- 账密管理重构:单表同时容纳"关联已导入账号"(account_id 非空)与"独立录入"条目
DROP TABLE IF EXISTS account_secrets;
CREATE TABLE secret_entries (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id INTEGER UNIQUE,
  title TEXT NOT NULL DEFAULT '',
  login_name TEXT NOT NULL DEFAULT '',
  password TEXT NOT NULL DEFAULT '',
  updated_at INTEGER NOT NULL
);
