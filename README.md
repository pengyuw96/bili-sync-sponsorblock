# bili-sync-sponsorblock

<p align="center">
  <strong>基于 <a href="https://github.com/NeeYoonc/bili-sync-up">NeeYoonc/bili-sync-up</a> v3.1.3 的社区 Fork</strong><br/>
  集成 <a href="https://github.com/hanydd/BilibiliSponsorBlock">BilibiliSponsorBlock</a> 分段：可<strong>裁剪</strong>或写入<strong>章节标记</strong>（Emby / Jellyfin / Plex 时间轴手动跳过）
</p>

<p align="center">
  <a href="https://github.com/pengyuw96/bili-sync-sponsorblock/releases"><img src="https://img.shields.io/github/v/release/pengyuw96/bili-sync-sponsorblock?style=flat-square&label=release" alt="Release"/></a>
  <a href="https://github.com/NeeYoonc/bili-sync-up"><img src="https://img.shields.io/badge/forked%20from-NeeYoonc%2Fbili--sync--up-blue?style=flat-square" alt="Forked from"/></a>
  <a href="https://github.com/NeeYoonc/bili-sync-up/releases/tag/v3.1.3"><img src="https://img.shields.io/badge/upstream-v3.1.3-informational?style=flat-square" alt="Upstream"/></a>
  <a href="https://github.com/hanydd/BilibiliSponsorBlock"><img src="https://img.shields.io/badge/segments-BilibiliSponsorBlock-orange?style=flat-square" alt="SponsorBlock"/></a>
  <img src="https://img.shields.io/badge/local%20version-3.1.4.1-success?style=flat-square" alt="Local version"/>
</p>

> [!IMPORTANT]
> **这不是上游官方版本。** 本仓库是社区维护的 Fork，在原项目之上增加了 B 站 SponsorBlock 相关能力。日常同步、Web 管理等功能请以上游文档为准。

---

## 来源与致谢

| 角色 | 仓库 / 文档 | 说明 |
|------|-------------|------|
| **Fork 自** | [NeeYoonc/bili-sync-up](https://github.com/NeeYoonc/bili-sync-up) | 基线版本 **v3.1.3**（commit `f5a91a97`，2026-10-03） |
| **分段数据** | [hanydd/BilibiliSponsorBlock](https://github.com/hanydd/BilibiliSponsorBlock) | 社区标注的跳过片段 |
| **API 说明** | [BilibiliSponsorBlock Wiki · API](https://github.com/hanydd/BilibiliSponsorBlock/wiki/API) | `skipSegments` 等接口语义 |
| **上游文档** | [NeeYoonc.github.io/bili-sync-up](https://NeeYoonc.github.io/bili-sync-up/) | 安装、配置、迁移等通用说明 |

感谢原作者与 BilibiliSponsorBlock 社区的工作。

---

## 本 Fork 新增功能

相对上游 **v3.1.3**，本仓库额外加入：

### 1. SponsorBlock 裁剪（Cut）

- 下载合并完成后，按所选类别从文件中**剪切**赞助/垫片等片段（ffmpeg stream-copy）
- 默认 **fail-open**：API / ffmpeg 失败时保留原片，不中断任务
- 可与「保留未裁剪原片」配合使用
- **仅 B 站分 P**；YouTube / 抖音 / TikTok 路径未接入裁剪

### 2. 章节标记（Mark）

- 将所选类别写入 **Matroska / MP4 章节**（中文标题，如「赞助广告」「片头」）
- Emby / Jellyfin / Plex 等可在时间轴上**手动点章节跳过**（不是浏览器插件那种自动 seek）
- 若同时裁剪，标记时间戳会按已删除区间自动对齐

### 3. Web 设置与验收

- 设置页「SponsorBlock 裁剪」：启用开关、**裁剪类别** / **章节标记类别** 两套勾选（同一类别不能又裁又标，裁优先）
- 服务器心跳与「检测」按钮；接口 `GET /api/sponsorblock/health`
- 分 P 验收栏第 6 项「视频剪切」；结果码见 [`NOTES.md`](./NOTES.md)

### 4. 与「按章节切分」互斥

若视频源开启了 `split_chapters_after_download`，本 Fork 的裁剪与标记都会跳过（`skipped_chapters`），避免互相冲突。

---

## 模式怎么选

| 你想要的效果 | 建议 |
|--------------|------|
| 文件里广告直接消失 | 只勾 **裁剪类别** |
| 原片进 Emby，时间轴手动跳 | 只勾 **章节标记类别** |
| 部分砍掉、其余可点跳 | 两类勾选不同类别（例如裁 `sponsor`，标 `intro` / `outro`） |
| 像浏览器插件一样全自动跳 | 媒体库大多做不到；继续用**裁剪**，或 Plex 等第三方工具 |

详细配置示例与结果码：[`NOTES.md`](./NOTES.md)

---

## 快速开始

上游通用安装方式仍适用，见 [上游文档](https://NeeYoonc.github.io/bili-sync-up/) 与原版 Docker 说明。使用本 Fork 时请构建 / 导入本仓库对应镜像（例如本地标签 `bili-sync-sponsorblock:3.1.4.1`），不要与官方镜像混淆。

```bash
git clone https://github.com/pengyuw96/bili-sync-sponsorblock.git
cd bili-sync-sponsorblock
# 前端嵌入构建后编译后端，参见上游开发流程与本仓库 Dockerfile.qnap / NOTES.md
```

首次启用：Web → 设置 → **SponsorBlock 裁剪** → 勾选启用与类别 → 保存 → 再下载一部带社区标注的 B 站视频验证。

---

## 版本

| 项目 | 值 |
|------|-----|
| 本 Fork 版本字符串 | 界面 `v3.1.4.1`；Cargo 包版本 `3.1.4`（Cargo 不接受四段版本号） |
| 上游基线 | [v3.1.3](https://github.com/NeeYoonc/bili-sync-up/releases/tag/v3.1.3)（`f5a91a97`）。上游最新发布号是 [v3.1.4.1](https://github.com/NeeYoonc/bili-sync-up/releases/tag/v3.1.4.1)，本仓库尚未并入该版本的漫画源等改动。 |
| 已发布 Release | [v3.1.2-sponsorblock-mark](https://github.com/pengyuw96/bili-sync-sponsorblock/releases/tag/v3.1.2-sponsorblock-mark) |

---

## 许可证

遵循上游项目的 **MIT** 许可证。对本 Fork 新增代码亦按 MIT 使用；分段数据版权与规则以 BilibiliSponsorBlock 社区为准。

---

## 上游原有能力（摘要）

本 Fork **继承** bili-sync-up 的 NAS 向同步能力，例如：收藏夹 / 投稿 / 稍后再看 / 番剧、Web 管理、配置热重载、任务队列等。完整说明请阅读上游仓库，此处不重复展开。

<p align="center">
  <sub>Forked from <a href="https://github.com/NeeYoonc/bili-sync-up">NeeYoonc/bili-sync-up</a> · Segments by <a href="https://github.com/hanydd/BilibiliSponsorBlock">BilibiliSponsorBlock</a></sub>
</p>
