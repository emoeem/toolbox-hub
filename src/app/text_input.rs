//! 单行文本输入：带光标的那种。
//!
//! 之前三处过滤框（工具列表、软件包中心、文件选择器）都只有 `push` / `pop`：
//! 光标永远在末尾，打错开头只能全删重打，粘贴也只会往尾巴上接。
//!
//! **光标用字符下标，不是字节下标**：包名里出现中文、或者你粘了一段 emoji，
//! 按字节走就会切在字符中间 —— 那是会 panic 的 bug，不是显示错位。所以所有
//! 位置计算都走 `char_indices`，插入/删除都用字符个数。
//!
//! 这一层刻意不碰终端、不碰渲染：它只是「一段文本 + 一个光标」，
//! 谁想画就把 [`TextInput::split_at_cursor`] 的两半接起来画。

/// 一段带光标的单行文本。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInput {
    text: String,
    /// 光标在第几个**字符**之前（`0` = 最前，`len` = 最后）。
    cursor: usize,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// 字符个数（不是字节数）。
    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// 光标位置（第几个字符之前）。
    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 光标两侧的文本，渲染时直接拼（左边 + 光标符号 + 右边）。
    pub fn split_at_cursor(&self) -> (&str, &str) {
        let byte = self.byte_at(self.cursor);
        self.text.split_at(byte)
    }

    /// 整段替换（光标移到末尾）。
    pub fn set(&mut self, text: &str) {
        self.text = text.replace(['\n', '\r', '\t'], " ");
        self.cursor = self.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// 在光标处插入一个字符。
    pub fn insert(&mut self, ch: char) {
        let byte = self.byte_at(self.cursor);
        self.text.insert(byte, ch);
        self.cursor += 1;
    }

    /// 在光标处插入一段文本（粘贴走这里）。
    ///
    /// 换行/制表符折成空格：这是个单行框，粘进来的多行文本不该在里面造成
    /// 「一条命令被拆成两行」的错觉 —— 更不能顺手当回车使。
    pub fn insert_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.insert(if ch == '\n' || ch == '\r' || ch == '\t' {
                ' '
            } else {
                ch
            });
        }
    }

    /// 退格（删光标前一个字符）。
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_at(self.cursor - 1);
        let end = self.byte_at(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    /// 删除光标处那个字符（`Delete` 键）。
    pub fn delete(&mut self) {
        if self.cursor >= self.len() {
            return;
        }
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.len();
    }

    /// 第 `index` 个字符对应的字节位置。
    fn byte_at(&self, index: usize) -> usize {
        self.text
            .char_indices()
            .nth(index)
            .map(|(byte, _)| byte)
            .unwrap_or(self.text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_and_deletes_at_the_cursor() {
        let mut input = TextInput::new();
        input.insert_str("fzf");
        assert_eq!(input.text(), "fzf");
        assert_eq!(input.cursor(), 3, "打完光标在末尾");

        // 回到中间插一个字符（这正是以前做不到的事：打错开头只能全删）
        input.left();
        input.left();
        input.insert('x');
        assert_eq!(input.text(), "fxzf");
        assert_eq!(input.cursor(), 2);

        input.delete();
        assert_eq!(input.text(), "fxf");
        assert_eq!(input.cursor(), 2);

        // 退格删的是光标**前面**那个字符（此时光标在 2，删掉的是 'x'）
        input.backspace();
        assert_eq!(input.text(), "ff");
        assert_eq!(input.cursor(), 1);

        // 边界不能越界或 panic
        input.home();
        input.backspace();
        assert_eq!(input.text(), "ff", "已经在最前面，退格什么也不删");
        input.end();
        input.delete();
        assert_eq!(input.text(), "ff", "已经在最后面，Delete 什么也不删");
        assert_eq!(input.cursor(), 2);
    }

    /// 中文/emoji 是**多字节**：光标必须按字符走，否则会切在字符中间。
    #[test]
    fn the_cursor_counts_characters_not_bytes() {
        let mut input = TextInput::new();
        input.insert_str("中文包");
        assert_eq!(input.len(), 3, "三个字符");
        assert_eq!(input.text().len(), 9, "但九个字节");
        input.left();
        input.insert('好');
        assert_eq!(input.text(), "中文好包");
        input.backspace();
        assert_eq!(input.text(), "中文包");
        assert_eq!(input.cursor(), 2);

        let (before, after) = input.split_at_cursor();
        assert_eq!(before, "中文");
        assert_eq!(after, "包");
    }

    #[test]
    fn navigation_clamps_at_both_ends() {
        let mut input = TextInput::new();
        input.set("abc");
        assert_eq!(input.cursor(), 3, "set 之后停在末尾");
        input.right();
        assert_eq!(input.cursor(), 3, "右到头不动");
        input.home();
        assert_eq!(input.cursor(), 0);
        input.left();
        assert_eq!(input.cursor(), 0, "左到头不动");
        input.end();
        assert_eq!(input.cursor(), 3);
    }

    /// 粘贴：多行文本折成空格，光标跟着走。
    #[test]
    fn paste_stays_on_one_line() {
        let mut input = TextInput::new();
        input.insert_str("fzf\nripgrep\tjq");
        assert_eq!(input.text(), "fzf ripgrep jq");
        assert!(!input.text().contains('\n'), "单行框里不该出现换行");
        assert_eq!(input.cursor(), input.len());

        // 粘在中间也不乱
        input.home();
        input.insert_str("sudo ");
        assert_eq!(input.text(), "sudo fzf ripgrep jq");
    }

    #[test]
    fn set_and_clear_reset_the_cursor() {
        let mut input = TextInput::new();
        input.set("fzf");
        input.set("bash");
        assert_eq!(input.text(), "bash");
        assert_eq!(input.cursor(), 4);
        input.clear();
        assert!(input.is_empty());
        assert_eq!(input.cursor(), 0);
    }
}
