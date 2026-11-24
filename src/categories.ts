/**
 * 分类与应用归类。
 *
 * 存在 `settings.json` 里，跟输入记录同一个数据目录：换机器、整个目录拷走时
 * 分类会跟着走，也不怕清浏览器缓存把标签全丢掉。
 *
 * 分工很清楚——这个模块只维护内存模型和交互，真正的校验与落盘在 Rust 侧
 * （`categories_save`）。前端传上去的东西一律当作不可信输入：重复 id、悬空
 * 父级、非法颜色都会在那边被收拾掉，返回的才是权威版本。
 *
 * 数据形状：
 *   categories: 扁平数组 + parentId 表达层级（上级分类）
 *   appMap:     process 名（chrome.exe）→ 分类 id
 *
 * 读写方式：启动时 `init()` 拉一次，之后每次改动整份覆盖保存。
 * 全部算下来也就几 KB，不值得为它设计增量协议。
 */

import { invoke } from '@tauri-apps/api/core';

export interface Category {
  id: string;
  name: string;
  /** 表情符号，单个字符或多个组合（游戏手柄、调色板……） */
  icon: string;
  /** #rrggbb，用于色点与图表 */
  color: string;
  /** null = 顶层分类 */
  parentId: string | null;
  /** 内置分类不可删除（未分类） */
  builtin?: boolean;
}

export interface CategorySnapshot {
  categories: Category[];
  appMap: Record<string, string>;
}

/** 在普通浏览器里打开 stats.html 调 UI 时，退回 localStorage，不依赖后端。 */
const HAS_TAURI = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
const LS_FALLBACK = 'kurocat.categories.v2';
export const UNCATEGORIZED_ID = 'uncat';

/** 与参考图一致的一套默认分类：图标、颜色、名称都对得上。 */
export function defaultCategories(): Category[] {
  return [
    { id: 'game', name: '游戏', icon: '🎮', color: '#f59e0b', parentId: null },
    { id: 'media', name: '媒体', icon: '🎬', color: '#ef4444', parentId: null },
    { id: 'social', name: '社交', icon: '💬', color: '#ec4899', parentId: null },
    { id: 'work', name: '工作', icon: '💼', color: '#3b82f6', parentId: null },
    { id: 'creative', name: '创作', icon: '🎨', color: '#9ca3af', parentId: null },
    { id: 'reference', name: '参考', icon: '🌐', color: '#22c55e', parentId: null },
    { id: 'tools', name: '工具', icon: '🛠️', color: '#8b5cf6', parentId: null },
    {
      id: UNCATEGORIZED_ID,
      name: '未分类',
      icon: '📁',
      color: '#4b5563',
      parentId: null,
      builtin: true,
    },
  ];
}

let state: CategorySnapshot = loadState();
const listeners: Array<() => void> = [];

function isHexColor(v: unknown): v is string {
  return typeof v === 'string' && /^#[0-9a-fA-F]{6}$/.test(v);
}

function cleanIcon(v: unknown): string {
  if (typeof v !== 'string') return '🏷️';
  // 允许 emoji / 变体选择符，长度兜底防止把一整段文字塞进来
  const trimmed = v.trim();
  return trimmed ? Array.from(trimmed).slice(0, 4).join('') : '🏷️';
}

function cleanName(v: unknown): string {
  if (typeof v !== 'string') return '';
  return v.trim().slice(0, 20);
}

function normalize(raw: unknown): CategorySnapshot {
  const fallback: CategorySnapshot = { categories: defaultCategories(), appMap: {} };
  if (!raw || typeof raw !== 'object') return fallback;
  const input = raw as Partial<CategorySnapshot>;
  if (!Array.isArray(input.categories)) return fallback;

  const seen = new Set<string>();
  const categories: Category[] = [];
  for (const item of input.categories) {
    if (!item || typeof item !== 'object') continue;
    const c = item as Partial<Category>;
    const id = typeof c.id === 'string' ? c.id.trim().slice(0, 40) : '';
    const name = cleanName(c.name);
    if (!id || !name || seen.has(id)) continue;
    seen.add(id);
    categories.push({
      id,
      name,
      icon: cleanIcon(c.icon),
      color: isHexColor(c.color) ? c.color.toLowerCase() : '#8b5cf6',
      parentId: typeof c.parentId === 'string' && c.parentId !== id ? c.parentId : null,
      builtin: Boolean(c.builtin),
    });
  }
  if (!categories.length) return fallback;

  // 未分类永远存在，而且不可删
  if (!categories.some((c) => c.id === UNCATEGORIZED_ID)) {
    const uncat = defaultCategories().find((c) => c.id === UNCATEGORIZED_ID)!;
    categories.push(uncat);
  } else {
    const uncat = categories.find((c) => c.id === UNCATEGORIZED_ID)!;
    uncat.builtin = true;
    uncat.parentId = null;
  }

  // 父分类必须存在，且不能形成环（简单两层足够用，环直接拍平）
  const ids = new Set(categories.map((c) => c.id));
  for (const c of categories) {
    if (c.parentId && !ids.has(c.parentId)) c.parentId = null;
  }
  for (const c of categories) {
    const parent = c.parentId ? categories.find((p) => p.id === c.parentId) : null;
    if (parent && parent.parentId === c.id) {
      parent.parentId = null;
    }
  }

  const appMap: Record<string, string> = {};
  if (input.appMap && typeof input.appMap === 'object') {
    for (const [app, cid] of Object.entries(input.appMap)) {
      if (typeof app !== 'string' || typeof cid !== 'string') continue;
      if (!app || !ids.has(cid)) continue;
      appMap[app.slice(0, 120)] = cid;
    }
  }

  return { categories, appMap };
}

function loadState(): CategorySnapshot {
  const fallback = (): CategorySnapshot => ({ categories: defaultCategories(), appMap: {} });
  if (HAS_TAURI) {
    // 后端值在 init() 里异步拉到；在那之前先用默认分类占位，
    // 不会有任何写入，所以不会覆盖磁盘上的真实数据。
    return fallback();
  }
  try {
    const raw = localStorage.getItem(LS_FALLBACK);
    if (!raw) return fallback();
    return normalize(JSON.parse(raw));
  } catch {
    return fallback();
  }
}

/** 启动时调用一次。失败不致命：退回默认分类，用户还能继续改。 */
export async function init(): Promise<void> {
  if (!HAS_TAURI) return;
  try {
    const res = await invoke<{ categories: unknown; appCategories: Record<string, string> }>(
      'categories_get',
    );
    state = normalize({ categories: res.categories, appMap: res.appCategories });
  } catch (err) {
    console.warn('[categories] 读取分类失败，先用默认分类', err);
  }
  notify();
}

let saving = false;
let saveDirty = false;

function notify(): void {
  for (const cb of listeners) cb();
}

/**
 * 排队保存。连续改（比如一次拖好几个下拉框）时合并成「最后一份」，
 * 不并发打后端，也不会让先发出的旧状态覆盖后发的新状态。
 */
function queueSave(): void {
  if (saving) {
    saveDirty = true;
    return;
  }
  void runSave();
}

async function runSave(): Promise<void> {
  saving = true;
  try {
    do {
      saveDirty = false;
      const payload = snapshot();
      const res = await invoke<{ categories: Category[]; appCategories: Record<string, string> }>(
        'categories_save',
        { categories: payload.categories, appCategories: payload.appMap },
      );
      // 以后端返回值为准：它会补「未分类」、断掉悬空父级、丢掉非法颜色。
      // 正常情况两边一致，所以只悄悄换掉内存，不再重绘一次（避免列表闪烁）。
      state = {
        categories: Array.isArray(res?.categories) ? res.categories : payload.categories,
        appMap: res?.appCategories ?? payload.appMap,
      };
    } while (saveDirty);
  } catch (err) {
    console.warn('[categories] 保存分类失败', err);
  } finally {
    saving = false;
  }
}

function persist(): void {
  if (HAS_TAURI) {
    queueSave();
  } else {
    try {
      localStorage.setItem(LS_FALLBACK, JSON.stringify(state));
    } catch {
      // 隐私模式 / 配额满：内存里仍然生效，只是下次打开丢失
    }
  }
  notify();
}

export function subscribe(fn: () => void): void {
  listeners.push(fn);
}

export function snapshot(): CategorySnapshot {
  return { categories: state.categories.map((c) => ({ ...c })), appMap: { ...state.appMap } };
}

export function listCategories(): Category[] {
  return state.categories.map((c) => ({ ...c }));
}

export function findCategory(id: string | null | undefined): Category | null {
  if (!id) return null;
  return state.categories.find((c) => c.id === id) ?? null;
}

export function childrenOf(parentId: string): Category[] {
  return state.categories.filter((c) => c.parentId === parentId).map((c) => ({ ...c }));
}

/** 顶层祖先。统计按分类汇总时，子分类并入最顶层，避免出现「游戏 / 游戏·手游」两行。 */
export function rootOf(id: string): Category {
  let cur = findCategory(id);
  let guard = 0;
  while (cur && cur.parentId && guard++ < 8) {
    const parent = findCategory(cur.parentId);
    if (!parent) break;
    cur = parent;
  }
  return cur ?? findCategory(UNCATEGORIZED_ID) ?? defaultCategories()[0];
}

export function uncategorized(): Category {
  return findCategory(UNCATEGORIZED_ID) ?? defaultCategories().find((c) => c.id === UNCATEGORIZED_ID)!;
}

export function categoryForApp(app: string): Category {
  const cid = state.appMap[app];
  return (cid ? findCategory(cid) : null) ?? uncategorized();
}

export function categoryIdForApp(app: string): string {
  return categoryForApp(app).id;
}

export function setAppCategory(app: string, categoryId: string): void {
  if (!app) return;
  if (!findCategory(categoryId)) return;
  state.appMap[app] = categoryId;
  persist();
}

export function clearAppCategory(app: string): void {
  if (!(app in state.appMap)) return;
  delete state.appMap[app];
  persist();
}

function newId(): string {
  return `c${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
}

export function createCategory(input: {
  name: string;
  icon: string;
  color: string;
  parentId: string | null;
}): Category {
  const cat: Category = {
    id: newId(),
    name: cleanName(input.name) || '新分类',
    icon: cleanIcon(input.icon),
    color: isHexColor(input.color) ? input.color.toLowerCase() : '#8b5cf6',
    parentId: input.parentId && findCategory(input.parentId) ? input.parentId : null,
  };
  state.categories.push(cat);
  persist();
  return cat;
}

export function updateCategory(
  id: string,
  patch: { name?: string; icon?: string; color?: string; parentId?: string | null },
): void {
  const cat = state.categories.find((c) => c.id === id);
  if (!cat) return;
  if (patch.name !== undefined) cat.name = cleanName(patch.name) || cat.name;
  if (patch.icon !== undefined) cat.icon = cleanIcon(patch.icon);
  if (patch.color !== undefined && isHexColor(patch.color)) cat.color = patch.color.toLowerCase();
  if (patch.parentId !== undefined) {
    const next = patch.parentId;
    if (!next) {
      cat.parentId = null;
    } else if (next !== id && findCategory(next) && !wouldCycle(id, next)) {
      cat.parentId = next;
    }
  }
  // 未分类保持顶层，不能被挂到别人下面
  if (cat.id === UNCATEGORIZED_ID) cat.parentId = null;
  persist();
}

function wouldCycle(id: string, parentId: string): boolean {
  let cur: Category | null = findCategory(parentId);
  let guard = 0;
  while (cur && guard++ < 8) {
    if (cur.id === id) return true;
    cur = cur.parentId ? findCategory(cur.parentId) : null;
  }
  return false;
}

/** 删除分类：子分类上移一层，属于它的应用回到「未分类」。 */
export function deleteCategory(id: string): void {
  const cat = state.categories.find((c) => c.id === id);
  if (!cat || cat.builtin) return;
  const parentId = cat.parentId;
  state.categories = state.categories.filter((c) => c.id !== id);
  for (const child of state.categories) {
    if (child.parentId === id) child.parentId = parentId;
  }
  for (const [app, cid] of Object.entries(state.appMap)) {
    if (cid === id) {
      if (parentId) state.appMap[app] = parentId;
      else delete state.appMap[app];
    }
  }
  persist();
}

/** 每个分类下挂了多少个应用（设置页列表右侧的数字）。 */
export function appCountByCategory(): Record<string, number> {
  const out: Record<string, number> = {};
  for (const cid of Object.values(state.appMap)) {
    out[cid] = (out[cid] ?? 0) + 1;
  }
  return out;
}

/** 按分类汇总一组「应用 → 数值」，子分类并入顶层。 */
export function rollupByCategory(
  values: Array<{ app: string; value: number }>,
): Array<{ category: Category; value: number }> {
  const map = new Map<string, { category: Category; value: number }>();
  for (const row of values) {
    if (row.value <= 0) continue;
    const root = rootOf(categoryIdForApp(row.app));
    const slot = map.get(root.id) ?? { category: root, value: 0 };
    slot.value += row.value;
    map.set(root.id, slot);
  }
  return Array.from(map.values()).sort((a, b) => b.value - a.value);
}
