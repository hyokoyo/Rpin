# RPin

RPin 是面向 Windows 10/11 x64 的轻量级 Rust 截图工具，支持区域截图、全屏截图和贴图窗口。

## 构建

需要 Rust stable（x64 MSVC）、Visual Studio C++ Build Tools 和 Windows SDK。在仓库根目录使用 PowerShell 执行：

```powershell
cargo test --locked
cargo build --release --locked
```

可执行文件生成在 `target\release\rpin.exe`。RPin 不需要额外运行时或安装程序。

## 使用

- 按 `F1` 打开区域截图，拖动选择范围。松开鼠标后可以贴图、保存为 PNG 或复制到剪贴板。
- 按 `Shift+F1` 截取鼠标所在的显示器并复制到剪贴板。
- 按 `Ctrl+Shift+F1` 截取鼠标所在的显示器并另存为 PNG。
- 按 `T` 可将选区创建为贴图；`Ctrl+T` 创建贴图并复制图像。
- 贴图窗口可拖动、缩放、调节透明度、置顶、隐藏或关闭。托盘菜单可管理现有贴图。
- 右键托盘图标可打开配置、重新加载配置、切换开机自启或退出程序。

## 配置和日志

首次启动时，配置文件会创建在 `%APPDATA%\RPin\config.toml`，日志保存在 `%LOCALAPPDATA%\RPin\logs\`。

快捷键、通知、保存选项和贴图默认行为可在配置文件中设置。配置格式可参考首次启动生成的 `config.toml`；修改后从托盘菜单选择“重新加载配置”。

关闭选区后，RPin 默认保留 Direct2D 渲染资源 60 秒，供连续截图复用。可在已有的 `[ui]` 段中设置 `direct2d_idle_seconds = 60`，单位为秒，设为 `0` 时关闭选区后立即释放。重新加载配置后生效。

## 许可证

本项目使用 MIT 许可证，详见 [LICENSE](LICENSE)。
