import './style.css';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { DEFAULT_TAIL_KEY_IDS, type KeyboardOrientation, type TailKeyMode } from './catEngine';
import { DEFAULT_BUILTIN_CATS, type CatSkinConfig } from './catSkins';
import type { KeyboardLayoutId } from './keyboardLayout';

interface AppSettings {
  x: number | null;
  y: number | null;
  scale: number;
  alwaysOnTop: boolean;
  clickThroughLocked: boolean;
  showKeyBubbles: boolean;
  soundEnabled: boolean;
  autostart: boolean;
  catBreed: string;
  mcpEnabled: boolean;
  mcpPort: number;
  keyboardLayout: KeyboardLayoutId;
  keyboardOrientation: KeyboardOrientation;
  privacyPasswordGuard: boolean;
  tailKeyMode: TailKeyMode;
  tailKeyIds: string[];
  obsEnabled: boolean;
  obsPort: number;
  obsHidePet: boolean;
  inputLogEnabled?: boolean;
  inputLogPaused?: boolean;
}

interface UpdateInfo {
  currentVersion: string;
  latestVersion: string;
  hasUpdate: boolean;
  releaseUrl: string;
  assetName: string | null;
  downloadUrl: string | null;
  assetSha256: string | null;
  assetSize: number | null;
  installKind: string;
  notesSummary: string;
}

/** 绿色版：已下载但还没应用的更新包。 */
interface PendingPortableUpdate {
  version: string;
  filePath: string;
  sha256: string | null;
}

/** Rust 侧 `kuro-update-status` 广播的更新进度事件。 */
interface UpdateProgressEvent {
  stage: 'checking' | 'available' | 'downloading' | 'verifying' | 'installing' | 'ready' | 'up_to_date' | 'error';
  message: string;
  info: UpdateInfo | null;
}

const TAIL_MODE_LABELS: Record<TailKeyMode, string> = {
  smart: '智能第三只手',
  custom: '自定义按键',
  edge: '近尾边缘区',
  off: '仅摆动',
};

let settings: AppSettings = {
  x: null,
  y: null,
  scale: 1,
  alwaysOnTop: true,
  clickThroughLocked: false,
  showKeyBubbles: true,
  soundEnabled: true,
  autostart: false,
  catBreed: 'black',
  mcpEnabled: true,
  mcpPort: 27420,
  keyboardLayout: '65',
  keyboardOrientation: 'cat',
  privacyPasswordGuard: true,
  tailKeyMode: 'smart',
  tailKeyIds: [...DEFAULT_TAIL_KEY_IDS],
  obsEnabled: false,
  obsPort: 27421,
  obsHidePet: false,
};

let catList: CatSkinConfig[] = [...DEFAULT_BUILTIN_CATS];
let sliderDragging = false;
let currentAppVersion = '4.16.0';

// 更新流程状态：菜单窗口与宠物窗口共用 Rust 侧同一套 install_update 管线。
let updateBusy = false;
let latestUpdateInfo: UpdateInfo | null = null;
let hintRestoreTimer: number | null = null;

const updateHintEl = document.getElementById('update-hint') as HTMLSpanElement;

function setUpdateHint(text: string, ready = false): void {
  if (!updateHintEl) return;
  updateHintEl.textContent = text;
  updateHintEl.classList.toggle('copied', ready);
}

/** 延时把版本徽标恢复成当前版本号（ready 状态不被覆盖）。 */
function restoreHintLater(ms: number): void {
  if (hintRestoreTimer !== null) window.clearTimeout(hintRestoreTimer);
  hintRestoreTimer = window.setTimeout(() => {
    hintRestoreTimer = null;
    if (!updateHintEl || updateHintEl.classList.contains('copied')) return;
    updateHintEl.textContent = `v${currentAppVersion}`;
  }, ms);
}

const menuEl = document.getElementById('context-menu') as HTMLDivElement;
const breedPillsEl = document.getElementById('breed-pills') as HTMLDivElement;
const scaleSliderEl = document.getElementById('scale-slider') as HTMLInputElement;
const scaleValueEl = document.getElementById('scale-value') as HTMLButtonElement;
const tailModeBadgeEl = document.getElementById('tail-mode-badge') as HTMLSpanElement;
const tailKeysCountEl = document.getElementById('tail-keys-count') as HTMLSpanElement;
const btnObsHideEl = document.getElementById('btn-obs-hide') as HTMLButtonElement;

function closeMenu(): void {
  void invoke('close_context_menu');
}

function updateScaleSliderVisual(scaleVal: number): void {
  const pctNum = Math.round(Math.max(20, Math.min(150, scaleVal * 100)));
  if (scaleSliderEl) {
    scaleSliderEl.value = String(pctNum);
    const fillPct = ((pctNum - 20) / (150 - 20)) * 100;
    scaleSliderEl.style.setProperty('--pct', `${fillPct.toFixed(1)}%`);
  }
  if (scaleValueEl) {
    scaleValueEl.textContent = `${pctNum}%`;
  }
}

function renderBreedPills(): void {
  if (!breedPillsEl) return;
  breedPillsEl.innerHTML = '';
  for (const cat of catList) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'breed-pill';
    btn.dataset.breed = cat.id;
    const emoji = cat.emoji ? `${cat.emoji} ` : '🐱 ';
    btn.textContent = `${emoji}${cat.name}`;
    const accent = cat.accentColor || (cat.id === 'ragdoll' ? '#7ac7ff' : '#c8f538');
    btn.style.setProperty('--pill-accent', accent);
    btn.classList.toggle('active', cat.id === settings.catBreed);
    breedPillsEl.appendChild(btn);
  }
}

function syncUI(): void {
  const setCheck = (id: string, on: boolean) => {
    const el = document.getElementById(id);
    if (el) el.textContent = on ? '✓' : '';
  };

  setCheck('chk-privacy', settings.privacyPasswordGuard !== false);
  setCheck('chk-bubbles', settings.showKeyBubbles);
  setCheck('chk-lock', settings.clickThroughLocked);
  setCheck('chk-sound', settings.soundEnabled);
  setCheck('chk-top', settings.alwaysOnTop);
  setCheck('chk-autostart', settings.autostart);
  setCheck('chk-mcp', settings.mcpEnabled);
  setCheck('chk-obs', Boolean(settings.obsEnabled));
  setCheck('chk-obs-hide', Boolean(settings.obsEnabled && settings.obsHidePet));

  if (btnObsHideEl) {
    btnObsHideEl.classList.toggle('hidden', !settings.obsEnabled);
  }
  const obsHint = document.getElementById('obs-copy-hint');
  if (obsHint && !obsHint.classList.contains('copied')) {
    obsHint.textContent = settings.obsEnabled ? `:${settings.obsPort || 27421}` : '免抠像';
  }

  const statsHint = document.getElementById('stats-hint');
  if (statsHint) {
    const recording = Boolean(settings.inputLogEnabled) && !settings.inputLogPaused;
    statsHint.textContent = recording ? '记录中' : '未开启';
    statsHint.classList.toggle('rec-on', recording);
  }

  if (!sliderDragging) {
    updateScaleSliderVisual(settings.scale);
  }

  const kbLayout: KeyboardLayoutId = settings.keyboardLayout === '108' ? '108' : '65';
  const kbOrient: KeyboardOrientation = settings.keyboardOrientation === 'viewer' ? 'viewer' : 'cat';
  document.querySelectorAll<HTMLButtonElement>('[data-kblayout]').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.kblayout === kbLayout);
  });
  document.querySelectorAll<HTMLButtonElement>('[data-kborient]').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.kborient === kbOrient);
  });

  const tailMode: TailKeyMode =
    settings.tailKeyMode === 'custom' ||
    settings.tailKeyMode === 'edge' ||
    settings.tailKeyMode === 'off'
      ? settings.tailKeyMode
      : 'smart';
  const tailIds = Array.isArray(settings.tailKeyIds) ? settings.tailKeyIds : [...DEFAULT_TAIL_KEY_IDS];
  document.querySelectorAll<HTMLButtonElement>('[data-tailmode]').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.tailmode === tailMode);
  });
  if (tailModeBadgeEl) {
    tailModeBadgeEl.textContent = TAIL_MODE_LABELS[tailMode];
  }
  if (tailKeysCountEl) {
    tailKeysCountEl.textContent = `${tailIds.length} 键`;
  }

  const activeCat = catList.find((c) => c.id === settings.catBreed) ?? catList[0];
  renderBreedPills();

  const headerEl = document.getElementById('menu-header');
  if (headerEl && activeCat) {
    const emoji = activeCat.emoji || '🐱';
    headerEl.textContent = `${emoji} KuroCat ${activeCat.name}`;
    headerEl.style.color = activeCat.accentColor || '#c8f538';
  }
}

async function applySettingsPatch(patch: Partial<AppSettings>): Promise<void> {
  settings = { ...settings, ...patch };
  syncUI();
  settings = await invoke<AppSettings>('update_settings', { patch: settings });
  syncUI();
}

async function copyText(text: string): Promise<void> {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  document.body.appendChild(ta);
  ta.select();
  document.execCommand('copy');
  document.body.removeChild(ta);
}

async function copyMcpConfig(): Promise<void> {
  const hint = document.getElementById('mcp-copy-hint');
  try {
    const jsonText = await invoke<string>('get_mcp_config');
    await copyText(jsonText);
    if (hint) {
      hint.textContent = '✓ 已复制';
      hint.classList.add('copied');
      setTimeout(() => {
        hint.textContent = 'AI 捏猫';
        hint.classList.remove('copied');
      }, 1800);
    }
  } catch (err) {
    console.error('Failed to copy MCP config:', err);
  }
}

async function copyObsUrl(): Promise<void> {
  const hint = document.getElementById('obs-copy-hint');
  try {
    if (!settings.obsEnabled) {
      await applySettingsPatch({ obsEnabled: true });
    }
    const url = await invoke<string>('get_obs_url');
    await copyText(url);
    if (hint) {
      hint.textContent = '✓ 已复制';
      hint.classList.add('copied');
      setTimeout(() => {
        hint.classList.remove('copied');
        hint.textContent = settings.obsEnabled ? `:${settings.obsPort || 27421}` : '免抠像';
      }, 2000);
    }
  } catch (err) {
    console.error('Failed to copy OBS url:', err);
  }
}

async function createDesktopShortcut(): Promise<void> {
  const hint = document.getElementById('shortcut-hint');
  try {
    await invoke<string>('create_desktop_shortcut');
    if (hint) {
      hint.textContent = '✓ 已创建';
      hint.classList.add('copied');
      setTimeout(() => {
        hint.textContent = 'KuroCat';
        hint.classList.remove('copied');
      }, 1800);
    }
  } catch (err) {
    console.error('Failed to create shortcut:', err);
    if (hint) {
      hint.textContent = '失败';
      setTimeout(() => {
        hint.textContent = 'KuroCat';
      }, 1800);
    }
  }
}

function startInstallUpdate(info: UpdateInfo): void {
  updateBusy = true;
  setUpdateHint('下载中…');
  // 成功路径由 kuro-update-status 事件驱动（downloading/verifying/installing/ready）。
  void invoke('install_update', { info }).catch((msg) => {
    updateBusy = false;
    console.error('Update install failed:', msg);
    setUpdateHint('更新失败');
    restoreHintLater(4000);
  });
}

async function triggerManualUpdateCheck(): Promise<void> {
  if (updateBusy) return;
  // 绿色版：已经有下载好的包时，这一项直接变成「应用并重启」，不再走网络检查。
  try {
    const pending = await invoke<PendingPortableUpdate | null>('get_pending_update');
    if (pending?.version) {
      setUpdateHint('应用更新中…');
      await invoke('apply_portable_update');
      return;
    }
  } catch {
    // 没有待更新包，继续正常检查
  }
  // 上一轮已经发现过新版本：这次点击直接开始下载安装，不再重复检查。
  if (latestUpdateInfo?.hasUpdate && latestUpdateInfo.downloadUrl) {
    startInstallUpdate(latestUpdateInfo);
    return;
  }
  updateBusy = true;
  setUpdateHint('检查中…');
  try {
    const info = await invoke<UpdateInfo>('check_for_update');
    latestUpdateInfo = info;
    if (!info.hasUpdate) {
      updateBusy = false;
      setUpdateHint(`✓ 最新 v${info.currentVersion}`);
      restoreHintLater(2200);
      return;
    }
    if (info.downloadUrl) {
      // 与宠物窗口一致：发现新版本直接自动下载安装，不跳浏览器。
      startInstallUpdate(info);
      return;
    }
    // 找不到匹配的更新包（Win7 兼容版 / 手动安装场景）：退回打开发布页。
    updateBusy = false;
    setUpdateHint(`发现 v${info.latestVersion}`);
    if (info.releaseUrl) {
      void invoke('open_release_page', { url: info.releaseUrl });
    }
    restoreHintLater(4000);
  } catch {
    updateBusy = false;
    setUpdateHint('检查失败');
    restoreHintLater(2200);
  }
}

async function bootstrap(): Promise<void> {
  try {
    const [loadedSettings, loadedCats, ver] = await Promise.all([
      invoke<AppSettings>('get_settings'),
      invoke<CatSkinConfig[]>('get_cats'),
      invoke<string>('get_app_version'),
    ]);
    settings = loadedSettings;
    if (Array.isArray(loadedCats) && loadedCats.length > 0) {
      catList = loadedCats;
    }
    if (ver) {
      currentAppVersion = ver;
      const hint = document.getElementById('update-hint');
      if (hint) hint.textContent = `v${ver}`;
      // 绿色版有待应用的更新包时，把版本徽标换成「⟳ 待更新」提示
      try {
        const pending = await invoke<PendingPortableUpdate | null>('get_pending_update');
        const pendingVer = pending?.version?.replace(/^v/i, '');
        if (pendingVer && hint) hint.textContent = `⟳ v${pendingVer}`;
      } catch {
        // 没有待更新包，保持显示当前版本
      }
    }
  } catch (err) {
    console.error('Failed to load menu state:', err);
  }
  syncUI();

  const isWin7 = await invoke<boolean>('is_win7_ulw_mode').catch(() => false);
  if (isWin7) {
    document.documentElement.classList.add('win7-ulw-mode');
  }

  document.getElementById('btn-close-menu')?.addEventListener('click', () => closeMenu());

  window.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closeMenu();
  });

  if (scaleSliderEl) {
    scaleSliderEl.addEventListener('pointerdown', () => {
      sliderDragging = true;
    });
    scaleSliderEl.addEventListener('input', () => {
      sliderDragging = true;
      const nextScale = Math.max(0.2, Math.min(1.5, Number(scaleSliderEl.value) / 100));
      updateScaleSliderVisual(nextScale);
      void invoke('preview_scale', { scale: nextScale });
    });
    const commitSliderScale = () => {
      sliderDragging = false;
      const nextScale = Math.max(0.2, Math.min(1.5, Number(scaleSliderEl.value) / 100));
      void applySettingsPatch({ scale: nextScale });
    };
    scaleSliderEl.addEventListener('change', commitSliderScale);
    scaleSliderEl.addEventListener('pointerup', () => {
      sliderDragging = false;
    });
  }

  if (scaleValueEl) {
    scaleValueEl.addEventListener('click', (e) => {
      e.stopPropagation();
      sliderDragging = false;
      updateScaleSliderVisual(1.0);
      void applySettingsPatch({ scale: 1.0 });
    });
  }

  menuEl.addEventListener('click', (e) => {
    const target = (e.target as HTMLElement).closest<HTMLElement>(
      '[data-action], [data-breed], [data-kblayout], [data-kborient], [data-tailmode]',
    );
    if (!target) return;

    const kbLayoutAttr = target.dataset.kblayout;
    if (kbLayoutAttr === '65' || kbLayoutAttr === '108') {
      void applySettingsPatch({ keyboardLayout: kbLayoutAttr });
      return;
    }

    const kbOrientAttr = target.dataset.kborient;
    if (kbOrientAttr === 'cat' || kbOrientAttr === 'viewer') {
      void applySettingsPatch({ keyboardOrientation: kbOrientAttr });
      return;
    }

    const tailModeAttr = target.dataset.tailmode;
    if (
      tailModeAttr === 'smart' ||
      tailModeAttr === 'custom' ||
      tailModeAttr === 'edge' ||
      tailModeAttr === 'off'
    ) {
      void applySettingsPatch({ tailKeyMode: tailModeAttr });
      return;
    }

    const breedAttr = target.dataset.breed;
    if (breedAttr) {
      void applySettingsPatch({ catBreed: breedAttr });
      return;
    }

    switch (target.dataset.action) {
      case 'toggle-privacy':
        void applySettingsPatch({ privacyPasswordGuard: !(settings.privacyPasswordGuard !== false) });
        break;
      case 'toggle-bubbles':
        void applySettingsPatch({ showKeyBubbles: !settings.showKeyBubbles });
        break;
      case 'toggle-lock':
        void applySettingsPatch({ clickThroughLocked: !settings.clickThroughLocked });
        closeMenu();
        break;
      case 'toggle-sound':
        void applySettingsPatch({ soundEnabled: !settings.soundEnabled });
        break;
      case 'toggle-top':
        void applySettingsPatch({ alwaysOnTop: !settings.alwaysOnTop });
        break;
      case 'toggle-autostart':
        void applySettingsPatch({ autostart: !settings.autostart });
        break;
      case 'toggle-obs': {
        const nextObs = !settings.obsEnabled;
        void applySettingsPatch({
          obsEnabled: nextObs,
          obsHidePet: nextObs ? settings.obsHidePet : false,
        });
        break;
      }
      case 'copy-obs':
        void copyObsUrl();
        break;
      case 'toggle-obs-hide':
        void applySettingsPatch({ obsHidePet: !settings.obsHidePet });
        if (!settings.obsHidePet) {
          closeMenu();
        }
        break;
      case 'toggle-mcp':
        void applySettingsPatch({ mcpEnabled: !settings.mcpEnabled });
        break;
      case 'copy-mcp':
        void copyMcpConfig();
        break;
      case 'open-tail-picker':
        void invoke('open_tail_picker_from_menu');
        break;
      case 'open-stats':
        void invoke('open_stats_window');
        closeMenu();
        break;
      case 'desktop-shortcut':
        void createDesktopShortcut();
        break;
      case 'reset-pos':
        void invoke('reset_position');
        closeMenu();
        break;
      case 'check-update':
        void triggerManualUpdateCheck();
        break;
      case 'quit':
        void invoke('quit_app');
        break;
    }
  });

  await listen<AppSettings>('kuro-settings', (ev) => {
    settings = ev.payload;
    syncUI();
  });

  // 更新进度：只响应菜单窗口自己发起的 install_update 流程（updateBusy 期间），
  // 宠物窗口 / 托盘触发的更新事件不去抢，避免两边同时下载。
  await listen<UpdateProgressEvent>('kuro-update-status', (ev) => {
    if (!updateBusy) return;
    const { stage, message, info } = ev.payload;
    const ver = info?.latestVersion;
    switch (stage) {
      case 'downloading':
        setUpdateHint(`⬇ v${ver} 下载中…`);
        break;
      case 'verifying':
        setUpdateHint('校验中…');
        break;
      case 'installing':
        // 绿色版「应用并重启」路径的 installing 事件不带 info。
        setUpdateHint(ver ? `安装 v${ver} 中…` : '应用更新中…');
        break;
      case 'ready':
        updateBusy = false;
        if (info) latestUpdateInfo = info;
        setUpdateHint(`⟳ v${ver} 待重启`, true);
        break;
      case 'up_to_date':
        updateBusy = false;
        setUpdateHint(`✓ 最新 v${info?.currentVersion ?? currentAppVersion}`);
        restoreHintLater(2200);
        break;
      case 'error':
        updateBusy = false;
        setUpdateHint('更新失败');
        restoreHintLater(4000);
        break;
      default:
        break;
    }
    if (message) console.info(`[update] ${stage}: ${message}`);
  });

  await listen<CatSkinConfig[]>('kuro-cats', (ev) => {
    if (Array.isArray(ev.payload) && ev.payload.length > 0) {
      catList = ev.payload;
      syncUI();
    }
  });

  await listen('kuro-menu-opened', async () => {
    try {
      const [latestSettings, latestCats] = await Promise.all([
        invoke<AppSettings>('get_settings'),
        invoke<CatSkinConfig[]>('get_cats'),
      ]);
      settings = latestSettings;
      if (Array.isArray(latestCats) && latestCats.length > 0) {
        catList = latestCats;
      }
      syncUI();
      menuEl.scrollTop = 0;
    } catch {}
  });
}

void bootstrap();
