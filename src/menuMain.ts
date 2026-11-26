import './style.css';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { DEFAULT_TAIL_KEY_IDS, type KeyboardOrientation, type TailKeyMode } from './catEngine';
import { DEFAULT_BUILTIN_CATS, type CatSkinConfig } from './catSkins';
import type { KeyboardLayoutId } from './keyboardLayout';
import {
  applyDomTranslations,
  isMacPlatform,
  localizeBreedName,
  normalizeLanguageSetting,
  setActiveLocale,
  t,
} from './i18n';

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
  renderMode?: '2d' | '3d';
  collarColor?: 'none' | 'red' | 'blue' | 'pink' | 'green';
  headphoneMode?: 'auto' | 'on' | 'off';
  fpsCap?: number;
  language?: string;
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
  info?: UpdateInfo | null;
}

type MenuTabId = 'appearance' | 'system';

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
  renderMode: '3d',
  collarColor: 'none',
  headphoneMode: 'off',
  fpsCap: 0,
  language: 'auto',
};

let catList: CatSkinConfig[] = [...DEFAULT_BUILTIN_CATS];
let resolvedBackendLocale: string | null = null;
let sliderDragging = false;
let currentAppVersion = '5.3.0';
let cachedUpdateInfo: UpdateInfo | null = null;
let updateBusy = false;
let activeTab: MenuTabId = 'appearance';
let settingsPatchSeq = 0;
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
const tabBodyEl = document.getElementById('menu-tab-body') as HTMLDivElement | null;
const breedPillsEl = document.getElementById('breed-pills') as HTMLDivElement;
const scaleSliderEl = document.getElementById('scale-slider') as HTMLInputElement;
const scaleValueEl = document.getElementById('scale-value') as HTMLButtonElement;
const tailModeBadgeEl = document.getElementById('tail-mode-badge') as HTMLSpanElement;
const tailKeysCountEl = document.getElementById('tail-keys-count') as HTMLSpanElement;
const btnObsHideEl = document.getElementById('btn-obs-hide') as HTMLButtonElement;
const btnCheckUpdateEl = document.getElementById('btn-check-update') as HTMLButtonElement | null;

function formatBreedPillLabel(cat: CatSkinConfig): string {
  const displayName = localizeBreedName(cat.id, cat.name, cat.builtin);
  if (cat.emoji) {
    return displayName.startsWith(cat.emoji) ? displayName : `${cat.emoji} ${displayName}`;
  }
  return `🐱 ${displayName}`;
}

function formatHeaderCatName(cat: CatSkinConfig): string {
  const emoji = cat.emoji || '🐱';
  const displayName = localizeBreedName(cat.id, cat.name, cat.builtin);
  const bareName =
    cat.emoji && displayName.startsWith(cat.emoji)
      ? displayName.slice(cat.emoji.length).trimStart()
      : displayName;
  return `${emoji} KuroCat ${bareName}`;
}

function switchTab(nextTab: MenuTabId): void {
  activeTab = nextTab;
  document.querySelectorAll<HTMLButtonElement>('[data-tab]').forEach((btn) => {
    const isActive = btn.dataset.tab === activeTab;
    btn.classList.toggle('active', isActive);
    btn.setAttribute('aria-selected', isActive ? 'true' : 'false');
  });
  document.querySelectorAll<HTMLElement>('[data-tab-panel]').forEach((panel) => {
    panel.classList.toggle('hidden', panel.dataset.tabPanel !== activeTab);
  });
  if (tabBodyEl) {
    tabBodyEl.scrollTop = 0;
  }
}

function renderFooterUpdateState(customLabel?: string, ready = false): void {
  const hint = document.getElementById('update-hint');
  const isReady = ready || Boolean(cachedUpdateInfo?.hasUpdate);
  if (btnCheckUpdateEl) {
    btnCheckUpdateEl.classList.toggle('update-ready', isReady);
  }
  if (!hint) return;
  if (customLabel) {
    hint.textContent = customLabel;
  } else if (cachedUpdateInfo?.hasUpdate) {
    hint.textContent = `⬆ ${t('menu.update_ready', { ver: cachedUpdateInfo.latestVersion })}`;
  } else {
    hint.textContent = t('menu.check_update', { ver: currentAppVersion });
  }
}

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
    const label = formatBreedPillLabel(cat);
    btn.textContent = label;
    btn.title = label;
    const accent = cat.accentColor || (cat.id === 'ragdoll' ? '#7ac7ff' : '#c8f538');
    btn.style.setProperty('--pill-accent', accent);
    btn.classList.toggle('active', cat.id === settings.catBreed);
    breedPillsEl.appendChild(btn);
  }
}

function syncUI(): void {
  setActiveLocale(settings.language, resolvedBackendLocale);
  applyDomTranslations();

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
    obsHint.textContent = settings.obsEnabled ? `:${settings.obsPort || 27421}` : t('menu.obs_hint');
  }
  const mcpHint = document.getElementById('mcp-copy-hint');
  if (mcpHint && !mcpHint.classList.contains('copied')) {
    mcpHint.textContent = t('menu.copy_mcp');
  }
  const hooksHint = document.getElementById('hooks-copy-hint');
  if (hooksHint && !hooksHint.classList.contains('copied')) {
    hooksHint.textContent = t('menu.copy_hooks');
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

  const fpsCap = settings.fpsCap === 30 ? '30' : '0';
  document.querySelectorAll<HTMLButtonElement>('[data-fpscap]').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.fpscap === fpsCap);
  });

  const langSetting = normalizeLanguageSetting(settings.language);
  document.querySelectorAll<HTMLButtonElement>('[data-lang]').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.lang === langSetting);
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
    tailModeBadgeEl.textContent = t(`menu.tail_badge_${tailMode}`);
  }
  if (tailKeysCountEl) {
    tailKeysCountEl.textContent = t('menu.tail_keys_count', { count: tailIds.length });
  }

  const activeCat = catList.find((c) => c.id === settings.catBreed) ?? catList[0];
  renderBreedPills();

  const renderMode = settings.renderMode === '2d' ? '2d' : '3d';
  const collarColor =
    settings.collarColor === 'red' ||
    settings.collarColor === 'blue' ||
    settings.collarColor === 'pink' ||
    settings.collarColor === 'green'
      ? settings.collarColor
      : 'none';
  document.querySelectorAll<HTMLButtonElement>('[data-rendermode]').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.rendermode === renderMode);
  });
  document.querySelectorAll<HTMLButtonElement>('[data-collar]').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.collar === collarColor);
  });
  const renderModeRowEl = document.getElementById('render-mode-row');
  if (renderModeRowEl) {
    renderModeRowEl.classList.toggle('hidden', settings.catBreed === 'dafeiyu');
  }
  const collarRowEl = document.getElementById('collar-row');
  if (collarRowEl) {
    collarRowEl.classList.toggle('hidden', renderMode === '2d' || settings.catBreed === 'dafeiyu');
  }

  const headphoneMode =
    settings.headphoneMode === 'auto' || settings.headphoneMode === 'on'
      ? settings.headphoneMode
      : 'off';
  document.querySelectorAll<HTMLButtonElement>('[data-headphonemode]').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.headphonemode === headphoneMode);
  });
  const headphoneRowEl = document.getElementById('headphone-row');
  if (headphoneRowEl) {
    headphoneRowEl.classList.toggle('hidden', settings.catBreed === 'dafeiyu');
  }

  const headerEl = document.getElementById('menu-header');
  if (headerEl && activeCat) {
    headerEl.textContent = formatHeaderCatName(activeCat);
    headerEl.style.color = activeCat.accentColor || '#c8f538';
  }

  if (!updateBusy) {
    renderFooterUpdateState();
  }
}

async function applySettingsPatch(patch: Partial<AppSettings>): Promise<void> {
  const seq = ++settingsPatchSeq;
  settings = { ...settings, ...patch };
  syncUI();
  try {
    const updated = await invoke<AppSettings>('update_settings', { patch: settings });
    if (seq !== settingsPatchSeq) return;
    settings = updated;
    syncUI();
  } catch (err) {
    console.error('Failed to update settings:', err);
  }
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
      hint.textContent = t('menu.copied_hint');
      hint.classList.add('copied');
      setTimeout(() => {
        hint.textContent = t('menu.copy_mcp');
        hint.classList.remove('copied');
      }, 1800);
    }
  } catch (err) {
    console.error('Failed to copy MCP config:', err);
  }
}

async function copyAgentHooksConfig(): Promise<void> {
  const hint = document.getElementById('hooks-copy-hint');
  try {
    const jsonText = await invoke<string>('get_agent_hooks_config');
    await copyText(jsonText);
    if (hint) {
      hint.textContent = t('menu.copied_hint');
      hint.classList.add('copied');
      setTimeout(() => {
        hint.textContent = t('menu.copy_hooks');
        hint.classList.remove('copied');
      }, 1800);
    }
  } catch (err) {
    console.error('Failed to copy Agent hooks config:', err);
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
      hint.textContent = t('menu.copied_hint');
      hint.classList.add('copied');
      setTimeout(() => {
        hint.classList.remove('copied');
        hint.textContent = settings.obsEnabled ? `:${settings.obsPort || 27421}` : t('menu.obs_hint');
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
      hint.textContent = t('menu.created_hint');
      hint.classList.add('copied');
      setTimeout(() => {
        hint.textContent = 'KuroCat';
        hint.classList.remove('copied');
      }, 1800);
    }
  } catch (err) {
    console.error('Failed to create shortcut:', err);
    if (hint) {
      hint.textContent = '✕';
      setTimeout(() => {
        hint.textContent = 'KuroCat';
      }, 1800);
    }
  }
}

function startInstallUpdate(info: UpdateInfo): void {
  updateBusy = true;
  renderFooterUpdateState(t('menu.downloading'), true);
  // 成功路径由 kuro-update-status 事件驱动（downloading/verifying/installing/ready）。
  void invoke('install_update', { info }).catch((msg) => {
    updateBusy = false;
    console.error('Update install failed:', msg);
    renderFooterUpdateState(`⚠ ${t('menu.update_failed')}`, false);
    setTimeout(() => {
      renderFooterUpdateState();
    }, 4000);
  });
}

async function triggerManualUpdateCheck(): Promise<void> {
  if (updateBusy) return;
  // 绿色版：已经有下载好的包时，这一项直接变成「应用并重启」，不再走网络检查。
  try {
    const pending = await invoke<PendingPortableUpdate | null>('get_pending_update');
    if (pending?.version) {
      renderFooterUpdateState(t('menu.applying_update'), true);
      closeMenu();
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
  renderFooterUpdateState(t('menu.checking_short'), false);
  try {
    const info = await invoke<UpdateInfo>('check_for_update');
    latestUpdateInfo = info;
    cachedUpdateInfo = info;
    if (!info.hasUpdate) {
      updateBusy = false;
      renderFooterUpdateState(`✓ v${info.currentVersion}`, false);
      setTimeout(() => {
        renderFooterUpdateState();
      }, 2200);
      return;
    }
    if (info.downloadUrl) {
      // 与宠物窗口一致：发现新版本直接自动下载安装，不跳浏览器。
      startInstallUpdate(info);
      return;
    }
    // 找不到匹配的更新包（Win7 兼容版 / 手动安装场景）：退回打开发布页。
    updateBusy = false;
    renderFooterUpdateState(`⬆ ${t('menu.update_ready', { ver: info.latestVersion })}`, true);
    if (info.releaseUrl) {
      void invoke('open_release_page', { url: info.releaseUrl });
    }
  } catch {
    updateBusy = false;
    renderFooterUpdateState(`⚠ ${t('menu.update_failed')}`, false);
    setTimeout(() => {
      renderFooterUpdateState();
    }, 2200);
  }
}

async function bootstrap(): Promise<void> {
  if (isMacPlatform()) {
    const shortcutBtn = document.getElementById('btn-desktop-shortcut');
    if (shortcutBtn) shortcutBtn.classList.add('hidden');
  }

  let resolveInitReady!: () => void;
  const initReadyPromise = new Promise<void>((resolve) => {
    resolveInitReady = resolve;
  });

  // Register menu click delegation immediately so fast clicks right after side-menu
  // recreation (post 25s idle destroy) are queued behind initial IPC load rather than dropped.
  menuEl.addEventListener('click', (e) => {
    const tabBtn = (e.target as HTMLElement).closest<HTMLButtonElement>('[data-tab]');
    if (tabBtn) {
      const next = tabBtn.dataset.tab;
      if (next === 'appearance' || next === 'system') {
        switchTab(next);
      }
      return;
    }

    const target = (e.target as HTMLElement).closest<HTMLElement>(
      '[data-action], [data-breed], [data-rendermode], [data-collar], [data-headphonemode], [data-kblayout], [data-kborient], [data-tailmode], [data-fpscap], [data-lang]',
    );
    if (!target) return;

    void initReadyPromise.then(() => {
      const fpsAttr = target.dataset.fpscap;
      if (fpsAttr === '0' || fpsAttr === '30') {
        void applySettingsPatch({ fpsCap: fpsAttr === '30' ? 30 : 0 });
        return;
      }

      const langAttr = target.dataset.lang;
      if (langAttr) {
        void applySettingsPatch({ language: normalizeLanguageSetting(langAttr) });
        return;
      }

      const headphoneAttr = target.dataset.headphonemode;
      if (headphoneAttr === 'auto' || headphoneAttr === 'on' || headphoneAttr === 'off') {
        void applySettingsPatch({ headphoneMode: headphoneAttr });
        return;
      }

      const renderModeAttr = target.dataset.rendermode;
      if (renderModeAttr === '2d' || renderModeAttr === '3d') {
        void applySettingsPatch({ renderMode: renderModeAttr });
        return;
      }

      const collarAttr = target.dataset.collar;
      if (
        collarAttr === 'none' ||
        collarAttr === 'red' ||
        collarAttr === 'blue' ||
        collarAttr === 'pink' ||
        collarAttr === 'green'
      ) {
        void applySettingsPatch({ collarColor: collarAttr });
        return;
      }

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
        case 'copy-hooks':
          void copyAgentHooksConfig();
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
  });

  let inTauri = true;
  try {
    const [loadedSettings, loadedCats, ver, cachedUpd, resolvedLoc] = await Promise.all([
      invoke<AppSettings>('get_settings'),
      invoke<CatSkinConfig[]>('get_cats'),
      invoke<string>('get_app_version'),
      invoke<UpdateInfo | null>('get_cached_update_info').catch(() => null),
      invoke<string>('get_resolved_locale').catch(() => ''),
    ]);
    settings = loadedSettings;
    if (Array.isArray(loadedCats) && loadedCats.length > 0) {
      catList = loadedCats;
    }
    if (ver) {
      currentAppVersion = ver;
    }
    if (cachedUpd) {
      cachedUpdateInfo = cachedUpd;
    }
    if (resolvedLoc) {
      resolvedBackendLocale = resolvedLoc;
    }
    // 绿色版有待应用的更新包时，页脚提示待更新版本（不影响后续手动检查流程）。
    try {
      const pending = await invoke<PendingPortableUpdate | null>('get_pending_update');
      const pendingVer = pending?.version?.replace(/^v/i, '');
      if (pendingVer) {
        renderFooterUpdateState(`⟳ v${pendingVer}`, true);
      }
    } catch {
      // 没有待更新包，保持显示当前版本
    }
  } catch (err) {
    inTauri = false;
    console.warn('Running menu outside Tauri IPC:', err);
    // Allow static browser layout verification via ?lang=en-US&tab=system&obs=1
    const params = new URLSearchParams(window.location.search);
    const qLang = params.get('lang');
    if (qLang) settings.language = qLang;
    if (params.get('obs') === '1') settings.obsEnabled = true;
    const qTab = params.get('tab');
    if (qTab === 'appearance' || qTab === 'system') activeTab = qTab;
    // Populate all 7 built-in cats for realistic layout verification outside Tauri
    catList = [
      ...DEFAULT_BUILTIN_CATS.slice(0, 2),
      { ...DEFAULT_BUILTIN_CATS[0], id: 'orange-tabby', name: '大橘猫', emoji: '🍊', accentColor: '#ff9f43' },
      { ...DEFAULT_BUILTIN_CATS[0], id: 'calico', name: '三花猫', emoji: '🌸', accentColor: '#ff7eb3' },
      { ...DEFAULT_BUILTIN_CATS[0], id: 'tuxedo', name: '奶牛猫', emoji: '🐄', accentColor: '#d0d7de' },
      { ...DEFAULT_BUILTIN_CATS[0], id: 'siamese', name: '暹罗猫', emoji: '☕', accentColor: '#c9a679' },
      DEFAULT_BUILTIN_CATS[2],
    ];
  }
  syncUI();
  renderFooterUpdateState();
  switchTab(activeTab);
  resolveInitReady();

  if (inTauri) {
    const isWin7 = await invoke<boolean>('is_win7_ulw_mode').catch(() => false);
    if (isWin7) {
      document.documentElement.classList.add('win7-ulw-mode');
    }
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
      void invoke('preview_scale', { scale: nextScale }).catch(() => {});
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

  if (!inTauri) return;

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

  await listen<UpdateProgressEvent>('kuro-update-status', (ev) => {
    if (ev.payload.info) {
      cachedUpdateInfo = ev.payload.info;
    }
    if (ev.payload.stage === 'available' && cachedUpdateInfo?.hasUpdate) {
      updateBusy = false;
      renderFooterUpdateState();
    } else if (
      ev.payload.stage === 'downloading' ||
      ev.payload.stage === 'verifying' ||
      ev.payload.stage === 'installing'
    ) {
      updateBusy = true;
      renderFooterUpdateState(t('menu.checking_short'), true);
    } else if (ev.payload.stage === 'up_to_date' || ev.payload.stage === 'error') {
      updateBusy = false;
      renderFooterUpdateState();
    }
  });

  await listen('kuro-menu-opened', async () => {
    try {
      const [latestSettings, latestCats, cachedUpd, resolvedLoc] = await Promise.all([
        invoke<AppSettings>('get_settings'),
        invoke<CatSkinConfig[]>('get_cats'),
        invoke<UpdateInfo | null>('get_cached_update_info').catch(() => null),
        invoke<string>('get_resolved_locale').catch(() => ''),
      ]);
      settings = latestSettings;
      if (Array.isArray(latestCats) && latestCats.length > 0) {
        catList = latestCats;
      }
      if (cachedUpd) {
        cachedUpdateInfo = cachedUpd;
      }
      if (resolvedLoc) {
        resolvedBackendLocale = resolvedLoc;
      }
      syncUI();
      renderFooterUpdateState();
      if (tabBodyEl) tabBodyEl.scrollTop = 0;
    } catch {}
  });
}

void bootstrap();
