-- ZCode 夜间/周末批处理队列(2026-10-23)
--
-- 背景:ZCode 的 Start Plan / Global Build 额度只在客户端通道可用,而夜间档
-- (23:00→次日 09:00)额度最可观(id17/id30 各 1e8)。日间额度小(3M)且单次
-- agent 回合就要吃掉约 26 万 token,所以「在线网关」不成立,「夜间批处理」成立。
--
-- 这张表就是批处理队列:用户先把任务排进来,调度器只在窗口内取任务执行。
--
-- status          pending | running | done | failed | canceled
-- provider_used   归因字段:执行后从 ZCode CLI 日志里读到的真实 providerId。
--                 **这是判断"到底烧的是免费套餐还是自有中转池"的唯一证据**,
--                 不能靠配置推断(CLI 会静默回退到 providerOrder[0])。
-- window_tag      执行时命中的窗口:night(夜间档) / forced(手动强制) / off(窗外)
CREATE TABLE IF NOT EXISTS batch_tasks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    batch_id        TEXT,
    prompt          TEXT    NOT NULL,
    model           TEXT    NOT NULL DEFAULT 'GLM-5.3',
    status          TEXT    NOT NULL DEFAULT 'pending',
    result          TEXT,
    error           TEXT,
    provider_used   TEXT,
    model_used      TEXT,
    input_tokens    INTEGER,
    output_tokens   INTEGER,
    duration_ms     INTEGER,
    window_tag      TEXT,
    attempts        INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    started_at      INTEGER,
    finished_at     INTEGER
);

CREATE INDEX IF NOT EXISTS idx_batch_tasks_status ON batch_tasks(status, id);
CREATE INDEX IF NOT EXISTS idx_batch_tasks_batch ON batch_tasks(batch_id);
