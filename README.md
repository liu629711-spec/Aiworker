<div align="center">

# AI Work Studio

**AI 应用统一管理工作台 · 整合版**

</div>

## 功能全景

### AI Work 助手原有能力（全部保留）
- **五应用管理**：Trae / WorkBuddy / CodeBuddy / 豆包 / Qoder 多账号签到、登录态切换、积分看板
- **API 网关**：OpenAI / Anthropic / Codex 三协议兼容，trae / buddy / qoder / custom 四池调度
- **成长中心自动化**、设备指纹隔离、凭证加密保管（vault）

### 新增：ZCode 生态（zai-claim 移植）
- **账号管理**：浏览器 OAuth 登录导入 / 本机导入 / CSV 批量账密登录（内嵌窗口自动填表+验证码+授权确认）
- **余额监控**：5 秒余额轮询 / 10 分钟活动检测，数据落库
- **一键切换**：写回 ZCode credentials.json 并重启客户端（含毒环境变量防护）
- **邀请关系**：官方奖励中心接口自动同步主号/小号绑定，分组视图展示
- **WorkBuddy 多账号**：会话文件捕获/切换/重命名、国内/海外账号互切、海外登录向导

## 开发

```bash
pnpm install
pnpm dev        # 前端 + Tauri 开发模式
pnpm build      # 前端构建
cd src-tauri && cargo build --release
```

## 数据位置

- 主应用数据：`%APPDATA%\AIWorkAssistant\`（沿用底座，含 aiwork.sqlite / vault）
- ZCode 域数据：`%APPDATA%\AIWorkAssistant\zcode.sqlite`（独立 sqlx 迁移体系，与主库互不干扰）
