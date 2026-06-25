# VVN（Vibe Visual Novel）

VVN 是一个桌面应用，给 **NovaRR**（`Nova: Remnant Rebuild`，原 nova2，Godot 4 + C# 视觉小说引擎）配套使用：用自然语言描述想要的剧情/演出效果，自动写回 NovaScript 剧本，并实时在跑着的 NovaRR 游戏窗口里看到效果——不需要每次手改完都重新导出游戏。

- VVN 仓库：https://github.com/Jackson-Wang-dev/Vibe-Visual-Novel
- NovaRR 仓库：https://github.com/Jackson-Wang-dev/NOVA-Remnant-Rebuilt

VVN **不能脱离 NovaRR 单独使用**——它本身不渲染任何游戏画面，只是一个远程控制 + AI 写脚本的外壳。

## 功能特性

- **自然语言生成/改写剧本**：调 DeepSeek 把需求转成 NovaScript，自动校验生成结果里引用的素材路径/音轨/音效/shader 是否真实存在（避免 AI 编出不存在的资源），校验或引擎加载失败会自动把报错喂回去重试（最多 3 次）。
- **实时联动预览**：改完剧本一键热重载（`reload`），不会丢失当前播放位置；也可以 `seek` 跳到任意之前到过的剧情节点重新预览，反复调同一段效果不用每次从头点。
- **版本历史**：每次保存/AI 写回前自动存一份快照，可按修改摘要搜索、随时回退。
- **资源描述**：给立绘/背景图调智谱 GLM 视觉模型生成一句文字描述，写回项目内的 JSON 索引，供后续 AI 生成理解素材内容。
- **纯本地运行**：前端 + 后端 + 跟 Godot 的通信都在本机完成，不需要额外起一个服务器。

## 工作原理

VVN 是一个 Tauri 桌面应用（Rust 后端 + React 前端）。点「进入项目」后，后端会用你填的 Godot 路径把 NovaRR 工程直接跑起来（`godot --path <NovaRR 目录>`，是一个真实的游戏窗口，不是导出包、不是 headless），然后通过本机 TCP（默认 `127.0.0.1:9999`，NovaRR 工程里的 `PreviewBridge`，**只在 Debug 构建里编译**）发送换行分隔的 JSON 指令：

- `reload` — 重新从磁盘解析剧本文件，并自动跳回之前推进到的位置附近（不会跳回标题重开）
- `seek` — 跳到之前到过的某个具体 `(nodeRecordId, dialogueIndex)` 位置重新预览
- `get_state` — 查询当前播放位置；NovaRR 这边玩家用鼠标点击推进剧情时也会主动把最新位置推送给 VVN

VVN 窗口只负责改剧本、发指令、显示状态；游戏画面始终在那个独立的 Godot 窗口里。

## 技术栈

- 前端：React 19 + TypeScript + Vite
- 桌面壳：Tauri 2（Rust）
- 大模型：DeepSeek（`deepseek-v4-flash` / `deepseek-v4-pro`，剧本生成）、智谱 `glm-4.6v-flash`（图片转文字描述）
- 与 NovaRR 的通信：自定义的换行分隔 JSON over TCP（`PreviewBridge`，仅 Debug 构建编译进引擎）

## 快速开始

### 环境要求

1. **Godot 4.6.3（.NET / Mono 版）** + 匹配的 .NET SDK —— 跑 NovaRR。
2. **Node.js 18+** —— 跑 VVN 前端。
3. **Rust**（用 [rustup](https://rustup.rs/) 装最新稳定版）+ Tauri 的系统依赖 —— 跑 VVN 后端。
   - Windows：需要 WebView2 Runtime（Win10/11 通常已自带）、Visual Studio C++ Build Tools（勾选"使用 C++ 的桌面开发"工作负载）。
4. **（可选，只有用到"AI 生成剧本"/"资源描述"两个功能才需要）**
   - 智谱 AI（[bigmodel.cn](https://open.bigmodel.cn)）API Key。
   - DeepSeek（[deepseek.com](https://www.deepseek.com)）API Key。

没有这两个 Key 也完全可以用 VVN：手改剧本 + 热重载 + Seek 预览这条核心联调链路不依赖任何 API Key。

### 拉取两个仓库

```bash
git clone https://github.com/Jackson-Wang-dev/NOVA-Remnant-Rebuilt.git NovaRR
git clone https://github.com/Jackson-Wang-dev/Vibe-Visual-Novel.git vvn
```

建议放在同一个上级文件夹下，方便后面在 VVN 设置里填路径。

### 先确认 NovaRR 自己能跑起来

这一步独立于 VVN，目的是排除"VVN 连不上是因为引擎本身有问题"这种情况。

1. 用 Godot 4.6.3 mono 版打开 `NovaRR/project.godot`（第一次打开会自动构建 C# 项目，构建失败先确认 .NET SDK 版本匹配）。
2. 按 F5 跑一下，确认能进标题界面、能进章节、能存读档。
3. （可选）跑一下自带的测试：`godot --headless --path . --run-tests --quit-on-finish`，应该全部 PASS（退出码 0）。

记下两个路径，后面填进 VVN 设置：
- Godot 可执行文件的**完整路径**。
- NovaRR 工程根目录的**完整路径**（`project.godot` 所在的那一层）。

### 构建并启动 VVN

```bash
cd vvn
npm install
npm run tauri dev
```

第一次跑会编译 Rust 后端，比较慢，之后有增量缓存。打长期使用的安装包/独立可执行文件：

```bash
npm run tauri build
```

（不要用 `npm run dev`——那只起前端，没有 Tauri 后端，调不动 Godot/PreviewBridge。）

## 配置

打开 VVN，点右上角「设置」，填：

| 字段 | 说明 |
| --- | --- |
| NovaRR 工程目录 | 上面记下的路径 |
| Godot 可执行文件路径 | 上面记下的路径 |
| PreviewBridge 端口 | 默认 `9999`，跟别的程序冲突再改 |
| 智谱 API Key | 可留空，留空则"生成资源描述"功能不可用 |
| DeepSeek API Key | 可留空，留空则"AI 生成剧本"功能不可用 |
| VFX 备注文件路径 | 可留空，是给 AI 生成提示用的可选补充材料 |

点「保存设置」。

### 关于共享 API Key

VVN 目前是纯本地应用，调用 DeepSeek/智谱接口时没有经过任何服务器代理转发，所以填在设置里的 Key 是以纯文本形式存在你本机的配置文件里——这意味着任何拿到这个 Key 的人都能直接用它。如果你是从作者本人那里拿到一个共享的 DeepSeek Key 用来试用：截至 2026-06-24 大概还有 9 元额度，谁都可能在用，请当一次性的体验额度对待，用尽随时会失效；正式/长期使用请在设置面板里换成你自己申请的 Key。

## 完整使用流程

点「进入项目」——后端会自动拉起 Godot（弹出游戏窗口）并通过 PreviewBridge 握手，状态面板会显示 `Godot Running` / `Bridge Connected`。这时你会同时看到两个窗口：VVN 主界面 + 一个正在运行的 NovaRR 游戏窗口。

- **正常游玩定位**：在 Godot 游戏窗口里像玩游戏一样点击推进剧情，VVN 状态面板会实时跟着显示 `currentNodeRecordId` / `currentDialogueIndex`（PreviewBridge 主动推送，不用手动查）。
- **手改剧本**：展开「开发者模块」→ 在 Scenario 文件树里选一个 `.txt`，在编辑区改完，点「保存并刷新」——会把改动写回磁盘并触发引擎 `reload`，Godot 窗口会自动重新解析剧本并跳回你刚才推进到的位置附近继续（不会跳回标题重开）。
- **AI 生成/改写**：主工作台直接用自然语言描述想要的效果（例如"把背景换成夜晚教室，加点雨声"），选好 `target_file`，点「开始生成」。后端会调 DeepSeek 生成 NovaScript，自动校验里面引用的素材路径/音轨/shader 是否真实存在，校验通过后写回文件并触发 `reload`；如果引擎 `reload` 报错（语法错、资源缺失等），会把报错信息喂回模型自动重试，最多 3 次。3 次都失败的话会把最后一次生成的脚本原样显示出来，方便你切到开发者模块手动修。
- **Seek 控件**：手填 `nodeRecordId` / `dialogueIndex` 跳到之前到过的某个具体位置重新预览。
- **版本历史**：每次「保存并刷新」或 AI 写回成功前，都会先在 NovaRR 工程目录下的 `vvn_data/history/` 里存一份快照（纯文件备份，跟 git 提交无关），可以在编辑区右上角「版本历史」里按时间/修改摘要找回退。
- **资源描述**：资源树里每张图旁边的「生成描述」按钮会调智谱 GLM 视觉模型给这张立绘/背景图生成一句文字描述，写回 `vvn_data/asset_descriptions.json`。

### 退出 / 切换项目

设置面板里的「退出当前项目」会关掉 VVN 自己拉起的那个 Godot 进程（配置不会丢，下次「进入项目」会照原配置重新拉起）。要换一个 NovaRR 工程，先退出当前项目，在设置里改路径，再重新进入项目。

## 项目结构（给开发者/贡献者）

```
src/
  App.tsx                     主界面：唯一的顶层组件，管理本地 UI 状态、调用后端命令
  bridge/PreviewBridgeClient.ts  包装 Tauri invoke() 调用与事件监听（state_changed / generation_summary_ready）
  bridge/protocol.ts           前后端共享的 TypeScript 类型定义，对应 Rust 侧的 struct

src-tauri/src/
  lib.rs                  Tauri 命令入口；AppRuntime（配置/Godot 进程/PreviewBridge 连接的状态管理）；AI 生成重试主循环
  llm.rs                  DeepSeek / 智谱 HTTP 调用封装
  generation.rs           Prompt 构造、生成结果解析、素材/音轨/shader 引用校验
  character_template.rs   生成结果里出现新角色时的自动注册
  version_history.rs      版本快照的读写
```

### 开发 & 构建命令

```bash
npm run tauri dev      # 前端 + Rust 后端一起跑，带热重载
npm run tauri build    # 打包成可分发的安装包/可执行文件
cd src-tauri && cargo test   # 4 个 PreviewBridge 协议反序列化单测
```

### 数据存放位置

- **应用配置**（项目路径、Godot 路径、端口、API Key）：本机 Tauri 应用配置目录下的 `settings.json`，纯文本，不进任何 git 仓库。
- **版本历史 / 资源描述索引**：写在 **NovaRR 工程目录下**的 `vvn_data/`（`vvn_data/history/`、`vvn_data/asset_descriptions.json`），随 NovaRR 工程一起存放，方便剧本和它的版本历史/资源描述放在一起管理。

## 已知限制

- VVN 目前只扫描 `resources/scenarios/*.txt`（剧本）和 `resources/standings`、`resources/backgrounds` 下的图片。其它素材类型（音频、视频、shader、CG 图）暂时没有专门的素材管理面板，仍需要手动放文件、在脚本里手写路径——AI 生成时的素材校验会读取这些已知路径，但找不到对应面板去浏览它们。
- 没有云端同步/多人协作——版本历史和配置都是纯本地文件。

## 排错

1. **PreviewBridge 只在 Debug 构建里编译**（NovaRR 源码里是 `#if DEBUG`）。VVN 拉起 Godot 用的是 `godot --path <project>`（直接跑游戏，不是导出包、不是 headless），这个跑法本身就是 Debug 配置，不需要特殊处理；但如果改成跑一个 Release 导出包，PreviewBridge 不存在，VVN 永远连不上。
2. **第一次「进入项目」前先确认 NovaRR 能独立跑起来**。VVN 只是无脑拉起一个进程，C# 编译错误它看不出来，只会表现成"等了 20 秒连不上 PreviewBridge"这种含糊的超时提示。
3. **端口冲突**：如果同时手动开着一个 Godot 编辑器会话在跑 NovaRR，又用 VVN 再拉起一份，两边会抢同一个 `9999` 端口——PreviewBridge 抢不到端口只会打日志、自己保持禁用状态（不会让 Godot 整个崩掉），但 VVN 那边会一直连不上、反复重试到超时。把其中一个关掉，或者在设置里改端口。
4. **API Key 留空**：设置依然能正常保存，但点「开始生成」/「生成描述」会报错——这两个是可选功能，核心的"手改剧本 + 热重载 + Seek 预览"流程完全不需要任何 API Key。
## Agents sidecar

VVN includes a Python agents sidecar in `sidecar/`. On Tauri startup the Rust runtime launches `binaries/vvn-agents`, reads the first stdout line as the local FastAPI port, and keeps the child process alive until project close. As of Prompt 6, `/generate` owns the codegen node: VVN sends the already-built prompt and DeepSeek API key per request, the sidecar runs a small LangGraph `codegen -> parse` graph, and returns structured `{ new_chars, script }`. VVN still owns retries, deterministic validators, writes, reload/seek, snapshots, and summaries.

The Windows sidecar artifact is bundled at `src-tauri/binaries/vvn-agents-x86_64-pc-windows-msvc.exe`. Rebuild it with:

```powershell
python -m PyInstaller --onefile --name vvn-agents sidecar\main.py --distpath src-tauri\binaries --workpath sidecar\build --specpath sidecar
Copy-Item -LiteralPath src-tauri\binaries\vvn-agents.exe -Destination src-tauri\binaries\vvn-agents-x86_64-pc-windows-msvc.exe -Force
```

`sidecar/build/` and `sidecar/*.spec` are local PyInstaller intermediates and are ignored by Git.
