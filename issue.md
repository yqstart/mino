# 全量代码审查问题清单

审查基线：提交 `16c64892c669024caf36301861101642b32060e0`，并包含工作树中现有的 `crates/mino-app/src/app.rs` 未提交改动。

> **复核说明（2026-09-28）**：本清单是一次历史全量审查记录（基线 `16c6489`），其中引用的行号已随版本迭代偏移、正文描述未必代表当前实现。已按当前代码（`main`，含 v1.1.2 之后改动）逐项复核，每条下方给出最新状态与证据。

审查范围：两个 crate 的全部 Rust 源码与测试、workspace/crate Cargo 配置、`.github/workflows`、`scripts`、根目录及 `docs` 下的项目文档。`target/` 和 Cargo registry 依赖源码不属于项目源码，未作为审查对象；仅在确认上游 API 契约时查阅。

严重级别：高 = 安全边界失效、错误主机操作或可导致进程内存失控；中 = 明确的功能错误、数据丢失风险、更新/发布失败或稳定复现的测试回归；低 = 低概率生命周期风险、次要状态错误或文档/维护脚本问题。

## 高严重级别

### 1. known_hosts 损坏或保存失败时会 fail-open，TOFU 校验可被静默绕过

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— `known_hosts.rs` 仅把 `NotFound` 视为首次连接；读取 / 解析失败直接 `Err`，保存失败 `map_err` 后拒绝信任（含 `损坏文件拒绝信任` 等单测）

**位置：** `crates/mino-core/src/ssh/known_hosts.rs:49`、`crates/mino-core/src/ssh/known_hosts.rs:132`

`load_known_hosts` 把所有读取错误和 TOML 解析错误都转换为空记录；首次记录新指纹时，`save_known_hosts` 失败也只写 warning，随后仍返回 `Ok(true)`。因此，只要 `known_hosts.toml` 损坏、不可读、目录只读或磁盘写满，下一次连接就会把已知主机当作首次连接并接受当前服务器密钥。此时原本用于阻止中间人攻击的安全边界失效，而且 UI 不会提示用户。

**建议：** 仅将 `NotFound` 视为首次连接；读取、解析和保存失败必须拒绝主机密钥并把错误传到 UI。损坏文件应保留并要求用户显式修复，不能自动重新信任。

### 2. bracketed paste 直接包装原始文本，可被内嵌结束序列提前退出

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— 粘贴改走 `bracketed_paste_payload`：先剥离 `ESC` 再包 `200~`/`201~`，`ESC[201~` 与换行注入有回归测试

**位置：** `crates/mino-app/src/views/terminal_view.rs:763`

开启 `BRACKETED_PASTE` 时，代码直接构造 `ESC[200~ + text + ESC[201~`。如果剪贴板文本自身包含 `ESC[201~`，它会提前结束括号粘贴；后续换行和控制序列将由 shell 正常解释，可能把本应只进入编辑缓冲区的粘贴内容直接执行。这是终端剪贴板注入的典型触发路径。

**建议：** 与成熟终端一致，在 bracketed paste 模式下移除或转义粘贴内容中的 `ESC`，并增加包含 `\x1b[201~` 和换行的回归测试。

### 3. SFTP 通过可变 Vec 下标绑定标签，延迟就绪后可能挂载到错误标签页

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— SSH/SFTP 共用 `connection_id`，挂载前校验目标标签仍匹配（不匹配即 close）；关闭标签按稳定 `tab_id` 取消 pending 连接

**位置：** `crates/mino-app/src/app.rs:529`、`crates/mino-app/src/app.rs:714`、`crates/mino-app/src/app.rs:748`

SSH 成功后把 `active_tab` 下标保存到 `pending_tab`，SFTP 稍后通过该下标挂载；`close_tab` 在删除或移动标签下标时没有同步更新 `pending_tab`。例如 SSH 标签创建后，用户再创建一个本地标签、关闭等待 SFTP 的 SSH 标签，原下标就可能指向这个本地标签；SFTP Ready 到达后会被挂载到错误会话。用户可能在以为属于另一终端的面板中操作远端文件。

**建议：** 为每个标签和每次连接分配稳定 ID/generation，将 SSH 与 SFTP 结果绑定到连接 ID；挂载前校验目标标签仍存在且主机身份一致。关闭标签时取消对应的待处理连接。

### 4. 本地终端和 SFTP 使用无界事件积压，非活动标签可持续耗尽内存

> **状态（2026-09-28 按当前代码复核）**：⚠️ 部分修复 —— Wakeup 改 `AtomicBool` 合并、事件队列 `MAX_PENDING_EVENTS=256`、PTY 写回 64KiB 上限、SFTP 事件 `channel(128)` + 满则丢进度、应用层每帧 drain 全部标签；**残余**：`ClipboardStore` 不受条数上限约束，极端 OSC52 大粘贴仍可撑大队列

**位置：** `crates/mino-core/src/terminal/mod.rs:67`、`crates/mino-core/src/terminal/mod.rs:82`、`crates/mino-core/src/ssh/sftp.rs:169`、`crates/mino-core/src/ssh/sftp.rs:407`、`crates/mino-app/src/app.rs:2685`、`crates/mino-app/src/views/sftp_view.rs:587`

本地 PTY 的每个 Wakeup 都追加到 `Mutex<Vec<SessionEvent>>`，但只有活动标签的 `TerminalView::show` 会 drain；后台运行 `yes` 一类高输出命令后切换标签，Wakeup 会无限积压。SFTP 也用 unbounded channel，并且每 64 KiB 发送一条带 `String` 的 Progress；只有活动标签且面板实际渲染时才 poll。大文件传输时切换标签或收起面板，会积累大量事件并持续增加内存。

**建议：** Wakeup 改为可合并的原子标记，不进入队列；进度按 transfer ID 保存最新值或使用 bounded/watch channel。应用层每帧应轮询所有存活会话的生命周期事件，而不是只轮询当前可见面板。

## 中严重级别

### 5. ANSI 背景段构建与缓存指纹都不正确

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— 背景段合并要求 `last.end == col`；`CellStyle::key()` 已含 bg；行指纹改用完整 `cell_keys`

**位置：** `crates/mino-app/src/views/terminal_view.rs:419`、`crates/mino-app/src/views/terminal_view.rs:1413`、`crates/mino-app/src/views/terminal_view.rs:1522`

存在两个独立错误：

1. 背景段只比较颜色，不检查 `last.end == col`。同色背景中间隔着默认背景时，后一个色块会把前一个段直接延长，错误覆盖中间的默认背景。
2. 行 hash 只包含前景色、样式和字符，不包含背景色；损坏行如果只改变背景，`hash == cache.hash` 会复用旧 `backgrounds`，颜色不会更新。

**建议：** 背景段只合并相邻 cell；把解析后的背景色和所有影响绘制的 cell 属性加入指纹，或分别比较文本与背景缓存。

### 6. 切换主题不会使终端行缓存失效，屏幕会混用旧主题颜色

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— `theme_revision()` 变化时清空行缓存并失效行网格（`last_theme_revision` 跟踪）

**位置：** `crates/mino-app/src/app.rs:934`、`crates/mino-app/src/app.rs:2612`、`crates/mino-app/src/views/terminal_view.rs:327`

终端缓存仅在 DPI 或尺寸变化时清空。主题切换后，未被终端 damage 标记的行继续使用旧 Galley 和旧背景段，ANSI 基本色、默认前景及显式背景会保留旧主题颜色，直到该行内容再次变化。

**建议：** 给主题增加 revision/index，`TerminalView` 检测变化后清空全部行缓存；增加“已有彩色内容后切主题”的渲染回归测试。

### 7. renderer 未实现 alacritty Cell 的完整字符语义，会丢组合字符并压缩隐藏字符列

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— `zerowidth()` 参与渲染与复制；HIDDEN / LEADING spacer 按空白 cell 处理，普通 WIDE_CHAR_SPACER 不进文本段

**位置：** `crates/mino-app/src/views/terminal_view.rs:1317`、`crates/mino-app/src/views/terminal_view.rs:1360`

渲染和复制都只读取 `cell.c`，完全忽略 `cell.zerowidth()`，因此分解形式的重音字符、部分组合输入内容会丢失。渲染还把 `HIDDEN` 当作可直接跳过的 spacer，导致后续字符向左移动，而不是保留一个空白 cell；`LEADING_WIDE_CHAR_SPACER` 也未按 alacritty 语义处理。

**建议：** 复用 alacritty 的 cell/selection 语义：主字符后追加 zerowidth 字符，隐藏字符绘制等宽空白，正确跳过两类宽字符 spacer。为组合字符、SGR conceal 和跨行宽字符补测试。

### 8. 自定义选区复制会破坏软换行内容和有意义的尾随空格

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— `selection_to_text` 不再无条件 `trim_end` / 插 `
`：仅末列无 `WRAPLINE` 才换行，组合符随 `zerowidth()` 复制

**位置：** `crates/mino-app/src/views/terminal_view.rs:1295`

`selection_to_text` 对每个网格行无条件 `trim_end()`，并在每两行之间无条件插入 `\n`，没有检查 `WRAPLINE`。复制长命令、URL 或程序输出的软换行区域时会插入不存在的换行；局部选择末尾的空格也会被删除。它同样遗漏 zerowidth 字符。

**建议：** 使用 `Term` 已有的 selection-to-string 行为，或完整实现 WRAPLINE、宽字符、组合字符和“仅整行选择时去填充空格”的规则。

### 9. 应用光标模式和多类功能键会丢失修饰键

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— APP_CURSOR + 修饰键走 `csi_with_mods`；Insert/Delete、F5–F12 均有 xterm 修饰形式

**位置：** `crates/mino-core/src/terminal/keys.rs:118`、`crates/mino-core/src/terminal/keys.rs:150`、`crates/mino-core/src/terminal/keys.rs:194`

当 `APP_CURSOR` 开启时，方向键/Home/End 无条件发送普通 SS3 序列，Shift/Alt/Ctrl 全被忽略；F5-F12、Insert/Delete/PageUp/PageDown 也没有生成 xterm 修饰形式。结果是 Ctrl+方向键、Shift+F5 等组合退化为未修饰按键，shell、vim/tmux 等无法区分。

**建议：** 按 xterm 规则在存在修饰键时优先发送 CSI `1;N` 或 `code;N~` 形式，并覆盖 APP_CURSOR + modifier、F5-F12 + modifier、Insert/Delete + modifier 测试。

### 10. 取消更新并不取消下载，重试会并发写同一个 DMG 文件

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— 重试前 `cancel_download()`；临时文件按 `sequence` 唯一命名；取消即删半成品且不上报 `Done`

**位置：** `crates/mino-app/src/app.rs:595`、`crates/mino-app/src/app.rs:2245`、`crates/mino-core/src/updater.rs:136`

“取消”只丢弃 `download_rx` 并切回 Idle，下载线程仍继续运行。用户立即重试同一版本时，新旧线程使用相同 `temp_dmg_path`，都通过 `File::create` 截断并写入同一文件；最终可能得到交错或被后一个线程删除的 DMG，状态却由新线程报告为完成。

**建议：** 引入 cancellation token，并让下载循环及时检查；每次下载使用唯一临时文件，完成后校验并原子 rename 到最终路径。开始新下载前等待或明确终止旧任务。

### 11. 更新器把 30 秒 global timeout 用于整个 DMG 响应体

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— 检查与下载拆成两个 agent：检查保留 30s 全局超时，下载按阶段超时（每次读取 30s，无整包限制）

**位置：** `crates/mino-core/src/updater.rs:79`

同一个 `make_agent` 同时用于 Atom 检查和资产下载，`timeout_global(30s)` 覆盖 DNS、连接以及完整响应体读取。只要 DMG 在 30 秒内下载不完，正常慢速网络就会稳定报“下载中断”。

**建议：** 检查更新保留短 global timeout；资产下载使用独立配置，只设置连接/单次读取超时或采用更合理的长总时限，并增加慢速流测试。

### 12. 安装脚本只要成功 spawn，应用就宣称安装成功并主动退出

> **状态（2026-09-28 按当前代码复核）**：⚠️ 部分修复 —— 安装脚本已用状态文件协议（`write_result ready/success/error`）+ 120s 超时，`error` 会回 UI；**残余**：收到 `ready` 即显示「已更新」并 0.9s 退出，之后的 `ditto` / 重启失败用户看不到（仅 `install-*.log`）

**位置：** `crates/mino-app/src/app.rs:663`、`crates/mino-app/src/app.rs:391`、`crates/mino-app/src/app.rs:2561`、`crates/mino-app/src/app.rs:2734`

`launch_installer` 只确认 `/bin/sh` 已启动，随后状态立刻改为 `Installed`，0.9 秒后关闭应用。脚本后续的 `hdiutil`、DMG 内容检查或 `ditto` 任一步失败都会直接 exit，应用已经退出，旧版本也不会被重新打开；UI 的“已更新”并不代表安装成功。

**建议：** 安装器应有可观测的阶段/结果协议。至少在失败分支重新打开原应用并留下可见日志；更稳妥的是先验证和 staging，新进程确认可替换后再让主进程退出。

### 13. 后台更新事件没有唤醒 egui，检查结果和下载进度依赖偶然重绘

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— 更新检查与下载线程都持有 `Context` 并在结果 / 进度时 `request_repaint()`

**位置：** `crates/mino-app/src/app.rs:544`、`crates/mino-app/src/app.rs:595`、`crates/mino-app/src/app.rs:2637`

更新检查和下载线程只向 `std::sync::mpsc` 发送事件，没有调用 `ctx.request_repaint`；UI 只有在下一次已经发生的 frame 中才会 poll。关闭全部终端标签或光标不闪烁时，手动检查结果和下载进度可以一直不更新，直到鼠标/键盘产生新帧。

**建议：** 后台任务持有 `egui::Context` clone，并在发送结果/节流后的进度时请求重绘；或使用统一的 UI wakeup 通道。

### 14. SSH 失败但 SFTP 已 Ready 时，后台 SFTP 连接会被永久保留

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— SSH 失败分支关闭 `pending_sftp` 与 `ready_sftp` 句柄（不再遗留后台连接）

**位置：** `crates/mino-app/src/app.rs:706`、`crates/mino-app/src/app.rs:760`

SFTP 先 Ready 时会被移动到 `ready_sftp`。若 SSH 终端随后失败，`poll_connection` 只显示 toast，不清理 `ready_sftp`。其中的 `SftpHandle` 继续保持命令发送端存活，后台线程停在 `cmd_rx.recv()`，SSH/SFTP 连接和线程会一直存在，直到发起下一次连接或退出进程。

**建议：** SSH/SFTP 使用同一 connection ID 和统一取消句柄；任一必需通道失败时关闭另一通道并清空所有相关状态。

### 15. 主机配置持久化失败对用户不可见，备份失败时还会给出错误成功提示

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— 备份失败时 `config_write_blocked` 禁止覆盖原文件；`save_config` 失败回滚内存态并 toast 真实结果

**位置：** `crates/mino-app/src/app.rs:446`、`crates/mino-app/src/app.rs:488`、`crates/mino-app/src/app.rs:682`

配置解析失败后，即使复制 `.bak` 也失败，启动 toast 仍声称“原文已备份”。之后新增/删除主机时，`save_config` 只写日志，不向 UI 返回失败；用户会以为配置已保存，重启后发现数据丢失。若原配置损坏且备份失败，后续一次成功保存还会覆盖唯一原文。

**建议：** 让 `save_config` 返回结果并显示持久错误；备份失败时禁止覆盖原文件，toast 必须反映真实结果。配置页可显示“未保存”状态并提供重试。

### 16. 多个 UI 测试直接读取用户真实 hosts.toml，失败日志会泄露主机信息

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— `MinoApp::new` 在测试构建走 `test_config_path("default")` 隔离路径；`cfg!(not(test))` 关闭自动更新

**位置：** `crates/mino-app/src/app.rs:2924`、`crates/mino-app/src/app.rs:2970`、`crates/mino-app/src/app.rs:3595`、`crates/mino-app/src/app.rs:3833`

至少 10 个测试直接调用 `MinoApp::new(cc)`，因此读取真实 `~/.config/mino/hosts.toml`，还会启动真实更新网络请求。本轮主题测试失败时，kittest accessibility tree 已把保存的主机名称和地址写入测试输出，实际证明了隐私泄露路径。这也违反项目在同文件中声明的测试隔离约定。

**建议：** 所有 app 测试统一使用 `new_with_config(test_config_path(...))`；测试构造器应默认禁用自动更新。增加 lint/helper，避免测试代码再次调用生产配置入口。

### 17. 文档声称无测试 sshd 时自动跳过，但两个集成测试仍会失败

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— 两个集成测试连接前先 `TcpStream::connect_timeout(500ms)` 探测端口，无测试 sshd 时跳过

**位置：** `crates/mino-core/tests/ssh_remote.rs:95`、`crates/mino-core/tests/sftp_remote.rs:65`、`.github/workflows/ci.yml:2`

两个测试只在私钥不存在时跳过，没有探测 `host:port` 是否可达。常见开发机已有 `~/.ssh/id_ed25519` 但未启动测试 sshd，此时 SSH 测试立即 Connection refused，SFTP 测试等待约 10 秒后失败。本轮 `cargo test -p mino-core` 已稳定复现。

**建议：** 连接前用短超时探测测试端口，或要求显式 `MINO_RUN_SSH_INTEGRATION=1` 才运行；CI 若要覆盖链路，应在 job 中真正启动测试 sshd。

### 18. 当前主题渲染测试稳定失败，workspace 测试不是绿色

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— 主题渲染测试当前稳定通过（`cargo test -p mino-app -- --test-threads=1`：157 passed）

**位置：** `crates/mino-app/src/app.rs:3592`、`crates/mino-app/src/app.rs:3618`

`app::theme_tests::三套主题渲染截图` 打开 ComboBox 后找不到第一个“深色”选项。完整 workspace 运行结果为 mino-app 62 passed / 1 failed；单独精确运行该用例也立即复现。跳过该用例后其余 62 个 app 测试通过。

**建议：** 修复 ComboBox 弹层交互或调整测试为稳定的 popup 查询流程；发布前保持完整 `cargo test --workspace -- --test-threads=1` 通过，不能依靠 skip。

### 19. 手动 Release workflow 会把分支名当版本和 tag，且不校验 Cargo 版本

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— `workflow_dispatch` 必须填 tag；构建前校验 tag 格式 / tag 存在 / Cargo 版本 / CHANGELOG 段落四件套

**位置：** `.github/workflows/release.yml:12`、`.github/workflows/release.yml:69`、`.github/workflows/release.yml:100`、`.github/workflows/release.yml:115`、`Cargo.toml:6`

workflow 文档允许手动 Run，但 `workflow_dispatch` 没有版本输入或 tag 校验；从 `main` 运行时，`GITHUB_REF_NAME` 是 `main`，流程会生成 `mino-main-...dmg` 并尝试发布 `main`。此外推送 `v0.2.0` 时也未验证 workspace 的 `CARGO_PKG_VERSION` 是否为 `0.2.0`，可能出现资产/Info.plist 为 0.2.0、二进制仍自报 0.1.0 并反复提示更新。

**建议：** 手动触发要求 semver tag 输入并 checkout 该 tag；在构建前比较 tag、Cargo metadata 和 CHANGELOG 版本，不一致直接失败。仅允许 `refs/tags/v*` 进入 publish job。

## 低严重级别

### 20. 延迟 SIGKILL 只保存裸 PID，存在 PID 复用后误杀无关进程的竞态

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— `signal_child_if_running` 在发信号前重新确认子进程身份，不再仅凭保存的裸 PID

**位置：** `crates/mino-core/src/terminal/mod.rs:368`

Session Drop 后无条件启动线程，300 ms 后对保存的 PID 发 SIGKILL。若 shell 已退出且系统在窗口期内复用该 PID，信号可能发给无关进程；“shell 已退时 kill 无害”只在 PID 未复用时成立。

**建议：** 延迟任务应能确认原 PTY child 仍是同一进程，例如由 PTY 线程完成信号取消 killer，或使用可验证的进程句柄/平台等待机制，避免只凭裸 PID。

### 21. 同名传输以显示 label 作为身份，重试会复用旧的完成/失败状态

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— 传输以唯一 `id` 为身份（`begin_transfer(id, …)`），label 仅用于展示

**位置：** `crates/mino-app/src/views/sftp_view.rs:181`、`crates/mino-app/src/views/sftp_view.rs:212`

`begin_transfer` 发现相同 label 就直接返回，Progress/Done/Error 也只更新第一个同名条目且不会重置 `finished`/`failed`。再次上传或下载同名文件时，界面可能从一开始就显示“完成”或“失败”，进度落在旧记录上。

**建议：** 每次操作分配唯一 transfer ID，label 只用于展示；重试应创建新记录或显式重置全部状态。

### 22. SECURITY.md 仍声明“未实现 known_hosts”，与当前实现和 CHANGELOG 冲突

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— SECURITY.md 已改为如实描述 TOFU、known_hosts 路径 / 0600 与读取失败拒绝连接

**位置：** `SECURITY.md:36`

安全政策告诉用户远程会话无条件接受任意服务器密钥，但当前代码和 CHANGELOG 已实现 TOFU。错误的安全现状会误导用户，也会掩盖本报告第 1 项中真正的 fail-open 条件。

**建议：** 更新安全现状，准确说明 TOFU、配置路径、首次信任含义以及损坏/权限错误的处理策略。

### 23. 测试 sshd 停止脚本可能杀死无关进程/会话

> **状态（2026-09-28 按当前代码复核）**：✅ 已修复 —— 脚本用 `is_test_sshd_pid` 校验 PID 身份，且已无宽泛的 `pkill` 兜底

**位置：** `scripts/test-sshd.sh:74`、`scripts/test-sshd.sh:83`

`stop` 直接信任 pidfile 并 `kill`，未验证 PID 仍属于该测试 sshd；随后执行宽泛的 `pkill -f "sshd-session:.*@ttys"`，可能命中不属于本测试实例的 SSH 会话。

**建议：** 校验 PID 的 executable/config 参数，只终止该测试 daemon 的子进程；避免按宽泛进程名全局 pkill。

## 本轮验证结果（2026-09-27 当时快照，部分已过期；当前状态以上方各条状态行为准）

- `cargo clippy --workspace --all-targets -- -D warnings`：通过，零警告。
- `cargo fmt --all --check`：通过。
- `git diff --check`：通过。
- `cargo test -p mino-app -- --test-threads=1 --skip app::theme_tests::三套主题渲染截图`：62 passed。
- `cargo test -p mino-app app::theme_tests::三套主题渲染截图 -- --exact --nocapture`：稳定失败，找不到“深色”选项。
- `cargo test -p mino-core -- --test-threads=1`：22 个单元测试通过；随后 SFTP 集成测试因本地 2222 端口未启动而失败。
- `cargo test -p mino-core --test ssh_remote -- --test-threads=1`：因本地 2222 端口未启动而失败，确认未按文档自动跳过。
- `cargo test --workspace -- --test-threads=1`：失败；首先暴露主题测试回归，完整绿色验证尚未成立。
