/**
 * 邀请关系画布（自绘 SVG，零新增依赖）。
 *
 * 为什么自绘而不是 React Flow：项目依赖里没有图形库，为一张关系图引入
 * 整套框架（含节点状态机、事件系统）不划算。这里只需要「拖动节点 + 连线」，
 * 用 pointer 事件 + SVG path 就够，渲染也更快。
 *
 * 布局规则（自动布局，也支持手动拖拽后的位置记忆）：
 * - 按邀请层级分层：有被邀请者的主号在第 0 层，其小号在第 1 层；
 *   小号自己后续获得邀请资格并邀到人时成为新的主号，其小号再下一层。
 * - 同层内水平均分，层间垂直间距固定。
 * - 手动拖动后记录该账号的偏移量，重排时沿用（避免每次刷新跳回原位）。
 *
 * 视觉编码（信息密度对齐列表视图，但关系一眼可见）：
 * - 主号：靛蓝边框 + 皇冠；小号：灰色边框
 * - 节点内显示：邮箱前缀 / 邀请码 / 已邀人数 / 任务进度 / 夜间卡
 * - 连线：主号 → 小号；已绑定的实线，未绑定（仅记录归属）虚线
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Crown, Moon } from 'lucide-react';
import type { AccountLive } from './types';

export interface GraphNode {
  id: number;
  label: string;
  /** 完整邮箱 */
  email: string;
  invite_code: string | null;
  invite_state: string | null;
  invited_by: number | null;
  invite_role: string | null;
  invited_count: number | null;
  night_card_days: number | null;
  task_login_device: number | null;
  task_bind_code: number | null;
  task_send_message: number | null;
}

interface Props {
  rows: AccountLive[];
  /** 点击节点时的回调（打开详情/操作） */
  onSelect?: (id: number) => void;
}

/** 布局常量（2026-10-22 放大：原 190×74 装不下邀请码+进度，字也太小） */
const NODE_W = 240;
const NODE_H = 96;
const H_GAP = 34;
const V_GAP = 108;
const PAD = 40;

interface Placed {
  node: GraphNode;
  x: number;
  y: number;
  depth: number;
  parentId: number | null;
}

/** 邮箱本地部分（@ 前），过长截断——节点宽度有限 */
function shortName(email: string): string {
  const local = email.split('@')[0] || email;
  return local.length > 18 ? `${local.slice(0, 17)}…` : local;
}

/** 账号 → 图节点 */
function toGraphNode(a: AccountLive): GraphNode {
  return {
    id: a.id,
    label: a.label,
    email: a.label,
    invite_code: a.invite_code,
    invite_state: a.invite_state,
    invited_by: a.invited_by,
    invite_role: a.invite_role,
    invited_count: a.invited_count,
    night_card_days: a.night_card_days,
    task_login_device: a.task_login_device,
    task_bind_code: a.task_bind_code,
    task_send_message: a.task_send_message,
  };
}

/**
 * 计算分层布局。
 *
 * 关键点：**不依赖 invite_role 字段**，而用 invited_by 关系推导层级——
 * 因为 role 是历史标记（捆绑成功时写一次），而 invited_by 才是真实归属。
 * 一个号可能既是别人的小号（有 invited_by）、又是别人的主号（有下挂），
 * 这种中间节点要能正确落在中间层。
 */
export function layoutGraph(rows: AccountLive[]): {
  placed: Placed[];
  width: number;
  height: number;
} {
  const nodes = rows.map(toGraphNode);
  const byId = new Map(nodes.map((n) => [n.id, n]));
  // 子 → 父（父必须是存在的账号，否则当作根）
  const parentOf = new Map<number, number | null>();
  for (const n of nodes) {
    const p = n.invited_by;
    parentOf.set(n.id, p != null && byId.has(p) && p !== n.id ? p : null);
  }

  // 计算每个节点的深度（根 = 0）。带环保护，避免脏数据让递归爆栈。
  const depthOf = new Map<number, number>();
  const resolveDepth = (id: number, seen: Set<number>): number => {
    const cached = depthOf.get(id);
    if (cached != null) return cached;
    if (seen.has(id)) return 0; // 环：断开
    seen.add(id);
    const p = parentOf.get(id) ?? null;
    const d = p == null ? 0 : resolveDepth(p, seen) + 1;
    depthOf.set(id, d);
    return d;
  };
  for (const n of nodes) resolveDepth(n.id, new Set());

  // 分桶
  const buckets = new Map<number, GraphNode[]>();
  for (const n of nodes) {
    const d = depthOf.get(n.id) ?? 0;
    if (!buckets.has(d)) buckets.set(d, []);
    buckets.get(d)!.push(n);
  }
  const depths = [...buckets.keys()].sort((a, b) => a - b);

  const placed: Placed[] = [];
  let maxRow = 0;
  for (const d of depths) {
    const bucket = buckets.get(d)!;
    // 同层内：主号（有下挂）排前面，其余按 id 升序，保证布局稳定
    bucket.sort((a, b) => {
      const aChild = nodes.some((n) => n.invited_by === a.id) ? 0 : 1;
      const bChild = nodes.some((n) => n.invited_by === b.id) ? 0 : 1;
      return aChild - bChild || a.id - b.id;
    });
    maxRow = Math.max(maxRow, bucket.length);
    bucket.forEach((n, i) => {
      placed.push({
        node: n,
        x: PAD + i * (NODE_W + H_GAP),
        y: PAD + d * (NODE_H + V_GAP),
        depth: d,
        parentId: parentOf.get(n.id) ?? null,
      });
    });
  }

  // 同层数量不同时，居中摆放（视觉更平衡）
  for (const d of depths) {
    const row = placed.filter((p) => p.depth === d);
    if (row.length === 0) continue;
    const rowWidth = row.length * NODE_W + (row.length - 1) * H_GAP;
    const fullWidth = maxRow * NODE_W + (maxRow - 1) * H_GAP;
    const offset = (fullWidth - rowWidth) / 2;
    for (const p of row) p.x += offset;
  }

  const width = Math.max(
    PAD * 2 + maxRow * NODE_W + Math.max(0, maxRow - 1) * H_GAP,
    PAD * 2 + NODE_W,
  );
  const height = PAD * 2 + depths.length * NODE_H + Math.max(0, depths.length - 1) * V_GAP;
  return { placed, width, height };
}

/** 单节点渲染（SVG 内的 foreignObject，便于用 HTML/Tailwind 排版） */
function NodeBox({
  p,
  selected,
  onPointerDown,
  onSelect,
}: {
  p: Placed;
  selected: boolean;
  onPointerDown: (e: React.PointerEvent, id: number) => void;
  onSelect?: (id: number) => void;
}) {
  const n = p.node;
  const isInviter = n.invite_role === 'inviter' || n.invited_count != null || n.invite_code != null;
  const tasksDone = [n.task_login_device, n.task_bind_code, n.task_send_message].filter(
    (v) => v === 1,
  ).length;
  return (
    <foreignObject
      x={p.x}
      y={p.y}
      width={NODE_W}
      height={NODE_H}
      style={{ overflow: 'visible' }}
    >
      <div
        // @ts-expect-error foreignObject 内需要 xmlns 才能正确渲染
        xmlns="http://www.w3.org/1999/xhtml"
        onPointerDown={(e) => onPointerDown(e, n.id)}
        onClick={() => onSelect?.(n.id)}
        className={[
          'h-full w-full cursor-grab select-none rounded-xl border-2 px-3.5 py-2.5 text-[13px] shadow-sm transition-shadow active:cursor-grabbing',
          selected ? 'ring-2 ring-indigo-400' : '',
          isInviter
            ? 'border-indigo-300 bg-indigo-50/90 dark:border-indigo-500/50 dark:bg-indigo-500/10'
            : 'border-slate-200 bg-white/90 dark:border-zinc-700 dark:bg-zinc-800/90',
        ].join(' ')}
      >
        <div className="flex items-center gap-1.5">
          {isInviter && <Crown size={14} className="shrink-0 text-indigo-500" />}
          <span
            className="truncate text-[13px] font-semibold text-slate-700 dark:text-zinc-100"
            title={n.email}
          >
            {shortName(n.email)}
          </span>
        </div>
        <div className="mt-1.5 flex flex-wrap items-center gap-1.5 text-[12px] text-slate-500 dark:text-zinc-400">
          {n.invite_code ? (
            <span className="rounded bg-slate-100 px-1.5 py-0.5 font-mono text-[12px] font-medium text-slate-700 dark:bg-zinc-700 dark:text-zinc-100">
              {n.invite_code}
            </span>
          ) : (
            <span className="text-[12px] text-slate-400">资格未查</span>
          )}
          {n.invited_count != null && <span>邀 {n.invited_count}</span>}
          {(n.night_card_days ?? 0) > 0 && (
            <span className="inline-flex items-center gap-0.5 text-amber-600 dark:text-amber-400">
              <Moon size={11} />
              {n.night_card_days} 天
            </span>
          )}
        </div>
        <div className="mt-1 text-[11px] text-slate-400 dark:text-zinc-500">
          {n.invite_role === 'invited' ? `任务 ${tasksDone}/3` : isInviter ? '主号' : '未标记'}
        </div>
      </div>
    </foreignObject>
  );
}

export default function InviteGraph({ rows, onSelect }: Props) {
  const { placed, width, height } = useMemo(() => layoutGraph(rows), [rows]);
  // 手动拖拽偏移：账号 id → {dx, dy}；不持久化（重开页面回到自动布局）
  const [offsets, setOffsets] = useState<Record<number, { dx: number; dy: number }>>({});
  const [selected, setSelected] = useState<number | null>(null);
  const drag = useRef<{ id: number; startX: number; startY: number; baseX: number; baseY: number } | null>(
    null,
  );

  const onPointerDown = useCallback(
    (e: React.PointerEvent, id: number) => {
      // 只响应左键；避免拖动时触发文本选择
      if (e.button !== 0) return;
      const cur = offsets[id] ?? { dx: 0, dy: 0 };
      drag.current = { id, startX: e.clientX, startY: e.clientY, baseX: cur.dx, baseY: cur.dy };
      setSelected(id);
      (e.target as HTMLElement).setPointerCapture?.(e.pointerId);
      e.preventDefault();
    },
    [offsets],
  );

  useEffect(() => {
    const move = (e: PointerEvent) => {
      const d = drag.current;
      if (!d) return;
      setOffsets((prev) => ({
        ...prev,
        [d.id]: { dx: d.baseX + (e.clientX - d.startX), dy: d.baseY + (e.clientY - d.startY) },
      }));
    };
    const up = () => {
      drag.current = null;
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
    return () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
    };
  }, []);

  /** 连线：从父节点底部中心 → 子节点顶部中心，贝塞尔曲线 */
  const edges = placed
    .filter((p) => p.parentId != null)
    .map((p) => {
      const parent = placed.find((q) => q.node.id === p.parentId);
      if (!parent) return null;
      const po = offsets[parent.node.id] ?? { dx: 0, dy: 0 };
      const co = offsets[p.node.id] ?? { dx: 0, dy: 0 };
      const x1 = parent.x + po.dx + NODE_W / 2;
      const y1 = parent.y + po.dy + NODE_H;
      const x2 = p.x + co.dx + NODE_W / 2;
      const y2 = p.y + co.dy;
      const midY = (y1 + y2) / 2;
      // 已绑定的号 invite_role=invited 用实线；仅记录归属的用虚线
      const solid = p.node.invite_role === 'invited';
      return (
        <path
          key={`${parent.node.id}-${p.node.id}`}
          d={`M ${x1} ${y1} C ${x1} ${midY}, ${x2} ${midY}, ${x2} ${y2}`}
          fill="none"
          stroke={solid ? 'rgb(129 140 248)' : 'rgb(203 213 225)'}
          strokeWidth={solid ? 1.8 : 1.2}
          strokeDasharray={solid ? undefined : '4 3'}
          opacity={0.9}
        />
      );
    });

  if (rows.length === 0) {
    return (
      <div className="rounded-lg border border-dashed border-slate-200 p-8 text-center text-sm text-slate-400 dark:border-zinc-700">
        暂无账号
      </div>
    );
  }

  return (
    <div className="overflow-auto rounded-xl border border-slate-200 bg-slate-50/60 dark:border-zinc-800 dark:bg-zinc-900/40">
      <svg
        width={width}
        height={height}
        className="block min-w-full"
        style={{ minHeight: 240 }}
        // 点击空白处取消选中
        onClick={() => setSelected(null)}
      >
        {edges}
        {placed.map((p) => {
          const o = offsets[p.node.id] ?? { dx: 0, dy: 0 };
          return (
            <NodeBox
              key={p.node.id}
              p={{ ...p, x: p.x + o.dx, y: p.y + o.dy }}
              selected={selected === p.node.id}
              onPointerDown={onPointerDown}
              onSelect={onSelect}
            />
          );
        })}
      </svg>
    </div>
  );
}
