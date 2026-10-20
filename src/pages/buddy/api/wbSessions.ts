/**
 * WorkBuddy 多账号会话（buddy-sessions）的前端接口层：
 * 页面只允许从这里拿数据/调接口，禁止直接 invoke。
 *
 * 对应 Rust 命令族 zcode_wb_*（zcode::modules::workbuddy::commands）：
 * 会话文件捕获/切换/海外登录向导，与 Buddy 账号池（workbuddy_*）是两套机制——
 * 本层管「登录态快照库」（整份会话文件），账号池管「凭证 + 网关/签到」。
 */
import { invoke } from '@tauri-apps/api/core';
import type { WbAccount, WbStatus } from '../../../types';

export const wbSessionsApi = {
  /** 当前登录态 + 客户端运行状态 */
  status: () => invoke<WbStatus>('zcode_wb_status'),
  /** 已捕获的账号列表(含 is_current 标记) */
  list: () => invoke<WbAccount[]>('zcode_wb_list'),
  /** 捕获 WorkBuddy 当前登录的账号(重复捕获即刷新会话) */
  capture: () => invoke<WbAccount>('zcode_wb_capture'),
  /** 切换到此账号:退出 WorkBuddy → 替换会话文件 → 重新启动 */
  switchTo: (id: number) => invoke<void>('zcode_wb_switch', { id }),
  remove: (id: number) => invoke<void>('zcode_wb_remove', { id }),
  /** 重命名备注 */
  rename: (id: number, label: string | null) =>
    invoke<void>('zcode_wb_rename', { id, label }),
  /** 自动探测失败时手动指定 WorkBuddy.exe 路径 */
  setExePath: (path: string) => invoke<void>('zcode_wb_set_exe_path', { path }),
  /** 海外登录向导:登出当前账号并以海外配置(登录页指 workbuddy.ai)重启 WorkBuddy */
  launchOverseaLogin: () => invoke<void>('zcode_wb_launch_oversea_login'),
};
