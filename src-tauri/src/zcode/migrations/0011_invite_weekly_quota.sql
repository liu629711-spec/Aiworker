-- 邀请名额口径修正(官方实测 2026-10-22,referral/summary 的 inviter.invited):
--   { total: 2, weekly: 0, reward_cap: 100, capped: false }
-- 结论:每人每周上限是 reward_cap(100),不是周末卡门槛 2;weekly 才是本周已邀数。
-- 旧代码拿 invited_count(累计)当"本周已用"、把 cap 写死 2,会让累计邀过 2 人的
-- 主号被误判为"名额已满",自动分配永远跳过它们。
-- invited_weekly 本周已邀人数(inviter.invited.weekly)
-- invite_cap     本周上限(inviter.invited.reward_cap;官方没给时回退 100)
-- invite_capped  官方是否已判定本期封顶(inviter.invited.capped)
ALTER TABLE accounts ADD COLUMN invited_weekly INTEGER;
ALTER TABLE accounts ADD COLUMN invite_cap INTEGER;
ALTER TABLE accounts ADD COLUMN invite_capped INTEGER;
