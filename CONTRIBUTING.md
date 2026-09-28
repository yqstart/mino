# 贡献指南

感谢关注 Mino。欢迎 Issue、讨论与 Pull Request。

## 开发环境

| 工具 | 建议版本 |
|---|---|
| Rust | stable（推荐 1.80+） |
| 系统 | macOS 12+（GUI 与发布产物）；CI 在 Ubuntu 上构建与测试 |

## 本地运行

```bash
git clone https://github.com/yqstart/mino.git
cd mino
cargo run
```

集成测试（SSH / SFTP 真实链路）需要本地测试 sshd：

```bash
./scripts/test-sshd.sh start     # 启动测试 sshd（127.0.0.1:2222，公钥认证，含 sftp subsystem）
cargo test --workspace -- --test-threads=1
./scripts/test-sshd.sh stop      # 用完关闭
```

`--test-threads=1` 是必需的：UI 测试（egui_kittest）串行运行可避免争抢主题静态与图形后端的假失败。

## 贡献方向

Mino 核心功能集已收敛，主打**轻量、快速、简洁**。

- **欢迎**：Bug 修复、性能与流畅度优化、交互体验打磨、视觉细节完善、测试补充
- **慎入**：大功能模块新增（请先开 Issue 讨论）
- **不做**：AI 集成、插件生态、重量级功能堆叠

优化类 PR 请说明可感知的效果或可量化指标（启动耗时 / 内存 / 帧率 / 操作延迟），「修一个验一个」。

## 分支与提交

- 主分支：`main`
- 提交信息：`type(scope): English past-tense summary`，正文用中文要点
  - `type` 取 `feat` / `fix` / `refactor` / `docs` / `style` / `test` / `chore`
  - summary 首词用英文过去式动词（`Added` / `Fixed` / `Restyled` / `Removed` 等）
  - 正文中文，每行以 ASCII 句点（`.`）收尾
- 一个 PR 聚焦一个问题；提交尽量原子化
- 功能改动必须附带测试（单元 / 集成 / UI 渲染，视场景）

## PR 流程

1. Fork 并创建分支：`fix/xxx` 或 `feat/xxx`
2. 本地通过全部检查：

   ```bash
   cargo build --workspace
   cargo test --workspace -- --test-threads=1
   cargo clippy --workspace --all-targets -- -D warnings
   cargo fmt --all --check
   ```

3. 提交并推送，创建 PR 说明改动与验证结果

## 代码风格

- 注释、文档、提交信息使用中文；标识符（变量 / 函数 / 文件）使用英文
- 遵循既有模块划分：`mino-core` 为纯引擎（禁止依赖 egui），`mino-app` 为 UI 层
- 架构约定见 [AGENTS.md](AGENTS.md) 与 [docs/技术架构.md](docs/技术架构.md)
