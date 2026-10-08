# 象胥 Xiangxu

> 实时屏幕翻译浮层 — 截屏 → 视觉模型识图 → LLM 翻译 → 置顶浮层显示

典出《周礼·秋官》——象胥，掌蛮夷戎狄之国使，传王之言而谕悦焉。

专为游戏对话实时翻译设计（原始目标场景：Pokémon Gamma Emerald），全程本地推理，无需联网，无需 API Key。

---

## 功能特性

- **实时监控翻译**：框选游戏对话框区域，自动检测画面变化 → OCR 识图 → 翻译 → 浮层显示
- **完全离线**：识图用本地视觉模型 (Qwen2.5-VL-3B)，翻译用本地语言模型 (Qwen2.5-1.5B)，不依赖云服务
- **流式输出**：OCR 和翻译均为流式，边识别边显示、边翻译边上屏，无需等整句完成
- **窗口吸附**：翻译面板可吸附到游戏窗口，跟随游戏窗口移动
- **悬浮显示**：提供工具条、悬浮球和字幕浮层，以及独立设置窗口
- **宝可梦术语表**：内置宝可梦、招式、特性、道具和丰缘地点译名；可编辑 `assets/glossary/custom.tsv` 补充术语，重启后生效
- **区域调整**：框选区域可随时拖拽移动、缩放，不用重新框选
- **多语言**：支持 English / 中文 / 日本語 之间的翻译
- **远程模式（可选）**：也可配置 OpenAI 兼容 API 做翻译，用本地识图 + 远程大模型

## 系统要求

| 项目 | 要求 |
|------|------|
| 操作系统 | Windows 10/11（x64） |
| GPU | NVIDIA 显卡（必需，驱动支持 CUDA 13.3） |
| 显存 | ≥ 4GB（模型全部卸载到 GPU） |
| 内存 | ≥ 8GB |
| 磁盘 | 模型约 4GB + CUDA 版 llama.cpp 约 0.7GB |
| Rust | 构建需要 Rust 1.85+（edition 2024） |

## 快速开始

### 方式 A：预编译版（免 Rust 环境，推荐）

从 [GitHub Releases](https://github.com/smallclover/Xiangxu/releases) 下载 `Xiangxu-release.zip`，解压后：

1. **双击 `xiangxu.exe`** 启动应用
2. 打开设置 → **「模型资源」**，点击 **「一键下载缺失模型」**（约 4GB，带进度条）
3. 下载完成后框选游戏对话框区域，开始翻译

> 预编译版已内置 CUDA 版 llama.cpp 与 CUDA 13.3 运行时 DLL，NVIDIA 显卡（驱动支持 CUDA 13.3）开箱即用；换机器或运行库缺失时，可再跑 `download_cuda_runtime.ps1` 补装。

### 方式 B：从源码构建

#### 1. 克隆仓库

```bash
git clone https://github.com/smallclover/Xiangxu.git
cd Xiangxu
```

#### 2. 下载模型（约 4GB）

```powershell
.\download_models.ps1
```

或在应用内设置 → 「模型资源」一键下载（推荐）。脚本/应用会从 Hugging Face 下载以下模型到 `assets/models/`：

| 模型 | 用途 | 大小 |
|------|------|------|
| Qwen2.5-1.5B-Instruct Q4_K_M | 翻译 | ~1.0 GB |
| Qwen2.5-VL-3B-Instruct Q4_K_M | 视觉识图 | ~1.8 GB |
| mmproj-f16 | 视觉投影 | ~1.3 GB |

#### 3. 准备 llama.cpp 服务端（仅 CUDA 版）

需要 NVIDIA 显卡（驱动支持 CUDA 13.3）：

```powershell
# 1. 从 llama.cpp Releases 下载 CUDA 版二进制，解压到 llama-cuda\
# 2. 下载 CUDA 13.3 运行时 DLL 到同一目录：
.\download_cuda_runtime.ps1
```

> 脚本会自动从 NVIDIA 官方下载 `cudart64_13.dll` / `cublas64_13.dll` / `cublasLt64_13.dll` 到 `llama-cuda\`。应用启动时自动使用 `llama-cuda\llama-server.exe`，识图与翻译全部走 GPU 推理。

> 也可以直接用预编译版（方式 A），其中已内置 CUDA 版 llama.cpp 与全部运行时，无需此步。

#### 4. 构建并运行

```bash
cargo run
```

首次编译需要几分钟（依赖较多）。编译完成后会弹出「象胥 实时翻译」窗口。

## 使用说明

### 基本流程

1. **打开游戏**，让游戏窗口可见
2. **点击「框选区域」** → 主窗口全屏化，拖拽选定游戏对话框区域 → 松开确认
3. **点击「开始监控」** → 象胥开始自动截取该区域、识别文字、翻译并显示
4. 译文会实时显示在主面板中，同时保留历史记录

### 窗口吸附

- 点击「吸附窗口」→ 进入取窗模式，鼠标指向哪个窗口就高亮哪个
- 左键确认 → 翻译面板吸附到该窗口，跟随窗口移动
- 框选区域也会跟随窗口移动（基于窗口相对偏移）

### 区域调整

- 点击「调整区域」→ 可拖拽已框选区域整体移动，或拖四角/四边缩放
- 无需重新框选

### 设置说明

在主面板的设置区可配置：

| 设置项 | 说明 |
|--------|------|
| 模型资源 | 一键下载缺失的模型文件（带进度条），缺失时高亮提示 |
| 翻译模式 | 本地模型（完全离线）/ 远程API（需填 Key）/ 模拟（测试用） |
| 源语言 / 目标语言 | OCR 识别语言 / 翻译目标语言 |
| 服务端exe | llama-server.exe 路径（自动指向 llama-cuda 目录） |
| 翻译模型 | GGUF 模型路径 |
| GPU层数(-ngl) | 99=全部卸载到GPU（默认）；显存紧张可调小 |
| 识图模型 / mmproj | 视觉模型及投影文件路径 |
| 识图GPU层数 | 视觉模型的 GPU 卸载层数 |
| 识图最大边长 | 送入视觉模型前的缩放上限（像素），调小可加快识图但降低小字识别率 |

### 快捷键

- 可在设置中注册全局热键，用于快速开关监控

## 项目结构

```
Xiangxu/
├── src/
│   ├── main.rs        # 入口、UI 面板、事件循环
│   ├── capture.rs     # 屏幕截图（xcap）
│   ├── monitor.rs     # 后台监控线程（截屏→哈希检测→OCR→翻译）
│   ├── ocr.rs         # OCR 接口（调用视觉模型识图）
│   ├── translate.rs   # 翻译后端（本地llama.cpp / 远程API / 模拟）
│   ├── download.rs    # 模型一键下载（后台线程 + 进度上报）
│   ├── lang.rs        # 语言枚举
│   ├── state.rs       # 应用状态定义
│   └── resources.rs   # 嵌入式资源（字体）
├── assets/
│   ├── fonts/         # 中文字体（msyhl.ttf，嵌入二进制）
│   └── models/        # AI 模型（需下载，不入 Git）
├── llama-cuda/        # CUDA 版 llama.cpp + CUDA 13.3 运行时 DLL（不入 Git）
├── download_models.ps1       # 模型下载脚本（应用内也可一键下载）
├── download_cuda_runtime.ps1 # CUDA 13.3 运行时 DLL 补装脚本
├── build_release.ps1         # 构建 release 并打包发布目录
├── Cargo.toml
└── LICENSE
```

## 技术架构

```
┌─────────────────────────────────────────────────────────┐
│  后台监控线程                                             │
│                                                         │
│  xcap 截屏 → 图像哈希对比 → 有变化？                      │
│         ↓ 是                                             │
│  Qwen2.5-VL-3B 流式 OCR（边识别边上屏）                   │
│         ↓                                               │
│  Qwen2.5-1.5B 流式翻译（边翻译边上屏）                     │
│         ↓                                               │
│  mpsc 通道回传 → 主线程 UI 更新                           │
└─────────────────────────────────────────────────────────┘
         ↕ mpsc
┌─────────────────────────────────────────────────────────┐
│  主线程（egui UI）                                       │
│                                                         │
│  置顶面板：原文 / 译文 / 历史记录 / 设置                    │
│  框选模式：全屏拖拽选定区域                                │
│  区域调整：拖拽移动 / 四角缩放                             │
│  窗口吸附：跟随游戏窗口                                    │
└─────────────────────────────────────────────────────────┘
```

- **截屏**：xcap 0.9.4，支持多显示器
- **识图**：Qwen2.5-VL-3B-Instruct（视觉语言模型，能识别艺术字/彩色字）
- **翻译**：Qwen2.5-1.5B-Instruct（本地）或 OpenAI 兼容 API（远程）
- **推理引擎**：llama.cpp（CUDA 13.3 加速）
- **UI 框架**：eframe / egui 0.34.3
- **字体**：微软雅黑（msyhl.ttf，嵌入二进制，避免中文乱码）

## 常见问题

**Q: 没有 Rust 环境能直接用吗？**
A: 可以。下载 GitHub Releases 里的预编译版 `xiangxu.exe`，双击即用，无需安装任何环境。

**Q: 模型在哪下载？**
A: 打开应用设置 → 「模型资源」→ 一键下载（推荐）；或运行 `download_models.ps1`。模型来自 Hugging Face 官方仓库，Apache-2.0 许可。

**Q: 没有 NVIDIA 显卡能用吗？**
A: 不能。识图与翻译均依赖 CUDA 13.3 加速（3B 视觉模型 + 语言模型全程 GPU 推理），需要 NVIDIA 显卡且驱动支持 CUDA 13.3。

**Q: 可以用其他模型吗？**
A: 可以。在设置中修改模型路径即可。翻译模型需是 GGUF 格式的 instruct 模型，识图模型需是支持视觉的 GGUF 模型且配套 mmproj 文件。

**Q: 识图很慢怎么办？**
A: 调小「识图最大边长」（如 512~640），减少送入视觉模型的 token 数。

**Q: 可以不用本地模型，用远程 API 翻译吗？**
A: 可以。翻译模式选「远程API」，填入 endpoint（如 `https://api.deepseek.com`）、API Key 和模型名。识图仍用本地视觉模型。

## 许可证

Apache License 2.0

模型文件遵循各自的原始许可（Qwen 系列均为 Apache-2.0）。

## 致谢

- [llama.cpp](https://github.com/ggml-org/llama.cpp) — GGUF 推理引擎
- [Qwen2.5](https://github.com/QwenLM/Qwen2.5) — 语言模型
- [Qwen2.5-VL](https://github.com/QwenLM/Qwen2.5-VL) — 视觉语言模型
- [egui](https://github.com/emilk/egui) — 即时模式 GUI
- [xcap](https://github.com/nashaofu/xcap) — 跨平台截屏
