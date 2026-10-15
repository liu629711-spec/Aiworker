-- WorkBuddy 账号库:每个条目 = 一份完整的官方登录会话文件(workbuddy-desktop.info 快照)
-- 会话文件本身含加密 token(官方 static protector 信封),整体存取、不解密不改写。
CREATE TABLE wb_accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    uid TEXT NOT NULL,              -- 官方 account.uid(登录账号唯一标识)
    label TEXT,                     -- 用户自定义备注名(默认用 uid 短标识)
    uin TEXT,                       -- 官方 uin(明文,可作辅助标识)
    region TEXT NOT NULL,           -- domestic(国内) | overseas(海外),由 auth.domain 判定
    domain TEXT,                    -- 登录域(明文),如 www.workbuddy.cn / www.workbuddy.ai
    session_blob BLOB NOT NULL,     -- 原始 .info 文件内容(密文整体保存)
    captured_at INTEGER NOT NULL,   -- 捕获时间(epoch 秒)
    updated_at INTEGER NOT NULL,
    UNIQUE(uid, region)             -- 同一账号在国内/海外是两个独立会话
);
