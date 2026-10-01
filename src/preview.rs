//! 终端里的图片预览。
//!
//! 用 [`ratatui_image`] 把图片画进终端。它能用的前提是终端支持某种图形协议
//! （kitty / sixel / iTerm2），否则退化成「半块字符」—— 那也能看出个大概，
//! 但一个像素被拆成两半、颜色也糊。所以这里做两件事：
//!
//! 1. **探测一次**：`Picker::from_query_stdio()` 会问终端一句「你会什么」，
//!    1 秒内没回答就退化成半块。探测**懒执行**（第一次真的要预览时才做），
//!    免得为了一个可能用不上的功能让启动慢半秒；
//! 2. **解码放后台**：一张 1200 万像素的 JPEG 解码要一两百毫秒，在界面线程里
//!    做就是「按一下 ↓ 卡一下」。所以选中项一变就丢给线程解，界面先显示
//!    「解码中…」。
//!
//! 解码结果只缓存**一张**（就是当前这张）：往上往下翻的时候不重复解，
//! 翻过去就不再占内存 —— 预览这东西，缓存一整屏图片反而是负担。

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
};

use image::DynamicImage;
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
};

/// 认得出来的图片后缀（不认识的就不去解码，免得对着一个 40MB 的二进制乱试）。
const IMAGE_SUFFIXES: [&str; 8] = ["png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "avif"];

/// 这个文件看起来是图片吗（只看后缀）。
pub fn looks_like_image(path: &Path) -> bool {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .is_some_and(|extension| IMAGE_SUFFIXES.contains(&extension.as_str()))
}

/// 预览状态。
pub struct Preview {
    /// 探测出来的画图工具；`None` = 还没探测过。
    picker: Option<Picker>,
    /// 探测过没有（探测失败也记着，别每次都问一遍终端）。
    detected: bool,
    /// 探测结果的说明（状态行显示：kitty / sixel / 半块 / 不支持）。
    pub protocol: String,
    /// 当前这张图的协议对象（拿它渲染）。
    state: Option<StatefulProtocol>,
    /// 已经解码好的是哪个文件。
    shown: Option<PathBuf>,
    /// 正在解码哪个文件。
    pending: Option<PathBuf>,
    /// 解码线程的入口。
    rx: Option<Receiver<(PathBuf, Result<DynamicImage, String>)>>,
    /// 最近一次失败的原因（显示用）。
    pub problem: Option<String>,
}

impl Default for Preview {
    fn default() -> Self {
        Self::new()
    }
}

impl Preview {
    pub fn new() -> Self {
        Self {
            picker: None,
            detected: false,
            // 标题上先写「图片预览」：协议要真去问终端才知道，
            // 而问一次要等到第一次真要画图时才做（见 detect 的注释）。
            protocol: String::from("图片预览"),
            state: None,
            shown: None,
            pending: None,
            rx: None,
            problem: None,
        }
    }

    /// 测试用：跳过终端探测，直接用半块字符。
    ///
    /// 真去探测会往终端写查询序列并等 1 秒回答 —— 在测试进程里那是灾难
    /// （既没有终端回答，又会污染输出）。
    #[cfg(test)]
    pub fn halfblocks_for_test() -> Self {
        let mut preview = Self::new();
        preview.detected = true;
        preview.protocol = String::from("半块字符（测试）");
        preview.picker = Some(Picker::halfblocks());
        preview
    }

    /// 探测一次终端能力（懒执行，只做一次）。
    ///
    /// 这一步会往终端写一个查询序列并等回答（最多 1 秒）。所以它必须发生在
    /// **已经进了备用屏幕、开了 raw 模式**之后：不然终端的回答会显示在屏幕上，
    /// 而没进 raw 模式时回车也会被回显。
    pub fn detect(&mut self) {
        if self.detected {
            return;
        }
        self.detected = true;

        match Picker::from_query_stdio() {
            Ok(picker) => {
                self.protocol = match picker.protocol_type() {
                    ProtocolType::Kitty => String::from("kitty 图形协议"),
                    ProtocolType::Sixel => String::from("sixel"),
                    ProtocolType::Iterm2 => String::from("iTerm2 图形协议"),
                    ProtocolType::Halfblocks => String::from("半块字符（终端不支持图形协议）"),
                };
                self.picker = Some(picker);
            }
            Err(error) => {
                // 探测失败不该让预览整个没了：退回半块，至少能看个形
                let picker = Picker::halfblocks();
                self.protocol = format!("半块字符（探测失败：{error}）");
                self.picker = Some(picker);
            }
        }
    }

    /// 当前协议名（界面显示用）。
    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    /// 这一屏要不要预览（够大才画，太小画了也看不清）。
    pub fn worth_showing(area_width: u16, area_height: u16) -> bool {
        area_width >= 28 && area_height >= 8
    }

    /// 选中项变了就叫一次：该解码的去后台解，已经有的直接复用。
    pub fn request(&mut self, path: &Path) {
        if self.shown.as_deref() == Some(path) || self.pending.as_deref() == Some(path) {
            return;
        }
        if !looks_like_image(path) {
            // 不是图片：把上一张清掉，免得看着像「这个视频长这样」
            self.state = None;
            self.shown = None;
            self.pending = None;
            self.problem = None;
            return;
        }

        self.detect();
        self.pending = Some(path.to_path_buf());
        self.problem = None;

        let owned = path.to_path_buf();
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name(String::from("preview-decode"))
            .spawn(move || {
                let _ = tx.send((owned.clone(), decode(&owned)));
            });
        match spawned {
            Ok(_) => self.rx = Some(rx),
            Err(error) => {
                self.pending = None;
                self.problem = Some(format!("开不了解码线程：{error}"));
            }
        }
    }

    /// 每帧收一次解码结果。返回 `true` 表示有变化（主循环据此重画）。
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        match rx.try_recv() {
            Ok((path, result)) => {
                self.rx = None;
                self.pending = None;
                match result {
                    Ok(image) => {
                        if let Some(picker) = self.picker.as_ref() {
                            self.state = Some(picker.new_resize_protocol(image));
                            self.shown = Some(path);
                        }
                    }
                    Err(error) => {
                        self.problem = Some(error);
                        self.state = None;
                        self.shown = None;
                    }
                }
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                self.pending = None;
                true
            }
        }
    }

    /// 现在有没有图可以画（渲染层用 `protocol_state`，这个是给测试与状态判断的）。
    #[cfg(test)]
    pub fn has_image(&self) -> bool {
        self.state.is_some()
    }

    pub fn is_decoding(&self) -> bool {
        self.pending.is_some()
    }

    /// 拿给渲染层用（`StatefulImage` 要可变借用）。
    pub fn protocol_state(&mut self) -> Option<&mut StatefulProtocol> {
        self.state.as_mut()
    }

    /// 关掉预览时把图放掉（一张解码后的位图不小）。
    pub fn clear(&mut self) {
        self.state = None;
        self.shown = None;
        self.pending = None;
        self.problem = None;
        self.rx = None;
    }
}

/// 读盘 + 解码（在后台线程里跑）。
fn decode(path: &Path) -> Result<DynamicImage, String> {
    let reader = image::ImageReader::open(path)
        .map_err(|error| format!("打不开：{error}"))?
        .with_guessed_format()
        .map_err(|error| format!("认不出格式：{error}"))?;
    reader
        .decode()
        .map_err(|error| format!("解码失败：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_suffixes_are_recognised_case_insensitively() {
        assert!(looks_like_image(Path::new("/tmp/a.png")));
        assert!(looks_like_image(Path::new("/tmp/a.JPG")));
        assert!(looks_like_image(Path::new("/tmp/a.jpeg")));
        assert!(!looks_like_image(Path::new("/tmp/a.mp4")));
        assert!(!looks_like_image(Path::new("/tmp/a")));
        assert!(!looks_like_image(Path::new("/tmp/a.png.txt")));
    }

    #[test]
    fn small_areas_are_not_worth_previewing() {
        assert!(!Preview::worth_showing(20, 20), "太窄：图会糊成一列");
        assert!(!Preview::worth_showing(40, 5), "太矮：只能看到一条");
        assert!(Preview::worth_showing(40, 12));
    }

    /// 不是图片就把上一张放掉 —— 否则「选中一个 mp4，屏幕上还留着刚才那张图」，
    /// 看着就像那个视频长这样。
    #[test]
    fn non_images_clear_the_previous_image() {
        let mut preview = Preview::new();
        // 造一个假的「已经有图」状态：直接塞一个半块协议进去
        let picker = Picker::halfblocks();
        let image = DynamicImage::new_rgb8(4, 4);
        preview.picker = Some(picker);
        preview.state = Some(
            preview
                .picker
                .as_ref()
                .expect("刚塞的")
                .new_resize_protocol(image),
        );
        preview.shown = Some(PathBuf::from("/tmp/a.png"));
        assert!(preview.has_image());

        preview.request(Path::new("/tmp/b.mp4"));
        assert!(!preview.has_image(), "换成非图片之后不该还留着上一张");
        assert!(preview.shown.is_none());
    }

    /// 真解码一张真图（自己造一张，不依赖外部素材）。
    #[test]
    fn decoding_works_and_reports_bad_files() {
        let dir = std::env::temp_dir().join(format!("toolbox-hub-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建目录");

        let good = dir.join("good.png");
        let image = DynamicImage::new_rgb8(8, 8);
        image.save(&good).expect("写测试图");
        let decoded = decode(&good).expect("该能解码");
        assert_eq!(decoded.width(), 8);

        let bad = dir.join("bad.png");
        std::fs::write(&bad, b"this is not a png").expect("写坏文件");
        assert!(decode(&bad).is_err(), "坏文件要报错而不是 panic");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
