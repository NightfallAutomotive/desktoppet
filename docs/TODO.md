# TODO

## Phase 4：设置与模型管理

- [ ] **P4-WINDOW-MODE（issue #1130）**：Windows 普通窗口、可配置纯色背景、OBS 窗口捕获与抠图、旧 v1 配置读取、禁用颜色预览和统一客户区等比例缩放已实现。依赖 ADR-0057、ADR-0054；协议见 ADR-0089。退出条件：配置/UI/runtime/HWND/D3D11 回归、`just check`、Windows product smoke、OBS BitBlt/WGC/抠图验收，以及 Windows 10 和 125/150/200% 跨屏实机验证。当前 Windows 11 OBS 与自动化验收已有证据，缺少的 Windows 10、跨屏与设置布局验收保持未勾选；macOS 不在本任务范围。证据见 `docs/benchmark/windows-window-mode-2026-10-08.md`。
- [ ] **P4-MOTION-OVERLAP（issue #1107）**：默认关闭的动作叠加开关、旧 v1 文档读取、独立计时/淡出/停止和按启动顺序求值已实现，标准模型跨组重复资源重播有自动化回归。依赖 ADR-0061、ADR-0012（动作音效）；协议见 ADR-0088。退出条件：workspace 与独立 config-store contract、`just check`、双平台 CI smoke，以及 Windows/macOS 实机多动作叠加、跨组重播和 800×600/DPI/Retina 目视验收。尚缺的平台实机证据保持未勾选。
- [ ] **P4-NESTED-MODEL-IMPORT（issue #1105）**：嵌套模型发现、单模型自动继续、多模型复用选择弹框、模式选择及顺序导入已实现。依赖 ADR-0037、ADR-0055、ADR-0059；协议与扫描边界见 ADR-0087。退出条件：自动化扫描/服务/GPUI 交互回归、`just check`、macOS smoke，以及 Windows/macOS 实机文件夹选择与拖入、800×600 和 DPI/Retina 目视验收。缺少的平台实机证据仍保持未勾选，不由另一平台编译或 headless 测试代替。

## Phase L：Linux 桌面支持

范围、平台边界与各阶段退出条件见 ADR-0090；目标仅 `x86_64-unknown-linux-gnu`，参考环境 KDE Plasma 6 Wayland/X11。L8 通过前不宣称 Linux 支持，编译通过或单一会话可用不等于完成。

- [ ] **L0-PLAN**：ADR-0090、本 Phase 与 Technical Design §14 已写入，ADR-0006 已标注部分取代。退出条件：维护者确认范围后勾选。
- [x] **L1-BUILD-BASELINE**：已完成：Linux 上 workspace `cargo check`/三组 clippy `-D warnings`/release check 通过，不链接 Cubism Core 的 crate 测试 496 项通过；未实现能力返回强类型 unsupported（overlay、窗口显隐、显示器查询、右键菜单、更新），剪贴板、文件选择、XDG 存储/自启动、系统版本、输入权限检查已有 Linux 实现；Linux bindings 经 `tools/cubism-bindgen` 双次生成；`test_supported_platform_cfg.py` 改为 Linux cfg 只允许出现在 ADR-0090 列出的 crate；`verify.yml` 新增 `linux-workspace` job。依赖 L0。证据：fork CI run 39863651537 全部 27 个 job 通过，含 `Test workspace (ubuntu-latest)` 与 Windows/macOS workspace、product smoke、packaging。记录见 ADR-0090 “L1 implementation record”。
- [x] **L2-CUBISM-CORE**：已完成：维护者从官方页面取得 `5-r.5` ZIP（hash 与 inspector 通过），仅 vendor `lib/linux/x86_64/libLive2DCubismCore.a` 并静态链接，provenance 写入 `docs/cubism/cubism-sdk-source-and-license.md`；本机完整 `cargo test --locked --workspace` 1156 项通过，三个预置模型经 Core 加载；`linux-workspace` job 已去掉测试排除项。依赖 L1。证据：同一 CI run 的 Linux job 运行完整 `cargo test --workspace` 并通过。退出条件：三个预置模型的 Live2D 测试在 Linux 通过，Linux CI 运行完整 `cargo test --workspace`。
- [ ] **L3-RENDERER-SPIKE**：已实现：决策为 `wgpu`（GPUI 已在 Linux 依赖图中，固定 `=29.0.4`，30.x 阻塞原因见 ADR-0090），WGSL 逐行移植 Metal shader，离屏 renderer 与 Linux 模型封面捕获已接入；三个预置模型通过共享 frame smoke 检查，AMD RADV/NVIDIA/llvmpipe 三种 Vulkan 驱动同帧 >99.93% 像素一致，证据见 `docs/benchmark/linux-renderer-spike-2026-10-22.md`。原退出条件引用的 D3D11/Metal 参考图并不存在，已在 ADR-0090 更正。依赖 L2。剩余：Linux CI（lavapipe）首次运行 GPU 测试通过后勾选；GLES 在 Flatpak 内未能验证。退出条件：三个预置模型离屏渲染通过 frame smoke 检查，独立驱动同帧一致，backend 决策写入 ADR。
- [ ] **L4-OVERLAY**：进行中。已实现（KDE Wayland）：`wlr-layer-shell` 主猫窗口、`wgpu` surface 呈现（Mailbox，premultiplied alpha）、fractional scale + viewporter、`wp_alpha_modifier_v1` 窗口不透明度、点击穿透（空 input region）、置顶（Overlay/Bottom layer）、显隐（销毁并在原位重建）、模型切换与失败保留、产品 frame loop 与 `--run-seconds` 退出；产品在 KDE Wayland 上运行并正常退出，`product_window_smoke` 5 秒呈现 301 帧。维护者已在 KDE Wayland 目视确认透明与置顶。已实现 `wl_pointer` 左键拖动、右键拖动缩放（共享 `ResizeDrag`）与 keep-inside-screen。Wayland 无法实现 hover hide（无全局指针，隐藏时穿透），右键菜单待 L6。剩余：拖动/缩放实机验收、多显示器、X11 后端（可支持 hover hide）、移除 overlay crate 的 Linux `dead_code` 豁免。原范围：X11 与 KDE Wayland（`wlr-layer-shell`）主猫窗口：透明、置顶、穿透、拖动/缩放、分数缩放，owner 与 shutdown 顺序符合 AGENTS.md §4.3。依赖 L3。同时移除 L1 为 overlay/app/系统菜单加的 Linux `dead_code` 豁免。退出条件：实机验证与反复开关、正常退出证据。
- [ ] **L5-INPUT**：listen-only evdev 键鼠、`EVIOCGKEY` 校正、热插拔、`input` 组权限状态、logind 睡眠/锁屏/VT 切换 Reset、X11 绝对光标与 Wayland 相对累积、`gilrs` Linux 手柄。依赖 L1。退出条件：压力测试边沿丢失 0、生命周期 Reset 矩阵、`gilrs` 空闲 CPU 与 reset/shutdown 预算复测。
- [ ] **L6-SYSTEM-INTEGRATION**：托盘/菜单（SNI）、`GlobalShortcuts` portal 与 X11 快捷键、文件选择、剪贴板、XDG autostart、单实例 socket、主题。依赖 L4、L5。退出条件：两种 KDE 会话的设置 UI 与系统菜单 smoke，800×600 与 100/125/150/200% 布局验收。
- [ ] **L7-PACKAGING-UPDATE**：`bongocat-packaging` 产出 AppImage，`UpdateTargetTriple`、`deny.toml`、packaging contract 与 `linux-x86_64.json` fragment 同步，签名更新。依赖 L6。退出条件：实机安装、更新、回滚、卸载。
- [ ] **L8-SUPPORT-DECISION**：8 小时 soak 无资源增长，发布能力矩阵（含 GNOME Wayland 结论），新 ADR 声明支持范围，同步 README/CHANGELOG 双语。依赖 L7。
