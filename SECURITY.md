# 安全政策

## 支持范围

当前维护版本：

| 版本 | 支持 |
|---|---|
| 最新发布版（`main` 分支，当前 `1.1.x`） | ✅ 接受安全报告 |
| 更早版本 / 未发布的实验分支 | ❌ 仅尽力处理 |

## 报告漏洞

请**不要**在公开 Issue 中披露可利用的安全漏洞。

优先使用 GitHub 私有漏洞报告：

1. 打开仓库 **Security → Advisories → Report a vulnerability**
2. 或访问：https://github.com/yqstart/mino/security/advisories/new

若无法使用上述渠道，可通过仓库维护者 GitHub 主页私信，主题注明「Mino Security」。

报告请尽量包含：

- 影响版本 / 提交
- 复现步骤或 PoC（概念验证）
- 预期影响（本地文件越权、命令注入、凭据泄露等）
- 是否已有公开讨论或利用

## 处理承诺

- 收到报告后 7 天内确认并回复
- 确认漏洞后优先修复受影响版本，并视严重程度发布安全更新
- 修复发布前不公开漏洞细节；修复后按需披露并致谢报告者

## 安全相关现状

- 主机密码明文存储于 `~/.config/mino/hosts.toml`（规划接入系统钥匙串，见 [CHANGELOG](CHANGELOG.md)）
- SSH 主机密钥采用 TOFU（首次连接信任）：指纹保存于 `~/.config/mino/known_hosts.toml`，后续连接必须匹配；密钥变化会拒绝连接并提示处理方式
- `hosts.toml` 与 `known_hosts.toml` 均以 0600 权限保存；known_hosts 读取、解析或保存失败时会拒绝连接，不会降级为重新信任
- 以上为已知限制；首次连接前请通过可信渠道核对服务器指纹，密码明文存储仍计划后续接入系统钥匙串
