//! WorkBuddy 账号管理:国内/海外多账号捕获与一键切换。
//!
//! 原理(源码逆向结论,见项目日志 2026-10-15):
//! - WorkBuddy 登录态落在全局共享目录
//!   `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`;
//! - 文件里 auth.token 经官方 static protector 信封加密(keyId 机器恒定),
//!   **整体复制到任何实例都能解**——因此切换 = 整文件替换,零解密零改写;
//! - 文件内 auth.domain 为明文:含 workbuddy.ai/codebuddy.ai 即海外会话,
//!   否则国内——官方启动时正据此决定走 .cn 还是 .ai,替换文件即同时切换了站点环境。

pub mod commands;
pub mod service;
