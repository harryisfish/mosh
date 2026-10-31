# GitHub CI/CD 流程

本仓库把“合并”和“发布”分成两个不可绕过的阶段：每个 PR 必须通过 CI，只有合并到 `master` 后打出的 `rust-v<版本>` tag 才能生成正式 Release。构建产物只在一次构建中生成，后续步骤下载同一批 artifact，避免“验证的文件”和“发布的文件”不一致。

## 流程

```text
feature 分支
    │ PR
    ▼
CI（fmt / clippy / test / examples / PTY+网络验证 / 依赖审查）
    │ required checks
    ▼
master（合并后再次 CI）
    │ git tag rust-vX.Y.Z
    ▼
Rust release：六个平台构建 → 包校验 → 上传 artifact
    │ production environment 审批
    ▼
GitHub Release（tar.gz、deb、rpm、SHA256SUMS）
    │
    ▼
Homebrew formula PR（手工更新并验证）
```

## 分支和合并规则

- `master` 是唯一集成分支；功能、修复和版本准备均从 `master` 建分支。
- `master` 开启分支保护：禁止直接 push，要求 PR、至少一名 reviewer、`CI / quality` 和 `CI / dependency-review` 通过，并要求分支已更新。
- 发布 tag 必须指向已经合并且 CI 通过的 `master` 提交。tag 名必须与 `rust/Cargo.toml` 版本一致，例如版本 `0.1.3` 使用 `rust-v0.1.3`。
- 版本 PR 同时更新 `rust/Cargo.toml`、`Cargo.lock`、`docs/releases/rust-v<VERSION>.md`、README 安装示例和相关文档。

## GitHub 设置

在 **Settings → Environments** 创建 `production` 环境：

1. 配置至少一名 required reviewer；
2. 不配置长期云端密钥，发布使用 GitHub Actions 的短期 `GITHUB_TOKEN`；
3. 在 **Settings → Branches** 将上面的两个 CI 检查设为 required status checks；
4. 在 **Settings → Actions → General** 允许 actions 读取仓库内容，第三方 action 只允许组织审核过的来源。

`rust-release.yml` 的 `publish` job 绑定 `production`，因此构建和验证会先完成，审批只发生在写入 GitHub Release 的最后一步。拒绝或取消审批不会发布任何半成品。

## 各阶段职责

### PR 阶段

`.github/workflows/rust.yml` 在 Ubuntu 和 macOS 上运行格式检查、Clippy、单元测试、examples、真实 PTY/网络 CLI 验证；依赖变更还由 dependency review 阻止 high 及以上漏洞。

### 合并后阶段

合并到 `master` 会重新运行同一套 CI，确保 PR 合并队列没有改变验证结果。`.github/workflows/security.yml` 每周和依赖相关变更运行 `cargo audit --locked`。

### 发布阶段

推送 `rust-v*` tag 后，release workflow 为 GNU、musl、macOS Intel 和 Apple Silicon 构建六个目标。musl 目标额外生成 deb/rpm，并在 Alpine、Debian、Ubuntu、Fedora、openSUSE 容器中安装验证。所有 job 上传 artifact；`publish` 下载并合并这些 artifact，生成并校验 `SHA256SUMS` 后创建 Release。

### 回滚

发布错误时先在 GitHub Release 页面撤下错误资产并标记为 draft，不删除已被用户下载的 tag。修复后提升补丁版本（例如 `0.1.3` → `0.1.4`），重新走版本 PR 和新 tag；客户端和服务端始终使用同一版本。若需停止自动发布，取消 `production` 审批或禁用 workflow，不要删除历史 Release。

## 日常操作

```sh
# 发布 0.1.3（必须是已合并的 master 提交）
git switch master && git pull --ff-only
git tag rust-v0.1.3
git push origin rust-v0.1.3
```

发布完成后，按 `docs/rust-release.md` 下载 Release 的 `SHA256SUMS`，运行 `scripts/update-homebrew-formula.py`，在独立分支提交 Homebrew formula PR。不要使用 PR 构建产生的 checksum。
