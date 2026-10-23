//! 注册域:马甲邮箱注册 → 邮箱验证 → 入池 → 邀请捆绑 的全自动编排。
//! - `imap.rs`      :Gmail IMAP 收验证邮件(注册链路用)
//! - `flow_script.rs`:注入注册/验证/授权页面的自动化脚本
//! - `flow.rs`      :把注册/验证/授权/入池/捆绑串成一条流水线,进度经事件广播

pub mod commands;
pub mod flow;
pub mod flow_script;
pub mod imap;
pub mod relay;
pub mod tasks;
