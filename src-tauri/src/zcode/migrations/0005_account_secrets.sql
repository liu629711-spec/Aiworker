-- 账密管理:每个账号可录入登录名与密码(本地明文落库,与其他凭证同库同策略)
CREATE TABLE account_secrets (
  account_id INTEGER PRIMARY KEY,
  login_name TEXT NOT NULL DEFAULT '',
  password TEXT NOT NULL DEFAULT '',
  updated_at INTEGER NOT NULL
);
