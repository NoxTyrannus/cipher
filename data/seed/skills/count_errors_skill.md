---
name: count-errors
description: 统计指定目录下 .log 文件中 ERROR 出现次数并写入结果文件
inputs:
  path: 要扫描的目录
  output: 结果文件路径
steps:
  - 使用 shell 命令 grep -R ERROR <path> | wc -l 统计错误数
  - 将统计数字写入 <output>
---
