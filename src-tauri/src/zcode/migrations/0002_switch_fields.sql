-- 账号切换所需字段:写回 credentials.json 时的完整身份信息
ALTER TABLE accounts ADD COLUMN user_info TEXT;
ALTER TABLE accounts ADD COLUMN zai_access_token TEXT;
ALTER TABLE accounts ADD COLUMN zai_refresh_token TEXT;
