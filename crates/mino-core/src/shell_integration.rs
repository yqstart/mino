//! Shell 集成：受管 zsh 片段安装与 vendor 插件管理。
//!
//! 安装形态对照其它终端的 zsh 安装器（只取受管块 / 幂等 / 备份三点）：
//! - `~/.zshrc` 里用一对标记注释包住一行 `source "$HOME/.config/mino/shell/mino.zsh"`；
//! - 集成脚本与 starship 预设内嵌在二进制里（`assets/`），安装时落到
//!   `~/.config/mino/`，因此卸载只需要删掉受管块，不依赖原始压缩包；
//! - 备份只在首次安装时生成一次，之后永不覆盖（用户的 dotfiles 改动不会被
//!   第二次安装覆盖掉）。
//!
//! 所有路径都由调用方传入（`zshrc` / `config_dir` 参数化），方便测试用临时
//! 目录隔离，绝不隐式读写真实 HOME。

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// zsh 集成脚本（安装到 `<config_dir>/shell/mino.zsh`）。
pub const SCRIPT: &str = include_str!("../assets/shell-integration/mino.zsh");

/// 打包的 starship 预设（安装到 `<config_dir>/starship.toml`）。
pub const STARSHIP_TOML: &str = include_str!("../assets/vendor/starship.toml");

/// 受管块开始标记。
const BLOCK_START: &str = "# >>> mino shell integration >>>";
/// 受管块结束标记。
const BLOCK_END: &str = "# <<< mino shell integration <<<";
/// 受管块里的 source 行。写 `$HOME` 而不是展开后的绝对路径：配置目录整体
/// 搬迁（或 .zshrc 被同步到别的机器）后依然有效。
const BLOCK_SOURCE: &str = r#"source "$HOME/.config/mino/shell/mino.zsh""#;
/// 集成脚本文件名。
const SCRIPT_NAME: &str = "mino.zsh";
/// starship 预设文件名。
const STARSHIP_NAME: &str = "starship.toml";
/// `.zshrc` 所在的配置子目录名。
const SHELL_DIR_NAME: &str = "shell";
/// vendor 目录名（相对 `shell/`）。
const VENDOR_DIR_NAME: &str = "vendor";
/// 首次安装的备份后缀（备份文件与 `.zshrc` 同目录）。
const BACKUP_SUFFIX: &str = ".mino.bak";

/// 安装结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    /// 本次是否写入了 `.zshrc` 的受管块（首次安装、或块内容有变化时为 `true`；
    /// 已是最新形态时为 `false`，调用方可据此知道没有触碰文件）。
    pub installed: bool,
    /// 首次安装生成的备份（`<zshrc>.mino.bak`）；已有备份或非首次安装为 `None`。
    pub backup: Option<PathBuf>,
    /// 集成脚本落盘路径（`<config_dir>/shell/mino.zsh`）。
    pub script_path: PathBuf,
    /// starship 预设落盘路径（`<config_dir>/starship.toml`）。
    pub starship_path: PathBuf,
}

/// 安装 shell 集成。
///
/// 先落集成脚本与 starship 预设（`.zshrc` 的 source 行指向它们，必须先生效），
/// 再处理 `.zshrc` 里的受管块：
/// - 没有块：视为首次安装。原文件非空时先备份成 `<zshrc>.mino.bak`（已有备份
///   不覆盖），再追加受管块；
/// - 已有块：整块替换，内容一致时不写文件（重复安装幂等）。
///
/// `zshrc` 是指向 dotfiles 仓库的符号链接时，按链接目标原子替换：对链接路径
/// 直接临时文件 + rename 会把链接本身变成一个普通文件，破坏用户的 dotfiles
/// 布局（备份仍按传入路径命名，保持安装器对外语义直观）。
pub fn install(zshrc: &Path, config_dir: &Path) -> io::Result<InstallOutcome> {
    let shell_dir = shell_dir(config_dir);
    let script_path = shell_dir.join(SCRIPT_NAME);
    let starship_path = config_dir.join(STARSHIP_NAME);
    // 0600：配置目录里是用户偏好与本机路径，没有给同机其他用户开放读取的
    // 理由（默认 umask 022 会给出 0644）。
    write_atomic(&script_path, SCRIPT, Some(0o600))?;
    write_atomic(&starship_path, STARSHIP_TOML, Some(0o600))?;
    // vendor 目录先建好（插件克隆交给 bootstrap_vendor），权限收敛到 0700。
    create_private_dir(&vendor_dir(config_dir))?;

    let target = write_target(zshrc);
    let existing = read_optional(&target)?.unwrap_or_default();
    let block = block_text();

    let mut backup = None;
    let new_content = match find_block(&existing) {
        Some((start, end)) => {
            let mut content = String::with_capacity(existing.len() + block.len());
            content.push_str(&existing[..start]);
            content.push_str(&block);
            content.push_str(&existing[end..]);
            content
        }
        None => {
            if !existing.is_empty() {
                let path = backup_path(zshrc);
                // 已有备份不覆盖：第二次安装（先卸载再装）不会冲掉用户第一次
                // 安装前的 .zshrc。
                if !path.exists() {
                    write_atomic(&path, &existing, existing_mode(&target))?;
                    backup = Some(path);
                }
            }
            let mut content = String::with_capacity(existing.len() + block.len() + 1);
            content.push_str(&existing);
            if !existing.is_empty() {
                // 前置分隔换行：uninstall 会按同一规则去掉，这样原文件即使没有
                // 行尾换行，卸载后也能逐字节还原。
                content.push('\n');
            }
            content.push_str(&block);
            content
        }
    };

    let installed = new_content != existing;
    if installed {
        // 保留原权限位（`.zshrc` 常见 0644，不能被原子替换改成 0600）。
        write_atomic(&target, &new_content, existing_mode(&target))?;
    }

    Ok(InstallOutcome {
        installed,
        backup,
        script_path,
        starship_path,
    })
}

/// 卸载：删除 `.zshrc` 里的受管块（含安装时补的分隔空行），返回是否删掉。
///
/// 只动受管块，不备份、不改其它内容；文件不存在或没有块时返回 `false` 且
/// 不触碰文件。
pub fn uninstall(zshrc: &Path) -> io::Result<bool> {
    let target = write_target(zshrc);
    let Some(existing) = read_optional(&target)? else {
        return Ok(false);
    };
    let Some((start, end)) = find_block(&existing) else {
        return Ok(false);
    };
    // 一并去掉块前那个分隔换行（见 install），还原到安装前的字节。
    let start = if start > 0 && existing.as_bytes()[start - 1] == b'\n' {
        start - 1
    } else {
        start
    };
    let mut content = String::with_capacity(existing.len() - (end - start));
    content.push_str(&existing[..start]);
    content.push_str(&existing[end..]);
    write_atomic(&target, &content, existing_mode(&target))?;
    Ok(true)
}

/// `.zshrc` 是否已包含受管块（文件不存在或读不出来时为 `false`）。
pub fn is_installed(zshrc: &Path) -> bool {
    match std::fs::read_to_string(zshrc) {
        Ok(content) => find_block(&content).is_some(),
        Err(_) => false,
    }
}

/// vendor 目录：`<config_dir>/shell/vendor`（与 `mino.zsh` 里的加载路径一致）。
pub fn vendor_dir(config_dir: &Path) -> PathBuf {
    shell_dir(config_dir).join(VENDOR_DIR_NAME)
}

/// `shell/` 子目录：集成脚本与 vendor 插件都放在这里。
fn shell_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(SHELL_DIR_NAME)
}

/// 可选加载的外部插件（克隆到 vendor 目录，缺失即跳过）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plugin {
    /// 历史命令内联建议（`zsh-autosuggestions`）。
    ZshAutosuggestions,
    /// 按访问频率跳目录（`zsh-z`）。
    ZshZ,
    /// 补充补全定义（`zsh-completions`）。
    ZshCompletions,
    /// 命令语法高亮（`zsh-syntax-highlighting`）。
    ZshSyntaxHighlighting,
}

impl Plugin {
    /// 全部插件。顺序就是 `mino.zsh` 的加载顺序：autosuggestions 必须最先
    /// （其它插件要包装它定义的 widget），syntax-highlighting 必须最后。
    pub const ALL: [Plugin; 4] = [
        Plugin::ZshAutosuggestions,
        Plugin::ZshZ,
        Plugin::ZshCompletions,
        Plugin::ZshSyntaxHighlighting,
    ];

    /// vendor 目录下的子目录名（与 `mino.zsh` 里的加载路径一致）。
    pub fn dir_name(self) -> &'static str {
        match self {
            Plugin::ZshAutosuggestions => "zsh-autosuggestions",
            Plugin::ZshZ => "zsh-z",
            Plugin::ZshCompletions => "zsh-completions",
            Plugin::ZshSyntaxHighlighting => "zsh-syntax-highlighting",
        }
    }

    /// 上游仓库地址。
    pub fn repo_url(self) -> &'static str {
        match self {
            Plugin::ZshAutosuggestions => "https://github.com/zsh-users/zsh-autosuggestions.git",
            Plugin::ZshZ => "https://github.com/agkozak/zsh-z.git",
            Plugin::ZshCompletions => "https://github.com/zsh-users/zsh-completions.git",
            Plugin::ZshSyntaxHighlighting => {
                "https://github.com/zsh-users/zsh-syntax-highlighting.git"
            }
        }
    }

    /// 入口路径（相对插件目录）。`mino.zsh` 就是按这个路径判断有没有插件。
    fn entry_rel_path(self) -> &'static str {
        match self {
            Plugin::ZshAutosuggestions => "zsh-autosuggestions.zsh",
            Plugin::ZshZ => "zsh-z.plugin.zsh",
            Plugin::ZshCompletions => "src",
            Plugin::ZshSyntaxHighlighting => "zsh-syntax-highlighting.zsh",
        }
    }

    /// 入口是否是目录（只有 zsh-completions 是往 FPATH 里塞目录）。
    fn entry_is_dir(self) -> bool {
        matches!(self, Plugin::ZshCompletions)
    }

    /// 克隆目标目录。
    fn target_dir(self, vendor: &Path) -> PathBuf {
        vendor.join(self.dir_name())
    }

    /// vendor 目录下是否已有可用插件（入口存在即视为装好）。
    fn is_present(self, vendor: &Path) -> bool {
        let entry = self.target_dir(vendor).join(self.entry_rel_path());
        if self.entry_is_dir() {
            entry.is_dir()
        } else {
            entry.is_file()
        }
    }
}

/// 尚未安装的插件，按加载顺序返回（vendor 目录下缺少入口的算缺失）。
pub fn missing_plugins(config_dir: &Path) -> Vec<Plugin> {
    let vendor = vendor_dir(config_dir);
    Plugin::ALL
        .into_iter()
        .filter(|plugin| !plugin.is_present(&vendor))
        .collect()
}

/// 把缺失的插件逐个克隆到 `<config_dir>/shell/vendor`。
///
/// - 逐个串行 `git clone --depth 1 --single-branch`：插件只需要当前版本，
///   浅克隆省时间省流量；串行是为了避免并发克隆在慢网络下互相挤占。
/// - 已存在的插件跳过并计为成功。
/// - 单个插件失败不影响其它插件：网络不可用时全部返回 `Err`，不 panic。
/// - 禁用交互：`GIT_TERMINAL_PROMPT=0` 且 stdin 接 `/dev/null`，否则缺少凭据
///   或走代理时 git 会停下来等输入，把安装流程挂死。
///
/// 返回顺序与 [`Plugin::ALL`] 一致，调用方可按 `Ok`/`Err` 分别展示成功与
/// 失败（`Err` 里是可以直接给用户看的错误摘要）。
pub fn bootstrap_vendor(config_dir: &Path) -> Vec<Result<Plugin, String>> {
    bootstrap_vendor_with(config_dir, "git")
}

/// [`bootstrap_vendor`] 的实现；`git_bin` 可替换，测试用不存在的可执行文件
/// 覆盖失败路径（不触网）。
fn bootstrap_vendor_with(config_dir: &Path, git_bin: &str) -> Vec<Result<Plugin, String>> {
    let vendor = vendor_dir(config_dir);
    if let Err(error) = create_private_dir(&vendor) {
        return Plugin::ALL
            .into_iter()
            .map(|_plugin| {
                Err(format!(
                    "创建 vendor 目录 {} 失败：{error}",
                    vendor.display()
                ))
            })
            .collect();
    }
    Plugin::ALL
        .into_iter()
        .map(|plugin| clone_plugin(plugin, &vendor, git_bin))
        .collect()
}

/// 克隆单个插件（已存在则跳过）。
fn clone_plugin(plugin: Plugin, vendor: &Path, git_bin: &str) -> Result<Plugin, String> {
    if plugin.is_present(vendor) {
        return Ok(plugin);
    }
    let target = plugin.target_dir(vendor);
    if target.exists() {
        // 空目录是上次 `git clone` 失败留下的（git 半路失败时会把自己创建的
        // 目录清空但保留），里面没有任何数据，删掉重来——否则网络抖动一次就
        // 永远装不上，用户还得自己去删空目录。
        if is_empty_dir(&target) {
            std::fs::remove_dir(&target)
                .map_err(|error| format!("清理空目录 {} 失败：{error}", target.display()))?;
        } else {
            // 有内容但入口缺失：多半是克隆中断或用户手工放的半成品，交给用户
            // 处理——不递归删除不属于本次安装的目录，也不硬塞进 git clone
            //（git 拒绝克隆到非空目录）。
            return Err(format!(
                "{} 已存在但内容不完整，请删除 {} 后重试",
                plugin.dir_name(),
                target.display()
            ));
        }
    }
    let output = Command::new(git_bin)
        .args([
            "clone",
            "--depth",
            "1",
            "--single-branch",
            plugin.repo_url(),
        ])
        .arg(&target)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("无法执行 {git_bin}（{}）：{error}", plugin.dir_name()))?;
    if output.status.success() {
        return Ok(plugin);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "克隆 {} 失败：{}",
        plugin.repo_url(),
        stderr_summary(&stderr)
    ))
}

/// 目录是否为空；读不到内容时按“非空”处理（宁可让用户确认，也不擅自删目录）。
fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

/// git 报错摘要：取 stderr 最后一行非空内容（前面多是 "Cloning into ..." 之类
/// 的进度输出，最后一行才是真正的 failure）。
fn stderr_summary(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .unwrap_or_else(|| "无错误输出".into())
}

/// 受管块全文（含行尾换行）。
fn block_text() -> String {
    format!("{BLOCK_START}\n{BLOCK_SOURCE}\n{BLOCK_END}\n")
}

/// 定位受管块，返回 `(起始字节, 结束字节)`：区间含首尾标记及其行尾换行。
///
/// 只有成对标记才算块。起标记在而结束标记缺失（用户手工编辑弄坏）时按
/// “块延伸到文件末尾”处理，避免重复安装堆出第二个块。
fn find_block(content: &str) -> Option<(usize, usize)> {
    let mut start: Option<usize> = None;
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        let text = line.trim_end_matches(['\r', '\n']);
        if start.is_none() && text == BLOCK_START {
            start = Some(offset);
        } else if start.is_some() && text == BLOCK_END {
            return Some((start.unwrap_or(0), offset + line.len()));
        }
        offset += line.len();
    }
    start.map(|start| (start, content.len()))
}

/// 读取文件；不存在时返回 `None`（其它错误照常抛出，避免把不可读的
/// `.zshrc` 当成空文件直接覆盖）。
fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// 原子写入：临时文件 + rename（与 `config::HostConfig::save` 同模式）。
///
/// 进程被杀或磁盘满时不会把目标文件截断成半截内容；`mode` 给出最终权限位
/// （`None` 表示交给 umask，用于新建 `.zshrc` 这类普通用户文件）。
fn write_atomic(path: &Path, content: &str, mode: Option<u32>) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mino".into());
    let tmp = path.with_file_name(format!("{name}.tmp-{}", std::process::id()));
    let result = (|| {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            if let Some(mode) = mode {
                // 创建时就带上目标权限，避免“先按 umask 建、再 chmod”之间
                // 的短暂可读窗口。
                options.mode(mode);
            }
            options.open(&tmp)?
        };
        #[cfg(not(unix))]
        let mut file = std::fs::File::create(&tmp)?;
        use std::io::Write;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        // 失败清理临时文件，不留下半截内容。
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// `.zshrc` 的写入目标：是符号链接时解析成真实文件，让链接本身保持有效。
fn write_target(zshrc: &Path) -> PathBuf {
    std::fs::canonicalize(zshrc).unwrap_or_else(|_| zshrc.to_path_buf())
}

/// 目标文件当前的权限位（unix；文件不存在时 `None`，交给 umask）。
#[cfg(unix)]
fn existing_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777)
}

/// 目标文件当前的权限位（非 unix 平台无此概念）。
#[cfg(not(unix))]
fn existing_mode(_path: &Path) -> Option<u32> {
    None
}

/// 备份路径：`<zshrc>.mino.bak`（与 `.zshrc` 同目录）。
fn backup_path(zshrc: &Path) -> PathBuf {
    let mut name = zshrc
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(BACKUP_SUFFIX);
    zshrc.with_file_name(name)
}

/// 创建目录并收敛权限到 0700；已存在时不动（可能是用户自建并有意放宽的目录）。
fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 失败不影响使用（例如文件系统不支持权限位），忽略即可。
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录：每个测试一份，绝不触碰真实 `~/.zshrc` 与 `~/.config/mino`。
    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mino-shell-integration-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");
        dir
    }

    /// 让插件在 vendor 目录里“已装好”（写入口相对路径）。
    fn place_plugin(plugin: Plugin, config_dir: &Path) {
        let entry = plugin
            .target_dir(&vendor_dir(config_dir))
            .join(plugin.entry_rel_path());
        if plugin.entry_is_dir() {
            std::fs::create_dir_all(&entry).unwrap();
        } else {
            std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
            std::fs::write(&entry, "# 占位\n").unwrap();
        }
    }

    #[test]
    fn 安装幂等只留一个受管块() {
        let dir = tmp_dir("idempotent");
        let zshrc = dir.join(".zshrc");
        let original = "export EDITOR=vim\nsource ~/.aliases\n";
        std::fs::write(&zshrc, original).unwrap();
        let config = dir.join("config");

        let first = install(&zshrc, &config).unwrap();
        assert!(first.installed, "首次安装必须写入受管块");
        assert_eq!(
            first.backup.as_deref(),
            Some(backup_path(&zshrc).as_path()),
            "首次安装必须按 <zshrc>.mino.bak 备份"
        );
        assert_eq!(
            std::fs::read_to_string(backup_path(&zshrc)).unwrap(),
            original
        );
        assert_eq!(first.script_path, config.join("shell").join("mino.zsh"));
        assert_eq!(
            std::fs::read_to_string(&first.script_path).unwrap(),
            SCRIPT,
            "集成脚本必须与内嵌内容一致"
        );
        assert_eq!(
            std::fs::read_to_string(&first.starship_path).unwrap(),
            STARSHIP_TOML,
            "starship 预设必须与内嵌内容一致"
        );

        let after_first = std::fs::read_to_string(&zshrc).unwrap();
        assert_eq!(
            after_first.matches(BLOCK_START).count(),
            1,
            "受管块只能有一个：{after_first}"
        );
        assert_eq!(after_first.matches(BLOCK_END).count(), 1);
        assert_eq!(after_first.matches(BLOCK_SOURCE).count(), 1);
        assert!(after_first.starts_with(original), "原内容必须原样保留");
        assert!(is_installed(&zshrc));

        // 第二次安装：内容一致，不写文件（mtime 都不动），也不再产生备份。
        let second = install(&zshrc, &config).unwrap();
        assert!(!second.installed, "重复安装不应再改写 .zshrc");
        assert!(second.backup.is_none(), "非首次安装不应产生备份");
        assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), after_first);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 卸载后与原始内容逐字节一致() {
        // 覆盖有/无行尾换行、空文件、多字节字符四种形态：卸载把安装时补的
        // 分隔换行也一起去掉，字节级还原。
        let originals = [
            "",
            "export EDITOR=vim\n",
            "export EDITOR=vim",
            "\n\n# 注释：中文\n",
            "source ~/.aliases",
        ];
        let dir = tmp_dir("uninstall");
        let config = dir.join("config");

        for (index, original) in originals.iter().enumerate() {
            let zshrc = dir.join(format!(".zshrc-{index}"));
            std::fs::write(&zshrc, original).unwrap();
            assert!(!is_installed(&zshrc));

            install(&zshrc, &config).unwrap();
            assert!(is_installed(&zshrc), "安装后必须能识别出受管块");

            assert!(uninstall(&zshrc).unwrap(), "有块时必须报告删除成功");
            assert_eq!(
                std::fs::read_to_string(&zshrc).unwrap(),
                *original,
                "卸载后必须与安装前逐字节一致：{original:?}"
            );
            assert!(!is_installed(&zshrc));
            // 再卸载一次：没有块，不报告删除、也不改文件。
            assert!(!uninstall(&zshrc).unwrap());
            assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), *original);
        }

        // 文件不存在时卸载是空操作。
        assert!(!uninstall(&dir.join(".zshrc-missing")).unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 没有zshrc时安装会创建并写入受管块() {
        let dir = tmp_dir("create");
        let zshrc = dir.join(".zshrc");
        let config = dir.join("config");
        assert!(!zshrc.exists());

        let outcome = install(&zshrc, &config).unwrap();
        assert!(outcome.installed);
        assert!(outcome.backup.is_none(), "空文件没有可备份的内容");

        let content = std::fs::read_to_string(&zshrc).unwrap();
        assert_eq!(content, block_text(), "新建的 .zshrc 应当只有受管块");
        assert!(content.starts_with(BLOCK_START));
        assert!(content.contains(r#"source "$HOME/.config/mino/shell/mino.zsh""#));
        assert!(content.ends_with(&format!("{BLOCK_END}\n")));
        assert!(is_installed(&zshrc));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 备份只在首次安装产生且不覆盖旧备份() {
        let dir = tmp_dir("backup");
        let zshrc = dir.join(".zshrc");
        let original = "export FOO=1\n";
        std::fs::write(&zshrc, original).unwrap();
        let config = dir.join("config");
        let backup = backup_path(&zshrc);

        assert!(install(&zshrc, &config).unwrap().backup.is_some());
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);

        // 已装状态下再安装：不产生备份、不覆盖旧备份。
        assert!(install(&zshrc, &config).unwrap().backup.is_none());
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);

        // 卸载后再安装：块没了算首次安装，但备份已存在，仍不覆盖。
        assert!(uninstall(&zshrc).unwrap());
        std::fs::write(&backup, "用户自己改过的备份\n").unwrap();
        assert!(install(&zshrc, &config).unwrap().backup.is_none());
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "用户自己改过的备份\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 集成脚本包含关键片段且不含其它终端专有标记() {
        for needle in [
            "MINO_SHELL_INTEGRATION",
            "133;A",
            "133;B",
            "133;C;",
            "133;D;",
            "]7;file://",
            "MINO_SMART_TAB_MODE",
            "MINO_SMART_TAB_DISABLE",
            "suggestion_first",
            "completion_first",
            "expand-or-complete",
            "autosuggest-accept",
            "zsh-autosuggestions",
            "zsh-z",
            "zsh-completions",
            "syntax-highlighting",
            "starship",
            "STARSHIP_CONFIG",
            "add-zsh-hook",
        ] {
            assert!(SCRIPT.contains(needle), "集成脚本缺少关键片段：{needle}");
        }
        // 不得残留其它终端模拟器的专有逻辑（大小写两种都查）。
        let lower = SCRIPT.to_lowercase();
        assert!(!lower.contains("wezterm"), "集成脚本残留 wezterm 专有逻辑");
        assert!(!lower.contains("kaku"), "集成脚本残留专有逻辑：kaku");
        assert!(!lower.contains("setuservar"), "集成脚本残留用户变量上报");
        assert!(!lower.contains("tmux;"), "集成脚本残留 tmux 透传");

        // starship 预设按原样打包。
        assert!(STARSHIP_TOML.contains("[git_status]"));
        assert!(STARSHIP_TOML.contains("add_newline = false"));
    }

    #[test]
    fn vendor目录与缺失插件检测() {
        let dir = tmp_dir("vendor");
        let config = dir.join("config");
        assert_eq!(vendor_dir(&config), config.join("shell").join("vendor"));

        let mut missing = missing_plugins(&config);
        missing.sort_by_key(|plugin| plugin.dir_name());
        let mut expected = Plugin::ALL.to_vec();
        expected.sort_by_key(|plugin| plugin.dir_name());
        assert_eq!(missing, expected, "全新配置目录下四个插件都算缺失");
        for plugin in Plugin::ALL {
            assert!(!plugin.repo_url().is_empty());
            assert!(!plugin.dir_name().is_empty());
        }

        // 入口齐了就不算缺失；只有目录、没有入口的半成品仍算缺失。
        for plugin in Plugin::ALL {
            place_plugin(plugin, &config);
        }
        assert!(missing_plugins(&config).is_empty());
        let zsh_z_dir = Plugin::ZshZ.target_dir(&vendor_dir(&config));
        std::fs::remove_file(zsh_z_dir.join(Plugin::ZshZ.entry_rel_path())).unwrap();
        // 留着其它文件模拟“克隆到一半”的目录：这种非空目录不能自动清掉。
        std::fs::write(zsh_z_dir.join("partial-clone"), "半成品\n").unwrap();
        assert_eq!(missing_plugins(&config), vec![Plugin::ZshZ]);

        // 已装好的插件在 bootstrap 里跳过并计为成功（不触网）；缺入口的半成品
        // 目录返回错误，不硬塞进 git clone。
        let results = bootstrap_vendor_with(&config, "mino-no-such-git-binary");
        assert_eq!(results.len(), Plugin::ALL.len());
        for (plugin, result) in Plugin::ALL.into_iter().zip(&results) {
            match result {
                Ok(done) => assert_eq!(*done, plugin, "已装好的插件应跳过并计为成功"),
                Err(message) => {
                    assert_eq!(plugin, Plugin::ZshZ, "只有缺入口的半成品该失败");
                    assert!(message.contains("内容不完整"), "{message}");
                }
            }
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 插件克隆失败不panic() {
        // 用不存在的 git 可执行文件覆盖失败路径：不触网、不 panic，
        // 每个插件各自返回错误。
        let dir = tmp_dir("clone-fail");
        let config = dir.join("config");
        let results = bootstrap_vendor_with(&config, "mino-no-such-git-binary");
        assert_eq!(results.len(), Plugin::ALL.len());
        for (result, plugin) in results.iter().zip(Plugin::ALL) {
            match result {
                Err(message) => assert!(
                    message.contains(plugin.dir_name()),
                    "错误信息要指出是哪个插件：{message}"
                ),
                Ok(value) => panic!("不存在的 git 不可能克隆成功：{value:?}"),
            }
        }
        // vendor 目录会被创建出来（供下次重试），但没有半截插件目录。
        let vendor = vendor_dir(&config);
        assert!(vendor.is_dir());
        assert!(std::fs::read_dir(&vendor).unwrap().next().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 上次克隆失败留下的空目录会被清理后重试() {
        // `git clone` 半路失败时会留下空目录（实测）：直接报“已存在”会让网络
        // 抖动一次就永远装不上，所以空目录要清掉重试，非空的才交给用户。
        let dir = tmp_dir("clone-retry");
        let config = dir.join("config");
        let vendor = vendor_dir(&config);
        let leftover = Plugin::ZshZ.target_dir(&vendor);
        std::fs::create_dir_all(&leftover).unwrap();

        let results = bootstrap_vendor_with(&config, "mino-no-such-git-binary");
        match &results[Plugin::ALL.iter().position(|p| *p == Plugin::ZshZ).unwrap()] {
            Err(message) => assert!(
                message.contains("无法执行"),
                "空目录应当被清理后真正去克隆，而不是报“已存在”：{message}"
            ),
            Ok(value) => panic!("不存在的 git 不可能克隆成功：{value:?}"),
        }
        assert!(!leftover.exists(), "空目录应当已被清理");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 安装保留原zshrc权限位且写入目标与源文件一致() {
        // .zshrc 通常是 0644：原子替换后不能被改成 0600（否则其它工具读不到）。
        let dir = tmp_dir("mode");
        let zshrc = dir.join(".zshrc");
        std::fs::write(&zshrc, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // 显式给 0644：不依赖运行测试的 umask。
            std::fs::set_permissions(&zshrc, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let config = dir.join("config");
        install(&zshrc, &config).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&zshrc).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644, "不能改变 .zshrc 的权限位");

            // 集成脚本按 0600 落盘（配置目录里没有给其他人读的内容）。
            let script_mode = std::fs::metadata(config.join("shell").join("mino.zsh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(script_mode, 0o600);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn 符号链接的zshrc按链接目标写入() {
        // dotfiles 用户常把 ~/.zshrc 链接到仓库：安装不能把链接替换成普通文件。
        let dir = tmp_dir("symlink");
        let real = dir.join("dotfiles-zshrc");
        std::fs::write(&real, "export FOO=1\n").unwrap();
        let link = dir.join(".zshrc");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let config = dir.join("config");

        install(&link, &config).unwrap();
        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "链接必须保留"
        );
        assert!(is_installed(&link));
        assert!(std::fs::read_to_string(&real)
            .unwrap()
            .contains(BLOCK_SOURCE));

        assert!(uninstall(&link).unwrap());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "export FOO=1\n");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());

        std::fs::remove_dir_all(&dir).ok();
    }
}
