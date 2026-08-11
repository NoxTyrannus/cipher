# Cipher

> 终端原生 AI 代理 — 三种工作模式，本地记忆系统

Cipher 是一个运行在终端中的 AI 代理：你在终端里输入任务，Cipher 通过 LLM 思考、调用工具（读文件/写文件/执行命令/沙箱代码）、沉淀记忆来完成任务。所有数据保存在本地（DuckDB 注册表 + TriviumDB 向量记忆），隐私可控。

## 特性

- **三种工作模式**：UNNI（协同交互）/ KEEP（持续执行）/ LOOP（迭代循环），Tab 键切换
- **本地记忆系统**：经验沉淀、认知图、偏好学习、工具使用方式迭代
- **沙箱执行**：Wasmtime 沙箱运行代码，权限边界可控
- **多模型支持**：MiniMax M3 / Kimi K3 / SenseNova / DeepSeek / Doubao / GLM 等

## 安装

### 方式一：下载预编译二进制（推荐）

一行命令，无需 Rust 工具链，自动检测 OS 和架构：

```bash
bash <(curl -sL https://github.com/NoxTyrannus/cipher/raw/main/install.sh) --download
```

或者先 clone 仓库再安装：

```bash
git clone https://github.com/NoxTyrannus/cipher.git
cd cipher
bash install.sh --download
```

Windows 用 PowerShell：

```powershell
powershell -Command "iwr -Uri https://github.com/NoxTyrannus/cipher/raw/main/install.ps1 -OutFile install.ps1; .\install.ps1 -Download"
```

`install.sh --download` 自动检测平台（Linux x86_64/arm64、macOS x86_64/arm64、Windows x86_64/arm64），从 GitHub Release 下载对应安装包，解压后安装到 `~/.local/bin`（不可写时降级 `sudo` 到 `/usr/local/bin`）。

每个 Release 附 `SHA256SUMS.txt`，可用 `sha256sum -c` 校验下载完整性。

### 方式二：源码构建安装

前置要求：Rust 工具链 1.96.0（`rustup` 自动按 `rust-toolchain.toml` 安装）；Linux / macOS / Windows。

```bash
git clone https://github.com/NoxTyrannus/cipher.git
cd cipher
bash install.sh
```

`install.sh` 检查工具链并执行 `cargo build --release`，然后把 `cipher` 复制到 `~/.local/bin`（不可写时降级 `sudo` 安装到 `/usr/local/bin`）——安装完成后即可在任意目录直接运行 `cipher`。

其他选项：
- `bash install.sh --test` — 从源码构建，模拟安装流程验证
- `bash install.sh --no-install` — 仅构建，不安装

### 初始化

```bash
cipher setup
```

`setup` 引导配置：选择模型供应商 → 填写模型标识与 API key → 连接性验证。也可随时用 `cipher config` 管理模型与配置。

### 启动

```bash
cipher
```

进入 TUI 交互界面。可选全局参数：`--config <路径>` / `--data-dir <路径>`。

## 三种工作模式

| 模式 | 名称 | 语义 | 使用场景 |
|---|---|---|---|
| **UNNI** | 协同交互 | 与你充分交互：执行任务、汇报结果、接受反馈 | 日常任务、明确目标、需要反馈确认 |
| **KEEP** | 持续执行 | 你交付目标后自主持续推进，期间只听不说，最终一次性汇报 | 多步骤任务、创作、构建 |
| **LOOP** | 迭代循环 | 自主循环迭代改进，期间不说话，只看产物变化 | 探索修复、优化打磨、自进化 |

**切换**：TUI 中按 `Tab` 循环切换（UNNI → KEEP → LOOP → UNNI）。

- UNNI：think 与 say 自由使用，交互式推进
- KEEP：say 是整个周期唯一一次汇报机会（留给最终交付）；think 每轮携带执行意图
- LOOP：say 由系统剥离（不可用于交流）；think 驱动每轮迭代，产物增量是推进信号

## 配置

配置文件：`~/.config/cipher/config.toml`（首次运行自动创建）。

| 配置项 | 说明 | 默认值 |
|---|---|---|
| `data_dir` | 数据目录（注册表/记忆/产物） | `~/.local/share/cipher/` |
| `default_mode` | 默认工作模式（unni/keep/loop） | `unni` |
| `memory_mode` | 记忆模式（见下） | `mixed` |

### 记忆模式（memory_mode）

| 模式 | 语义 |
|---|---|
| `sync` | 同步模式：每轮记忆 settle 完成后再继续 |
| `mixed` | 混合模式（推荐）：记忆 settle 有界等待（≤5s），超时携带已沉淀部分继续 |
| `async` | 异步模式：记忆异步落库，不阻塞主流程 |

### 身份自定义

出厂身份默认名 **cipher**。可在 TUI 中输入 `/config` 进入配置面板 → Agent 改名 修改，或直接编辑数据目录下 `prompts/SOUL.md`。

### 模型超参数

模型温度/采样参数可在 `cipher setup` 或 `cipher config` 中配置；未配置时按模型官方建议值（如 MiniMax M3：temperature=1.0，top_p=0.95）。

## 工具能力

| 工具 | 说明 |
|---|---|
| `file.read` / `file.write` | 读写本地文件（沙箱路径限制） |
| `file.list` / `file.delete` / `file.move` | 文件管理 |
| `text.grep` | 文本检索 |
| `shell.exec` (Unix) / `powershell.exec` (Windows) | 执行系统命令（权限策略控制） |

## 模型支持

| 系列 | 提供商 |
|---|---|
| DeepSeek | DeepSeek |
| M3 | MiniMax |
| GLM | 智谱AI |
| Kimi | 月之暗面 |
| SenseNova | 商汤科技 |
| Doubao | 字节跳动 |

## 隐私

- API key 仅保存在本地模型注册表（`data_dir` 内），不写入日志
- 所有数据（对话、记忆、产物）本地存储，不依赖云端
- LLM 请求仅发送必要上下文

## License

MIT License — 详见 [LICENSE](LICENSE)