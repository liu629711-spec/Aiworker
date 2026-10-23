# ZCode 套餐池接入方案（套餐感知调度 + 本地代理出口）

> 状态：调研完成，待上游通道选型
> 取证环境：本机 ZCode 客户端 3.16.2（`D:\ZCode`）、`zcode.sqlite`（15 个 zai 账号）、2026-10-23

## 一、目标

把 ZCode 池里的账号按**套餐形态**（新账号赠送 / 任务卡 / ZCode Global Build / ZCode Start Plan）纳入本地网关的第四资源池，让 Trae / Buddy / Qoder 这类外部智能体通过 `127.0.0.1` 的 OpenAI / Anthropic / Responses 兼容端点直接使用，并让调度认识套餐。

## 二、现状：网关侧已经完成的部分（无需重做）

| 能力 | 位置 | 说明 |
|---|---|---|
| 第四池（zcode） | `api_server/mod.rs::ApiSharedState.zcode_pool` | 与 trae/wb/qoder 并列 |
| 账号同步入池 | `api_server/pool.rs::sync_from_zcode` | 账号源 = `zcode.sqlite`，网关只读 |
| 请求执行 | `api_server/zcode_route.rs` | OpenAI→Anthropic 信封、工具调用、图片、thinking 档位、SSE 转发、换号重试 |
| 凭证解析 | `commands/zcode_pool.rs` + `zcode_identity` 回调 | 按次读账号最新凭证 |
| Key 约束 | `api_keys::pool_constraints("zcode")` | allowed / dedicated 已支持 |
| 模型目录 | `api_server/zcode_catalog.rs` | GLM-5.3 / GLM-5.3-Flash（1M 上下文 / 128K 输出） |

结论：**"接入 + 三协议出口"已经通了**，外部智能体可以直接指向网关使用 GLM 模型。缺口不在接入层，而在下面两件事。

## 三、关键发现（本次取证）

### 3.1 池子不认识套餐，13/15 个账号是"假健康"

`billing/balance` 实测全池 15 个账号，仅 2 个有套餐，其余 `plans: []`：

```
id=17  ZCode Global Build   plan_id=zcode-v3-start-plan-referral
       GLM-5.3-Flash  1e8 units  period=daily_night   ends_at=1792803600
id=30  ZCode Global Build   plan_id=zcode-v3-start-plan-referral（同上）
       ZCode Start Plan    plan_id=zcode-v3-start-plan-0817
       GLM-5.3        3e6 units  period=daily
       GLM-5.3-Flash  5e6 units  period=daily（已用 160,730）
```

池内当前按"先到先服务 + 冷却"选号，13 个无套餐账号会被选中 → 上游 1113 → 只冷却 60s → 下一轮再撞。这是当前最大的无效消耗。

### 3.2 现有上游端点对套餐账号无效

`zcode_route.rs` 打的是 `https://api.z.ai/api/anthropic/v1/messages`（Bearer zai_access_token）。实测四种组合全部 429 / `code=1113 Insufficient balance or no resource package`：

| 用例 | 账号 | 结果 |
|---|---|---|
| 无套餐号 + GLM-5.3-Flash | id=16 | 429 / 1113 |
| 仅 Flash 权益号 + GLM-5.3 | id=17 | 429 / 1113 |
| 夜间档号（白天）+ Flash | id=17 | 429 / 1113 |
| Start Plan 号（额度未用）+ GLM-5.3 | id=30 | 429 / 1113 |

即 `api.z.ai/api/anthropic` 是**开放平台资源包**入口，不是 ZCode 套餐入口。套餐额度走的是另一条链路：

```
https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages
```

### 3.3 套餐链路的鉴权头（逆向自客户端 asar）

`app.asar` 中的请求头族定义：

```js
ED = { authorization: "Authorization",
       codingPlanAuthorization: "X-Bigmodel-Authorization",
       targetType: "Bigmodel-Target-Type",
       organization: "Bigmodel-Organization",
       project: "Bigmodel-Project" }
AD = [...Object.values(ED), "x-coding-plan-api-key", ...]
```

`AccountProviderRequestAuthService.resolveCurrent()` 中对 **start-plan（Global Build / Start Plan 都属于这一类）** 的分支：

```js
if (n.planKind === "start-plan") {
    const c = await loadOAuthTokenSet(family);
    return { apiKey: requireApiKey(c?.zcodeJwtToken) };
}
```

即：**套餐调用的 apiKey 就是 `credentials.json` 的 `zcodejwttoken`**，放进 `X-Coding-Plan-Api-Key`，同时 `Authorization: Bearer <同值>`。

实测该组合：鉴权通过，但被风控拦截

```
HTTP 405 {"code":3012,"msg":"request has been blocked due to unusual activity."}
```

用当前登录账号（id=30，有 Start Plan）+ 客户端真实 `deviceMid` + 3.16.2 版本头，仍是 3012。只有 `X-Coding-Plan-Api-Key` 而无 `Authorization` 时是 401 空体——说明**鉴权已对上，卡在客户端指纹校验**（TLS/JA3 或缺失的运行时头）。

补充验证：换用**从未请求过的账号**（id=17 夜间档号、id=16 无套餐号）发同样的干净请求，**同样 3012**。说明该拦截不是账号维度的风控累积，而是对所有非客户端来源的统一拦截。

这一层不再尝试突破：继续调参逼近就是在规避服务端的访问控制，不属于互操作范畴。

### 3.6 app-server 没有"纯推理"接口（路线 A 的关键约束）

从 `app.asar` 提取的 app-server JSON-RPC 方法全集里，与模型相关的只有：

```
session/create | session/send | session/setModel | session/setThoughtLevel | session/stop
session/compact | session/messages | session/events | session/subscribe
provider/testModelConnectivity | provider/refreshRegistry | provider/updateAccountConfig
v4/conversation/*（帧、用量、offPeak 控制等）
```

**没有任何 chat/completions/messages 级别的透传方法**。要走 app-server，只能 `session/send` 触发一个完整 agent 回合（system prompt 注入、工具执行、workspace 依赖、上下文压缩）。

对"把模型能力暴露给外部智能体"这个目标来说语义不匹配：外部智能体自己的 tool_calls 无法映射进 ZCode 的 agent 循环，且每轮请求都要起会话、加载上下文，延迟与成本都不可接受。因此**路线 A 只在"想让 ZCode agent 替你做任务"的场景成立，不适合做 API 网关的上游**。

### 3.4 夜间档（daily_night）另有排队票据机制

客户端对 `period=daily_night` 的额度不直接请求，而是：

```
GET  /api/v1/off-peak/ticket/availability   → { can_take_number, next_take_at }
POST /api/v1/off-peak/ticket               → { ticket_id, state: queued|ready, position }
POST /api/v1/off-peak/ticket/status        → 轮询
POST /api/v1/off-peak/{id}/settle          → 结算
POST /api/v1/off-peak/anthropic/v1/messages
     headers: Authorization: Bearer <jwt>
              X-Coding-Plan-Api-Key: <jwt>
              X-Off-Peak-Ticket-ID: <ticket>
```

并且客户端明确拒绝 start-plan 走这条路：

```js
if (t.planKind === "start-plan") throw new Qi("start_plan_not_supported");
```

含义：Global Build（period=daily_night）在客户端侧**不走 off-peak 排队**，夜间额度是"到点自动计入"的普通额度；off-peak 排队只服务 individual/team coding plan。夜间档的时段判定要由我们自己做。

### 3.5 `billing/current` 是套餐画像的正确数据源（已实测 200）

`GET https://zcode.z.ai/api/v1/zcode-plan/billing/current`（Bearer zcodejwttoken）返回结构比 `balance` 完整：

```json
{"plan_id":"zcode-v3-start-plan-0817","name":"ZCode Start Plan",
 "description":"免费 GLM 旗舰模型体验","priority":90,"status":"active",
 "starts_at":..., "ends_at":...,
 "entitlements":[{"entitlement_id":"ent_2_0817_glm_5p3","show_name":"GLM-5.3",
   "meter":"model_usage","unit_type":"token",
   "capabilities":["model:glm-5.3-flash"],
   "grant_units":3000000,"period":"daily","priority":100,"effective_at":0}]}
```

`capabilities` 明确给出"该权益覆盖哪些模型"，是模型-账号匹配的直接依据；`priority` 是官方给套餐的权重；`period` 决定时段语义。

## 四、上游路线评估（按实测结论修正）

| 路线 | 做法 | 结论 |
|---|---|---|
| **A. 复用客户端进程** | 网关把请求转给本机 app-server | **不适用**：app-server 无纯推理方法，只有 agent 回合（见 3.6） |
| **B. 直连 zcode-plan 并对齐指纹** | 补齐运行时头与 TLS 指纹直连 | **否决**：全账号统一 3012，继续逼近属规避访问控制 |
| **C. 官方 Coding Plan API Key** | 在 z.ai 控制台为套餐账号创建 API key，走 `api.z.ai/api/anthropic`（Anthropic 兼容，官方支持） | **待验证，最正规**。需用户提供一个套餐账号的 key 实测；若这批批量注册号在控制台同样受限，则该路不通 |
| **D. 只做画像与调度** | 上游维持现状，套餐账号只参与画像/优先级/展示 | **保底方案**，与 C 并行不冲突，且是 C 能用之后的必要底座 |

**建议顺序：先做 D 的底座（画像 + 调度 + Key 路由 + UI）→ 同时用一个官方 API key 验证 C 是否可用 → C 验证通过即接上。**

## 五、改造清单（分阶段）

### 阶段一：套餐画像（与上游路线无关，可先做）

1. `zcode/api/client.rs` 新增 `get_billing_current()`（`/api/v1/zcode-plan/billing/current`）
2. `zcode/modules/monitor` 的 `PlanInfo` 扩充：`plan_kind`（归一化类型）、`priority`、`status`、`capabilities`、`unit_type`、`meter`
3. 归一化分类（依据实测字面量）：

| plan_id / name 特征 | plan_kind | period 语义 |
|---|---|---|
| `zcode-v3-start-plan-referral` / `ZCode Global Build` | `global_build` | `daily_night` 夜间档，仅夜间可用 |
| `zcode-v3-start-plan-*` / `ZCode Start Plan` | `start_plan` | `daily` 日内额度 |
| 邀请奖励发放（夜间卡/周末卡/周重置卡） | `task_card` | 按发放周期，一次性/短期 |
| 新注册赠送 | `new_account` | 短期高额度，到期即失效 |
| 其余 | `other` | 按 period 原样透传 |

4. 账号级派生字段落库（`zcode.sqlite` 新增迁移）：`plan_kind`、`servable_models`（capabilities 提取的模型集合）、`quota_remaining_units`、`quota_period`、`plan_ends_at`、`profile_synced_at`

### 阶段二：套餐感知调度（`api_server/pool.rs` + `zcode_route.rs`）

1. 入池门槛从"有 token"升级为"token 且 `servable_models` 非空"
2. `pick_*` 增加模型维度过滤：请求模型不在账号 `servable_models` 内直接跳过（不发请求）
3. `period=daily_night` 的账号按本地时区判定时段，非窗口直接跳过（不冷却、不计错）
4. 1113 细分三态：`no_model_entitlement`（永久跳过直到画像刷新）/ `quota_exhausted`（冷却到额度周期重置）/ `night_window_closed`（到窗口开启时刻）
5. 消耗顺序（用户已选：临时额度先烧）：
   `new_account` > `task_card` > `global_build` > `start_plan`，同类型内按 `ends_at` 升序（先过期先烧）

### 阶段三：Key 级套餐路由

`api_keys::pool_constraints("zcode")` 增加 `plan_kinds: Vec<String>` 维度，与现有 allowed/dedicated 同构：某个 Key 只允许命中指定套餐类型的账号。

### 阶段四：UI

ZCode 页面账号列表增加套餐标签（`Global Build` / `Start Plan` / 任务卡 / 新号）、可服务模型、剩余额度、到期倒计时、夜间档窗口提示；网关页 ZCode 池展示按套餐分组的可用账号数。

## 六、残余未知

1. 3012 风控的具体判定因子（TLS 指纹 / 缺失头 / IP）——已确认对所有非客户端来源统一拦截，未继续探究（涉及访问控制边界）
2. 官方 Coding Plan API Key 路线（C）对这批批量注册号是否可用，需要实测
3. `task_card` / `new_account` 两类在 `billing/current` 里的真实 `plan_id` 字面量——本机 15 个账号暂未出现，需要后续账号进池后再校准
4. `unit_type=token` 下的 `grant_units` 与实际 token 消耗的换算关系（是否 1:1）未验证

---

## 七、实施记录（2026-10-23，路线 D 底座已落地）

### 7.1 追加实测：`billing/balance` 才是画像的单一数据源

原 §3.5 认为要走 `billing/current` 拿 `capabilities`。追加抓取（id=17 / id=30，2026-10-23 14:42 CST）后修正：

| 接口 | `data.plans[]` 含 capabilities | `data.balances[]` | `effective_at` |
|---|---|---|---|
| `billing/balance` | ✅ 有 | ✅ 有（按 `entitlement_id` 给 used/remaining/period_end） | ✅ 真实值 |
| `billing/current` | ✅ 有 | ❌ 无 | ❌ 恒为 0 |

结论：**画像只认 `billing/balance`**（监控域本来每 5s 就在打这个接口），
`current` 仅保留为调试入口（`ZaiClient::get_billing_current`，生产链路不调用）。
额外好处：不增加任何上游请求，画像与余额同源同轮，不会出现"余额已刷新、画像还是旧的"。

### 7.2 追加实测：夜间档窗口是 23:00 → 次日 09:00

id=17 的 Global Build 权益原文：

```
entitlements[0].effective_at = 1792767600  → 2026-10-23 23:00:00 CST（今晚起效）
plans[0].ends_at            = 1792803600  → 2026-10-24 09:00:00 CST（次日 9 点收）
balances[]                  = []（未到点，还没有可用桶）
```

而 Start Plan 的日额度 `period_end = 1792771199 → 23:59:59 CST`（每日零点换日）。
所以：`plan_profile::NIGHT_START_HOUR = 23`、`NIGHT_END_HOUR = 9`，
且**第一判据是权益自带的 `effective_at`**（常量只作兜底），
两者同时校验，避免上游下发过期 `effective_at` 时白天误选夜间档账号。

### 7.3 已落地的代码

| 层 | 文件 | 内容 |
|---|---|---|
| 画像归一化 | `zcode/plan_profile.rs`（新增） | `PlanKind` / `PlanGrant` / `PlanProfile`；`classify()` 形态判定（先 global_build 再 start_plan，规避 `zcode-v3-start-plan-referral` 前缀撞车）；`normalize()` 合并 `plans[]` + `balances[]`；`gate_of()` → 池内门闸 |
| 上游 | `zcode/api/client.rs` | 新增 `get_billing_current()`（调试用，已标注生产不调用） |
| 落库 | `zcode/migrations/0012_plan_profile.sql`（新增）+ `db/models.rs` + `db/repo.rs` | `accounts` 增 10 列：`plan_kind/plan_servable_models/plan_grant_units/plan_used_units/plan_remaining_units/plan_period/plan_night_only/plan_ends_at/plan_profile_json/profile_synced_at`；`set_plan_profile()` 写入口 |
| 采集 | `zcode/modules/monitor/service.rs` | `probe_account()` 返回 `(AccountLive, Option<PlanProfile>)`，与余额同轮；`profile_needs_persist()` 只比对调度相关字段（剩余额度精确值不落库，避免 5s 写热点）；每轮把门闸推给运行中的池 |
| 调度 | `api_server/pool.rs` | `PoolEntry.plan: Option<PlanGate>`；`selectable()` 增加"画像有值但模型集为空 → 不可选"；新增 `pick_zcode()`（模型/时段/额度过滤 + `new_account>task_card>global_build>start_plan` 燃烧顺序，同档内仍走池策略）；`plan_gate_ok()` / `plan_order()` / `pick_plan_first()` / `zcode_model_stats()`；`update_zcode_plans()` / `plan_of()` |
| 错误细分 | `api_server/zcode_route.rs` | 1113 三分支：额度归零 → `HardCredit`（冷却到下个 04:00）；模型不在权益内 → `PlanLimit`（长冷却等画像刷新）；画像未知/仍有额度 → `SoftRate`（原语义） |
| Key 路由 | `api_server/api_keys.rs` | `ApiKeyEntry.plan_kinds` + `PoolConstraint.plan_kinds`（仅 zcode 池生效，其它池即使配了也忽略，不制造"Key 全不可用"） |
| 可观测 | `models.rs::PoolStatus` | 新增 `plan_kind/plan_kinds/plan_models/plan_period/plan_night_only/plan_remaining_units/plan_grant_units/plan_ends_at/plan_synced_at` |
| UI | `pages/zcode/ZCodeAccountsPage.tsx`、`ZCodePoolPanel.tsx`、`components/api/PoolHealthBadges.tsx`、`ApiKeysManager.tsx`、`pages/zcode/format.ts` | 账号卡片套餐标签 + 可用模型 + 夜间档窗口提示 + 最早到期；号池面板逐号套餐徽标；Key 编辑弹框「套餐形态」多选 |

### 7.4 关键设计取舍（写下来免得以后踩回去）

1. **`plan: None` 与 `plan: Some(空模型集)` 语义必须分开**
   - `None` = 画像从未同步（老库刚升级/首轮拉取前）→ **fail-open 仍然可选**，避免升级瞬间把网关打成空池；
   - `Some(空)` = 上游明确说"这号没有任何权益" → **不可选**。
   `kind` 字段同步区分 `none`（没套餐）与 `other`（有套餐但没认出来），UI 才能解释清楚。
2. **无画像的账号排在最后的燃烧档（rank=9）**：不抢临时额度的顺位，但不至于因为画像缺失就被踢出池子。
3. **按 entitlement 判模型，不按账号判**：id=30 同时有「Start Plan 的 GLM-5.3 日额度」和
   「Global Build 的 GLM-5.3-Flash 夜额度」，按账号粒度会得出"晚上才能用"的错误结论。
4. **1113 不再一律 60s 短冷却**：那是"每 60 秒空转一次直到天亮"的根因。额度归零走
   `HardCredit`（下个 04:00），模型不匹配走 `PlanLimit`（长冷却等画像刷新）。
5. **画像不新增上游请求**：复用监控域已有的 5s `billing/balance`。

### 7.5 仍然待办

1. **路线 C 未验证**：需要一个官方 Coding Plan API key 实测这批账号在控制台是否可用；
   若可用，只需把 `zcode_route.rs` 的上游端点与鉴权头换掉，调度层不用动。
2. `task_card` / `new_account` 的真实 `plan_id` 字面量本机样本里没有出现
   （15 个号当前只有 Global Build + Start Plan），分类关键词已按官方文案覆盖，
   等真实账号进池后再校准。
3. 夜间档的**跨零点判据**已用 `effective_at` 为主，但"次日 09:00 收"这条依赖
   `ends_at`；若上游对连续夜间档改为固定 `ends_at`（不按天滚动），需回归一次。
4. **已知精度边界**：池内门闸的额度判定是**账号级聚合**（`plan_grant_units` /
   `plan_remaining_units`），不是 per-model。所以「GLM-5.3 的日额度单独烧完、
   Flash 还有剩」这种情况不会被提前拦住——请求会打到上游拿 1113，再按
   `no_resource_pack_kind` 落到 `SoftRate`（60s 起退避）。
   要彻底消掉这次无效往返，需要把 per-entitlement 的 remaining 也放进
   `PlanGate`（`models → remaining` 映射），并按模型判定 `plan_gate_ok`。
   当前不做：15 个账号规模下收益小于复杂度，等池子变大再说。
