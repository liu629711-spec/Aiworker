-- 窗口位置记忆:key = 窗口 label,x/y 为屏幕坐标(保存时不校验屏幕范围,恢复时由前端 Window API 裁剪)
CREATE TABLE window_state (
  label TEXT PRIMARY KEY,
  x INTEGER NOT NULL,
  y INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
