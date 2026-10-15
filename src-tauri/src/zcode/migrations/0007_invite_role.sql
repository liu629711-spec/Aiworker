-- 邀请关系标识(对应 ZCode 奖励中心的邀请活动):
-- invite_role: inviter=邀请者(主号,持有邀请链接), invited=被邀请者(通过链接注册的小号), NULL=未标记
-- invited_by : 被邀请者归属的邀请者账号 id(绑定关系,可空=未指定)
ALTER TABLE accounts ADD COLUMN invite_role TEXT;
ALTER TABLE accounts ADD COLUMN invited_by INTEGER;
