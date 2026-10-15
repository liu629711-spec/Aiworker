//! zcode 域:自包含的 z.ai 账号/检测/领取/WorkBuddy 模块(移植自 zai-claim)。
//! 使用独立 sqlx SQLite 池(zcode.sqlite),与主项目 rusqlite 存储互不干扰。

pub mod api;
pub mod credentials;
pub mod db;
pub mod error;
pub mod modules;
pub mod state;
pub mod util;
