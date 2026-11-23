# PXPIPE 原版对照

9Router 的安装器动态选择 npm 发行版。这里用 `pxpipe-proxy@0.16.0` 作为对照。发布元数据的 `gitHead` 是 `9e0e0e99b4483f40e8a58036bb1f6a1a81c331e3`。 [npm 发行元数据](https://registry.npmjs.org/pxpipe-proxy/0.16.0)

`pxpipe_assets` 保存该发行版的 Spleen、Unifont 和 JetBrains Mono 10/12/14 光栅及授权文本。压缩数据依次保存小端字形数、码点、偏移、宽度标志和像素。灰度图使用字节偏移，位图使用位偏移。

对照用例使用合成文本。图像比较解码后的像素、画布尺寸和分页。PNG 压缩字节数随 Deflate 实现变化，网关仍按实际编码字节数报告 `imageBytes`。原版只在重新录制和原版回放时需要。`PXPIPE_LIBRARY` 指向解包后的 npm `package` 目录。

Rust 按内置模型配置和 `PXPIPE_GPT_PROFILES` 的最长前缀匹配处理 Anthropic Messages。回放覆盖视觉计费、备用字体、计费行、动态环境块、历史分页和 pin 追加位置。原生渲染上限为单页 8,388,608 像素、512 倍换行标记横向放大和 32 次笔画膨胀。超出原生范围的配置沿用现有回退路径。

```sh
PXPIPE_LIBRARY=/path/to/package rust-anything app record . --no-traps --timeout 12 --inputs inputs.jsonl -- node --disable-warning=MODULE_TYPELESS_PACKAGE_JSON original.mjs
PXPIPE_LIBRARY=/path/to/package rust-anything app replay .
RUSTC_WRAPPER= cargo build --offline --locked
rust-anything app replay . --build target/debug/pxpipe-transform-replay
```
