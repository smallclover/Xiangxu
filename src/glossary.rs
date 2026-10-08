//! 术语表：宝可梦等游戏专有名词的「英文→中文」映射，
//! 在送翻译模型前对原文做**词边界匹配 + 预替换**，并生成 prompt 提示，
//! 让 1.5B 小模型也能输出与官方译名一致的专有名词。
//!
//! 数据文件（`assets/glossary/`，每行 `英文\t中文`，`#` 开头为注释）：
//!   - pokemon.tsv / moves.tsv / abilities.tsv / items.tsv：由 `tools/fetch_glossary.py`
//!     从 PokeAPI 生成（全国图鉴/技能/特性/道具，全量，勿手工编辑）。
//!   - locations.tsv：丰缘地区地点（绿宝石），来源 52poke wiki，人工校对。
//!   - custom.tsv：用户自定义补充，**优先级最高**（同名英文覆盖其他表），重启生效。
//!
//! 运行时匹配策略：
//!   - 只把**原文中实际命中的**术语注入 prompt，术语表大小不影响运行开销；
//!   - 最长匹配优先（`Lilycove Department Store` 先于短词）；
//!   - 词边界：命中位置前后不能是 ASCII 字母/数字（`Mew` 不会误命中 `Mewtwo`，
//!     `Treecko's` 里的 `Treecko` 可以命中）；
//!   - 匹配在 ASCII 小写化后的字节串上进行（长度不变，位置与原文一一对应）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// 术语表：英文小写 key → 中文译名。
pub struct Glossary {
    /// 按 key 长度降序排列，保证最长匹配优先。
    sorted: Vec<(String, String)>,
}

impl Glossary {
    /// 从 glossary 目录加载全部 TSV；目录不存在时得到空表（功能静默关闭，不影响主流程）。
    fn load() -> Self {
        let dir = glossary_dir();
        let mut map: HashMap<String, String> = HashMap::new();
        let mut insert_file = |path: PathBuf| {
            let Ok(content) = std::fs::read_to_string(&path) else {
                return;
            };
            // 去掉 UTF-8 BOM（Windows 下常见），否则首行注释判定失效。
            let content = content.trim_start_matches('\u{feff}');
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let Some((en, zh)) = line.split_once('\t') else {
                    continue;
                };
                let zh = zh.trim_end_matches("\t[HANT]").trim();
                let en = normalize_key(en.trim());
                let zh = zh.trim().to_string();
                if en.is_empty() || zh.is_empty() {
                    continue;
                }
                // 同一术语注册多种拼写变体（后续同 key 不覆盖，先注册者胜）：
                //   '-'↔' '  : PokeAPI 用连字符（poke-ball），游戏文本用空格（Poké Ball）；
                //   é↔e      : 显示形式带变音符（Pokémon），OCR 常输出纯 ASCII（Pokemon）；
                //   poke↔poké: 同上，针对词级替换。
                let base = en.to_lowercase();
                let spaced = base.replace('-', " ");
                for k in [&base, &spaced] {
                    register(&mut map, k.clone(), &zh);
                    register(&mut map, k.replace('é', "e"), &zh);
                    register(&mut map, poke_acute(k), &zh);
                }
            }
        };
        // 顺序：先基础表，custom.tsv 最后加载（同 key 覆盖前面的，优先级最高）。
        for name in [
            "pokemon.tsv",
            "moves.tsv",
            "abilities.tsv",
            "items.tsv",
            "locations.tsv",
        ] {
            insert_file(dir.join(name));
        }
        insert_file(dir.join("custom.tsv"));

        let mut sorted: Vec<(String, String)> = map.into_iter().collect();
        sorted.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
        Glossary { sorted }
    }

    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    /// 对原文做术语匹配与预替换。
    /// 返回 `(替换后的文本, 命中术语列表(英文原文, 中文))`；命中列表按出现顺序、已去重。
    pub fn apply(&self, text: &str) -> (String, Vec<(String, String)>) {
        if self.sorted.is_empty() || text.is_empty() {
            return (text.to_string(), Vec::new());
        }
        // ASCII 小写化：字节长度不变，索引可与原文一一对应。
        let lower: Vec<u8> = text.bytes().map(|b| b.to_ascii_lowercase()).collect();
        let is_word = |b: u8| b.is_ascii_alphanumeric();
        // 命中区间（原文字节位置）与对应中文。
        let mut hits: Vec<(usize, usize, &str)> = Vec::new();
        // 占用掩码：同一处只被最长术语占用一次。
        let mut occupied = vec![false; text.len()];

        for (key, zh) in &self.sorted {
            let k = key.as_bytes();
            let mut pos = 0usize;
            while let Some(found) = find_sub(&lower[pos..], k) {
                let start = pos + found;
                let end = start + k.len();
                let before_ok = start == 0 || !is_word(lower[start - 1]);
                let after_ok = end == lower.len() || !is_word(lower[end]);
                let overlap = occupied[start..end].iter().any(|&b| b);
                if before_ok && after_ok && !overlap {
                    for o in &mut occupied[start..end] {
                        *o = true;
                    }
                    hits.push((start, end, zh));
                }
                pos = if end > start { end } else { start + 1 };
                if pos >= lower.len() {
                    break;
                }
            }
        }

        if hits.is_empty() {
            return (text.to_string(), Vec::new());
        }

        // 按出现位置重建字符串，并生成去重后的命中术语列表（大小写不敏感去重）。
        hits.sort_by_key(|(s, _, _)| *s);
        let mut out = String::with_capacity(text.len() + 32);
        let mut cursor = 0usize;
        let mut applied: Vec<(String, String)> = Vec::new();
        for (start, end, zh) in &hits {
            out.push_str(&text[cursor..*start]);
            out.push_str(zh);
            cursor = *end;
            let en = text[*start..*end].to_string();
            let key = en.to_lowercase();
            if !applied.iter().any(|(e, _)| e.to_lowercase() == key) {
                applied.push((en, zh.to_string()));
            }
        }
        out.push_str(&text[cursor..]);
        (out, applied)
    }
}

/// 全局术语表（惰性加载，进程内只读一份）。
pub fn global() -> &'static Glossary {
    static G: OnceLock<Glossary> = OnceLock::new();
    G.get_or_init(Glossary::load)
}

/// 命中术语 → prompt 提示片段（英文，告诉模型已替换的名词要原样保留）。
pub fn term_hint(applied: &[(String, String)]) -> Option<String> {
    if applied.is_empty() {
        return None;
    }
    let pairs: Vec<String> = applied
        .iter()
        .map(|(en, zh)| format!("\"{en}\" -> \"{zh}\""))
        .collect();
    Some(format!(
        "The following proper nouns have already been translated into Chinese in the text. \
         Keep them EXACTLY as they appear (do not re-translate, do not transliterate): {}",
        pairs.join("; ")
    ))
}

/// 字节子串搜索（等价 `haystack.iter().position(window == needle)`，避免整串 utf8 转换）。
fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// 归一化英文 key：
/// PokeAPI 对 Z 招式的物理/特殊变体使用 `breakneck-blitz--physical` 这类内部名，
/// 去掉后缀后变成基础名 `breakneck-blitz`，游戏文本中的写法即可直接命中
/// （两个变体的中文译名相同，先注册者胜，无冲突）。
fn normalize_key(en: &str) -> &str {
    en.strip_suffix("--physical")
        .or_else(|| en.strip_suffix("--special"))
        .unwrap_or(en)
}

/// 注册一个术语变体；已存在同 key 时不覆盖（保序：先注册者优先，custom.tsv 最后加载即最高优先）。
fn register(map: &mut HashMap<String, String>, key: String, zh: &str) {
    if key.is_empty() || map.contains_key(&key) {
        return;
    }
    map.insert(key, zh.to_string());
}

/// 把 key 里的独立单词 "poke" 替换为 "poké"（官方显示形式带变音符）。
fn poke_acute(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 1);
    let mut chars = key.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if key[i..].starts_with("poke") {
            let before_ok = i == 0 || !key[..i].ends_with(|p: char| p.is_ascii_alphanumeric());
            let after = &key[i + 4..];
            let after_ok =
                after.is_empty() || !after.starts_with(|p: char| p.is_ascii_alphanumeric());
            if before_ok && after_ok {
                out.push_str("poké");
                // 跳过 "poke" 的其余 3 个字符。
                for _ in 0..3 {
                    chars.next();
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// glossary 目录定位：exe 同目录 → 工作目录 → 编译时仓库根。
/// （与 translate.rs 的模型路径解析同一策略：便携包与开发环境都能找到。）
fn glossary_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && dir.join("assets/glossary").is_dir()
    {
        return dir.join("assets/glossary");
    }
    if PathBuf::from("assets/glossary").is_dir() {
        return PathBuf::from("assets/glossary");
    }
    if let Some(dir) = option_env!("CARGO_MANIFEST_DIR") {
        return PathBuf::from(dir).join("assets/glossary");
    }
    PathBuf::from("assets/glossary")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_glossary() -> Glossary {
        Glossary {
            sorted: vec![
                ("lilycove department store".into(), "水静百货".into()),
                ("route 101".into(), "101号道路".into()),
                ("treecko".into(), "木守宫".into()),
                ("mewtwo".into(), "超梦".into()),
                ("mew".into(), "梦幻".into()),
            ],
        }
    }

    #[test]
    fn word_boundary_prevents_partial_match() {
        let g = test_glossary();
        // Mew 不能误命中 Mewtwo 内部。
        let (t, hits) = g.apply("Mewtwo appeared!");
        assert_eq!(t, "超梦 appeared!");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "Mewtwo");
    }

    #[test]
    fn possessive_and_plural() {
        let g = test_glossary();
        // 所有格 's 应命中。
        let (t, hits) = g.apply("Treecko's Pound");
        assert!(t.contains("木守宫"));
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn longest_match_first() {
        let g = test_glossary();
        let (t, _hits) = g.apply("Go to Lilycove Department Store.");
        assert!(t.contains("水静百货"));
        assert!(!t.to_lowercase().contains("lilycove"));
    }

    #[test]
    fn case_insensitive_and_position() {
        let g = test_glossary();
        let (t, hits) = g.apply("Walk along ROUTE 101 to meet Treecko.");
        assert_eq!(t, "Walk along 101号道路 to meet 木守宫.");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn no_match_unchanged() {
        let g = test_glossary();
        let (t, hits) = g.apply("Hello, world!");
        assert_eq!(t, "Hello, world!");
        assert!(hits.is_empty());
    }

    #[test]
    fn repeated_terms_dedup() {
        let g = test_glossary();
        let (t, hits) = g.apply("Treecko and TREECKO");
        assert_eq!(t, "木守宫 and 木守宫");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn z_move_variant_normalized() {
        assert_eq!(
            normalize_key("breakneck-blitz--physical"),
            "breakneck-blitz"
        );
        assert_eq!(normalize_key("breakneck-blitz--special"), "breakneck-blitz");
        assert_eq!(normalize_key("karate-chop"), "karate-chop");
        // 归一化后基础名应能命中游戏文本里的写法（load() 会同时注册连字符/空格变体）。
        let g = Glossary {
            sorted: vec![
                ("breakneck-blitz".into(), "究极无敌大冲撞".into()),
                ("breakneck blitz".into(), "究极无敌大冲撞".into()),
            ],
        };
        let (t, hits) = g.apply("Use Breakneck Blitz now!");
        assert!(t.contains("究极无敌大冲撞"));
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn poke_acute_variants() {
        assert_eq!(poke_acute("poke-ball"), "poké-ball");
        assert_eq!(poke_acute("poke ball"), "poké ball");
        assert_eq!(poke_acute("pokeflute"), "pokeflute"); // 词内不替换
        assert_eq!(poke_acute("pokemon"), "pokemon"); // "poke" 非独立词，不替换
        assert_eq!(poke_acute("great-ball"), "great-ball"); // 无 poke 不变
        assert_eq!(poke_acute("go to the poke mart"), "go to the poké mart");
    }

    #[test]
    fn accent_insensitive_via_variants() {
        // 手工构造含变体的表：模拟 load() 注册的 pokémon/pokemon 双变体。
        let g = Glossary {
            sorted: vec![
                ("pokémon league".into(), "宝可梦联盟".into()),
                ("pokemon league".into(), "宝可梦联盟".into()),
            ],
        };
        let (t1, _) = g.apply("Welcome to Pokémon League!");
        assert!(t1.contains("宝可梦联盟"));
        let (t2, _) = g.apply("Welcome to Pokemon League!");
        assert!(t2.contains("宝可梦联盟"));
    }
}
