/**
 * Trae 签到重试工具：对 9074「当前参与用户太多」临时限流做带间隔的重试领取。
 *
 * 用法：node --experimental-sqlite scripts/trae_checkin_retry.mjs [uid] [间隔秒] [次数]
 * 默认 uid=1325345171053931，间隔 75s，最多 10 次（约 12 分钟）。
 * 领成功（code=0）即停；9095=设备已签（今天该设备签过了）；1001=JWT 失效需重新登录。
 */
import { DatabaseSync } from 'node:sqlite';
import crypto from 'node:crypto';
import { join } from 'node:path';

const uid = process.argv[2] ?? '1325345171053931';
const gapSec = Number(process.argv[3] ?? 75);
const maxTry = Number(process.argv[4] ?? 10);

const db = new DatabaseSync(join(process.env.APPDATA, 'AIWorkAssistant', 'data', 'aiwork.sqlite'), { readOnly: true });
const d = JSON.parse(db.prepare('SELECT data FROM accounts WHERE user_id=?').get(uid).data);
const dev = JSON.parse(db.prepare('SELECT data FROM device_map WHERE pk=?').get(uid).data);
if (!d.jwt) throw new Error(`账号 ${uid} 无 JWT，请先在应用内重新登录/续期`);

const hex = (n) => crypto.randomBytes(n).toString('hex');
const auth = d.jwt.trim().startsWith('Cloud-IDE-JWT ') ? d.jwt.trim() : 'Cloud-IDE-JWT ' + d.jwt.trim();

async function claim(tag) {
  const headers = {
    accept: '*/*', 'accept-language': 'zh-CN',
    authorization: auth,
    'content-type': 'application/json',
    'user-agent': 'VSCode 1.107.1 (TRAE SOLO CN)',
    'x-market-client-id': 'VSCode 1.107.1',
    'x-market-user-id': dev.market_user_id ?? '',
    'x-user-region': 'CN', 'x-device-id': dev.device_id,
    'x-lgw-req-sdk-type': '3', 'package-type': 'stable_cn',
    'x-request-id': hex(32), 'x-lscbd-aid': '787976', 'x-lscbd-platform': 'windows',
    'app-version': '0.1.45', 'x-tt-trace-id': '00-' + hex(16) + '-01',
    'vscode-sessionid': dev.session_id ?? '',
    'sec-fetch-dest': 'empty', 'sec-fetch-mode': 'no-cors', 'sec-fetch-site': 'none',
  };
  const r = await fetch('https://api.trae.cn/trae/api/v2/ug/checkin_credits/claim', { method: 'POST', headers, body: '{}' });
  const text = await r.text();
  console.log(`[${new Date().toLocaleTimeString('zh-CN')}] ${tag} HTTP ${r.status} => ${text.slice(0, 300)}`);
  let code = null;
  try { code = JSON.parse(text).code; } catch {}
  return code;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
for (let i = 1; i <= maxTry; i++) {
  const code = await claim(`第 ${i}/${maxTry} 次`);
  if (code === 0) { console.log('领取成功！'); process.exit(0); }
  if (code === 9095) { console.log('9095：该设备今日已签到（换设备或明天再试）'); process.exit(2); }
  if (code === 1001) { console.log('1001：JWT 失效，请在应用内重新登录该账号'); process.exit(3); }
  if (i < maxTry) await sleep(gapSec * 1000);
}
console.log(`重试 ${maxTry} 次仍是临时限流，${gapSec * maxTry} 秒后跑本脚本再试`);
process.exit(1);
