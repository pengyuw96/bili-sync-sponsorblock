# bili-sync-sponsorblock

基于 [NeeYoonc/bili-sync-up](https://github.com/NeeYoonc/bili-sync-up)（上游 [amtoaer/bili-sync](https://github.com/amtoaer/bili-sync)）的社区 fork，主要加入 [BilibiliSponsorBlock](https://github.com/hanydd/BilibiliSponsorBlock) 广告片段剪辑，并针对 Emby / Jellyfin 媒体库做了适配。

- 当前版本：**v3.1.4.6**（Cargo 包版本号为 `3.1.4`，Cargo 不支持四段版本号）
- 上游代码基线：bili-sync-up **v3.1.3**（`f5a91a97`）
- 非官方版本，问题请勿反馈到上游。

## 一、SponsorBlock 剪辑（核心功能）

视频下载合并完成后，如果开启了 `sponsor_block.enabled`，会按下面的流程处理：

1. **获取片段**：用 BV 号和 cid 请求 BilibiliSponsorBlock API `/api/skipSegments`，默认服务器是 `bsbsb.top`，失败时用镜像 `bsbsb.xyz`。只处理 `actionType = skip` 且类别在配置里的片段。
2. **计算保留区间**：合并重叠的删除区间，取反得到要保留的区间；太短的片段（`min_segment_seconds`）和太短的保留间隙（`min_keep_gap_seconds`）会被忽略或并入删除区间。
3. **ffmpeg 剪辑**，有两种模式：
   - `cut_mode = keyframe`（默认）：流复制，不重新编码，速度快、画质无损。每段保留区间的起点会**吸附到它之前最近的关键帧**，这与 ffmpeg 在 `-c copy` 下的实际行为一致。
   - `cut_mode = accurate`：用 libx264/aac 重新编码，切点精确到帧，但很耗 CPU。失败时会回退到 keyframe 模式。
   - 剪完后原子替换原文件，**不再保留未剪辑的原片**（旧版本的 `*.sb-orig` / `*.sponsor-original.*` 不再生成）。
4. **字幕和弹幕同步对齐**：字幕（srt/ass）和弹幕（ass）使用和视频**同一组吸附后的删除区间**来平移时间轴，所以音画、字幕、弹幕都在同一条时间线上。每个文件只平移一次。剪辑计划记录在隐藏文件 `.{文件名}.sponsorblock.json` 里，之后刷新弹幕或重写字幕时不会被重复平移。
5. **时长修正**：剪辑后用 ffprobe 测量实际时长，并据此更新章节结束时间、同名 NFO 的 `<runtime>` 和数据库里的分 P 时长。
6. **章节标记（可选）**：`mark_categories` 里的类别不剪辑，而是写成 MP4/MKV 章节（使用中文标题），同时在字幕和弹幕中插入 `〔SB〕` 提示，方便在 Emby/Jellyfin 里手动跳到下一章节。同一类别如果同时出现在剪辑和标记里，以剪辑为准。
7. **容错**：默认 `fail_open`，API 或 ffmpeg 出错时保留原下载文件，只记日志。视频源开启了「按章节拆分」时会跳过 SponsorBlock 处理。

**为什么剪完后可能还残留约 1 秒？** keyframe 模式不重新编码，流复制只能从关键帧开始切。B 站视频的关键帧间隔（GOP）一般在 1～数秒，剪辑起点会被提前到上一个关键帧，所以广告结束处可能多留不到一个 GOP 的画面。如果介意，可以改用 `accurate` 模式。字幕和弹幕按吸附后的实际切点对齐，不会因此错位。

**界面与接口：**

- Web 设置页可以开关 SponsorBlock、选择类别和模式，并检测服务器状态（心跳接口 `GET /api/sponsorblock/health`，默认每 300 秒检测一次）。
- 分 P 状态新增第 6 位「视频剪切」，详情页会显示剪辑结果（`cut` / `none` / `marked` / `failed_open` 等）。数据库迁移会让已有媒体库只补跑这一步。
- 目前只处理 B 站视频，YouTube / 抖音等外部来源不受影响。

## 二、Emby / Jellyfin 适配

- **单 P 视频统一文件名**：视频、`-thumb`、`-fanart`、NFO、字幕、弹幕都用文件夹名作为文件名（和 3Blue1Brown 这类目录的结构一致），这样 Emby 能正确显示封面。同时修复了文件夹图片文件名为空的问题。
- **下载完成后通知扫库**（参考 MoviePilot / Sonarr 的实现）：
  - 一整轮下载（包括 SponsorBlock 剪辑）全部结束后，再等待 `delay_seconds`（默认 300 秒），然后**只通知一次**。如果等待期间又开始了新一轮，会继续顺延。
  - 优先定向扫描：`POST /Library/Media/Updated`；失败时可回退到全库扫描 `POST /Library/Refresh`（`fallback_full_refresh`）。
  - 支持路径映射，按最长前缀把容器内路径（如 `/bili-sync/...`）转换成媒体服务器看到的路径。套件版 Emby 填 NAS 上的真实路径即可。
  - API Key 只放在 `X-Emby-Token` 请求头里，不写入日志，接口也只返回「是否已设置」。

## 三、其他改动

- **并发**：视频处理并发数默认 1，限制在 1～2 之间，避免 NAS 上 ffmpeg 抢占资源。
- **开箱即用的单一挂载**：媒体目录只需要挂载到容器内的 `/bili-sync`。
- **仓库精简**：删除了上游的 Docker、compose、CI、文档站和截图资源，只保留完整源码，需要自己编译。不发布 Release。

## 编译

```sh
cd web && npm ci && npm run build && cd ..
cargo build --release --target x86_64-unknown-linux-musl -p bili_sync
tar czf bili-sync-rs-Linux-x86_64-musl.tar.gz -C target/x86_64-unknown-linux-musl/release bili-sync-rs
```

## 部署（Docker / 威联通）

1. 把上面打包好的 tarball 放到 `Dockerfile.qnap` 同一目录，把 `Dockerfile.qnap` 重命名为 `Dockerfile` 后构建镜像。镜像基于 Alpine 3.22，自带 ffmpeg。
2. 参考 `deploy/docker-compose.nas.yml` 运行容器：
   - 媒体目录 → `/bili-sync`（Web 界面里的路径都写 `/bili-sync/...`）
   - 配置目录 → `/app/.config/bili-sync`
   - 端口 `12345`，建议用媒体用户运行（`user: uid:gid`）
3. `deploy/` 里的辅助脚本默认只做演练（dry-run），加参数才会真正执行：
   - `cleanup-sb-orig.sh <目录> [--delete]`：清理旧版本留下的未剪辑原片
   - `migrate-episode-names.sh <宿主目录> <宿主根> <容器根> [--apply]`：把旧的 `S01E27 - 标题.*` 单 P 文件改为统一文件名，并生成更新数据库路径的 SQL

## 配置示例

```json
"sponsor_block": {
  "enabled": true,
  "server_address": "https://www.bsbsb.top",
  "mirror_server_addresses": ["https://www.bsbsb.xyz"],
  "categories": ["sponsor", "padding"],
  "mark_categories": [],
  "cut_mode": "keyframe",
  "fail_open": true
}
```

更详细的实现说明见 `NOTES.md`，变更历史见 `docs/changelog.md`。

## 致谢

- [amtoaer/bili-sync](https://github.com/amtoaer/bili-sync)：原始项目
- [NeeYoonc/bili-sync-up](https://github.com/NeeYoonc/bili-sync-up)：本 fork 的直接上游
- [hanydd/BilibiliSponsorBlock](https://github.com/hanydd/BilibiliSponsorBlock)：片段数据与 API
- [MoviePilot](https://github.com/jxxghp/MoviePilot)、[Sonarr](https://github.com/Sonarr/Sonarr)：媒体服务器通知的参考实现

许可证见 `License`（MIT）。
