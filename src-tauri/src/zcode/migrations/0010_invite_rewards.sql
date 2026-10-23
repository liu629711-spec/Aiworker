-- 邀请奖励卡进度(对应官方奖励中心 referral/summary 的 inviter.rewards):
-- night_card_days   已发放的夜间蹬蹬卡天数(每邀 1 个完成新手任务的新号 +1,本期上限 100)
-- night_card_state  发放状态:in_progress=进行中 / achieved=已达成 / issued=已发放
-- weekend_progress  周末蹬蹬卡进度(每周邀满 2 个新号即可参与本周周末活动)
-- weekend_target    周末卡目标(官方给 2)
-- weekly_reset_used 周重置卡已用次数(需 Coding Plan 订阅;本期邀满 10 个发一次)
--
-- 被邀请者(新号)侧的新手任务完成度(invitee.tasks:login_device/referral_bind_code/send_message):
-- task_login_device / task_bind_code / task_send_message 均 0/1/NULL
-- invitee_state    done=新手任务已全部完成(夜间卡已发到账)
ALTER TABLE accounts ADD COLUMN night_card_days INTEGER;
ALTER TABLE accounts ADD COLUMN night_card_state TEXT;
ALTER TABLE accounts ADD COLUMN weekend_progress INTEGER;
ALTER TABLE accounts ADD COLUMN weekend_target INTEGER;
ALTER TABLE accounts ADD COLUMN weekly_reset_used INTEGER;
ALTER TABLE accounts ADD COLUMN task_login_device INTEGER;
ALTER TABLE accounts ADD COLUMN task_bind_code INTEGER;
ALTER TABLE accounts ADD COLUMN task_send_message INTEGER;
ALTER TABLE accounts ADD COLUMN invitee_state TEXT;
