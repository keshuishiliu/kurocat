/**
 * 应用标注：自定义显示名 + 忽略开关。
 *
 * 与分类同一套思路：存在 settings.json 里（后端权威），前端只维护内存模型，
 * 校验与落盘都在 Rust 侧（`app_overrides_save`）。区别在于保存粒度是
 * 「单个程序的单个字段」而不是整份覆盖——改名和忽略都是单点操作，
 * 整份覆盖会让两个窗口的并发编辑互相清掉对方的标注。
 *
 * 展示优先级：自定义名 > exe 版本资源的显示名 > 进程名。
 * 忽略只影响**新数据**与统计口径：recorder 不再记录被忽略程序的新事件，
 * 历史数据保留，取消忽略即可恢复。
 */

import { invoke } from '@tauri-apps/api/core';

export interface AppOverrides {
  /** 进程名 → 自定义显示名 */
  displayNames: Record<string, string>;
  /** 进程名 → true = 忽略（只会有 true 的条目） */
  ignored: Record<string, boolean>;
}

/** 在普通浏览器里打开 stats.html 调 UI 时，退回 localStorage，不依赖后端。 */
const HAS_TAURI = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
const LS_FALLBACK = 'kurocat.appOverrides.v1';

let state: AppOverrides = loadState();
const listeners: Array<() => void> = [];

function cleanKey(v: unknown): string {
  if (typeof v !== 'string') return '';
  return v.trim().slice(0, 120);
}

function cleanName(v: unknown): string {
  if (typeof v !== 'string') return '';
  return v.trim().slice(0, 40);
}

function normalize(raw: unknown): AppOverrides {
  const fallback: AppOverrides = { displayNames: {}, ignored: {} };
  if (!raw || typeof raw !== 'object') return fallback;
  const input = raw as Partial<AppOverrides>;
  const displayNames: Record<string, string> = {};
  if (input.displayNames && typeof input.displayNames === 'object') {
    for (const [app, name] of Object.entries(input.displayNames)) {
      const key = cleanKey(app);
      const val = cleanName(name);
      if (key && val) displayNames[key] = val;
    }
  }
  const ignored: Record<string, boolean> = {};
  if (input.ignored && typeof input.ignored === 'object') {
    for (const [app, on] of Object.entries(input.ignored)) {
      const key = cleanKey(app);
      if (key && on === true) ignored[key] = true;
    }
  }
  return { displayNames, ignored };
}

function loadState(): AppOverrides {
  if (HAS_TAURI) {
    // 后端值在 init() 里异步拉到；在那之前先用空表占位，
    // 不会有任何写入，所以不会覆盖磁盘上的真实数据。
    return { displayNames: {}, ignored: {} };
  }
  try {
    const raw = localStorage.getItem(LS_FALLBACK);
    if (!raw) return { displayNames: {}, ignored: {} };
    return normalize(JSON.parse(raw));
  } catch {
    return { displayNames: {}, ignored: {} };
  }
}

function persist(): void {
  if (!HAS_TAURI) {
    try {
      localStorage.setItem(LS_FALLBACK, JSON.stringify(state));
    } catch {
      // 隐私模式 / 配额满：内存里仍然生效，只是下次打开丢失
    }
  }
  notify();
}

function notify(): void {
  for (const cb of listeners) cb();
}

/** 启动时调用一次。失败不致命：退回空表，只是没有自定义名与忽略标记。 */
export async function init(): Promise<void> {
  if (!HAS_TAURI) return;
  try {
    const res = await invoke<{
      appDisplayNames: Record<string, string>;
      appIgnored: Record<string, boolean>;
    }>('app_overrides_get');
    state = normalize({ displayNames: res.appDisplayNames, ignored: res.appIgnored });
  } catch (err) {
    console.warn('[appOverrides] 读取应用标注失败，先用空表', err);
  }
  notify();
}

// ---- 排队保存：按程序合并字段，串行发出，后端返回值是权威快照 ----

type AppPatch = { displayName?: string; ignored?: boolean };
const queue = new Map<string, AppPatch>();
let saving = false;

function enqueueSave(app: string, patch: AppPatch): void {
  const cur = queue.get(app) ?? {};
  if (patch.displayName !== undefined) cur.displayName = patch.displayName;
  if (patch.ignored !== undefined) cur.ignored = patch.ignored;
  queue.set(app, cur);
  if (!saving) void runSave();
}

async function runSave(): Promise<void> {
  saving = true;
  try {
    while (queue.size) {
      const [app, patch] = queue.entries().next().value as [string, AppPatch];
      queue.delete(app);
      const res = await invoke<{
        appDisplayNames: Record<string, string>;
        appIgnored: Record<string, boolean>;
      }>('app_overrides_save', {
        process: app,
        displayName: patch.displayName,
        ignored: patch.ignored,
      });
      // 以后端返回值为准（trim / 上限 / 空名清除都在那边做了）
      state = normalize({ displayNames: res.appDisplayNames, ignored: res.appIgnored });
      notify();
    }
  } catch (err) {
    console.warn('[appOverrides] 保存应用标注失败', err);
  } finally {
    saving = false;
    // 排队期间又来了新的改动（notify 回调里再改）接着发
    if (queue.size) void runSave();
  }
}

export function subscribe(fn: () => void): void {
  listeners.push(fn);
}

/** 自定义显示名；没有返回 null。 */
export function displayNameFor(process: string): string | null {
  return state.displayNames[process] ?? null;
}

export function isIgnored(process: string): boolean {
  return state.ignored[process] === true;
}

/** 设置自定义显示名；传空串 = 清除，退回 exe 自带名。 */
export function setDisplayName(process: string, name: string): void {
  if (!process) return;
  const clean = cleanName(name);
  if (clean) state.displayNames[process] = clean;
  else delete state.displayNames[process];
  persist();
  if (HAS_TAURI) enqueueSave(process, { displayName: clean });
}

export function setIgnored(process: string, on: boolean): void {
  if (!process) return;
  if (on) state.ignored[process] = true;
  else delete state.ignored[process];
  persist();
  if (HAS_TAURI) enqueueSave(process, { ignored: on });
}
