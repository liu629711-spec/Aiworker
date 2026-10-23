/**
 * 页内使用引导（可折叠步骤条）。
 *
 * 为什么需要：ZCode 的「自动注册」「马甲邮箱」两个页面把内部实现词（IMAP、
 * App Password、Firefox Relay、Zen 浏览器）直接摆在界面上，新用户不知道
 * 从哪一步开始、也不知道每个前置条件为什么存在。这里把「先做什么、为什么、
 * 缺了会怎样」写成有序步骤，并支持折叠——老用户不用一直看。
 *
 * 不做自动检测/一键跳转：那需要后端暴露配置完整度接口，本组件只承载说明。
 */
import { useState } from 'react';
import { HelpCircle, ChevronDown, ChevronRight } from 'lucide-react';

export interface GuideStep {
  /** 步骤标题（动词开头，一句话） */
  title: string;
  /** 具体怎么做：包含在哪填什么、为什么必须这样 */
  detail: string;
  /** 可选：这一步的前置条件不满足时会发生什么 */
  warning?: string;
}

export default function GuidePanel({
  title = '怎么用（点开看步骤）',
  steps,
  defaultOpen = false,
}: {
  title?: string;
  steps: GuideStep[];
  defaultOpen?: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className="rounded-xl border border-sky-200 bg-sky-50/60 dark:border-sky-500/30 dark:bg-sky-500/10">
      <button
        className="flex w-full items-center gap-2 px-4 py-2.5 text-left text-sm font-medium text-sky-800 dark:text-sky-200"
        onClick={() => setOpen((v) => !v)}
      >
        {open ? <ChevronDown size={15} /> : <ChevronRight size={15} />}
        <HelpCircle size={15} />
        {title}
        <span className="ml-auto text-xs font-normal text-sky-600/70 dark:text-sky-300/70">
          {steps.length} 步
        </span>
      </button>
      {open && (
        <ol className="space-y-2.5 border-t border-sky-200/70 px-4 py-3 dark:border-sky-500/20">
          {steps.map((s, i) => (
            <li key={s.title} className="flex gap-2.5 text-sm">
              <span className="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-sky-200 text-[11px] font-semibold text-sky-800 dark:bg-sky-500/30 dark:text-sky-100">
                {i + 1}
              </span>
              <div className="min-w-0">
                <div className="font-medium text-slate-800 dark:text-zinc-100">{s.title}</div>
                <div className="mt-0.5 text-[13px] leading-relaxed text-slate-600 dark:text-zinc-300">
                  {s.detail}
                </div>
                {s.warning && (
                  <div className="mt-1 rounded bg-amber-100/70 px-2 py-1 text-[12px] text-amber-800 dark:bg-amber-500/15 dark:text-amber-300">
                    注意：{s.warning}
                  </div>
                )}
              </div>
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}
