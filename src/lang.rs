//! 语言枚举：OCR 源语言与翻译目标语言共用。

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Zh,
    En,
    Ja,
}

impl Lang {
    /// 下拉框展示名。
    pub fn label(self) -> &'static str {
        match self {
            Lang::Zh => "中文",
            Lang::En => "English",
            Lang::Ja => "日本語",
        }
    }

    /// 翻译提示词里使用的自然语言名。
    pub fn target_hint(self) -> &'static str {
        match self {
            Lang::Zh => "Simplified Chinese",
            Lang::En => "English",
            Lang::Ja => "Japanese",
        }
    }
}
