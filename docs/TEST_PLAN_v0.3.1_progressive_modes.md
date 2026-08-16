# Cipher v0.3.1 渐进模式测试任务书

> 目标：按设计思想培养 cipher：
> UNNI 充分协同并学习偏好 → KEEP 一次对齐、多次独立完成 → LOOP 永续思考并完成 Glanstia 真实 skill 吸收。
> 测试原则：真实 MiniMax-M3；不心疼 token；测试期间不改代码。

## 0. 环境与产物目录

```bash
cd /mnt/4A7695DB4A18F737/cipher/src
git checkout v0.3.1-dev
git log --oneline -3
cargo build
cargo build --examples

export MINIMAX_API_KEY='sk-cp-KyPmA6lrWokG0M1NH48_QJSyuU-R0czflhUF0pOMXaBpZkZq3EqNcfB1LVJps07t7OBE48Gnhb7T6VbM_2GGE1rrvQcpEvtbIbZsQnXIhsvz9PMHEn5OyNI'

rm -rf /tmp/cipher-v031-progressive
mkdir -p /tmp/cipher-v031-progressive/{workspace,config,data,logs}
```

Glanstia 准备：

```bash
rm -rf /tmp/Glanstia_System-skill
cd /tmp
git clone https://github.com/NoxTyrannus/Glanstia_System-skill.git
cd Glanstia_System-skill
rm -rf unzipped
mkdir unzipped
unzip -q Glanstia_System0.1.2.zip -d unzipped
cp -r unzipped/Glanstia_System /tmp/cipher-v031-progressive/workspace/Glanstia_System
```

## 1. 阶段 A：UNNI 偏好培养

目的：让 cipher 在 UNNI 下通过对话和模式切换，记住用户在各模式下的偏好。

启动：

```bash
cd /mnt/4A7695DB4A18F737/cipher/src
XDG_CONFIG_HOME=/tmp/cipher-v031-progressive/config \
XDG_DATA_HOME=/tmp/cipher-v031-progressive/data \
RUST_LOG=debug \
./target/debug/cipher \
  --config /tmp/cipher-v031-progressive/config/cipher/config.toml \
  --data-dir /tmp/cipher-v031-progressive/data \
  run
```

应用 cwd 设为：

```bash
/tmp/cipher-v031-progressive/workspace
```

按顺序输入以下内容（每轮等待 agent 回复）：

1. UNNI：
```text
在 UNNI 模式下，我喜欢你直接推进、简洁汇报，不要反复确认。
```
2. UNNI：
```text
以后所有文件工作都放在当前工作区，不要问我路径。
```
3. Tab 切到 KEEP：
```text
在 KEEP 模式下，我喜欢你独立完成，不中途汇报，除非目标本身有歧义。
```
4. Tab 切到 LOOP：
```text
在 LOOP 模式下，用户的新指令应该作为持续输入的养料，不是重启任务。
```
5. Tab 切回 UNNI：
```text
请复述你记住的、我在三种模式下的偏好。
```

阶段 A 通过标准：
- cipher 在 UNNI 复述中至少提到三种模式偏好；
- 日志出现 `memory.attention.write`，且写入内容覆盖三模式偏好；
- 没有出现用户偏好被当成一次性任务的执行动作。

## 2. 阶段 B：KEEP 独立创作

目的：一次对齐，多次独立完成。

使用同一数据目录，默认模式设为 KEEP。新开进程：

```bash
cd /tmp/cipher-v031-progressive/workspace

XDG_CONFIG_HOME=/tmp/cipher-v031-progressive/config \
XDG_DATA_HOME=/tmp/cipher-v031-progressive/data \
RUST_LOG=debug \
/mnt/4A7695DB4A18F737/cipher/src/target/debug/cipher \
  --config /tmp/cipher-v031-progressive/config/cipher/config.toml \
  --data-dir /tmp/cipher-v031-progressive/data \
  run
```

只输入一次目标：

```text
写一部三章短篇小说，主题是“一个在永夜中守护最后火种的旅人”。
使用我之前表达过的偏好：中文、简洁、章节文件放在 novel/ 目录。
每章独立成文，自动推进，不需要再问我。
```

随后**不再输入任何内容**。

运行 20 分钟，或出现以下任一条件后外部停止：

- `novel/chapter_01.md`
- `novel/chapter_02.md`
- `novel/chapter_03.md`

均存在且非空。

阶段 B 通过标准：
- 3 个章节文件存在且字数 > 0；
- 日志显示多轮 think → 执行链自动推进；
- say 次数 <= 1；
- 如果出现 say，必须是目标对齐或最终交付，不是过程汇报。

## 3. 阶段 C：LOOP 吸收 Glanstia

目的：在 LOOP 永续模式下，把真实 skill 变成 cipher 自身能力并实际执行验证。

继续使用同一数据目录，工作区已有：

- `novel/`
- `Glanstia_System/`

默认模式设为 LOOP（mix off 先测）。

启动：

```bash
cd /tmp/cipher-v031-progressive/workspace

XDG_CONFIG_HOME=/tmp/cipher-v031-progressive/config \
XDG_DATA_HOME=/tmp/cipher-v031-progressive/data \
RUST_LOG=debug \
/mnt/4A7695DB4A18F737/cipher/src/target/debug/cipher \
  --config /tmp/cipher-v031-progressive/config/cipher/config.toml \
  --data-dir /tmp/cipher-v031-progressive/data \
  run
```

第一轮输入：

```text
现在进入长期循环任务：把 Glanstia_System 中的 soul_guide 转化为 cipher 自身能力。
要求：
1. 只重组 soul_guide，不要扩展到 soul_maker/soul_summon/soul_nurture；
2. gno 依赖必须标记为 external_dependency，不得伪装成内置能力；
3. 生成 capability.import JSON 后先用 json.validate 校验；
4. 导入后必须创建两个测试 .soulmd，实际执行导入后的能力；
5. 必须产生 souls.csv 和 Hades 归档文件；
6. 最后把真实验证结果写入 validation.txt。
```

运行 5 分钟后，注入渐进指令：

```text
继续推进。如果能力导入失败，不要绕开 soul_guide；先修正 import JSON，再重试。gno 只作为外部依赖记录。
```

再运行 5 分钟后，注入：

```text
检查你的工作是真实完成还是只写了总结。如果 souls.csv 或 Hades 归档还不存在，就继续执行，不要宣布完成。
```

运行最多 30 分钟，或出现全部产物后外部停止。

阶段 C 通过标准：
- `validation.txt` 存在，内容说明真实验证结果；
- `souls.csv` 存在，至少 2 行；
- `Hades/` 下至少有 2 个 `.soulmd`；
- 日志出现 `OK capability.import`；
- 日志出现导入后能力被实际执行的证据；
- 没有依赖 `[COMPLETE]` 自报完成的假象。

## 4. LOOP mix on 对照

阶段 C 完成后，用全新工作区重复阶段 C，但配置中：

```toml
[mode_styles.loop]
mix_thinking = true
```

记录同样指标并与 mix off 对照。

## 5. 回传报告格式

```text
阶段 A：通过/失败
证据：

阶段 B：通过/失败
章节文件数量/大小：
say 次数：
自动推进轮次：
证据：

阶段 C mix off：通过/失败
validation.txt：
souls.csv：
Hades 文件数：
OK capability.import：
导入后实际执行证据：
[COMPLETE] 次数：
invalid_json_output 次数：
耗时：
证据：

阶段 C mix on：通过/失败
同上
```

测试会话不得修改代码；所有失败和漂移现象原样记录。
