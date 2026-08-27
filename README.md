# OSS 上传助手
为了避免oss browswer 上传下载出现任务失败，方便多文件大文件传输而写的GUI，本质用的还是ossutil。

## 它解决什么

- **卡断不用重传** — 断点目录固定，重开软件再点一次就接着传
- **不重复传** — 已存在且未修改的文件自动跳过
- **传完自动校验** — 对比本地 / 远端文件数，心里有底
- **路径防呆** — 实时预览完整 `oss://` 地址，不会传到野目录
- **凭证不上命令行** — 写临时 0600 配置文件，任务结束即删

## 跑起来

环境：Node 18+、Rust 1.77+、Windows 需 WebView2。

```bash
# 1. 把 ossutil 放进 src-tauri/binaries/
#    Windows: ossutil.exe   macOS/Linux: ossutil (chmod +x)

npm install
npm run tauri:dev      # 开发
npm run tauri:build    # 打包安装包
```

打包产物在 `src-tauri/target/release/bundle/`，发给同事直接装，对方不需要任何环境。

## 目录结构

```
ossup/
  index.html            界面结构
  src/main.js           前端逻辑
  src/style.css         样式
  src-tauri/
    src/lib.rs          全部 Rust 逻辑（配置 / 进程 / 进度 / 校验）
    binaries/           放 ossutil 可执行文件
    tauri.conf.json     窗口 / 打包配置
```

## 实际拼出来的命令

```
ossutil cp -r -u -f \
  --config-file <配置目录>/session.ossutilconfig \
  --checkpoint-dir <配置目录>/checkpoints \
  --output-dir <配置目录>/output \
  -j 5 --parallel 8 --part-size 16777216 \
  <本地文件夹> oss://<bucket>/<prefix>/
```

## 实现上的坑

- **进度按字节读，不按行读** — ossutil 用 `\r` 重绘同一行，`read_line` 会一直阻塞
- **子进程用 `try_wait()` 轮询** — 直接 await `wait()` 会持锁，“停止”按钮会死锁
- **拖拽走 Tauri 的 `onDragDropEvent`** — 浏览器 File API 拿不到本地绝对路径

## 参数怎么调

| 场景 | jobs | parallel | part-size |
| --- | --- | --- | --- |
| 大量小文件（图片、标注 json） | 16–32 | 4 | 8 MB |
| 少量大文件（4K 视频、压缩包） | 2–3 | 16–32 | 32–64 MB |
| 混合 | 5 | 8 | 16 MB |

带宽吃满了就别再加，并发过高反而容易被服务端限速。

## 安全说明

本地凭证只做了 base64 混淆，不是加密。

## 其他

- 图标：换成自己的就放一张 1024x1024 PNG，跑 `npm run tauri icon path/to/icon.png`
- 鉴权报错：到“高级设置”里勾上“用命令行参数传递凭证”再试
- 进度解析对 ossutil 1.x / 2.x 都做了宽松匹配，取不到的字段就不显示
