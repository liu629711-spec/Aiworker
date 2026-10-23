-- 套餐画像落库(方案见 docs/pending/ZCode套餐池接入方案.md 阶段一/二)
--
-- 背景:billing/balance 只能告诉我们"有没有套餐",不告诉我们"这个账号能用哪个模型、
-- 哪个时段能用、先用谁的额度"。billing/current 才有 capabilities/period/priority,
-- 归一化后落在下面这组字段里,供 api_server::pool 选号使用。
--
-- plan_kind            归一化形态:new_account|task_card|global_build|start_plan|other
-- plan_servable_models 该账号可服务的模型(逗号分隔,catalog id);**空 = 不可用于推理**
-- plan_grant_units     权益发放总量(token)
-- plan_used_units      已消耗量
-- plan_remaining_units 剩余量(接口没给时按 grant-used 推算)
-- plan_period          聚合时段口径:daily|daily_night|…
-- plan_night_only      1 = 只有夜间档权益(白天选号必须跳过)
-- plan_ends_at         最早到期时间(unix 秒;同形态内"先过期先烧"的排序键)
-- plan_profile_json    完整 PlanProfile 序列化(含 grants 明细,UI 展开用)
-- profile_synced_at    画像同步时间(unix 秒;NULL = 从未同步过,按无套餐处理)
ALTER TABLE accounts ADD COLUMN plan_kind TEXT;
ALTER TABLE accounts ADD COLUMN plan_servable_models TEXT;
ALTER TABLE accounts ADD COLUMN plan_grant_units INTEGER;
ALTER TABLE accounts ADD COLUMN plan_used_units INTEGER;
ALTER TABLE accounts ADD COLUMN plan_remaining_units INTEGER;
ALTER TABLE accounts ADD COLUMN plan_period TEXT;
ALTER TABLE accounts ADD COLUMN plan_night_only INTEGER;
ALTER TABLE accounts ADD COLUMN plan_ends_at INTEGER;
ALTER TABLE accounts ADD COLUMN plan_profile_json TEXT;
ALTER TABLE accounts ADD COLUMN profile_synced_at INTEGER;
