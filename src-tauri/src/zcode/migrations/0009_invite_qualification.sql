-- 邀请资格字段(对应官方奖励中心 incentive/referral/summary 的 inviter 节点):
-- invite_code       该账号自己的邀请码(8 位,字符集 23456789ABCDEFGHJKLMNPQRSTUVWXYZ)
-- invite_state      active=已获得邀请资格 / locked=冷却中(官方按账号年龄/邀请数卡) / NULL=未查过
-- invite_unlock_at  locked 时官方给出的预计开放时间(unix 秒;active 时为 NULL)
-- invited_count     该账号已成功邀请的人数(inviter.invited.total)
-- invite_synced_at  资格查询时间(unix 秒)
-- 捆绑结果仍走既有的 invite_role/invited_by 两列。
ALTER TABLE accounts ADD COLUMN invite_code TEXT;
ALTER TABLE accounts ADD COLUMN invite_state TEXT;
ALTER TABLE accounts ADD COLUMN invite_unlock_at INTEGER;
ALTER TABLE accounts ADD COLUMN invited_count INTEGER;
ALTER TABLE accounts ADD COLUMN invite_synced_at INTEGER;
