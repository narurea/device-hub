//! 常駐 (常時起動) — G HUB と同じく「閉じても裏で動き続ける」ための部品。
//!
//! - **スタートアップ登録**: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` に
//!   `"<exe>" --background` を書く (管理者権限不要・ユーザー単位)。
//! - **タスクトレイ**: 専用スレッドに隠しウィンドウ + `Shell_NotifyIconW`。
//!   左クリック/ダブルクリックで窓を開き、右クリックでメニュー (開く / 終了)。
//!   エクスプローラー再起動 (`TaskbarCreated`) でアイコンを付け直す。
//! - **閉じても常駐**: 窓の × は `on_window_should_close` で横取りして隠すだけ。
//!   デバイスのワーカー・プロセス監視 (自動プロファイル)・電池ポーリングは動き続ける。
//!
//! GPUI 側とはアトミック変数でやり取りする (トレイスレッドは GPUI に触らない)。
//! UI のティックが `take_request()` を拾って窓を出す/終了する。
//! Windows 以外では何もしない (ビルドだけ通す)。

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// トレイからの依頼。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// 窓を表示して前面へ。
    Show,
    /// アプリを終了。
    Quit,
}

static REQUEST: AtomicU8 = AtomicU8::new(0);
static TRAY_UP: AtomicBool = AtomicBool::new(false);
static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(true);

/// コマンドライン引数。スタートアップ登録はこれを付けて起動する。
pub const BACKGROUND_ARG: &str = "--background";

/// `--background` 付きで起動されたか (ログイン時の自動起動)。
pub fn started_in_background() -> bool {
    std::env::args().any(|a| a == BACKGROUND_ARG)
}

/// UI ティックから呼ぶ: 溜まっている依頼を 1 件取り出す。
pub fn take_request() -> Option<Request> {
    match REQUEST.swap(0, Ordering::AcqRel) {
        1 => Some(Request::Show),
        2 => Some(Request::Quit),
        _ => None,
    }
}

#[allow(dead_code)]
fn post(r: Request) {
    let v = match r {
        Request::Show => 1,
        Request::Quit => 2,
    };
    // Quit は Show で上書きしない。
    if r == Request::Show && REQUEST.load(Ordering::Acquire) == 2 {
        return;
    }
    REQUEST.store(v, Ordering::Release);
}

/// トレイアイコンが出ているか (出ていないのに隠すと戻せないので、隠す前に確認する)。
pub fn tray_active() -> bool {
    TRAY_UP.load(Ordering::Acquire)
}

/// × で閉じたときに常駐するか (prefs から反映)。
pub fn set_close_to_tray(on: bool) {
    CLOSE_TO_TRAY.store(on, Ordering::Release);
}

/// × を押されたとき: 常駐するなら true (呼び出し側は窓を隠して閉じない)。
pub fn should_hide_on_close() -> bool {
    CLOSE_TO_TRAY.load(Ordering::Acquire) && tray_active()
}

// ==================================================================== Windows 実装

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
    };
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
        DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, IsIconic, IsWindowVisible,
        LoadIconW, LoadImageW, PostMessageW, RegisterClassW, RegisterWindowMessageW,
        SetForegroundWindow, ShowWindow, TrackPopupMenu, TranslateMessage, IDI_APPLICATION,
        IMAGE_ICON, LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON, SM_CYSMICON,
        SW_HIDE, SW_RESTORE, SW_SHOW, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU,
        WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const VALUE: &str = "DeviceHub";
    const WM_TRAY: u32 = WM_APP + 1;
    const ID_OPEN: usize = 1;
    const ID_QUIT: usize = 2;

    static TASKBAR_CREATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn command_line() -> Option<String> {
        let exe = std::env::current_exe().ok()?;
        Some(format!("\"{}\" {}", exe.display(), BACKGROUND_ARG))
    }

    fn open_run(access: u32) -> Option<HKEY> {
        let mut key: HKEY = std::ptr::null_mut();
        let path = wide(RUN_KEY);
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, access, &mut key) };
        (rc == 0).then_some(key)
    }

    pub fn autostart_enabled() -> bool {
        let Some(key) = open_run(KEY_QUERY_VALUE) else {
            return false;
        };
        let name = wide(VALUE);
        let mut buf = vec![0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let mut ty = 0u32;
        let rc = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null(),
                &mut ty,
                buf.as_mut_ptr() as *mut u8,
                &mut len,
            )
        };
        unsafe { RegCloseKey(key) };
        if rc != 0 || ty != REG_SZ {
            return false;
        }
        // 別の場所の exe を指していたら「無効」扱い (移動後に設定し直させる)。
        let v = String::from_utf16_lossy(&buf[..(len as usize / 2)]);
        let v = v.trim_end_matches('\0');
        command_line().is_some_and(|c| v.eq_ignore_ascii_case(&c))
    }

    pub fn set_autostart(on: bool) -> Result<(), String> {
        let key = open_run(KEY_SET_VALUE).ok_or("レジストリ (Run) を開けません")?;
        let name = wide(VALUE);
        let rc = if on {
            let cmd = command_line().ok_or("実行ファイルの場所が分かりません")?;
            let data = wide(&cmd);
            unsafe {
                RegSetValueExW(
                    key,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    data.as_ptr() as *const u8,
                    (data.len() * 2) as u32,
                )
            }
        } else {
            let rc = unsafe { RegDeleteValueW(key, name.as_ptr()) };
            // 2 = ERROR_FILE_NOT_FOUND (元から無い) は成功扱い。
            if rc == 2 {
                0
            } else {
                rc
            }
        };
        unsafe { RegCloseKey(key) };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!("レジストリへの書き込みに失敗しました (code {rc})"))
        }
    }

    fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid
    }

    fn add_icon(hwnd: HWND) -> bool {
        let mut nid = tray_data(hwnd);
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        unsafe {
            // exe に埋め込んだアイコン (リソース ID 1、build.rs)。無ければ既定の絵。
            let hinst = GetModuleHandleW(std::ptr::null());
            let icon = LoadImageW(
                hinst,
                // MAKEINTRESOURCE(1): ポインタではなく整数 ID。
                std::ptr::without_provenance::<u16>(1),
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            );
            nid.hIcon = if icon.is_null() {
                LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
            } else {
                icon as _
            };
        }
        let tip = wide("Device HUB — 常駐中 (クリックで開く)");
        let n = tip.len().min(nid.szTip.len() - 1);
        nid.szTip[..n].copy_from_slice(&tip[..n]);
        unsafe { Shell_NotifyIconW(NIM_ADD, &nid) != 0 }
    }

    fn show_menu(hwnd: HWND) {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            let open = wide("Device HUB を開く");
            let quit = wide("終了");
            AppendMenuW(menu, MF_STRING, ID_OPEN, open.as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(menu, MF_STRING, ID_QUIT, quit.as_ptr());
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            // 前面にしておかないとメニュー外クリックで閉じない (MS KB135788)。
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                pt.x,
                pt.y,
                0,
                hwnd,
                std::ptr::null(),
            );
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            match cmd as usize {
                ID_OPEN => post(Request::Show),
                ID_QUIT => post(Request::Quit),
                _ => {}
            }
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_TRAY {
            match (lp & 0xFFFF) as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => post(Request::Show),
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                _ => {}
            }
            return 0;
        }
        let tc = TASKBAR_CREATED.load(Ordering::Relaxed);
        if tc != 0 && msg == tc {
            // エクスプローラーが再起動した: アイコンを付け直す。
            TRAY_UP.store(add_icon(hwnd), Ordering::Release);
            return 0;
        }
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    pub fn start_tray() {
        if TRAY_UP.load(Ordering::Acquire) {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let spawned = std::thread::Builder::new()
            .name("devhub-tray".into())
            .spawn(move || unsafe {
                let class = wide("DeviceHubTray");
                let hinst = GetModuleHandleW(std::ptr::null());
                let mut wc: WNDCLASSW = std::mem::zeroed();
                wc.lpfnWndProc = Some(wndproc);
                wc.hInstance = hinst;
                wc.lpszClassName = class.as_ptr();
                RegisterClassW(&wc);
                let title = wide("Device HUB Tray");
                // 通常の (表示しない) トップレベル窓: TaskbarCreated のブロードキャストを受けるため
                // メッセージ専用窓 (HWND_MESSAGE) にはしない。
                let hwnd = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    title.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    hinst,
                    std::ptr::null::<c_void>(),
                );
                if hwnd.is_null() {
                    let _ = tx.send(false);
                    return;
                }
                let tc = wide("TaskbarCreated");
                TASKBAR_CREATED.store(RegisterWindowMessageW(tc.as_ptr()), Ordering::Relaxed);
                let ok = add_icon(hwnd);
                TRAY_UP.store(ok, Ordering::Release);
                let _ = tx.send(ok);
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
                TRAY_UP.store(false, Ordering::Release);
            });
        if spawned.is_ok() {
            // アイコンが出るまで少しだけ待つ (起動直後に隠す判定に使うため)。
            let _ = rx.recv_timeout(std::time::Duration::from_millis(1500));
        }
    }

    /// 終了前にトレイアイコンを消す (残ると次にマウスを乗せるまで幽霊アイコンが残る)。
    pub fn remove_tray() {
        // トレイ窓は同じプロセス — プロセス終了で消えるが、明示的に消しておく。
        // (窓ハンドルはスレッド内なので、クラス名で探して NIM_DELETE する。)
        unsafe {
            let class = wide("DeviceHubTray");
            let hwnd = windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW(
                class.as_ptr(),
                std::ptr::null(),
            );
            if !hwnd.is_null() {
                Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
            }
        }
        TRAY_UP.store(false, Ordering::Release);
    }

    pub fn hide_window(hwnd: isize) {
        if hwnd != 0 {
            unsafe { ShowWindow(hwnd as HWND, SW_HIDE) };
        }
    }

    /// 隠した窓を戻す (GPUI の activate_window と併用 — こちらは隠れている窓の表示だけ)。
    pub fn show_window(hwnd: isize) {
        if hwnd == 0 {
            return;
        }
        unsafe {
            let h = hwnd as HWND;
            if IsIconic(h) != 0 {
                ShowWindow(h, SW_RESTORE);
            } else {
                ShowWindow(h, SW_SHOW);
            }
            SetForegroundWindow(h);
        }
    }

    pub fn window_hidden(hwnd: isize) -> bool {
        hwnd != 0 && unsafe { IsWindowVisible(hwnd as HWND) == 0 }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn autostart_enabled() -> bool {
        false
    }
    pub fn set_autostart(_on: bool) -> Result<(), String> {
        Err("スタートアップ登録は Windows のみ対応です".into())
    }
    pub fn start_tray() {}
    pub fn remove_tray() {}
    pub fn hide_window(_hwnd: isize) {}
    pub fn show_window(_hwnd: isize) {}
    pub fn window_hidden(_hwnd: isize) -> bool {
        false
    }
}

pub use imp::{hide_window, remove_tray, show_window, start_tray, window_hidden};

/// スタートアップ登録の状態 (0 = 未確認, 1 = 無効, 2 = 有効)。描画のたびに
/// レジストリを読まないようキャッシュする。
static AUTOSTART: AtomicU8 = AtomicU8::new(0);

/// Windows ログイン時に自動起動するか (レジストリが正)。
pub fn autostart_enabled() -> bool {
    match AUTOSTART.load(Ordering::Acquire) {
        0 => {
            let on = imp::autostart_enabled();
            AUTOSTART.store(if on { 2 } else { 1 }, Ordering::Release);
            on
        }
        v => v == 2,
    }
}

/// スタートアップ登録を切り替える。
pub fn set_autostart(on: bool) -> Result<(), String> {
    imp::set_autostart(on)?;
    AUTOSTART.store(if on { 2 } else { 1 }, Ordering::Release);
    Ok(())
}

/// 起動時の初期化: トレイを出し、初回だけスタートアップ登録を行う
/// (「常時起動」が既定。設定画面で外せる)。返り値: prefs を保存し直す必要があるか。
pub fn init(prefs: &mut crate::prefs::Prefs) -> bool {
    set_close_to_tray(prefs.close_to_tray);
    start_tray();
    if !prefs.autostart_initialized && cfg!(windows) {
        prefs.autostart_initialized = true;
        let _ = set_autostart(true);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_is_not_overwritten_by_show() {
        post(Request::Quit);
        post(Request::Show);
        assert_eq!(take_request(), Some(Request::Quit));
        assert_eq!(take_request(), None);
        post(Request::Show);
        assert_eq!(take_request(), Some(Request::Show));
    }
}
