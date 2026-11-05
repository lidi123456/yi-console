# 内置操作电脑运行时

[English](README.en.md)

Windows x64 的 yi-agentd 内置 MIT 许可证的 [Cua Driver](https://github.com/trycua/cua)，版本和摘要固定在 `manifest.json`。构建前执行：

```sh
python scripts/prepare-computer-use.py
```

脚本仅下载固定发行版和许可证，校验 SHA-256 后提取允许的可执行文件。Windows 构建在未准备驱动时失败；生成的二进制不提交 git。发布 CI 在编译 Windows daemon 前准备产物，因此最终用户无需另外下载驱动。运行时将驱动和 MIT 许可证释放到自己的版本目录，校验可执行文件摘要，关闭驱动遥测与更新检查，不从 PATH 选择执行文件。

首版其他操作系统不内置或启用该驱动。使用方式和限制见[操作电脑](../../docs/computer-use.md)。
