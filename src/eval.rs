//! `--eval` / `--sweep` 用のテキスト整形評価ハーネス。
//!
//! JSON のケースを `TextFormatter` だけに通し、カテゴリ別の正解率と
//! 失敗内容、ファジーしきい値ごとの誤爆・表記揺れ救出数を出力する。
//! 音声キャプチャや ASR は初期化しない。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::config::Config;
use crate::text::{self, TextFormatter, WordEntry};

/// スイープで比較するファジーしきい値 (%)。
const SWEEP_THRESHOLDS: [f64; 4] = [70.0, 75.0, 80.0, 85.0];

/// 完全一致モードでのみ評価するケースのカテゴリ。
///
/// ファジー一致は「似た語を置換する」機能なので、一般語が別の製品名へ
/// 寄せられることがある (例: 「コンピューター」→ Composer)。このカテゴリは
/// ファジー有効時の評価対象から外す。
const EXACT_ONLY_CATEGORY: &str = "exact_guard";

/// 評価対象の 1 ケース。
#[derive(Clone, Debug, PartialEq, Eq)]
struct EvalCase {
    category: String,
    input: String,
    expected: String,
}

/// 1 ケースの実行結果。
#[derive(Clone, Debug)]
struct Outcome {
    case: EvalCase,
    actual: String,
    pass: bool,
    /// 入力を書き換えてはいけないケースを書き換えた (誤爆)。
    false_positive: bool,
}

/// 1 しきい値分の集計。
struct ThresholdResult {
    threshold: f64,
    outcomes: Vec<Outcome>,
}

impl ThresholdResult {
    fn pass_count(&self) -> usize {
        self.outcomes.iter().filter(|outcome| outcome.pass).count()
    }

    fn false_positive_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.false_positive)
            .count()
    }

    fn fuzzy_total(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.case.category == "fuzzy_variant")
            .count()
    }

    fn fuzzy_pass_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.case.category == "fuzzy_variant" && outcome.pass)
            .count()
    }
}

/// 評価を実行し、プロセス終了コードを返す。
///
/// 既定しきい値で全ケース合格なら 0、失敗があれば 1。
pub(crate) fn run(path: &Path, sweep: bool, config: &Config) -> i32 {
    let cases = match load_cases(path) {
        Ok(cases) => cases,
        Err(exc) => {
            eprintln!("評価ケースを読み込めません: {} ({exc})", path.display());
            return 2;
        }
    };
    if cases.is_empty() {
        eprintln!("評価ケースが空です: {}", path.display());
        return 2;
    }

    let external = text::load_external_replacements(Path::new(text::REPLACEMENTS_PATH));
    let words = text::load_words(Path::new(text::WORDS_PATH));
    let replacements: Vec<(String, String)> = config
        .replacements
        .iter()
        .chain(external.iter())
        .cloned()
        .collect();

    // ファジー無効時は fuzzy_variant、有効時は完全一致専用のケースを外す。
    // --sweep はしきい値比較のためファジーを強制する。
    let fuzzy = config.fuzzy || sweep;
    let skip_category = if fuzzy {
        EXACT_ONLY_CATEGORY
    } else {
        "fuzzy_variant"
    };
    let skipped = cases
        .iter()
        .filter(|case| case.category == skip_category)
        .count();
    let cases: Vec<EvalCase> = cases
        .into_iter()
        .filter(|case| case.category != skip_category)
        .collect();

    let formatter = build_formatter(config, &replacements, &words, config.fuzzy_threshold, fuzzy);
    let outcomes = evaluate(&cases, &formatter);

    println!("=== テキスト整形 評価 ===");
    println!("ケース: {}", path.display());
    if skipped > 0 {
        if fuzzy {
            println!("スキップ: {EXACT_ONLY_CATEGORY} {skipped} 件 (完全一致専用)");
        } else {
            println!("スキップ: fuzzy_variant {skipped} 件 (--fuzzy で有効)");
        }
    }
    print_report(config.fuzzy_threshold, &outcomes);

    if sweep {
        print_sweep(&cases, &replacements, &words, config);
    }

    if outcomes.iter().all(|outcome| outcome.pass) {
        0
    } else {
        1
    }
}

/// 評価用の整形器を、指定したしきい値とファジー設定で作る。
fn build_formatter(
    config: &Config,
    replacements: &[(String, String)],
    words: &[WordEntry],
    threshold: f64,
    fuzzy: bool,
) -> TextFormatter {
    TextFormatter::new(
        replacements,
        words,
        threshold,
        fuzzy,
        config.remove_tags,
        config.remove_ja_spaces,
    )
}

/// 全ケースを整形し、合否と誤爆フラグを付けて返す。
fn evaluate(cases: &[EvalCase], formatter: &TextFormatter) -> Vec<Outcome> {
    cases
        .iter()
        .map(|case| {
            let actual = formatter.format(&case.input);
            let pass = actual == case.expected;
            let false_positive = case.expected == case.input && !pass;
            Outcome {
                case: case.clone(),
                actual,
                pass,
                false_positive,
            }
        })
        .collect()
}

/// カテゴリ別の正解率と失敗ケースを出力する。
fn print_report(threshold: f64, outcomes: &[Outcome]) {
    let total = outcomes.len();
    let passed = outcomes.iter().filter(|outcome| outcome.pass).count();
    let rate = percentage(passed, total);

    let mut categories: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for outcome in outcomes {
        let entry = categories
            .entry(outcome.case.category.as_str())
            .or_insert((0, 0));
        entry.1 += 1;
        if outcome.pass {
            entry.0 += 1;
        }
    }

    println!("しきい値: {:.0}%", threshold);
    println!("カテゴリ別:");
    for (category, (category_pass, category_total)) in &categories {
        println!(
            "  {category:<16} {category_pass:>3}/{category_total:<3} ({:>5.1}%)",
            percentage(*category_pass, *category_total)
        );
    }
    println!("合計: {passed}/{total} ({rate:.1}%)");

    let failures: Vec<&Outcome> = outcomes.iter().filter(|outcome| !outcome.pass).collect();
    if failures.is_empty() {
        println!("失敗ケース: なし");
        return;
    }

    println!("失敗ケース: {} 件", failures.len());
    for (index, outcome) in failures.iter().enumerate() {
        println!("  {}. [{}]", index + 1, outcome.case.category);
        println!("     入力: {:?}", outcome.case.input);
        println!("     期待: {:?}", outcome.case.expected);
        println!("     実際: {:?}", outcome.actual);
        println!(
            "     差分: {}",
            diff_summary(&outcome.case.expected, &outcome.actual)
        );
    }
}

/// しきい値を変えながら誤爆と表記揺れ救出数を比較する。
fn print_sweep(
    cases: &[EvalCase],
    replacements: &[(String, String)],
    words: &[WordEntry],
    config: &Config,
) {
    let results: Vec<ThresholdResult> = SWEEP_THRESHOLDS
        .iter()
        .map(|&threshold| {
            let formatter = build_formatter(config, replacements, words, threshold, true);
            ThresholdResult {
                threshold,
                outcomes: evaluate(cases, &formatter),
            }
        })
        .collect();

    // 誤爆 0 のうち、表記揺れ救出数が最大のもの。同数なら低いしきい値
    // (表記揺れを拾う余地が大きい方) を選ぶ。
    let best = results
        .iter()
        .enumerate()
        .filter(|(_, result)| result.false_positive_count() == 0)
        .max_by(|(_, left), (_, right)| {
            left.fuzzy_pass_count()
                .cmp(&right.fuzzy_pass_count())
                .then_with(|| right.threshold.total_cmp(&left.threshold))
        })
        .map(|(index, _)| index);

    println!();
    println!("=== しきい値スイープ ===");
    println!("しきい値  Pass/Total    正解率   誤爆  表記揺れ救出  判定");
    for (index, result) in results.iter().enumerate() {
        let total = result.outcomes.len();
        let passed = result.pass_count();
        let marker = if Some(index) == best {
            "← 最良"
        } else {
            ""
        };
        println!(
            "{:>5.0}%    {:>3}/{:<3}    {:>5.1}%   {:>3}   {:>3}/{:<3}     {marker}",
            result.threshold,
            passed,
            total,
            percentage(passed, total),
            result.false_positive_count(),
            result.fuzzy_pass_count(),
            result.fuzzy_total(),
        );
    }

    println!();
    match best {
        Some(index) => {
            let result = &results[index];
            println!(
                "最良: {:.0}% (誤爆 {} 件 / 表記揺れ救出 {}/{})",
                result.threshold,
                result.false_positive_count(),
                result.fuzzy_pass_count(),
                result.fuzzy_total()
            );
        }
        None => println!("誤爆 0 件のしきい値はありませんでした。"),
    }
    println!(
        "誤爆 = 入力と期待値が同一のケースを書き換えた件数、\
         表記揺れ救出 = fuzzy_variant カテゴリの合格数。同点は低いしきい値を優先。"
    );
}

fn percentage(part: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        100.0 * part as f64 / total as f64
    }
}

/// 最初に食い違う位置と、その前後の中身を短く示す。
fn diff_summary(expected: &str, actual: &str) -> String {
    let expected_chars: Vec<char> = expected.chars().collect();
    let actual_chars: Vec<char> = actual.chars().collect();

    let mut prefix = 0;
    while prefix < expected_chars.len()
        && prefix < actual_chars.len()
        && expected_chars[prefix] == actual_chars[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < expected_chars.len() - prefix
        && suffix < actual_chars.len() - prefix
        && expected_chars[expected_chars.len() - 1 - suffix]
            == actual_chars[actual_chars.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let expected_middle: String = expected_chars[prefix..expected_chars.len() - suffix]
        .iter()
        .collect();
    let actual_middle: String = actual_chars[prefix..actual_chars.len() - suffix]
        .iter()
        .collect();
    format!(
        "{}文字目から 期待 {:?} / 実際 {:?}",
        prefix + 1,
        expected_middle,
        actual_middle
    )
}

/// 評価ケース JSON を BOM 許容で読み込む。
fn load_cases(path: &Path) -> Result<Vec<EvalCase>, String> {
    let bytes = fs::read(path).map_err(|exc| exc.to_string())?;
    let text = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => String::from_utf8_lossy(rest).into_owned(),
        None => String::from_utf8_lossy(&bytes).into_owned(),
    };
    parse_cases(&text)
}

/// JSON 配列を `EvalCase` へ変換し、必須フィールドを検証する。
fn parse_cases(text: &str) -> Result<Vec<EvalCase>, String> {
    let value: Value = serde_json::from_str(text).map_err(|exc| exc.to_string())?;
    let array = value
        .as_array()
        .ok_or_else(|| "トップレベルは配列である必要があります".to_string())?;

    let mut cases = Vec::with_capacity(array.len());
    for (index, item) in array.iter().enumerate() {
        let object = item
            .as_object()
            .ok_or_else(|| format!("{} 件目がオブジェクトではありません", index + 1))?;
        let field = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{} 件目に文字列の {name} がありません", index + 1))
        };
        let category = field("category")?;
        if category.is_empty() {
            return Err(format!("{} 件目の category が空です", index + 1));
        }
        cases.push(EvalCase {
            category: category.to_string(),
            input: field("input")?.to_string(),
            expected: field("expected")?.to_string(),
        });
    }
    Ok(cases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cases_and_rejects_bad_shape() {
        let cases = parse_cases(
            r#"[
                {"category":"particle_protect","input":"ドッカーで","expected":"Docker で"},
                {"category":"natural_ja","input":"今日は晴れ","expected":"今日は晴れ"}
            ]"#,
        )
        .expect("valid cases");
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].category, "particle_protect");

        assert!(parse_cases(r#"{"category":"x"}"#).is_err());
        assert!(parse_cases(r#"[{"category":"","input":"a","expected":"a"}]"#).is_err());
        assert!(parse_cases(r#"[{"category":"x","input":"a"}]"#).is_err());
    }

    #[test]
    fn diff_points_at_first_difference() {
        assert_eq!(
            diff_summary("Docker を起動", "ドッカーを起動"),
            "1文字目から 期待 \"Docker \" / 実際 \"ドッカー\""
        );
        assert_eq!(
            diff_summary("GitHub Actions", "GitHub Action"),
            "14文字目から 期待 \"s\" / 実際 \"\""
        );
    }

    #[test]
    fn percentage_handles_empty() {
        assert_eq!(percentage(0, 0), 0.0);
        assert_eq!(percentage(1, 2), 50.0);
    }

    /// コミット済みケースを指定モードで評価し、失敗だけ返す。
    fn committed_failures(fuzzy: bool) -> Vec<Outcome> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cases =
            load_cases(&root.join("tests/text_cases.json")).expect("評価ケースを読み込める");
        let config = Config::default();
        let external = text::load_external_replacements(&root.join(text::REPLACEMENTS_PATH));
        let words = text::load_words(&root.join(text::WORDS_PATH));
        let replacements: Vec<(String, String)> = config
            .replacements
            .iter()
            .chain(external.iter())
            .cloned()
            .collect();
        let formatter = build_formatter(
            &config,
            &replacements,
            &words,
            config.fuzzy_threshold,
            fuzzy,
        );
        let skip_category = if fuzzy {
            EXACT_ONLY_CATEGORY
        } else {
            "fuzzy_variant"
        };
        evaluate(&cases, &formatter)
            .into_iter()
            .filter(|outcome| outcome.case.category != skip_category)
            .filter(|outcome| !outcome.pass)
            .collect()
    }

    #[test]
    fn committed_cases_pass_without_fuzzy() {
        let failures = committed_failures(false);
        assert!(failures.is_empty(), "完全一致モードで失敗: {failures:?}");
    }

    #[test]
    fn committed_cases_pass_with_fuzzy() {
        let failures = committed_failures(true);
        assert!(failures.is_empty(), "ファジー有効時に失敗: {failures:?}");
    }
}
