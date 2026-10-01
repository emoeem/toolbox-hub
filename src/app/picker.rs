//! 文件选择器：在**工作目录**里浏览、挑文件，填进表单的路径字段。
//!
//! 设计上跟 fzf 一致：**字母一律进过滤词，导航只用方向键**。
//! 试过「j/k 也能上下移动」的写法，代价是没法筛 `j` 开头的文件 —— 不值得。
//!
//! 性能上有一条硬规矩：**列表构建不 stat**。目录只在换目录时读一次，过滤是纯内存，
//! 体积在显示时才按需读。实测 `/usr/lib`（7543 项）：带 stat 90ms、不带 4ms ——
//! 而打字是每敲一个字重建一次列表的。

use std::{
    fs,
    path::{Path, PathBuf},
};

/// 列表里的一项。
///
/// **故意不带体积**：给每个条目 stat 一次会让大目录卡住。实测 `/usr/lib`
/// （7543 项）带 stat 要 90ms、只读目录只要 4ms —— 而选择器是**每敲一个字**
/// 重建列表的。体积改成显示时按需读（一帧只读可见的那十几行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

impl Entry {
    /// 体积：按需读，读不到就显示 `-`。
    pub fn size_label(&self) -> String {
        if self.is_dir {
            return String::from("目录");
        }
        match fs::metadata(&self.path) {
            Ok(meta) => crate::media::human_size(meta.len()),
            Err(_) => String::from("-"),
        }
    }
}

/// 选择器状态。
pub struct Picker {
    dir: PathBuf,
    /// 整个目录的条目：**换目录时才读一次**。
    all: Vec<Entry>,
    /// 过滤后可见的下标（过滤是纯内存操作，不碰磁盘）。
    visible: Vec<usize>,
    pub selected: usize,
    /// 输入即过滤（空 = 全显示）。
    pub filter: String,
    /// 要填的表单字段下标。
    pub field: usize,
    /// 目标字段是多值吗 —— 决定选中后「追加」还是「替换」。
    pub repeatable: bool,
    /// 目标字段要填目录（决定提示语，不影响键位）。
    pub dir_only: bool,
    /// Tab 标记的文件（多选）。只在本次选择器会话里有效。
    marked: Vec<PathBuf>,
}

impl Picker {
    /// 目标字段的两件事：下标 + 它是不是多值、是不是要填目录。
    pub fn open(dir: &Path, field: usize, repeatable: bool, dir_only: bool) -> Self {
        let mut picker = Self {
            dir: dir.to_path_buf(),
            all: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            filter: String::new(),
            field,
            repeatable,
            dir_only,
            marked: Vec::new(),
        };
        picker.reload();
        picker
    }

    /// Tab：标记/取消标记当前项，并往下走一格（连着标记一串很顺）。
    pub fn toggle_mark(&mut self) {
        let Some(entry) = self.entry(self.selected) else {
            return;
        };
        if entry.is_dir {
            return;
        }

        let path = entry.path.clone();
        match self.marked.iter().position(|item| item == &path) {
            Some(index) => {
                self.marked.remove(index);
            }
            None => self.marked.push(path),
        }
        self.move_selection(1);
    }

    pub fn is_marked(&self, path: &Path) -> bool {
        self.marked.iter().any(|item| item == path)
    }

    pub fn marked_count(&self) -> usize {
        self.marked.len()
    }

    /// 按**列表顺序**取出标记的文件 —— 填进字段的顺序应该和眼睛看到的一致。
    pub fn marked_files(&self) -> Vec<PathBuf> {
        (0..self.len())
            .filter_map(|index| self.entry(index))
            .filter(|entry| !entry.is_dir && self.is_marked(&entry.path))
            .map(|entry| entry.path.clone())
            .collect()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 读一遍目录（换目录时用）。**只读名字和类型**，不 stat：
    /// `file_type()` 在 Linux 上直接来自 `readdir` 的 d_type，是免费的。
    pub fn reload(&mut self) {
        let mut all: Vec<Entry> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|item| {
                let name = item.file_name().to_string_lossy().to_string();
                // 隐藏文件不显示：要处理的媒体文件基本不会是 .xxx
                if name.starts_with('.') {
                    return None;
                }
                let is_dir = item.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
                Some(Entry {
                    name,
                    path: item.path(),
                    is_dir,
                })
            })
            .collect();

        all.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        self.all = all;
        self.apply_filter();
    }

    /// 应用过滤词（**纯内存**，打字走这条路）。
    pub fn apply_filter(&mut self) {
        self.visible = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches(&entry.name, &self.filter))
            .map(|(index, _)| index)
            .collect();

        if self.selected >= self.visible.len() {
            self.selected = self.visible.len().saturating_sub(1);
        }
    }

    /// 可见条目数。
    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    /// 第 `index` 个**可见**条目。
    pub fn entry(&self, index: usize) -> Option<&Entry> {
        self.visible
            .get(index)
            .and_then(|&index| self.all.get(index))
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        self.entry(self.selected)
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.visible.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.visible.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.visible.len().saturating_sub(1);
    }

    /// 上翻一层目录；已经在根目录就返回 `false`。
    pub fn go_to_parent(&mut self) -> bool {
        let Some(parent) = self.dir.parent().map(Path::to_path_buf) else {
            return false;
        };
        self.enter_dir(&parent);
        true
    }

    /// 进入某个目录（清掉过滤词）。
    pub fn enter_dir(&mut self, path: &Path) {
        self.dir = path.to_path_buf();
        self.filter.clear();
        self.selected = 0;
        self.reload();
    }

    /// 打字：只重算过滤，**不碰磁盘**。
    pub fn push_char(&mut self, ch: char) {
        self.filter.push(ch);
        self.selected = 0;
        self.apply_filter();
    }

    /// 退格：过滤词非空就删一个字，否则上翻一层目录。
    pub fn backspace(&mut self) {
        if self.filter.pop().is_some() {
            self.selected = 0;
            self.apply_filter();
        } else {
            self.go_to_parent();
        }
    }
}

/// 不区分大小写的子串匹配。
fn matches(name: &str, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    name.to_lowercase().contains(&filter.to_lowercase())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::Picker;

    /// 造一个临时目录：两个子目录、三个文件、一个隐藏文件。
    fn temp_tree(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("toolbox-hub-picker-{tag}-{nanos}"));
        let nested = dir.join("sub");
        fs::create_dir_all(&nested).expect("mkdir");
        fs::write(dir.join("clip.mp4"), vec![0u8; 2048]).expect("write");
        fs::write(dir.join("music.m4a"), vec![0u8; 1024]).expect("write");
        fs::write(dir.join("notes.txt"), "x").expect("write");
        fs::write(dir.join(".hidden"), "x").expect("write");
        fs::write(nested.join("deep.mp4"), "x").expect("write");
        dir
    }

    #[test]
    fn lists_directories_first_and_hides_dotfiles() {
        let dir = temp_tree("list");
        let picker = Picker::open(&dir, 0, false, false);

        let names: Vec<&str> = (0..picker.len())
            .filter_map(|index| picker.entry(index))
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["sub", "clip.mp4", "music.m4a", "notes.txt"]);
        assert!(picker.entry(0).expect("第一项").is_dir, "目录要排在前面");
        assert!(!names.contains(&".hidden"), "隐藏文件不该出现: {names:?}");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_filter_narrows_the_list_and_ignores_case() {
        let dir = temp_tree("filter");
        let mut picker = Picker::open(&dir, 0, false, false);

        for ch in "MP4".chars() {
            picker.push_char(ch);
        }
        let names: Vec<&str> = (0..picker.len())
            .filter_map(|index| picker.entry(index))
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["clip.mp4"], "只留 mp4：{names:?}");

        picker.push_char('z');
        assert!(picker.is_empty(), "筛不到就是空的");

        // 删掉过滤词，列表恢复（注意别删多了 —— 删空之后再退格就上翻目录了）
        for _ in 0.."MP4z".len() {
            picker.backspace();
        }
        assert_eq!(picker.filter, "");
        assert_eq!(picker.len(), 4);
        assert_eq!(picker.dir(), dir.as_path(), "删过滤词不该换目录");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn selecting_wraps_around_and_navigation_enters_and_leaves_directories() {
        let dir = temp_tree("nav");
        let mut picker = Picker::open(&dir, 2, true, false);
        assert_eq!(picker.field, 2);
        assert!(picker.repeatable);

        picker.move_selection(-1);
        assert_eq!(picker.selected, 3, "往上越过第一项要绕回最后一项");
        picker.select_first();
        assert_eq!(picker.selected, 0);
        picker.select_last();
        assert_eq!(picker.selected, 3);

        // 进入第一个目录
        let sub = picker.entry(0).expect("第一项").path.clone();
        assert!(picker.entry(0).expect("第一项").is_dir);
        picker.enter_dir(&sub);
        assert_eq!(picker.dir(), sub.as_path());
        let names: Vec<&str> = (0..picker.len())
            .filter_map(|index| picker.entry(index))
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["deep.mp4"]);

        // 退回上一层
        assert!(picker.go_to_parent());
        assert_eq!(picker.dir(), dir.as_path());

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn backspace_on_an_empty_filter_goes_up_one_level() {
        let dir = temp_tree("back");
        let sub = dir.join("sub");
        let mut picker = Picker::open(&sub, 0, false, false);

        // 过滤词为空：退格 = 上翻一层
        picker.backspace();
        assert_eq!(picker.dir(), dir.as_path());
        assert_eq!(picker.filter, "", "上翻不等于往过滤词里写字");

        // 有过滤词：退格 = 删一个字，**不换目录**
        let here = picker.dir().to_path_buf();
        picker.push_char('m');
        picker.backspace();
        assert_eq!(picker.filter, "");
        assert_eq!(picker.dir(), here.as_path(), "删过滤词时不该动目录");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn tab_marks_files_in_list_order_and_ignores_directories() {
        let dir = temp_tree("mark");
        let mut picker = Picker::open(&dir, 0, true, false);

        // 第一项是目录：标记它没有意义
        assert!(picker.entry(0).expect("第一项").is_dir);
        picker.toggle_mark();
        assert_eq!(picker.marked_count(), 0, "目录不该被标记");

        // 移到文件区，连标两个（toggle_mark 标完会往下走一格）
        picker.move_selection(1);
        picker.toggle_mark();
        picker.toggle_mark();
        assert_eq!(picker.marked_count(), 2);

        let marked_files = picker.marked_files();
        let marked: Vec<&str> = marked_files
            .iter()
            .filter_map(|path| path.file_name()?.to_str())
            .collect();
        assert_eq!(
            marked,
            vec!["clip.mp4", "music.m4a"],
            "按列表顺序，不是标记顺序"
        );

        // 再标一个，然后取消它
        picker.toggle_mark();
        assert_eq!(picker.marked_count(), 3);
        picker.move_selection(-1);
        picker.toggle_mark();
        assert_eq!(picker.marked_count(), 2, "再按一次是取消标记");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn missing_directory_just_shows_nothing() {
        let picker = Picker::open(&PathBuf::from("/nonexistent/picker"), 0, false, false);
        assert!(picker.is_empty());
        assert!(picker.selected_entry().is_none());
    }
}
