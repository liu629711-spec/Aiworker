/**
 * 账密管理 + CSV 批量导入 + 批量登录（zai-claim SecretsApp 移植，AI Work 设计语言）。
 *
 * - 账密仅存本机库（secret_entries），供随时查看与批量登录队列使用
 * - CSV 格式：email,password（与「账号导入模板.csv」一致）
 * - 批量登录：内嵌窗口自动填表 → 验证 → 授权，逐账号排队；Rust 后端跑队列，页面切换不中断
 */
import { useCallback, useEffect, useState } from 'react';
import {
  KeyRound,
  Plus,
  FileSpreadsheet,
  Zap,
  Square,
  Eye,
  EyeOff,
  Save,
  Trash2,
  Loader2,
  Info,
} from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, EmptyState } from '../../components/ui';
import { cn } from '../../lib/cn';
import { accountsApi, onBatchDone, onBatchProgress } from './api/accounts';
import { secretsApi } from './api/secrets';
import type { BatchProgress, SecretItem } from './types';

/** 行状态徽标 */
function StateBadge({ p }: { p: BatchProgress }) {
  if (p.state === 'running') {
    return (
      <Badge tone="blue">
        <Loader2 size={11} className="animate-spin" /> 登录中
      </Badge>
    );
  }
  if (p.state === 'success') return <Badge tone="green">成功</Badge>;
  if (p.state === 'failed') return <Badge tone="red">失败</Badge>;
  if (p.state === 'cancelled') return <Badge tone="slate">已取消</Badge>;
  return <Badge tone="slate">{p.state}</Badge>;
}

/** 单行：独立条目可改名/删除；已关联账号行名称只读；密码默认打码 */
function SecretRow({
  row,
  onChanged,
  batchState,
  onLogin,
  disabled,
}: {
  row: SecretItem;
  onChanged: () => void;
  batchState?: BatchProgress;
  /** 只登这一条（注册已完成的马甲补走 OAuth 入池，不必整队重跑） */
  onLogin?: () => void;
  disabled?: boolean;
}) {
  const linked = row.account_id !== null;
  const [title, setTitle] = useState(row.title);
  const [loginName, setLoginName] = useState(row.login_name || (linked ? row.title : ''));
  const [password, setPassword] = useState(row.password);
  const [show, setShow] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // 外部数据刷新（增删/CSV 重导）后同步最新值
  useEffect(() => {
    setTitle(row.title);
    setLoginName(row.login_name || (linked ? row.title : ''));
    setPassword(row.password);
  }, [row.id, row.title, row.login_name, row.password, linked]);

  const dirty =
    title !== row.title ||
    loginName !== (row.login_name || (linked ? row.title : '')) ||
    password !== row.password;

  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      await secretsApi.save(row.id, title.trim(), loginName.trim(), password);
      onChanged();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const remove = async () => {
    try {
      await secretsApi.remove(row.id);
      onChanged();
    } catch (e) {
      setError(String(e));
    }
  };

  const inputCls =
    'h-8 rounded-lg border border-slate-200 bg-white px-2.5 text-xs text-slate-700 outline-none transition placeholder:text-slate-400 focus:border-sky-400 focus:ring-2 focus:ring-sky-100 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:focus:border-sky-500/50 dark:focus:ring-sky-500/10';

  return (
    <div className="rounded-xl border border-slate-200/70 bg-white px-3 py-2.5 dark:border-zinc-800 dark:bg-zinc-900/60">
      <div className="flex flex-wrap items-center gap-2">
        {linked ? (
          <span
            className="max-w-40 truncate text-xs font-semibold text-slate-700 dark:text-zinc-200"
            title={row.title}
          >
            {row.title}
          </span>
        ) : (
          <input
            className={cn(inputCls, 'w-32')}
            placeholder="名称"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
          />
        )}
        <input
          className={cn(inputCls, 'min-w-44 flex-1')}
          placeholder="登录名（邮箱/手机号）"
          value={loginName}
          onChange={(e) => setLoginName(e.target.value)}
        />
        <div className="relative min-w-44 flex-1">
          <input
            className={cn(inputCls, 'w-full pr-8')}
            type={show ? 'text' : 'password'}
            placeholder="密码"
            value={password}
            autoComplete="off"
            onChange={(e) => setPassword(e.target.value)}
          />
          <button
            className="absolute right-1.5 top-1/2 -translate-y-1/2 text-slate-400 hover:text-slate-600 dark:hover:text-zinc-300"
            title={show ? '隐藏密码' : '显示密码'}
            onClick={() => setShow((v) => !v)}
          >
            {show ? <EyeOff size={14} /> : <Eye size={14} />}
          </button>
        </div>
        <button
          className={cn(
            'inline-flex items-center gap-1 rounded-lg px-2.5 py-1.5 text-xs font-medium transition active:scale-95',
            dirty
              ? 'bg-sky-500 text-white hover:bg-sky-600'
              : 'cursor-default bg-slate-100 text-slate-400 dark:bg-zinc-800 dark:text-zinc-500',
          )}
          onClick={save}
          disabled={!dirty || saving}
        >
          <Save size={13} /> {saving ? '…' : dirty ? '保存' : '已存'}
        </button>
        {!linked && (
          <button
            className="rounded-lg p-1.5 text-slate-400 transition hover:bg-rose-50 hover:text-rose-500 dark:hover:bg-rose-500/10"
            title="删除此条目"
            onClick={remove}
          >
            <Trash2 size={14} />
          </button>
        )}
        {!linked && onLogin && loginName.trim() && password && (
          <button
            className="inline-flex items-center gap-1 rounded-lg bg-sky-50 px-2 py-1.5 text-xs font-medium text-sky-600 transition hover:bg-sky-100 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-sky-500/10 dark:text-sky-400"
            title="只导入这一条：打开授权窗口自动登录并入池（不影响队列里的其他条目）"
            onClick={onLogin}
            disabled={disabled}
          >
            <Zap size={12} /> 导入
          </button>
        )}
        {batchState && <StateBadge p={batchState} />}
      </div>
      {error && (
        <p className="mt-1 text-xs text-rose-500" title={error}>
          {error}
        </p>
      )}
    </div>
  );
}

export default function SecretsPanel() {
  const [rows, setRows] = useState<SecretItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [importing, setImporting] = useState(false);
  /** 批量登录：每条 secret 的最新进度 */
  const [batchStates, setBatchStates] = useState<Record<number, BatchProgress>>({});
  const [batchRunning, setBatchRunning] = useState(false);
  const [batchTotal, setBatchTotal] = useState(0);
  const [batchCurrent, setBatchCurrent] = useState(0);

  const reload = useCallback(() => {
    secretsApi
      .list()
      .then(setRows)
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    reload();
    // 页面重进时恢复按钮态（队列在 Rust 后端跑，不随页面切换而中断）
    accountsApi.batchLoginStatus().then(setBatchRunning).catch(() => {});
    const unsubs: Array<() => void> = [];
    onBatchProgress((p) => {
      setBatchStates((prev) => ({ ...prev, [p.secret_id]: p }));
      setBatchTotal(p.total);
      setBatchCurrent(p.index + 1);
      if (p.state === 'running') setBatchRunning(true);
    }).then((u) => unsubs.push(u));
    onBatchDone((d) => {
      setBatchRunning(false);
      reload();
      const skip = d.total - d.ok - d.fail;
      setNotice(
        `批量登录结束：成功 ${d.ok} / 失败 ${d.fail}` + (skip > 0 ? ` / 取消 ${skip}` : ''),
      );
    }).then((u) => unsubs.push(u));
    return () => unsubs.forEach((u) => u());
  }, [reload]);

  const add = async () => {
    try {
      await secretsApi.add('新账号', '', '');
      reload();
    } catch (e) {
      setError(String(e));
    }
  };

  /** 批量登录：所有未关联账号且账密齐全的条目进队列 */
  const startBatch = async () => {
    if (!rows) return;
    const pending = rows.filter(
      (r) => r.account_id === null && r.login_name.trim() && r.password,
    );
    if (pending.length === 0) {
      setError('没有待登录的条目：需要独立录入（非账号行）且登录名和密码都已填写');
      return;
    }
    setError(null);
    setNotice(null);
    setBatchStates({});
    try {
      const n = await accountsApi.batchLogin(pending.map((r) => r.id));
      setBatchRunning(true);
      setBatchTotal(n);
      setBatchCurrent(0);
    } catch (e) {
      setError(String(e));
    }
  };

  /**
   * 只导入一条：复用批量登录队列机制，但队列里只有这一个条目。
   * 用途：注册流水线在第 4/5 步中断的马甲——账号在 z.ai 服务端已建好，
   * 只需用同一账密走一次 OAuth 授权就能入池，不必重跑整批。
   */
  const loginOne = async (secretId: number) => {
    setError(null);
    setNotice(null);
    try {
      const n = await accountsApi.batchLogin([secretId]);
      setBatchRunning(true);
      setBatchTotal(n);
      setBatchCurrent(0);
      setNotice('已发起单条导入：授权窗口里自动登录并入池，遇到滑块请手动滑一下');
    } catch (e) {
      setError(String(e));
    }
  };

  const stopBatch = async () => {

    try {
      await accountsApi.cancelBatchLogin();
    } catch (e) {
      setError(String(e));
    }
  };

  /** CSV 导入（email,password） */
  const handleFileSelect = async (event: React.ChangeEvent<HTMLInputElement>) => {
    const file = event.target.files?.[0];
    if (!file) return;
    setImporting(true);
    setError(null);
    setNotice(null);
    try {
      const text = await file.text();
      const result = await secretsApi.importFromCSV(text);
      if (result.error) {
        setError(`导入失败：${result.error}`);
      } else {
        setNotice(
          `成功导入 ${result.imported} 个账号，跳过 ${result.skipped} 个重复项` +
            ((result.errors?.length || 0) > 0
              ? `（${result.errors.map((e: { [k: number]: string }) => `第${e[0]}行：${e[1]}`).join('；')}）`
              : ''),
        );
        reload();
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setImporting(false);
      event.target.value = '';
    }
  };

  return (
    <div className="animate-fade-in space-y-4">
      <PageHeader
        title="ZCode · 账密与批量登录"
        desc="账密仅存本机库 · CSV 批量导入 · 内嵌窗口自动登录队列"
      />

      {/* 操作行 */}
      <div className="flex flex-wrap items-center gap-2">
        <button className="btn-outline" onClick={add}>
          <Plus size={15} /> 添加条目
        </button>
        <label className="btn-outline cursor-pointer">
          <FileSpreadsheet size={15} />
          {importing ? '导入中…' : 'CSV 导入'}
          <input
            type="file"
            accept=".csv,.txt"
            onChange={handleFileSelect}
            disabled={importing}
            className="hidden"
          />
        </label>
        {batchRunning ? (
          <button className="btn-primary !bg-rose-500 hover:!bg-rose-600" onClick={stopBatch}>
            <Square size={14} /> 停止（{batchCurrent}/{batchTotal}）
          </button>
        ) : (
          <button className="btn-primary" onClick={startBatch} disabled={importing}>
            <Zap size={15} /> 批量登录
          </button>
        )}
        <span className="ml-auto text-xs text-slate-400">
          {rows ? `${rows.length} 个条目` : ''}
        </span>
      </div>

      {batchRunning && (
        <div className="flex items-start gap-2 rounded-xl border border-sky-200 bg-sky-50 px-4 py-3 text-sm text-sky-700 dark:border-sky-500/30 dark:bg-sky-500/10 dark:text-sky-300">
          <Info size={15} className="mt-0.5 shrink-0" />
          批量登录中（{batchCurrent}/{batchTotal}）：弹出窗口里自动填表并点验证，遇到滑块时手动滑一下，
          其余全自动；关掉登录窗口即跳过当前账号。切换到其他页面不会中断队列。
        </div>
      )}
      {notice && (
        <div className="flex items-center justify-between gap-3 rounded-xl border border-emerald-200 bg-emerald-50 px-4 py-3 text-sm text-emerald-700 dark:border-emerald-500/30 dark:bg-emerald-500/10 dark:text-emerald-300">
          <span className="min-w-0 truncate">{notice}</span>
          <button className="shrink-0 text-xs hover:opacity-70" onClick={() => setNotice(null)}>
            忽略
          </button>
        </div>
      )}
      {error && (
        <div className="flex items-center justify-between gap-3 rounded-xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-600 dark:border-rose-500/30 dark:bg-rose-500/10 dark:text-rose-300">
          <span className="min-w-0 truncate">{error}</span>
          <button className="shrink-0 text-xs hover:opacity-70" onClick={() => setError(null)}>
            忽略
          </button>
        </div>
      )}

      {/* 条目列表 */}
      {rows && rows.length === 0 ? (
        <EmptyState
          icon={<KeyRound size={26} />}
          title="暂无账密条目"
          hint="点「CSV 导入」批量导入（格式：email,password），或「添加条目」手动录入。导入后点「批量登录」自动逐个登录并导入账号。"
        />
      ) : (
        <div className="space-y-2">
          {rows?.map((r) => (
            <SecretRow
              key={r.id}
              row={r}
              onChanged={reload}
              batchState={batchStates[r.id]}
              onLogin={() => loginOne(r.id)}
              disabled={batchRunning}
            />
          ))}
        </div>
      )}

      <p className="text-xs text-slate-400 dark:text-zinc-500">
        登录名/密码仅保存在本机数据库，供随时查看与批量登录使用，不会上传。
      </p>
    </div>
  );
}
