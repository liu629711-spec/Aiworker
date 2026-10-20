/**
 * mint_trae_device.mjs — 为本机 mint 一对全新的 Trae 已注册设备号。
 *
 * 背景（2026-10-16 逆向结论）：
 * - Trae claim 校验 x-device-id 必须是服务端注册过的设备号（2026-09-30 起，
 *   派生号一律 9074「当前参与用户太多」——文案有误导性，与限流无关）。
 * - 设备号由 aha SDK 按机器指纹解析、服务端分配并持久注册（指纹→did 稳定映射，
 *   重复注册 new_user:0 返回同一号）。本机指纹锚定用户级 HKCU
 *   Software\Microsoft\DeveloperTools\deviceid（两个 Trae 安装同用户故同号）。
 * - 一台设备一天只能签一个账号（9095 按设备计配额），多账号签到 = 每账号各持
 *   一个真实注册设备号。
 * - 每次客户端启动会注册两个号：icube did（ICDRS 日志，aid 787976）与
 *   aha_doctor did（device_register 请求 URL 明文，aid 776330，不参与签到故
 *   配额每日全新——签到首选它）。
 *
 * 用法（三步，全程可逆、不需要管理员）：
 *   1) node scripts/mint_trae_device.mjs prepare          # 备份+换指纹+清缓存
 *   2) 重启 Trae CN 客户端（等 10 秒让 aha 完成注册；若要给新账号绑定，
 *      可顺手在客户端登录该账号，JWT 会被应用代理自动捕获）
 *   3) node scripts/mint_trae_device.mjs collect <uid>    # 读新号+还原+写 device_map
 *
 * collect 会把 device_map[uid] 写成 {device_id: 新doctor号, locked: true}——
 * locked 条目 OAuth 登录回写不覆盖（oauth.rs writeback_oauth_device）。
 *
 * 安全设计：prepare 全量备份（HKCU 值/TinyStorage/Remote_State/state.vscdb/
 * machineid/storage.json）；collect 无条件还原备份；若检测到指纹未变（重启后
 * did 与旧号相同）则中止且不动 device_map。误触发的 6 层设备重置同理会使指纹
 * 变化——mint 后旧号仍在服务端注册，已分配的账号不受影响。
 */
import { DatabaseSync } from 'node:sqlite';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

const APPDATA = process.env.APPDATA;
const TRAE_DIR = path.join(APPDATA, 'Trae CN');
const TRAE_LOGS = path.join(TRAE_DIR, 'logs');
const AHA_DIR = path.join(TRAE_DIR, 'aha');
const GLOBAL_STORAGE = path.join(TRAE_DIR, 'User', 'globalStorage');
const DB_PATH = path.join(APPDATA, 'AIWorkAssistant', 'data', 'aiwork.sqlite');
const BACKUP_ROOT = 'D:\\aiwork-studio\\backups';
const HKCU_KEY = 'HKCU\\Software\\Microsoft\\DeveloperTools';
const HKCU_VALUE = 'deviceid';

const log = (...a) => console.log('[mint]', ...a);
const die = (msg) => { console.error('[mint][错误]', msg); process.exit(1); };

// ── 备份 / 还原 ──────────────────────────────────────────────────────────────

function backupDir() {
  const dirs = fs.readdirSync(BACKUP_ROOT, { withFileTypes: true })
    .filter((d) => d.isDirectory() && d.name.startsWith('mint-'))
    .map((d) => d.name)
    .sort();
  if (dirs.length === 0) die('没有找到 mint-* 备份目录，请先运行 prepare');
  // 取最新一个（未被 consume 标记的）
  for (const name of dirs.reverse()) {
    const meta = path.join(BACKUP_ROOT, name, 'meta.json');
    if (fs.existsSync(meta)) {
      const m = JSON.parse(fs.readFileSync(meta, 'utf8'));
      if (!m.consumed) return { dir: path.join(BACKUP_ROOT, name), meta: m };
    }
  }
  die('所有 mint 备份都已被 consume，请重新 prepare');
}

function cmd(args) { return execFileSync('reg', args, { encoding: 'utf8' }); }

function readHkcuDeviceId() {
  const out = cmd(['query', HKCU_KEY, '/v', HKCU_VALUE]);
  const m = out.match(new RegExp(`${HKCU_VALUE}\\s+REG_SZ\\s+(\\S+)`));
  return m ? m[1].trim() : null;
}

function writeHkcuDeviceId(v) {
  cmd(['add', HKCU_KEY, '/v', HKCU_VALUE, '/t', 'REG_SZ', '/d', v, '/f']);
}

function copyFile(src, dst) {
  if (!fs.existsSync(src)) return false;
  fs.mkdirSync(path.dirname(dst), { recursive: true });
  fs.copyFileSync(src, dst);
  return true;
}

function prepare() {
  const ts = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
  const dir = path.join(BACKUP_ROOT, `mint-${ts}`);
  fs.mkdirSync(dir, { recursive: true });

  // 1) 备份全部身份要素
  const oldHkcu = readHkcuDeviceId();
  if (!oldHkcu) die(`注册表 ${HKCU_KEY}\\${HKCU_VALUE} 不存在（客户端从未注册过？）`);
  const files = {
    tiny_storage: path.join(AHA_DIR, 'TinyStorage'),
    remote_state: path.join(AHA_DIR, 'Remote_State'),
    state_vscdb: path.join(GLOBAL_STORAGE, 'state.vscdb'),
    machineid: path.join(TRAE_DIR, 'machineid'),
    storage_json: path.join(GLOBAL_STORAGE, 'storage.json'),
  };
  const backed = {};
  for (const [k, p] of Object.entries(files)) backed[k] = copyFile(p, path.join(dir, k));
  fs.writeFileSync(path.join(dir, 'hkcu_deviceid.txt'), oldHkcu);
  // 2026-10-19 补强：HardwareCache 子键（Uuid/MachineGuid/硬盘序列号）此前不在
  // 备份/清理范围——aha 服务端交叉这些硬件指纹把「换 deviceid 的新机器」重新
  // 解析回老 did（实测两轮 mint 都返回 1853105110786875/744827358569716）。
  // 备份整个子键，prepare 时删除，collect 时还原。
  const hcDir = `${HKCU_KEY}\\HardwareCache`;
  // reg export 必须由 cmd 执行（execFileSync('reg',...) 的 stdout 重定向写不进文件，
  // 2026-10-19 实测产物是 40 字节的「操作成功」文本 → import 报「非注册表文件」）
  const hcBackup = path.join(dir, 'hardware_cache.reg');
  try {
    execFileSync('cmd', ['/c', 'reg', 'export', hcDir, hcBackup, '/y'], { stdio: 'pipe' });
    if (fs.existsSync(hcBackup) && fs.statSync(hcBackup).size > 100) {
      backed.hardware_cache = hcBackup;
    } else {
      log('  HardwareCache 导出异常（文件过小），跳过');
    }
  } catch (e) {
    log('  HardwareCache 导出失败（可能不存在）:', e.message.slice(0, 80));
  }
  log('备份目录:', dir);
  log('  已备份:', Object.entries(backed).filter(([, v]) => v).map(([k]) => k).join(', ') || '（无文件）');
  log('  HKCU deviceid 原值:', oldHkcu);

  // 2) 换新指纹（用户级随机 UUID）+ 清 aha 设备号缓存 + 清硬件指纹缓存，强制重新注册
  const newId = crypto.randomUUID();
  writeHkcuDeviceId(newId);
  if (fs.existsSync(files.tiny_storage)) fs.rmSync(files.tiny_storage);
  try { cmd(['delete', hcDir, '/f']); log('HardwareCache（硬件指纹缓存）已清除'); } catch {}
  // 2026-10-19 二次补强：aha_doctor 本地事件缓存（%LOCALAPPDATA%\aha_doctor\<app>\event\）
  // 持久存着 install_id / device_id / cdid——三轮 mint 拿到同一组号的总根源。
  // 客户端启动时从这里直接读旧标识，指纹换了也照样原样上报。备份整个 event 目录
  // 后删除，collect 时还原。
  const doctorEventDir = path.join(process.env.LOCALAPPDATA || '', 'aha_doctor', 'Trae CN', 'event');
  if (fs.existsSync(doctorEventDir)) {
    const evBackup = path.join(dir, 'aha_doctor_event');
    fs.cpSync(doctorEventDir, evBackup, { recursive: true });
    fs.rmSync(doctorEventDir, { recursive: true, force: true });
    backed.aha_doctor_event = evBackup;
    log('aha_doctor event 缓存（device_id/cdid 持久层）已备份并清除');
  } else {
    log('aha_doctor event 目录不存在（跳过）');
  }
  log('HKCU deviceid 已换为:', newId);
  log('TinyStorage（did 缓存）已清除');

  // 3) 记录旧 did（用于 collect 时校验指纹确实变了）
  const oldDid = readLatestIcdrsDid();
  fs.writeFileSync(path.join(dir, 'meta.json'), JSON.stringify({
    prepared_at: new Date().toISOString(),
    old_hkcu_deviceid: oldHkcu,
    old_icube_did: oldDid,
    consumed: false,
  }, null, 2));

  log('');
  log('=== prepare 完成 ===');
  log('下一步：重启 Trae CN 客户端，等约 10 秒（aha 启动时自动注册新设备号）。');
  log('  · 若要把新号分配给一个新账号：重启后在客户端里登录该账号，');
  log('    JWT 会被应用代理自动捕获，无需手动操作。');
  log('  · 若只是屯一个备用号：重启后直接进行下一步。');
  log(`然后运行：node scripts/mint_trae_device.mjs collect <uid>`);
}

function readLatestIcdrsDid() {
  if (!fs.existsSync(TRAE_LOGS)) return null;
  const dirs = fs.readdirSync(TRAE_LOGS, { withFileTypes: true })
    .filter((d) => d.isDirectory() && /^\d{8}T\d{6}$/.test(d.name))
    .map((d) => d.name).sort();
  let best = null;
  for (const d of dirs) {
    const dir = path.join(TRAE_LOGS, d);
    for (const f of fs.readdirSync(dir)) {
      const fp = path.join(dir, f);
      let text;
      try { text = fs.readFileSync(fp, 'utf8'); } catch { continue; }
      for (const line of text.split('\n')) {
        const m = line.match(/^(\d{4}-\d{2}-\d{2}T[\d:.]+)\+08:00.*\[ICDRS\] \(init\) initialization done, did: (\d+)/);
        if (m && (!best || m[1] > best.ts)) best = { ts: m[1], did: m[2] };
      }
    }
  }
  return best ? best.did : null;
}

function readLatestDoctorDid() {
  // 首选：aha_doctor event 缓存（applog.cache2）的 device_id 字段（二进制 int64 LE）——
  // 这是客户端本轮注册后真正落盘的号；prepare 清空缓存后新注册会写入全新号。
  // 代理日志兜底：URL 里的 device_id 可能是上一轮的（客户端先上报旧号再拿到新号），
  // 直接取「最新一条」会读错（2026-10-19 实测 1466 mint 被 21:08 旧日志污染）。
  const evPath = path.join(
    process.env.LOCALAPPDATA || '', 'aha_doctor', 'Trae CN', 'event', 'applog.cache2',
  );
  if (fs.existsSync(evPath)) {
    const buf = fs.readFileSync(evPath);
    const idx = buf.indexOf(Buffer.from('device_id'));
    if (idx >= 0) {
      for (let i = idx; i < buf.length - 8; i++) {
        try {
          const v = buf.readBigUInt64LE(i);
          if (v > 100000000000000n && v < 10000000000000000n) return v.toString();
        } catch {}
      }
    }
  }
  // 兜底：代理日志（仅当 cache2 尚未写入时使用，可能与最新轮次有偏差）
  const logsDir = path.join(APPDATA, 'AIWorkAssistant', 'logs');
  if (!fs.existsSync(logsDir)) return null;
  const files = fs.readdirSync(logsDir).filter((f) => /^proxy_req_\d{4}-\d{2}-\d{2}\.log$/.test(f)).sort();
  let best = null;
  for (const f of files) {
    const text = fs.readFileSync(path.join(logsDir, f), 'latin1');
    const re = /\[(\d{4}-\d{2}-\d{2} [\d:]+)\] POST \S*device_register\/\?[^ \n]*?device_id=(\d+)/g;
    let m;
    while ((m = re.exec(text))) {
      const ts = m[1].replace(' ', 'T');
      if (!best || ts > best.ts) best = { ts, did: m[2] };
    }
  }
  return best ? best.did : null;
}

function collect(uid) {
  if (!/^\d{6,20}$/.test(uid)) die(`uid 应为 6-20 位数字，收到: ${uid}`);
  const { dir, meta } = backupDir();

  // 1) 读重启后注册的新号
  const newIcdrs = readLatestIcdrsDid();
  const newDoctor = readLatestDoctorDid();
  log('重启后 ICDRS did:', newIcdrs ?? '（未读到）');
  log('最新 doctor did:', newDoctor ?? '（未读到，需应用代理运行时才会记录）');

  const fresh = newDoctor && newDoctor !== meta.old_icube_did ? newDoctor : newIcdrs;
  if (!fresh) die('没有读到任何新设备号——客户端重启了吗？');
  if (fresh === meta.old_icube_did) {
    die('指纹未变化（did 与旧号相同）——mint 失败，已自动还原，未改动 device_map');
  }
  const source = (newDoctor && newDoctor !== meta.old_icube_did) ? 'aha_doctor' : 'icube';

  // 2) 无条件还原全部身份要素
  const restore = (k, p) => {
    const bak = path.join(dir, k);
    if (fs.existsSync(bak)) { copyFile(bak, p); return true; }
    return false;
  };
  writeHkcuDeviceId(meta.old_hkcu_deviceid);
  restore('tiny_storage', path.join(AHA_DIR, 'TinyStorage'));
  restore('remote_state', path.join(AHA_DIR, 'Remote_State'));
  restore('state_vscdb', path.join(GLOBAL_STORAGE, 'state.vscdb'));
  restore('machineid', path.join(TRAE_DIR, 'machineid'));
  restore('storage_json', path.join(GLOBAL_STORAGE, 'storage.json'));
  // HardwareCache 还原（2026-10-19 补强配套）：prepare 时整个子键被删，
  // 用导出的 .reg 文件原样灌回
  const hcReg = path.join(dir, 'hardware_cache.reg');
  if (fs.existsSync(hcReg)) {
    try { cmd(['import', hcReg.replace(/\//g, '\\')]); } catch (e) {
      log('HardwareCache 还原失败（可手动双击 .reg 恢复）:', e.message.slice(0, 80));
    }
  }
  // aha_doctor event 缓存还原（2026-10-19 二次补强配套）
  const evBackup = path.join(dir, 'aha_doctor_event');
  const doctorEventDir = path.join(process.env.LOCALAPPDATA || '', 'aha_doctor', 'Trae CN', 'event');
  if (fs.existsSync(evBackup)) {
    fs.rmSync(doctorEventDir, { recursive: true, force: true });
    fs.cpSync(evBackup, doctorEventDir, { recursive: true });
    log('aha_doctor event 缓存已还原');
  }
  meta.consumed = true;
  meta.collected_at = new Date().toISOString();
  meta.new_device_id = fresh;
  meta.new_device_source = source;
  fs.writeFileSync(path.join(dir, 'meta.json'), JSON.stringify(meta, null, 2));
  log('已还原：HKCU deviceid →', meta.old_hkcu_deviceid, '，TinyStorage/Remote_State/state.vscdb/machineid/storage.json 全部回滚');

  // 3) 写入 device_map（locked 防 OAuth 回写覆盖）
  const db = new DatabaseSync(DB_PATH);
  const row = db.prepare('SELECT data FROM device_map WHERE pk=?').get(uid);
  const dev = row ? JSON.parse(row.data) : {};
  const m = crypto.createHash('sha256').update(`market:${uid}`).digest();
  m[6] = (m[6] & 0x0f) | 0x40; m[8] = (m[8] & 0x3f) | 0x80;
  const uuid = (b) => [...b.subarray(0, 16)].map((x, i) =>
    (i === 4 || i === 6 || i === 8 || i === 10 ? '-' : '') + x.toString(16).padStart(2, '0')).join('');
  dev.device_id = fresh;
  dev.market_user_id = dev.market_user_id || uuid(m);
  dev.session_id = dev.session_id || crypto.createHash('sha256').update(`sess:${uid}`).digest('hex');
  dev.locked = true;
  dev.note = `mint ${meta.prepared_at.slice(0, 10)}：${source} 通道新注册设备号（旧指纹备份 ${path.basename(dir)}）；locked 防 OAuth 回写覆盖`;
  db.prepare('INSERT INTO device_map(pk, data) VALUES(?, ?) ON CONFLICT(pk) DO UPDATE SET data=excluded.data')
    .run(uid, JSON.stringify(dev));
  db.close();
  log(`device_map[${uid}] 已写入: device_id=${fresh} (来源 ${source}, locked=true)`);

  log('');
  log('=== collect 完成 ===');
  log('最后一步：再重启一次 Trae CN 客户端——客户端回到原指纹/原设备号，');
  log('  · 若你刚在客户端登录了新账号：重启后用应用「切换账号」回到 0640，');
  log('    或直接在客户端重新登录 0640。');
  log('  · 新账号的签到设备号已锁定，应用定时签到/手动签到都会用它。');
}

// ── 入口 ─────────────────────────────────────────────────────────────────────
const [cmd2, arg] = process.argv.slice(2);
if (cmd2 === 'prepare') prepare();
else if (cmd2 === 'collect') { if (!arg) die('用法: collect <uid>'); collect(arg); }
else {
  console.log('用法:');
  console.log('  node scripts/mint_trae_device.mjs prepare        # 备份+换指纹+清缓存');
  console.log('  # 重启 Trae CN 客户端，等 10 秒');
  console.log('  node scripts/mint_trae_device.mjs collect <uid>  # 读新号+还原+写 device_map');
  process.exit(1);
}
