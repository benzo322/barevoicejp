//! sherpa-onnx DLL の読み込み順の制御。
//!
//! 同梱の `onnxruntime.dll` を **絶対パスで先に** ロードしてから
//! `sherpa-onnx-c-api.dll` をロードする。
//!
//! Windows 11 25H2 (build 26200) の環境で、`C:\Windows\System32\onnxruntime.dll`
//! (OS 同梱の 1.17 系) が exe と同じディレクトリの DLL より先に解決される事例が
//! 確認されている。sherpa-onnx 1.13.8 は ORT API v28 を要求するため、1.17 が
//! 読まれると `OrtGetApiBase()->GetApi(28)` が NULL を返し、直後にアクセス違反
//! (0xc0000005) で落ちる。
//!
//! 一度モジュールを絶対パスで読み込んでおけば、以降ベース名で
//! `onnxruntime.dll` を要求するモジュールにはロード済みのものが再利用される。
//! 「ORT → sherpa」の順序を守ることが要点。
//!
//! この順序を保証するため、ビルド時に sherpa のインポートライブラリは
//! リンクしない (`build.rs` 参照)。シンボルはすべて実行時に解決する。

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryW};

/// NUL 終端の UTF-16 へ変換する。
fn wide(text: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    Path::new(text)
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect()
}

/// exe のあるディレクトリ。
fn exe_dir() -> Result<PathBuf, String> {
    let mut buf = vec![0u16; 32_768];
    // SAFETY: buf は書き込み可能で十分な長さがある。NULL を渡すと exe 自身のパスを返す。
    let len =
        unsafe { GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) };
    if len == 0 {
        return Err("exe のパスを取得できません".to_string());
    }
    buf.truncate(len as usize);
    let path = String::from_utf16_lossy(&buf);
    Path::new(&path)
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("exe のディレクトリを特定できません: {path}"))
}

/// DLL を絶対パスでロードする。
fn load(path: &Path) -> Result<HMODULE, String> {
    let name = wide(&path.to_string_lossy());
    // SAFETY: name は NUL 終端の UTF-16。
    let handle = unsafe { LoadLibraryW(name.as_ptr()) };
    if handle.is_null() {
        Err(format!("ロードできません: {}", path.display()))
    } else {
        Ok(handle)
    }
}

/// ロード済みモジュール。プロセス終了まで保持する。
struct Modules {
    onnxruntime: HMODULE,
    sherpa: HMODULE,
}

// SAFETY: モジュールハンドルはプロセス内有効な不変の値で、解放しない。
unsafe impl Send for Modules {}
unsafe impl Sync for Modules {}

static MODULES: OnceLock<Result<Modules, String>> = OnceLock::new();

/// 同梱 DLL を一度だけ、ORT → sherpa の順でロードする。
fn modules() -> Result<&'static Modules, String> {
    match MODULES.get_or_init(|| {
        let dir = exe_dir()?;
        // 先に ORT。これで以降の `onnxruntime.dll` 要求が同梱版へ解決される。
        let onnxruntime = load(&dir.join("onnxruntime.dll"))?;
        let sherpa = match load(&dir.join("sherpa-onnx-c-api.dll")) {
            Ok(handle) => handle,
            Err(exc) => {
                // 同梱されていない場合だけ素の名前解決に任せる。
                let name = wide("sherpa-onnx-c-api.dll");
                // SAFETY: name は NUL 終端の UTF-16。
                let handle = unsafe { LoadLibraryW(name.as_ptr()) };
                if handle.is_null() {
                    return Err(format!("{exc} (代替の解決にも失敗)"));
                }
                handle
            }
        };
        Ok(Modules {
            onnxruntime,
            sherpa,
        })
    }) {
        Ok(modules) => Ok(modules),
        Err(exc) => Err(exc.clone()),
    }
}

/// シンボルを 1 つ解決して関数ポインタへ変換する。
///
/// # Safety
/// `name` は NUL 終端の ASCII で、`T` が指すシグネチャは実際の関数と一致すること。
unsafe fn symbol<T: Copy>(module: HMODULE, name: &'static [u8]) -> Result<T, String> {
    // SAFETY: name は NUL 終端。戻り値は関数ポインタか NULL。
    let raw = unsafe { GetProcAddress(module, name.as_ptr()) };
    let Some(function) = raw else {
        let label = String::from_utf8_lossy(&name[..name.len() - 1]).into_owned();
        return Err(format!("シンボルが見つかりません: {label}"));
    };
    // SAFETY: 呼び出し側がシグネチャの一致を保証する。
    Ok(unsafe { std::mem::transmute_copy::<_, T>(&function) })
}

/// sherpa-onnx C-API の関数ポインタ束。
pub(crate) struct Api {
    pub(crate) create_offline_recognizer: unsafe extern "C" fn(*const c_void) -> *const c_void,
    pub(crate) destroy_offline_recognizer: unsafe extern "C" fn(*const c_void),
    pub(crate) create_offline_stream: unsafe extern "C" fn(*const c_void) -> *const c_void,
    pub(crate) destroy_offline_stream: unsafe extern "C" fn(*const c_void),
    pub(crate) accept_waveform: unsafe extern "C" fn(*const c_void, i32, *const f32, i32),
    pub(crate) decode: unsafe extern "C" fn(*const c_void, *const c_void),
    pub(crate) get_result: unsafe extern "C" fn(*const c_void) -> *const c_void,
    pub(crate) destroy_result: unsafe extern "C" fn(*const c_void),
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

/// C-API の関数ポインタを解決する。初回だけ実際の解決を行う。
pub(crate) fn api() -> Result<&'static Api, String> {
    match API.get_or_init(|| {
        let modules = modules()?;
        let sherpa = modules.sherpa;
        // SAFETY: sherpa はロード済みで、各シンボルのシグネチャは c-api.h と一致する。
        unsafe {
            Ok(Api {
                create_offline_recognizer: symbol(sherpa, b"SherpaOnnxCreateOfflineRecognizer\0")?,
                destroy_offline_recognizer: symbol(
                    sherpa,
                    b"SherpaOnnxDestroyOfflineRecognizer\0",
                )?,
                create_offline_stream: symbol(sherpa, b"SherpaOnnxCreateOfflineStream\0")?,
                destroy_offline_stream: symbol(sherpa, b"SherpaOnnxDestroyOfflineStream\0")?,
                accept_waveform: symbol(sherpa, b"SherpaOnnxAcceptWaveformOffline\0")?,
                decode: symbol(sherpa, b"SherpaOnnxDecodeOfflineStream\0")?,
                get_result: symbol(sherpa, b"SherpaOnnxGetOfflineStreamResult\0")?,
                destroy_result: symbol(sherpa, b"SherpaOnnxDestroyOfflineRecognizerResult\0")?,
            })
        }
    }) {
        Ok(api) => Ok(api),
        Err(exc) => Err(exc.clone()),
    }
}

/// ORT の `OrtApiBase`。
///
/// onnxruntime_c_api.h の定義どおり、**関数ポインタが直接 2 つ並ぶ**構造体。
/// 仮想関数テーブルへのポインタではないので、二重に間接参照してはいけない。
#[repr(C)]
struct OrtApiBase {
    get_api: unsafe extern "system" fn(u32) -> *const c_void,
    get_version_string: unsafe extern "system" fn() -> *const std::ffi::c_char,
}

/// 同梱 ORT の `OrtApiBase` を取り出す。
fn ort_api_base() -> Result<*const OrtApiBase, String> {
    let modules = modules()?;
    // SAFETY: モジュールはロード済み。OrtGetApiBase は OrtApiBase* を返す。
    unsafe {
        let get_api_base: unsafe extern "system" fn() -> *const OrtApiBase =
            symbol(modules.onnxruntime, b"OrtGetApiBase\0")?;
        let base = get_api_base();
        if base.is_null() {
            return Err("OrtApiBase が NULL です".to_string());
        }
        Ok(base)
    }
}

/// 同梱 ORT のバージョン文字列。診断ログ用。
pub(crate) fn onnxruntime_version() -> Result<String, String> {
    let base = ort_api_base()?;
    // SAFETY: base は有効な OrtApiBase。get_version_string は NUL 終端の文字列を返す。
    unsafe {
        let pointer = ((*base).get_version_string)();
        if pointer.is_null() {
            return Err("バージョン文字列が NULL です".to_string());
        }
        Ok(std::ffi::CStr::from_ptr(pointer)
            .to_string_lossy()
            .into_owned())
    }
}

/// ORT が指定の API バージョンを提供するか。
pub(crate) fn onnxruntime_supports(version: u32) -> Result<bool, String> {
    let base = ort_api_base()?;
    // SAFETY: base は有効な OrtApiBase。get_api は未対応版で NULL を返す。
    unsafe { Ok(!((*base).get_api)(version).is_null()) }
}

/// 起動時に一度だけ DLL を解決し、結果をログに残す。
///
/// 失敗しても致命的ではない。モデル未配置と同じ扱いで、設定画面から
/// 後からダウンロードできる余地を残す。
pub(crate) fn preload() -> Result<(), String> {
    modules()?;
    let version = onnxruntime_version().unwrap_or_else(|exc| format!("不明 ({exc})"));
    let api28 = onnxruntime_supports(28).unwrap_or(false);
    crate::info!("onnxruntime {version} / api28={api28}");
    // API v28 が無いと sherpa-onnx 1.13.8 は初期化できない。ここで気づければ
    // モデル読み込み中のアクセス違反ではなく、読める形で失敗させられる。
    if !api28 {
        return Err(format!(
            "同梱の onnxruntime.dll が API v28 を提供していません (version={version})。\n\
             古い onnxruntime.dll が優先して読み込まれている可能性があります。\n\
             exe と同じディレクトリの onnxruntime.dll を確認してください。"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同梱 ORT が sherpa-onnx 1.13.8 の要求する API v28 を提供していること。
    ///
    /// このテストは「System32 の古い ORT を掴んでいない」ことの回帰検出になる。
    /// 掴んでいれば `onnxruntime_version()` が 1.17 系を返して失敗する。
    ///
    /// **DLL が exe の隣に無い環境では意味を持たない。** CI ではテスト用 exe が
    /// `target/<profile>/deps/` 以外に置かれることがあり、同梱 DLL を解決できない。
    /// 実際の回帰確認は、起動ログの `onnxruntime <ver> / api28=true` で行う。
    #[test]
    #[ignore = "同梱 sherpa-onnx / onnxruntime DLL が exe の隣に必要"]
    fn bundled_onnxruntime_supports_api_v28() {
        let version = onnxruntime_version().expect("ORT のバージョンを取得できる");
        let supported = onnxruntime_supports(28).expect("GetApi を呼べる");
        assert!(
            supported,
            "同梱 onnxruntime が API v28 を提供していません (version={version})。\
             System32 の古い onnxruntime.dll を掴んでいる可能性があります"
        );
    }

    /// ロード済みモジュールが exe と同じディレクトリにあること。
    ///
    /// `bundled_onnxruntime_supports_api_v28` と同じ理由で既定では実行しない。
    #[test]
    #[ignore = "同梱 sherpa-onnx / onnxruntime DLL が exe の隣に必要"]
    fn loads_dlls_from_exe_directory() {
        let modules = modules().expect("同梱 DLL をロードできる");
        assert!(!modules.onnxruntime.is_null());
        assert!(!modules.sherpa.is_null());
        // exe 隣の ORT が使われているかをバージョンで間接確認する。
        let version = onnxruntime_version().expect("バージョンを取得できる");
        assert!(
            !version.starts_with("1.17"),
            "System32 の ORT 1.17 を掴んでいます (version={version})"
        );
    }

    /// DLL を伴わない純粋なロジックの確認。
    ///
    /// この 1 本は DLL を必要としないので CI でも走る。
    #[test]
    fn exe_dir_is_absolute_and_non_empty() {
        let dir = exe_dir().expect("exe のディレクトリを特定できる");
        assert!(dir.is_absolute(), "絶対パスであること: {}", dir.display());
        assert!(dir.parent().is_some(), "親を持つこと: {}", dir.display());
    }
}
