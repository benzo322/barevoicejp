//! テキスト整形と辞書ファイル。
//!
//! 旧 Python 実装の `TextFormatter` / `load_words` /
//! `load_external_replacements` を移植。タグ除去、空白正規化、
//! replacements.json の完全一致置換、words.txt のファジー置換、
//! 英日境界のスペース補正を同じ順序で適用する。

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::config::{
    FUZZY_LENGTH_SLACK, FUZZY_LENIENT_MIN_CHARS, FUZZY_LENIENT_THRESHOLD, FUZZY_MIN_CHARS,
};

/// 辞書ファイルの置き場所 (カレントディレクトリ基準)。
pub(crate) const REPLACEMENTS_PATH: &str = "replacements.json";
pub(crate) const HOTWORDS_PATH: &str = "hotwords.txt";
pub(crate) const WORDS_PATH: &str = "words.txt";
const HOTWORDS_TEMPLATE: &str = "GitHub Actions\nDocker\nAPI\n";
const WORDS_TEMPLATE: &str = "\
# 正式表記 : 代表読み
# 読みが4文字以上かつ正式表記が4文字以上の項目だけファジーマッチ対象
GitHub Actions : ギットハブアクションズ
GitHub : ギットハブ
Docker : ドッカー
Kubernetes : クバネティス
API : エーピーアイ
PR : ピーアール
DB : ディービー
";

/// 完全一致キーを境界判定する最小長。
///
/// これ未満 (2〜3 文字) の短い読みは、同種の仮名が連続する語の一部に
/// だけ現れる場合に一致させない (例: 「モニター」の「ター」)。助詞が
/// 続く場合は語の切れ目として扱う (例: 「ろぐを」「すどで」)。
/// 4 文字以上は語として十分に特定的なので、複合語
/// (「ドッカーコンテナ」) を壊さないよう部分一致を許す。
const EXACT_BOUNDARY_MIN_CHARS: usize = FUZZY_MIN_CHARS;

const PARTICLES: [char; 9] = ['で', 'に', 'を', 'は', 'が', 'の', 'と', 'へ', 'も'];

/// words.txt の 1 行分。
///
/// コロン無しの英字は `reading = None` (完全一致のみ)、
/// 日本語は語自体を読みとして扱う。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WordEntry {
    target: String,
    reading: Option<String>,
}

impl WordEntry {
    pub(crate) fn target(&self) -> &str {
        &self.target
    }

    pub(crate) fn reading(&self) -> Option<&str> {
        self.reading.as_deref()
    }
}

/// 日本語 (かな・漢字・々) かどうか。英日境界の空白補正に使う。
fn is_ja_char(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30ff}'
        | '\u{3400}'..='\u{4dbf}'
        | '\u{4e00}'..='\u{9fff}'
        | '\u{3005}'
        | '\u{3006}')
}

fn is_ascii_alpha(c: char) -> bool {
    c.is_ascii_alphabetic()
}

/// ひらがな・カタカナ・長音。ファジー一致のコア判定に使う。
fn kana_script(c: char) -> Option<u8> {
    match c {
        '\u{3041}'..='\u{3096}' => Some(1),
        '\u{30a1}'..='\u{30fa}' | '\u{30fc}' | '\u{31f0}'..='\u{31ff}' => Some(2),
        _ => None,
    }
}

/// コアが仮名だけで構成され、外側と文字種が連続していないか。
///
/// 漢字や英字を巻き込んだ一致 (例: 「ドッカーで起」) や、
/// 長いカタカナ語の途中だけに一致する (例: 「プロジェクト」の「ジェクト」)
/// ことを防ぐ。
fn is_fuzzy_core_boundary(text: &[char], start: usize, end: usize) -> bool {
    if start >= end || !text[start..end].iter().all(|&c| kana_script(c).is_some()) {
        return false;
    }
    let first_script = kana_script(text[start]);
    let last_script = kana_script(text[end - 1]);
    if start > 0 {
        let left_script = kana_script(text[start - 1]);
        if left_script.is_some() && left_script == first_script {
            return false;
        }
    }
    if end < text.len() {
        let right_script = kana_script(text[end]);
        if right_script.is_some() && right_script == last_script {
            return false;
        }
    }
    true
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// UTF-8 (BOM 許容) でファイルを読む。読めなければ `None`。
fn read_utf8_bom(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let text = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => String::from_utf8_lossy(rest).into_owned(),
        None => String::from_utf8_lossy(&bytes).into_owned(),
    };
    Some(text)
}

/// ファイルの更新時刻。存在しない・取得できない場合は `None`。
pub(crate) fn file_mtime_ns(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

// ---------------------------------------------------------------------------
// 辞書ファイル
// ---------------------------------------------------------------------------
/// hotwords.txt が無ければサンプルを生成する。
pub(crate) fn ensure_hotwords_file(path: &Path) {
    if path.exists() {
        return;
    }
    match fs::write(path, HOTWORDS_TEMPLATE) {
        Ok(()) => crate::info!("ホットワードテンプレートを生成しました: {}", path.display()),
        Err(exc) => crate::warn!(
            "ホットワードテンプレートを生成できません: {} ({exc})",
            path.display()
        ),
    }
}

/// words.txt が無ければサンプルを生成する。
pub(crate) fn ensure_words_file(path: &Path) {
    if path.exists() {
        return;
    }
    match fs::write(path, WORDS_TEMPLATE) {
        Ok(()) => crate::info!("単語辞書テンプレートを生成しました: {}", path.display()),
        Err(exc) => crate::warn!(
            "単語辞書テンプレートを生成できません: {} ({exc})",
            path.display()
        ),
    }
}

/// hotwords.txt の空行を除いた行数。
pub(crate) fn count_hotwords(path: &Path) -> usize {
    let Some(text) = read_utf8_bom(path) else {
        return 0;
    };
    text.lines().filter(|line| !line.trim().is_empty()).count()
}

/// words.txt の 1 行を `(正式表記, 読み)` として解釈する。
fn parse_word_line(line: &str) -> Option<(String, Option<String>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (target, reading) = if let Some((target, reading)) = line.split_once(':') {
        (target.trim(), Some(reading.trim().to_string()))
    } else if let Some((target, reading)) = line.split_once('：') {
        (target.trim(), Some(reading.trim().to_string()))
    } else {
        (line, None)
    };
    if target.is_empty() {
        return None;
    }
    let reading = match reading {
        Some(reading) if reading.is_empty() => return None,
        Some(reading) => Some(reading),
        None if is_ascii_word(target) => None,
        None => Some(target.to_string()),
    };
    Some((target.to_string(), reading))
}

/// 設定画面から単語を 1 件追加する。
pub(crate) fn add_word(path: &Path, target: &str, reading: &str) -> Result<(), String> {
    let target = target.trim();
    let reading = reading.trim();
    if target.is_empty() || reading.is_empty() {
        return Err("正式表記と読みの両方を入力してください".to_string());
    }
    if [target, reading]
        .iter()
        .any(|value| value.contains(['\n', '\r', ':', '：']))
    {
        return Err("正式表記・読みに改行やコロンは使えません".to_string());
    }
    let existing = read_utf8_bom(path).unwrap_or_default();
    let key = (target.to_string(), Some(reading.to_string()));
    if existing
        .lines()
        .filter_map(parse_word_line)
        .any(|entry| entry == key)
    {
        return Err("同じ単語が既に登録されています".to_string());
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("{target} : {reading}\n"));
    fs::write(path, text).map_err(|exc| format!("words.txt を更新できません: {exc}"))
}

/// 設定画面から単語を 1 件削除する。コメントと他の行はそのまま残す。
pub(crate) fn remove_word(
    path: &Path,
    target: &str,
    reading: Option<&str>,
) -> Result<usize, String> {
    let Some(text) = read_utf8_bom(path) else {
        return Ok(0);
    };
    let mut removed = 0usize;
    let mut kept = String::with_capacity(text.len());
    for line in text.lines() {
        let matches = parse_word_line(line)
            .map(|(entry_target, entry_reading)| {
                entry_target == target && entry_reading.as_deref() == reading
            })
            .unwrap_or(false);
        if matches {
            removed += 1;
        } else {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    if removed > 0 {
        fs::write(path, kept).map_err(|exc| format!("words.txt を更新できません: {exc}"))?;
    }
    Ok(removed)
}

/// `replacements.json` を読み込む。存在しなければ空のテンプレートを生成する。
pub(crate) fn load_external_replacements(path: &Path) -> Vec<(String, String)> {
    if !path.exists() {
        if let Err(exc) = fs::write(path, "{}\n") {
            crate::warn!(
                "置換辞書テンプレートを生成できません: {} ({exc})",
                path.display()
            );
        } else {
            crate::info!("置換辞書テンプレートを生成しました: {}", path.display());
        }
        return Vec::new();
    }

    let Some(text) = read_utf8_bom(path) else {
        crate::warn!("置換辞書を読み込めません: {}", path.display());
        return Vec::new();
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(exc) => {
            crate::warn!("置換辞書を読み込めません ({}): {exc}", path.display());
            return Vec::new();
        }
    };
    let Some(map) = value.as_object() else {
        crate::warn!(
            "置換辞書は JSON オブジェクトである必要があります: {}",
            path.display()
        );
        return Vec::new();
    };

    let mut result = Vec::with_capacity(map.len());
    for (key, value) in map {
        match value.as_str() {
            Some(value) if !key.is_empty() => {
                result.push((key.clone(), value.to_string()));
            }
            _ => crate::warn!("置換辞書の不正な項目を無視します: {key:?} -> {value:?}"),
        }
    }
    result
}

fn is_ascii_word(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '+' | '/' | '-'))
}

/// `words.txt` を解析する。存在しなければ空リスト。
///
/// 形式は `正式表記 : 代表読み`。コロン無しの英字は完全一致のみ、
/// 日本語は語自体を読みとして扱う。重複行は無視する。
pub(crate) fn load_words(path: &Path) -> Vec<WordEntry> {
    let Some(text) = read_utf8_bom(path) else {
        return Vec::new();
    };

    let mut entries: Vec<WordEntry> = Vec::new();
    let mut seen: HashSet<(String, Option<String>)> = HashSet::new();
    for (line_no, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (target, reading) = if let Some((target, reading)) = line.split_once(':') {
            (target.trim(), Some(reading.trim().to_string()))
        } else if let Some((target, reading)) = line.split_once('：') {
            (target.trim(), Some(reading.trim().to_string()))
        } else {
            (line, None)
        };

        let reading = match reading {
            Some(reading) if target.is_empty() || reading.is_empty() => {
                crate::warn!(
                    "{}:{}: 正式表記または読みが空のため無視します",
                    path.display(),
                    line_no + 1
                );
                continue;
            }
            Some(reading) => Some(reading),
            None => {
                if is_ascii_word(target) {
                    // コロン無しの英字は完全一致のみ
                    None
                } else {
                    // 読みが無い日本語は語自体を読みとして扱う
                    Some(target.to_string())
                }
            }
        };

        if target.is_empty() {
            continue;
        }
        let key = (target.to_string(), reading.clone());
        if !seen.insert(key.clone()) {
            continue;
        }
        entries.push(WordEntry {
            target: key.0,
            reading: key.1,
        });
    }
    entries
}

// ---------------------------------------------------------------------------
// 整形
// ---------------------------------------------------------------------------
/// 認識結果の整形器。
///
/// タグ除去 → 空白正規化 → 完全一致置換 → ファジー置換 →
/// 英日境界補正、の順に適用する。完全一致を先に適用することで、
/// 登録済みの読みが別語のファジー一致に奪われない。
pub(crate) struct TextFormatter {
    remove_tags: bool,
    remove_ja_spaces: bool,
    fuzzy: bool,
    fuzzy_threshold: f64,
    /// 長いキー順 (安定ソート)。1 回スキャンで比較するため文字単位で持つ。
    exact: Vec<(Vec<char>, String)>,
    /// 読みが長い順 (安定ソート)。
    fuzzy_entries: Vec<WordEntry>,
}

impl TextFormatter {
    /// 辞書としきい値から整形器を構築する。
    ///
    /// `fuzzy` が false のときは完全一致だけを適用する。
    /// `fuzzy_threshold` は 0–100 に丸められる。読みと正式表記が
    /// [`FUZZY_MIN_CHARS`] 未満の項目はファジー対象外。
    pub(crate) fn new(
        replacements: &[(String, String)],
        words: &[WordEntry],
        fuzzy_threshold: f64,
        fuzzy: bool,
        remove_tags: bool,
        remove_ja_spaces: bool,
    ) -> Self {
        let threshold = fuzzy_threshold.clamp(0.0, 100.0);

        let mut exact: Vec<(String, String)> = Vec::new();
        for (key, value) in replacements {
            if key.is_empty() {
                continue;
            }
            upsert(&mut exact, key, value);
        }

        let mut fuzzy_entries: Vec<WordEntry> = Vec::new();
        for entry in words {
            match &entry.reading {
                None => {
                    // コロン無しの英字は完全一致のみ (既存の置換を上書きしない)
                    if !exact.iter().any(|(key, _)| key == &entry.target) {
                        exact.push((entry.target.clone(), entry.target.clone()));
                    }
                }
                Some(reading) => {
                    upsert(&mut exact, reading, &entry.target);
                    // 読みが短い語や API/PR/DB のような短い正式名は誤爆防止のため
                    // ファジーマッチ対象外にする。日本語の正式表記は表記揺れの
                    // 必要がないため、ASCII の正式表記だけを対象にする。
                    if fuzzy
                        && char_len(reading) >= FUZZY_MIN_CHARS
                        && char_len(&entry.target) >= FUZZY_MIN_CHARS
                        && is_ascii_word(&entry.target)
                    {
                        fuzzy_entries.push(entry.clone());
                    }
                }
            }
        }

        let mut order: Vec<usize> = (0..exact.len()).collect();
        order.sort_by_key(|&index| std::cmp::Reverse(char_len(&exact[index].0)));
        let exact: Vec<(Vec<char>, String)> = order
            .into_iter()
            .map(|index| {
                let (key, target) = &exact[index];
                (key.chars().collect(), target.clone())
            })
            .collect();

        fuzzy_entries.sort_by_key(|entry| {
            std::cmp::Reverse(char_len(entry.reading.as_deref().unwrap_or("")))
        });

        Self {
            remove_tags,
            remove_ja_spaces,
            fuzzy,
            fuzzy_threshold: threshold,
            exact,
            fuzzy_entries,
        }
    }

    /// 生の認識結果を整形して返す。空入力は空文字列。
    pub(crate) fn format(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let mut text = text.to_string();
        if self.remove_tags {
            text = remove_tags(&text);
            text = remove_special_tokens(&text);
        }
        text = text.replace('\u{3000}', " ");
        text = collapse_whitespace(&text);
        if self.remove_ja_spaces {
            text = remove_ja_spaces(&text);
        }
        // 完全一致を先に適用し、登録済みの読みがファジー一致に奪われないようにする。
        text = self.apply_exact(&text);
        if self.fuzzy
            && let Some(replaced) = self.apply_fuzzy(&text)
        {
            text = replaced;
        }
        text = normalize_ascii_japanese_boundaries(&text);
        text.trim().to_string()
    }

    /// 完全一致キーを原文の 1 回スキャンで置換する。
    ///
    /// 各位置で最長のキーを選び、置換後の文字列は再走査しない。
    /// 短いキーは [`is_exact_key_boundary`] を通した場合だけ一致する。
    fn apply_exact(&self, text: &str) -> String {
        if self.exact.is_empty() {
            return text.to_string();
        }
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len() + 16);
        let mut index = 0;
        while index < chars.len() {
            let mut matched: Option<(&[char], &str)> = None;
            for (key, target) in &self.exact {
                // 先頭文字で大半のキーを捨てる。
                if key[0] != chars[index] || index + key.len() > chars.len() {
                    continue;
                }
                let end = index + key.len();
                if chars[index..end] == key[..] && is_exact_key_boundary(&chars, index, end) {
                    matched = Some((key, target.as_str()));
                    break;
                }
            }
            match matched {
                Some((key, target)) => {
                    let end = index + key.len();
                    out.push_str(&with_boundary_spaces(&chars, index, end, target));
                    index = end;
                }
                None => {
                    out.push(chars[index]);
                    index += 1;
                }
            }
        }
        out
    }

    /// ファジー一致で置換した文字列を返す。置換が無ければ `None`。
    fn apply_fuzzy(&self, text: &str) -> Option<String> {
        if self.fuzzy_entries.is_empty() || text.is_empty() {
            return None;
        }
        let text_chars: Vec<char> = text.chars().collect();
        let text_len = text_chars.len();

        // (score, match_len, start, entry)
        let mut candidates: Vec<(f64, usize, usize, &WordEntry)> = Vec::new();
        for entry in &self.fuzzy_entries {
            let Some(reading) = &entry.reading else {
                continue;
            };
            let reading_chars: Vec<char> = reading.chars().collect();
            let reading_len = reading_chars.len();
            if reading_len == 0 {
                continue;
            }

            let Some((mut score, start, mut end)) =
                partial_ratio_alignment(&reading_chars, &text_chars)
            else {
                continue;
            };
            if end <= start {
                continue;
            }

            // 末尾の助詞は置換に巻き込まず、元のテキストに残す。
            if PARTICLES.contains(&text_chars[end - 1]) {
                end -= 1;
                if end <= start {
                    continue;
                }
                score = ratio(&reading_chars, &text_chars[start..end]);
            }

            // partial_ratio は短い断片でも 100 になり得るため、
            // コア長が読みの ±FUZZY_LENGTH_SLACK 文字に収まることを要求する。
            let core_len = end - start;
            if core_len < reading_len.saturating_sub(FUZZY_LENGTH_SLACK).max(1)
                || core_len > reading_len + FUZZY_LENGTH_SLACK
            {
                continue;
            }
            if !is_fuzzy_core_boundary(&text_chars, start, end) {
                continue;
            }

            // 読みが長い語は揺れを拾いやすくするため、しきい値を緩和する。
            let threshold = if reading_len >= FUZZY_LENIENT_MIN_CHARS {
                self.fuzzy_threshold.min(FUZZY_LENIENT_THRESHOLD)
            } else {
                self.fuzzy_threshold
            };
            if score < threshold {
                continue;
            }
            candidates.push((score, end - start, start, entry));
        }

        if candidates.is_empty() {
            return None;
        }

        // 登録読みが長い単語を優先し、次にマッチ長、類似度、開始位置の早い順。
        candidates.sort_by(|a, b| {
            char_len(a.3.reading.as_deref().unwrap_or(""))
                .cmp(&char_len(b.3.reading.as_deref().unwrap_or("")))
                .reverse()
                .then(b.1.cmp(&a.1))
                .then(b.0.total_cmp(&a.0))
                .then(a.2.cmp(&b.2))
        });

        let mut occupied = vec![false; text_len];
        let mut selected: Vec<(usize, usize, String)> = Vec::new();
        for (_score, match_len, start, entry) in candidates {
            let end = start + match_len;
            if end > text_len || occupied[start..end].iter().any(|&used| used) {
                continue;
            }
            for slot in &mut occupied[start..end] {
                *slot = true;
            }
            selected.push((start, end, entry.target.clone()));
        }

        if selected.is_empty() {
            return None;
        }
        selected.sort_by_key(|item| item.0);

        let mut out = String::with_capacity(text.len() + 16);
        let mut position = 0;
        for (start, end, target) in selected {
            out.extend(&text_chars[position..start]);
            out.push_str(&with_boundary_spaces(&text_chars, start, end, &target));
            position = end;
        }
        out.extend(&text_chars[position..]);
        Some(out)
    }
}

/// Python dict の代入と同じく、既存キーは位置を保ったまま値を更新する。
fn upsert(entries: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some(existing) = entries.iter_mut().find(|(existing, _)| existing == key) {
        existing.1 = value.to_string();
    } else {
        entries.push((key.to_string(), value.to_string()));
    }
}

/// 部分一致の最良区間 (score, start, end) を返す。
///
/// rapidfuzz の `partial_ratio_alignment` と同じく、読みの長さ ±
/// [`FUZZY_LENGTH_SLACK`] のウィンドウをテキスト全体で走査し、
/// 最も類似する区間を採用する。
fn partial_ratio_alignment(reading: &[char], text: &[char]) -> Option<(f64, usize, usize)> {
    let reading_len = reading.len();
    if reading_len == 0 || text.is_empty() {
        return None;
    }
    let min_len = reading_len.saturating_sub(FUZZY_LENGTH_SLACK).max(1);
    let max_len = (reading_len + FUZZY_LENGTH_SLACK).min(text.len());

    let mut best: Option<(f64, usize, usize)> = None;
    for window_len in min_len..=max_len {
        for start in 0..=text.len() - window_len {
            let end = start + window_len;
            let score = ratio(reading, &text[start..end]);
            if best.is_none_or(|(best_score, _, _)| score > best_score) {
                best = Some((score, start, end));
            }
        }
    }
    best
}

/// 編集距離を文字数の和で正規化した類似度 (0–100)。
fn ratio(left: &[char], right: &[char]) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let left: String = left.iter().collect();
    let right: String = right.iter().collect();
    // rapidfuzz の ratio と同じく、編集距離を「両者の長さの和」で正規化する。
    let distance = strsim::levenshtein(&left, &right) as f64;
    let total = (left.chars().count() + right.chars().count()) as f64;
    100.0 * (1.0 - distance / total)
}

/// 置換語の前後が英日境界なら、読みやすさのため空白を補う。
fn with_boundary_spaces(text: &[char], start: usize, end: usize, replacement: &str) -> String {
    if replacement.is_empty() {
        return String::new();
    }
    let first = replacement.chars().next().unwrap();
    let last = replacement.chars().last().unwrap();
    let mut out = String::with_capacity(replacement.len() + 2);
    if start > 0
        && is_ascii_alpha(first)
        && is_ja_char(text[start - 1])
        && !replacement.starts_with(' ')
    {
        out.push(' ');
    }
    out.push_str(replacement);
    if end < text.len()
        && is_ascii_alpha(last)
        && is_ja_char(text[end])
        && !replacement.ends_with(' ')
    {
        out.push(' ');
    }
    out
}

/// 完全一致キーが語の境界に置かれているか。
///
/// 英数字のキーは前後が英数字でないこと、短い仮名キーは同種の仮名が
/// 連続する語の一部でないことを要求する。助詞 (「ろぐを」「すどで」) は
/// 語の切れ目として扱う。
fn is_exact_key_boundary(text: &[char], start: usize, end: usize) -> bool {
    let first = text[start];
    let last = text[end - 1];
    if first.is_ascii_alphanumeric() && start > 0 && text[start - 1].is_ascii_alphanumeric() {
        return false;
    }
    if last.is_ascii_alphanumeric() && end < text.len() && text[end].is_ascii_alphanumeric() {
        return false;
    }
    if end - start >= EXACT_BOUNDARY_MIN_CHARS {
        return true;
    }
    let Some(script) = kana_script(first) else {
        return true;
    };
    // 「モニター」の「ター」のように、より長い同種仮名語の途中に
    // だけ現れる短いキーは一致させない。助詞が続く場合は語の切れ目。
    if start > 0
        && kana_script(text[start - 1]) == Some(script)
        && !ends_with_boundary_particle(text, start)
    {
        return false;
    }
    if end < text.len()
        && kana_script(text[end]) == Some(script)
        && !starts_with_boundary_particle(text, end)
    {
        return false;
    }
    true
}

/// 境界として扱う助詞。単独で語に付くものを長い順に並べる。
const BOUNDARY_PARTICLES: &[&str] = &[
    "について",
    "に対して",
    "として",
    "によって",
    "ばかり",
    "から",
    "まで",
    "より",
    "だけ",
    "など",
    "でも",
    "しか",
    "ほど",
    "って",
    "で",
    "に",
    "を",
    "は",
    "が",
    "の",
    "と",
    "へ",
    "も",
];

/// `text[start..]` が助詞で始まるか。
fn starts_with_boundary_particle(text: &[char], start: usize) -> bool {
    BOUNDARY_PARTICLES
        .iter()
        .any(|particle| char_starts_with(text, start, particle))
}

/// `text[..end]` が助詞で終わるか。
fn ends_with_boundary_particle(text: &[char], end: usize) -> bool {
    BOUNDARY_PARTICLES.iter().any(|particle| {
        let length = particle.chars().count();
        end >= length && char_starts_with(text, end - length, particle)
    })
}

/// 文字スライスの `start` 位置が `prefix` で始まるか。
fn char_starts_with(text: &[char], start: usize, prefix: &str) -> bool {
    prefix
        .chars()
        .enumerate()
        .all(|(offset, expected)| text.get(start.saturating_add(offset)) == Some(&expected))
}

/// `<|...|>` 形式のモデルタグを除去する。
fn remove_tags(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        // `<|` の後、最初の `|` の直後が `>` のときだけタグとして除去する。
        if chars[index] == '<' && chars.get(index + 1) == Some(&'|') {
            let mut cursor = index + 2;
            while cursor < chars.len() && chars[cursor] != '|' {
                cursor += 1;
            }
            if cursor + 1 < chars.len() && chars[cursor] == '|' && chars[cursor + 1] == '>' {
                index = cursor + 2;
                continue;
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// `<s>` / `</s>` / `<unk>` などの特殊トークンを除去する。
fn remove_special_tokens(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let mut matched_len = 0;
        if chars[index] == '<' {
            let mut cursor = index + 1;
            if chars.get(cursor) == Some(&'/') {
                cursor += 1;
            }
            for token in ["s", "unk", "pad", "blank"] {
                let token_chars: Vec<char> = token.chars().collect();
                if cursor + token_chars.len() < chars.len()
                    && token_chars.iter().enumerate().all(|(offset, expected)| {
                        chars[cursor + offset].eq_ignore_ascii_case(expected)
                    })
                    && chars[cursor + token_chars.len()] == '>'
                {
                    matched_len = cursor - index + token_chars.len() + 1;
                    break;
                }
            }
        }
        if matched_len > 0 {
            index += matched_len;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

/// 連続する空白を 1 つへ畳み、前後の空白を落とす。
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_whitespace = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !in_whitespace {
                out.push(' ');
                in_whitespace = true;
            }
        } else {
            out.push(c);
            in_whitespace = false;
        }
    }
    out.trim().to_string()
}

/// 日本語同士の間にある空白だけを取り除く。
fn remove_ja_spaces(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index].is_whitespace() {
            let start = index;
            while index < chars.len() && chars[index].is_whitespace() {
                index += 1;
            }
            let prev = out.last().copied();
            let next = chars.get(index).copied();
            if let (Some(prev), Some(next)) = (prev, next)
                && is_ja_char(prev)
                && is_ja_char(next)
            {
                continue;
            }
            out.extend_from_slice(&chars[start..index]);
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out.into_iter().collect()
}

/// 英字と日本語の境界に半角スペースを補い、連続する空白を 1 つに畳む。
fn normalize_ascii_japanese_boundaries(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    for (index, &c) in chars.iter().enumerate() {
        // ` {2,}` -> " " と同じ (タブ等は既に空白へ畳まれている)。
        if c != ' ' || !out.ends_with(' ') {
            out.push(c);
        }
        if let Some(&next) = chars.get(index + 1)
            && ((is_ascii_alpha(c) && is_ja_char(next)) || (is_ja_char(c) && is_ascii_alpha(next)))
        {
            out.push(' ');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formatter() -> TextFormatter {
        let words = vec![
            WordEntry {
                target: "GitHub Actions".to_string(),
                reading: Some("ギットハブアクションズ".to_string()),
            },
            WordEntry {
                target: "Docker".to_string(),
                reading: Some("ドッカー".to_string()),
            },
            WordEntry {
                target: "API".to_string(),
                reading: Some("エーピーアイ".to_string()),
            },
            WordEntry {
                target: "AI".to_string(),
                reading: Some("あい".to_string()),
            },
        ];
        TextFormatter::new(
            &[("えーぴーあい".to_string(), "API".to_string())],
            &words,
            80.0,
            true,
            true,
            true,
        )
    }

    #[test]
    fn strips_tags_and_normalizes_spaces() {
        let f = formatter();
        assert_eq!(
            f.format("<|ja|>こんにちは<|NEUTRAL|> <|Speech|><|withitn|> 世界"),
            "こんにちは世界"
        );
        assert_eq!(f.format("</s><unk> テスト"), "テスト");
        assert_eq!(f.format(""), "");
    }

    #[test]
    fn short_exact_keys_need_word_boundaries() {
        let words = vec![
            WordEntry {
                target: "tar".to_string(),
                reading: Some("ター".to_string()),
            },
            WordEntry {
                target: "Koa".to_string(),
                reading: Some("コア".to_string()),
            },
            WordEntry {
                target: "Loki".to_string(),
                reading: Some("ロキ".to_string()),
            },
            WordEntry {
                target: "Sass".to_string(),
                reading: Some("サス".to_string()),
            },
            WordEntry {
                target: "Docker".to_string(),
                reading: Some("ドッカー".to_string()),
            },
            WordEntry {
                target: "log".to_string(),
                reading: Some("ろぐ".to_string()),
            },
            WordEntry {
                target: "AI".to_string(),
                reading: Some("あい".to_string()),
            },
        ];
        let f = TextFormatter::new(&[], &words, 80.0, false, true, true);

        // より長いカタカナ語の一部には一致しない。
        assert_eq!(f.format("モニターを確認する"), "モニターを確認する");
        assert_eq!(f.format("コンピューターを使う"), "コンピューターを使う");
        assert_eq!(f.format("センターに問い合わせる"), "センターに問い合わせる");
        assert_eq!(f.format("パラメーターを変更する"), "パラメーターを変更する");
        assert_eq!(f.format("コアラを見た"), "コアラを見た");
        assert_eq!(f.format("リバースプロキシを置く"), "リバースプロキシを置く");
        assert_eq!(f.format("サステナブルな設計"), "サステナブルな設計");
        assert_eq!(f.format("ペガサス"), "ペガサス");

        // 語として独立していれば従来どおり置換する。
        assert_eq!(f.format("ターを展開する"), "tar を展開する");
        assert_eq!(f.format("コアを確認する"), "Koa を確認する");
        // 4 文字以上のキーは複合語でも置換する。
        assert_eq!(f.format("ドッカーコンテナ"), "Docker コンテナ");
        // 置換結果を再走査しない。
        assert_eq!(f.format("ターとドッカー"), "tar と Docker");

        // 短いひらがな読みは助詞が続けば置換し、長い語の途中では置換しない。
        assert_eq!(f.format("ろぐを確認する"), "log を確認する");
        assert_eq!(f.format("このろぐを見る"), "この log を見る");
        assert_eq!(f.format("あいさつをする"), "あいさつをする");
        assert_eq!(f.format("あい"), "AI");
    }

    #[test]
    fn exact_mode_does_not_rewrite_similar_words() {
        let words = vec![
            WordEntry {
                target: "SharePoint".to_string(),
                reading: Some("シェアポイント".to_string()),
            },
            WordEntry {
                target: "Datadog".to_string(),
                reading: Some("データドッグ".to_string()),
            },
            WordEntry {
                target: "Docker".to_string(),
                reading: Some("ドッカー".to_string()),
            },
            WordEntry {
                target: "GitHub Actions".to_string(),
                reading: Some("ギットハブアクションズ".to_string()),
            },
        ];
        let f = TextFormatter::new(&[], &words, 80.0, false, true, true);

        // 類似しているだけの語は書き換えない。
        assert_eq!(
            f.format("パワーポイントで資料を作る"),
            "パワーポイントで資料を作る"
        );
        assert_eq!(f.format("データベースに接続する"), "データベースに接続する");
        assert_eq!(
            f.format("ギッタハブアクションズを使う"),
            "ギッタハブアクションズを使う"
        );

        // 読みの完全一致は置換する。
        assert_eq!(f.format("シェアポイントを開く"), "SharePoint を開く");
        assert_eq!(f.format("データドッグを確認する"), "Datadog を確認する");
        assert_eq!(f.format("ドッカーを起動"), "Docker を起動");
    }

    #[test]
    fn fuzzy_replaces_long_reading_and_exact_replaces() {
        let f = formatter();
        assert_eq!(
            f.format("ギッタハブアクションズを使う"),
            "GitHub Actions を使う"
        );
        assert_eq!(f.format("エーピーアイで書く"), "API で書く");
        assert_eq!(f.format("えーぴーあい"), "API");
    }

    #[test]
    fn keeps_particles_and_short_entries_out() {
        let f = formatter();
        assert_eq!(f.format("ドッカーを起動"), "Docker を起動");
        // 読みが 4 文字未満はファジー対象外 (完全一致は対象)
        assert_eq!(f.format("あお"), "あお");
        assert_eq!(f.format("あい"), "AI");
    }

    /// 移植元の Python 実装が出した結果との一致を確認する。
    #[test]
    fn matches_reference_text_formatter() {
        let words = load_words(Path::new(WORDS_PATH));
        let config = crate::config::Config::default();
        let f = TextFormatter::new(
            &config.replacements,
            &words,
            config.fuzzy_threshold,
            true,
            true,
            true,
        );
        let cases = [
            (
                "<|ja|>こんにちは、世界。<|NEUTRAL|><|Speech|><|withitn|>",
                "こんにちは、世界。",
            ),
            ("GitHub Actions を 使う", "GitHub Actions を使う"),
            ("ギッタハブアクションズ を 使う", "GitHub Actions を使う"),
            (
                "ドッカーを起動して API を叩く",
                "Docker を起動して API を叩く",
            ),
            ("えーぴーあいとエーアイ", "API と AI"),
            ("Hello 世界", "Hello 世界"),
            ("テスト 　です", "テストです"),
            ("ＡＰＩ", "ＡＰＩ"),
            ("ギットハブアクションズ", "GitHub Actions"),
            ("<|en|>Hello, world!<|NEUTRAL|>", "Hello, world!"),
            ("エーピーアイ", "API"),
            ("ドッカ", "Docker"),
            ("あいさつ", "あいさつ"),
            ("あお", "あお"),
            ("えーあい", "AI"),
            ("じーぴーてぃー と えるえるえむ", "GPT と LLM"),
            ("APIを使う", "API を使う"),
            ("APIを使う テスト", "API を使うテスト"),
            ("<|zh|>你好<|HAPPY|><|BGM|>", "你好"),
            ("エーピーアイ。", "API。"),
            ("えーぴーあい、", "API、"),
            ("ギットハブ アクションズ", "GitHub Actions"),
            ("ぎっとハブ あくしょんず", "ぎっとハブあくしょんず"),
            ("コンテナでドッカーを動かす", "コンテナで Docker を動かす"),
            ("ＡＰＩ と GitHub Actions", "ＡＰＩ と GitHub Actions"),
            ("  <|s|>  ", ""),
            ("<|unk|>テスト</s>", "テスト"),
        ];
        for (input, expected) in cases {
            assert_eq!(f.format(input), expected, "input={input:?}");
        }
    }

    #[test]
    fn inline_word_editing_roundtrips() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("時刻")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("BAREVoiceJP-words-{}-{unique}", std::process::id()));
        fs::create_dir_all(&dir).expect("一時ディレクトリ");
        let path = dir.join("words.txt");
        fs::write(&path, "# keep\nGitHub : ギットハブ\n").expect("初期辞書");

        add_word(&path, "TypeScript", "たいぷすくりぷと").expect("追加");
        assert!(add_word(&path, "TypeScript", "たいぷすくりぷと").is_err());
        let entries = load_words(&path);
        assert!(entries.iter().any(|entry| {
            entry.target() == "TypeScript" && entry.reading() == Some("たいぷすくりぷと")
        }));

        assert_eq!(
            remove_word(&path, "TypeScript", Some("たいぷすくりぷと")).expect("削除"),
            1
        );
        let text = fs::read_to_string(&path).expect("辞書を読む");
        assert!(text.contains("# keep"));
        assert!(!text.contains("TypeScript"));
        assert_eq!(
            remove_word(&path, "TypeScript", Some("たいぷすくりぷと")).expect("再削除"),
            0
        );
        fs::remove_dir_all(&dir).expect("一時ディレクトリを削除");
    }
}
