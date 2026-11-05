# localreel-plugin-strm

A LocalReel storage plugin that turns `.strm` files into remote media entries.

A `.strm` file is a small text file containing one HTTP(S) URL:

```text
http://nas.local/media/Show/Show.S01E01.mp4
```

The plugin lists those files as a storage source. Entry names come from the
file stem plus the URL extension (`Show.S01E01.strm` → `Show.S01E01.mp4`), and
reads are proxied with HTTP Range, so LocalReel can probe, thumbnail and stream
the remote file without downloading it first.

## Build

```sh
cargo build --release
./build.sh          # Windows: .\build.ps1
localreel-plugin-cli pack pkg --out dist/strm.lrplug
```

`build.sh`/`build.ps1` stage `pkg/manifest.json` and `pkg/bin/plugin`. The
package can be built with
[localreel-plugin-cli](https://github.com/LocalReel/localreel-plugin-cli):

```text
packed strm 0.1.0 -> dist/strm.lrplug (2 files, 1080458 bytes)
sha256: 752c7d3b...
```

## Install

```toml
# localreel.toml
[[plugins]]
package = "plugins/strm.lrplug"

[plugins.config]
root = "D:/strm"
```

Then add a source with kind `strm`:

```http
POST /api/v1/sources
{"uri": "D:/strm", "kind": "strm"}
```

The plugin opens outbound HTTP(S) connections, which is why its manifest
declares the `network` permission.

## 简体中文

把 `.strm` 文件变成远端媒体条目的 LocalReel 存储插件。

每个 `.strm` 文件写一个 HTTP(S) 地址；插件把它们列为媒体源，条目名由文件名和
URL 扩展名组成（`Show.S01E01.strm` → `Show.S01E01.mp4`），读取时按 Range 代理，
因此探测、缩略图和播放都不需要先下载整个文件。

构建：`./build.sh`（Windows 用 `build.ps1`），再用
`localreel-plugin-cli pack pkg --out dist/strm.lrplug` 打包。安装时在
`localreel.toml` 的 `[[plugins]]` 指向该包，`plugins.config.root` 指向 `.strm`
目录，然后以 `kind = "strm"` 添加来源。
