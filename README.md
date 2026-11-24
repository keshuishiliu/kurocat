> **本仓库是 [qianyiai/kurocat](https://github.com/qianyiai/kurocat) 的衍生分支**，起点为上游 **v4.9.1**（MIT License）。
> 桌宠本体——猫咪绘制、7 款皮肤、MCP 服务、OBS 透明直播源、自动更新——全部为上游原作者作品，本仓库未做重写。
> 上游的功能说明、安装包下载与 MCP 接入指南请看 [上游 README](https://github.com/qianyiai/kurocat#readme)（本仓库保留了 `README.en.md` / `README.ja.md` 两份原文）。
> **下面所有内容，都是本仓库相对上游的改动记录。**

# 🐈‍⬛ KuroCat（衍生修改版）

**当前版本 `v4.10.1`**（= 上游 v4.9.1 + 本仓库改动）。App 内「检查更新」与更新包地址校验已改为锚定本仓库 [keshuishiliu/kurocat](https://github.com/keshuishiliu/kurocat) Releases，不再跟随上游发布通道。

**下载**：[Releases · v4.10.1](https://github.com/keshuishiliu/kurocat/releases/latest) —— Windows 安装包 / macOS Universal `dmg` / Linux `deb` + `AppImage`，由 GitHub Actions 三平台构建，安装包未签名。

## 目录

1. [安全加固](#一安全加固)
2. [新增功能：键鼠输入记录](#二新增功能键鼠输入记录)
3. [新增配置项](#三新增配置项)
4. [改动文件清单](#四改动文件清单)
5. [构建与验证状态](#五构建与验证状态)
6. [已知限制](#六已知限制)

---

## 一、安全加固

上游代码审计发现 3 处可利用问题，另加 1 项配置收紧，均已修补。全部改动只收紧边界，不改变正常使用路径。

| # | 问题 | 位置 | 处理 |
| --- | --- | --- | --- |
| ① | **OBS 静态服务路径穿越**：请求行形如 `GET /C:/Windows/win.ini` 时，Windows 上 `Path::join` 遇到绝对路径会**替换**基准目录，可读取任意文件 | `src-tauri/src/obs.rs` | 新增 `sanitize_asset_path` 拒绝 `..`、`\`、`:`、NUL、前导 `/`、`is_absolute()`、百分号编码穿越、控制字符与 `<>"\|?*`；`load_asset` 再用 `canonicalize()` 做前缀校验 |
| ② | **MCP HTTP 服务无鉴权**：`127.0.0.1:27420` 对任何 `POST` 即执行工具，任意网页都能借 DNS rebinding 或跨站表单调用（改皮肤、改设置） | `src-tauri/src/mcp.rs` | 新增 `is_loopback_host` / `is_loopback_origin`，`Host` 必须为回环地址，带 `Origin` 时必须也是回环页，`Origin: null` 直接拒绝。stdio / CLI / SDK 等原生客户端不带 `Origin`，不受影响 |
| ③ | **更新信任链薄弱**：更新检查只信 JSON 里的 URL，可被伪造指向任意下载地址 | `src-tauri/src/updater.rs` | 新增 `validate_update_target`：`release_url` 必须锚定本仓库 `github.com/keshuishiliu/kurocat` 发布页，`download_url` 必须在同仓库 release 下，且哈希强制 64 位 sha256；另加 `is_safe_web_url` 限制外部链接协议 |
| ④ | **CSP 为空**：`csp: null` 时前端被注入即可任意加载外部脚本 | `src-tauri/tauri.conf.json` | 改为白名单策略：`script-src 'self'`、`object-src 'none'`、`frame-src 'none'`、`form-action 'none'`，仅放行 IPC 与 `127.0.0.1:27421`。`devCsp` 保持 `null` 以便开发热更新 |

> 审计结论：未发现后门。键鼠数据原本不落盘、不读剪贴板与窗口标题、无遥测，依赖树干净。

---

## 二、新增功能：键鼠输入记录

本地统计「敲了多少键、在哪个程序里、什么时候活跃」。**默认关闭**，数据只写本机，不上传任何地方。

### 记录什么 / 不记录什么

| 记录 | 不记录 |
| --- | --- |
| 按键**按下**：虚拟键码 `vk` + 扫描码 `scan` + 修饰位（Shift / CapsLock / 是否注入） | 按键抬起、长按自动重复（长按 3 秒只算 1 条） |
| 鼠标**按下**：左 / 中 / 右键 + 屏幕坐标 | 鼠标移动、滚轮（这两类只喂给猫咪做动作，不入库） |
| 前台进程 **exe 名**（如 `chrome.exe`） | 窗口标题、网页 URL、输入框内容 |
| 时间戳（毫秒） | 输入法候选词与上屏汉字（钩子层看不到，见[已知限制](#六已知限制)） |
| | 剪贴板、屏幕截图、任何网络传输 |

密码框内**整段跳过**（沿用上游的 UIA `CurrentIsPassword` + `ES_PASSWORD` 检测）；隐私状态翻转前后各 300 ms 的记录会被丢弃，避免边界泄露。

### 开关与隐私护栏

- **默认关闭**，且关闭状态下**一个文件都不会创建**——不建库、不起线程，第一次真正开启才落盘。
- 首次开启会弹一次告知框，明确记录范围、保存位置与当前保留期，确认后才生效（`localStorage` 记一条 consent，不会反复弹）。
- 「临时暂停」可在不丢数据的前提下随时停记，恢复即续写。
- `update_settings` 对保留期有**单向守卫**：只能改长或改成永久，改小必须走专门的清理命令并二次确认。避免一次普通设置写入就静默删光历史。

### 存储结构

目录：`%APPDATA%\kuro-cat-pet\input-log\`（macOS 为 `~/Library/Application Support/kuro-cat-pet/input-log/`）

**`events.db`**——明细，**按月分表** `events_YYYYMM`，跨月查询自动 UNION：

```sql
CREATE TABLE events_YYYYMM (
    ts     INTEGER NOT NULL,   -- 毫秒时间戳
    kind   INTEGER NOT NULL,   -- 0 = 按键按下, 1 = 鼠标按下
    code   INTEGER NOT NULL,   -- vk 或鼠标键号
    flags  INTEGER NOT NULL,   -- bit1 Shift, bit2 Caps, bit3 注入事件
    x      INTEGER,            -- 鼠标坐标，键盘行为 NULL
    y      INTEGER,
    app_id INTEGER
);
CREATE INDEX idx_events_YYYYMM_ts ON events_YYYYMM(ts);
```

**`meta.db`**——程序表 + 四张预聚合表，统计页面直接读这里，不扫明细：

| 表 | 内容 |
| --- | --- |
| `apps` | `id ↔ process` 进程名映射 |
| `daily_stats` | 按 `(day, kind, code)` 累计次数 |
| `daily_hours` | 按 `(day, hour)` 累计按键 / 点击次数 |
| `daily_apps` | 按 `(day, app_id, kind)` 累计次数与活跃秒数 |
| `monthly_app_keys` | 按 `(month, app_id, code)` 累计，供长期「某程序里最常按什么键」 |

四张表全部 `WITHOUT ROWID`，写入只有 upsert 累加，成本恒定。

### 写入与耐久

- `PRAGMA journal_mode = WAL` + `synchronous = NORMAL` + `wal_autocheckpoint = 32`。
  > NORMAL 意味着**断电或硬崩溃最多丢最近约 500 ms** 的记录，但数据库文件本身不会损坏。后两项是配套的：autocheckpoint 设小才能保证这个窗口真的只有 500 ms。
- 缓冲 500 ms 或攒够 500 行提交一次，钩子回调内只做「入队」，绝不碰磁盘。
- 退出时通过 `RunEvent::Exit` 兜底刷盘。

### 活跃时长算法

按会话式统计（sessionization）：相邻两次输入的间隔 `gap` 若 ≤ 阈值，就把 `gap` 计入活跃时长，否则视为中断。阈值即「活跃判定间隔」，**默认 5 分钟，可设 1–120**，只影响统计口径，不删任何数据。算法 O(1)，不需要遍历历史。

### 保留期

- 默认 **0 = 永久保留**。
- 设为 N 天时，清理时按边界精确 `DELETE`，整月过期的直接 `DROP TABLE` 再 `incremental_vacuum`，不做全库 `VACUUM`。
- 支持 1 天到 100 年（36500 天）；改小前先 `input_log_retention_preview` 告知将删除多少条。

### CSV 导出

流式写出，不在内存堆积，默认落到**桌面** `kurocat-input-YYYYMMDD-HHMMSS.csv`（桌面不存在时回落到应用数据目录）：

```
timestamp,local_time,kind,target,flags,app,x,y
```

`kind` 为 `key` / `mouse`，`target` 为键名（如 `Esc`、`F1`、`NumpadEnter`、`LMB`）或鼠标键名。

### 统计窗口

菜单 →「输入统计」打开**独立窗口**（1000×720，最小 880×620），左侧边栏三页：

| 页面 | 内容 |
| --- | --- |
| **仪表盘** | 今日击键数、连续操作时长、活跃程序数三张指标卡 + 记录状态网格（开关 / 暂停 / 保留期 / 数据目录） |
| **统计** | 时间范围 近 7 天 / 30 天 / 90 天 / 一年 / 全部；四张图：击键趋势、24 小时分布、应用 Top、按键 Top |
| **设置** | **「记录」与「保留期」两张卡片物理隔开**，避免误触把开关当成清理 |

图表为手写 SVG（`src/statsChart.ts`，251 行），不引入任何图表库。菜单项右侧实时显示「记录中 / 未开启」。

### IPC 命令

| 命令 | 作用 |
| --- | --- |
| `input_log_stats` | 读取状态：是否记录、暂停、保留期、活跃阈值、日志目录、今日汇总、库大小 |
| `input_log_configure` | 改开关 / 暂停 / 活跃阈值（**不动数据**） |
| `input_log_summary` | 六维聚合查询：`day` / `month` / `year` / `hour` / `app` / `key` |
| `input_log_retention_preview` | 预演：按 N 天会删多少条 |
| `input_log_apply_retention` | 真正执行清理（需二次确认） |
| `input_log_clear` | 清空全部记录（删目录并重建） |
| `input_log_export` | 导出最近 N 天为 CSV |
| `open_stats_window` | 打开统计窗口（已开则前置） |

---

## 三、新增配置项

配置文件：`%APPDATA%\kuro-cat-pet\settings.json`（macOS 见上方对照）。四个字段全部带 `serde` 默认值，**旧配置文件无需迁移**。

| 字段 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `input_log_enabled` | bool | `false` | 总开关。false 时不建库、不起线程、不落盘 |
| `input_log_active_gap_minutes` | u32 | `5` | 活跃判定间隔，1–120 分钟，越界自动夹紧 |
| `input_log_retention_days` | u32 | `0` | 保留天数，`0` = 永久。上限 36500 |
| `input_log_paused` | bool | `false` | 临时暂停。不清数据，恢复即续写 |

---

## 四、改动文件清单

相对上游 v4.9.1：**14 个文件修改 + 5 个文件新增，+651 / −17 行**（不含新增文件的合计约 3 260 行）。

### 新增

| 文件 | 行数 | 内容 |
| --- | --- | --- |
| `src-tauri/src/recorder.rs` | 1399 | 双库连接（写/查分离）、按月建表、编码与去重、批量事务、会话式活跃统计、进程名解析（200 ms 缓存）、隐私翻转窗口过滤、保留期清理、CSV 流式导出、六维聚合、vk 键名映射、5 个单元测试 |
| `stats.html` | 340 | 统计窗口三页骨架 |
| `src/statsMain.ts` | 579 | 窗口逻辑：状态刷新、图表数据装配、三个二次确认弹窗、演示模式 |
| `src/statsChart.ts` | 251 | 手写 SVG：折线、柱状、横向条形三种图元 + 轴标签避让 |
| `src/stats.css` | 688 | 窗口样式，跟随深浅色 |

### 修改

| 文件 | 变更 | 内容 |
| --- | --- | --- |
| `src-tauri/src/lib.rs` | +219 | `mod recorder`、setup 初始化与启动清理、8 个 IPC、`.build().run()` 捕获退出刷盘 |
| `src-tauri/src/mcp.rs` | +113 | 回环 Host / Origin 校验 |
| `src-tauri/src/updater.rs` | +109 | 更新来源锚定 + sha256 长度校验 + 外链协议白名单 |
| `src-tauri/src/input_hook.rs` | +95 | Shift/Caps 原子量、丢 repeat 与全部抬起、移动/滚轮不入库、缓存光标坐标供点击使用、`recorder::push()` |
| `src-tauri/src/obs.rs` | +54 | 资源路径净化 + `canonicalize` 前缀校验 |
| `src-tauri/src/config.rs` | +34 | 4 个字段 + 2 个 normalize |
| `src-tauri/Cargo.toml` | +5 | `rusqlite`（bundled，自带 SQLite 源码）+ `chrono` |
| `src-tauri/capabilities/default.json` | +7 | 允许 `stats` 窗口 |
| `src-tauri/tauri.conf.json` | ±3 | CSP 白名单 |
| `menu.html` / `src/menuMain.ts` / `src/style.css` | +23 | 「输入统计」入口与「记录中 / 未开启」状态色 |
| `vite.config.ts` | +1 | 注册 `stats.html` 入口 |

---

## 五、构建与验证状态

依赖：Node.js 18+、Rust 工具链、MSVC Build Tools（Windows）。

```bash
npm install
npm run build            # 前端：产出 dist/，含 stats.html
cd src-tauri && cargo build
```

首次 `cargo build` 会编译 **bundled SQLite**，约 1–2 分钟。

| 项 | 状态 |
| --- | --- |
| `npx tsc --noEmit` 类型检查 | ✅ 通过 |
| `npm run build` | ✅ 通过，产出 `dist/stats.html` |
| 后端全部 SQL 实跑 | ✅ 用 sqlite3 逐条执行：DDL 幂等、四表 upsert 累加、六维查询、`LIKE 'events\_%' ESCAPE`、边界 `DELETE`、`DROP` + `incremental_vacuum` |
| 界面三页布局 | ✅ 浏览器截图核对，修掉侧栏路径词中折断、趋势图末尾轴标签重叠 |
| **Rust 侧编译** | ✅ **GitHub Actions 三平台全通过**（Windows / macOS Universal / Linux amd64 + arm64），`cargo test` 单测通过 |
| 产物 | ✅ Windows NSIS 安装包 2.54 MB、macOS Universal dmg 7.03 MB、Linux deb 2.90 MB + AppImage 77.98 MB |

开发机本身没装 Rust 工具链，编译与打包由 `.github/workflows/release.yml` 在推送 `v*` 标签时于 GitHub 托管机上完成，产物自动挂到 Release。本地想自己编，装 rustup + MSVC Build Tools 后执行上面的命令即可。

---

## 六、已知限制

1. **中文输入法上屏内容不可见**。低级钩子只能拿到物理按键（你按了 `n` `i` `h` `a` `o`），拿不到候选词与上屏汉字。这是 Windows 钩子层的架构限制，不是实现缺陷——要拿到输入法结果只能读窗口文本，那等于记录输入框内容，本项目不做。
2. **长按只记 1 条**。丢弃自动重复是「只记按下」的自然推论：按住某键 3 秒，库里是 1 条记录。
3. **滚轮与鼠标移动不入库**，仅用于驱动猫咪动作。
4. **断电最多丢约 500 ms**。`synchronous = NORMAL` 的取舍，换取写放大显著下降。
5. **反作弊网游**下建议先开「暂停监听并隐藏（游戏反作弊护航）」，与上游行为一致。
6. 明细表按月分表，**跨年跨月的「全部」查询**会 UNION 所有月份表；记录了十年以上时首屏统计会变慢（预聚合四表不受影响）。

---

## 七、协议

本项目以 [MIT License](LICENSE) 发布，许可证全文与版权声明见仓库根目录的 [`LICENSE`](LICENSE)。

内置皮肤 **🐳 大肥鱼（鲸鱼娘）** 运行时素材基于 MIT 开源项目 [PC2005-cloud/dsh-pet](https://github.com/PC2005-cloud/dsh-pet)（参考衍生 [QCYTSN/dsh-dafeiyu](https://github.com/QCYTSN/dsh-dafeiyu)）制作，完整许可声明见 `src/assets/dafeiyu/LICENSE-MIT.txt`。
