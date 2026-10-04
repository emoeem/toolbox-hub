# 批量图片处理（image-batch）

一个目录 + 一个通配符 + 一个输出目录，剩下的交给 ImageMagick 7 的 `magick`。

## 动作

| 动作 | 命令 | 干什么 |
| --- | --- | --- |
| 批量转格式 | `image-batch convert` | jpg / png / webp / avif / tiff / gif / bmp 互转 |
| 批量缩放 | `image-batch resize` | 限制在框内 / 填满后裁剪 / 强行拉伸 |
| 批量去元数据 | `image-batch strip` | 删掉 EXIF / ICC / 注释 |

三个动作共有的三件套是 `输入目录`、`通配`、`输出目录`；
另外都支持 `连子目录一起处理`、`覆盖已存在的输出`、`只预演`。

**这是个会写文件的包**，所以 `danger = caution`。默认**不会覆盖**已存在的输出：
遇到同名文件会打印「跳过」，想覆盖得显式打开 `覆盖已存在的输出`。
拿不准先打开 `只预演`，它会把你将执行的 magick 命令原样打印出来。

## 参数速查

**批量转格式**：输入目录、通配、输出目录、输出格式、有损质量、顺便去元数据、递归、覆盖、预演。

**批量缩放**：输入目录、通配、输出目录、宽度、高度、适配方式、有损质量、
不按 EXIF 转正、递归、覆盖、预演。

适配方式对应 ImageMagick 的几何写法：

| 选项 | 实际传的 | 效果 |
| --- | --- | --- |
| 限制在框内（默认） | `-resize 1600x>` | 等比缩到框内，小图不放大 |
| 填满目标框后居中裁剪 | `-resize 400x400^ -gravity center -extent 400x400` | 结果恰好 400×400 |
| 强行拉伸 | `-resize 1600x900!` | 会改变比例 |

**批量去元数据**：输入目录、通配、输出目录、递归、覆盖、预演。

## 命令行直用

```console
$ image-batch convert --input-dir ~/照片 --glob '*.png' --output-dir ~/照片-jpg --format jpg --quality 88
$ image-batch resize  --input-dir . --glob '*.jpg' --output-dir ./small --width 1600
$ image-batch resize  --input-dir . --glob '*.jpg,*.png' --output-dir ./thumbs --width 400 --height 400 --fit cover
$ image-batch strip   --input-dir . --glob '*.jpg' --output-dir ./clean
$ image-batch resize  --input-dir . --output-dir ./small --dry-run
$ image-batch -h
```

## 输出与退出码

* stdout：写出来的文件路径 + 最后一行的统计。
* stderr：`跳过（已存在…）`、`处理失败 …` 以及参数错误。
* 退出码：0 = 全部成功；1 = 有文件失败或一个都没匹配上；2 = 用法不对。

```console
$ image-batch resize --input-dir . --output-dir ./small --width 1600
批量缩放  .  →  ./small  （宽 1600 高 auto，适配 inside；匹配 3 个文件）
按 EXIF 方向自动转正（--no-auto-orient 可关）

./small/a.jpg
./small/b.jpg

缩放：成功 2 个，跳过 0 个，失败 0 个
```

## 已知边界

* 需要 **ImageMagick 7**（命令是 `magick`）。ImageMagick 6 里叫 `convert`，
  这个脚本不做兼容；缺了会提示 `sudo pacman -S imagemagick`。
* `通配` 匹配的是**文件名**，不支持路径部分（例如 `sub/*.jpg`）；要处理子目录
  就打开 `连子目录一起处理`。
* 输出目录和输入目录请分开。指到同一个目录时，默认的「跳过已存在」会挡住大部分
  覆盖，但一旦打开 `覆盖已存在的输出`，就会就地改文件。

## 依赖

| 命令 | 包 |
| --- | --- |
| `magick` | imagemagick |

不需要 root，不联网。

## 卸载

```console
$ toolbox-hub uninstall image-batch
```
