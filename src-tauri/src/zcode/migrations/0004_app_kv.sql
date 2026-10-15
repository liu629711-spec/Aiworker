-- 应用级 KV 存储(快照持久化等)
CREATE TABLE app_kv (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
