//! 既知デバイスの対応表。
//!
//! **新しいデバイスを追加するときは、ここに 1 行足すのが最初の作業**
//! (手順全体は `docs/ADD_DEVICE.md`)。
//!
//! VID/PID は実機の列挙か、Web configurator の JS バンドル
//! (`navigator.hid.requestDevice` の filters / mouse.fit の `Config/env-models.json`) から採る。
//! 1 台のデバイスが有線・ドングル・8K ドングルで別々の PID を名乗ることがあるので、
//! それぞれ 1 行ずつ登録する (同じ model / driver を指す)。

use crate::model::{DeviceKind, DriverKind, LayoutKey, Transport};

/// 既知デバイス 1 件。
#[derive(Clone, Copy, Debug)]
pub struct KnownDevice {
    pub vid: u16,
    pub pid: u16,
    /// 設定用インターフェースの絞り込み (vendor-defined usage page)。分からなければ None。
    pub usage_page: Option<u16>,
    pub usage: Option<u16>,
    pub kind: DeviceKind,
    pub vendor: &'static str,
    /// 機種識別子 (ドライバ内で機種差を引くキー)。
    pub model: &'static str,
    /// 表示名。
    pub name: &'static str,
    pub transport: Transport,
    pub driver: DriverKind,
    /// 実機で検証済みか (false でも列挙はする。docs の対応表に反映するための印)。
    pub verified: bool,
}

const fn mf(
    pid: u16,
    model: &'static str,
    name: &'static str,
    transport: Transport,
    verified: bool,
) -> KnownDevice {
    KnownDevice {
        vid: 0x33E4,
        pid,
        usage_page: Some(0xFF02),
        usage: Some(0x02),
        kind: DeviceKind::Mouse,
        vendor: "G-Wolves",
        model,
        name,
        transport,
        driver: DriverKind::Mousefit,
        verified,
    }
}

/// 対応表。新デバイスはここへ追加する。
///
/// mouse.fit の PID は `https://www.mouse.fit/Config/env-models.json` (2026-11 取得) の
/// `PIDWired` / `PIDWireless` / `PIDWireless4K8K` から。usage_page/usage は
/// HTS Plus Pro 実機 (0xFF02 / 0x02) と同じと仮定 — **他機種は実機で要確認**。
pub const KNOWN: &[KnownDevice] = &[
    // ---------------------------------------------------------------- キーボード
    KnownDevice {
        vid: 0x1CA6,
        pid: 0x3009, // SU66 Max — 実機列挙で確認 (hub.xsyd.top の filters にも 12297 で載る)
        usage_page: Some(0xFFB0),
        usage: Some(0x01),
        kind: DeviceKind::Keyboard,
        vendor: "XSYD",
        model: "su66max",
        name: "SU66 Max",
        transport: Transport::Wired,
        driver: DriverKind::Xsyd,
        verified: true,
    },
    // ---------------------------------------------------------------- マウス (mouse.fit)
    // HTS Plus Pro (PAW3955 版 JM_052_3955)。有線も report-8/EEPROM 経路 — 実機検証済。
    mf(
        0x5219,
        "hts-plus-pro",
        "HTS Plus Pro",
        Transport::Wired,
        true,
    ),
    // 3955/3950 系共通の 8K ドングル (report-8/EEPROM 経路、実機検証済)。
    // 内側の機種は問い合わせられないので model で機種差を引く。
    // 表示名はペア機種名 (受信機自体の製品文字列ではなく中身のマウス名で出す)。
    mf(
        0x3854,
        "gw-dongle-3854",
        "HTS Plus Pro",
        Transport::Wireless24G,
        true,
    ),
    // HTX Ultra = mouse.fit の "HTXU"。センサー違いで 3 系統ある。
    mf(0x5608, "htx-ultra", "HTX Ultra", Transport::Wired, false),
    mf(
        0x5617,
        "htx-ultra",
        "HTX Ultra",
        Transport::Wireless24G,
        false,
    ),
    mf(
        0x5618,
        "htx-ultra-3950",
        "HTX Ultra (3950)",
        Transport::Wired,
        false,
    ),
    mf(
        0x5619,
        "htx-ultra-3955",
        "HTX Ultra (3955)",
        Transport::Wired,
        false,
    ),
    // ---------------------------------------------------------------- USB DAC (peq.szwalkplay.com)
    // TP55 — 実機列挙: 3302:4383 if=0 up=000C u=01 "TP55"。
    // 設定は consumer control IF 上の report 0x4B (CB 系 S1 プロトコル)。
    // [1,255] でブートローダ (0666:0888 "USB20") へ移行するので注意。
    KnownDevice {
        vid: 0x3302,
        pid: 0x4383,
        usage_page: Some(0x000C),
        usage: Some(0x01),
        kind: DeviceKind::Other,
        vendor: "WalkPlay",
        model: "tp55",
        name: "TP55",
        transport: Transport::Wired,
        driver: DriverKind::Walkplay,
        verified: false,
    },
    // ATK F1 V3 ULTIMATE+ — 実機列挙 373b:12a7。config = ff02/u2 (report-8/EEPROM)。
    // 製品文字列 "ATK F1 V3 ULTIMATE+"。CID/MID=1,62 (公式 DB と一致)。
    KnownDevice {
        vid: 0x373B,
        pid: 0x12A7,
        usage_page: Some(0xFF02),
        usage: Some(0x02),
        kind: DeviceKind::Mouse,
        vendor: "ATK",
        model: "atk-f1v3u",
        name: "ATK F1 V3 ULTIMATE+",
        transport: Transport::Wired,
        driver: DriverKind::Atk,
        verified: true,
    },
    // ATK 8K ドングル "Wireless mouse 8k dongle-L" (373b:1278)。
    // 同じ 16B/report-8 プロトコルを中継 — 内側の機種は cmd16 CID/MID で確定。
    // マウス休眠中は中継コマンドがタイムアウトする (ドングル自身の cmd29 等は応答)。
    KnownDevice {
        vid: 0x373B,
        pid: 0x1278,
        usage_page: Some(0xFF02),
        usage: Some(0x02),
        kind: DeviceKind::Mouse,
        vendor: "ATK",
        model: "atk-dongle",
        name: "ATK ワイヤレスドングル",
        transport: Transport::Wireless24G,
        driver: DriverKind::Atk,
        verified: true,
    },
    // ---------------------------------------------------------------- マウス (lamzu.net)
    // LAMZU Maya X V2 (LM20) 有線 — 実機列挙 37b0:0044。
    // 設定 = if2 FFFF/00 の 64B feature report、通知 = if1 FFA0/01 input report。
    KnownDevice {
        vid: 0x37B0,
        pid: 0x0044,
        usage_page: Some(0xFFFF),
        usage: Some(0x00),
        kind: DeviceKind::Mouse,
        vendor: "LAMZU",
        model: "maya-x-v2",
        name: "LAMZU MAYA X V2",
        transport: Transport::Wired,
        driver: DriverKind::Lamzu,
        verified: true,
    },
    // LAMZU Orbital 8K ドングル (37b0:0048) — 同じ FFFF:00 feature /
    // FFA0:01 通知レイアウトでマウス設定を中継する (実機列挙で確認)。
    KnownDevice {
        vid: 0x37B0,
        pid: 0x0048,
        usage_page: Some(0xFFFF),
        usage: Some(0x00),
        kind: DeviceKind::Mouse,
        vendor: "LAMZU",
        model: "lamzu-dongle",
        // 初期値は受信機名 — ドライバが GetDevicePID でペア先を確定してから
        // info.name/model を製品名 (例: LAMZU Maya X V2 / maya-x-v2) に確定する。
        name: "LAMZU 8K レシーバー",
        transport: Transport::Wireless24G,
        driver: DriverKind::Lamzu,
        verified: true,
    },
    // 同じ LM20 世代 (公式 env-models.json)。プロトコルは MAYA X V2 と共通 — 実機未確認。
    KnownDevice {
        vid: 0x37B0,
        pid: 0x0050,
        usage_page: Some(0xFFFF),
        usage: Some(0x00),
        kind: DeviceKind::Mouse,
        vendor: "LAMZU",
        model: "maya-v2",
        name: "LAMZU MAYA V2",
        transport: Transport::Wired,
        driver: DriverKind::Lamzu,
        verified: false,
    },
    // 同じ LM20 世代 (公式 env-models.json)。プロトコルは MAYA X V2 と共通 — 実機未確認。
    KnownDevice {
        vid: 0x37B0,
        pid: 0x0052,
        usage_page: Some(0xFFFF),
        usage: Some(0x00),
        kind: DeviceKind::Mouse,
        vendor: "LAMZU",
        model: "maya-m",
        name: "LAMZU MAYA M",
        transport: Transport::Wired,
        driver: DriverKind::Lamzu,
        verified: false,
    },
    // 同じ LM20 世代 (公式 env-models.json)。プロトコルは MAYA X V2 と共通 — 実機未確認。
    KnownDevice {
        vid: 0x37B0,
        pid: 0x0056,
        usage_page: Some(0xFFFF),
        usage: Some(0x00),
        kind: DeviceKind::Mouse,
        vendor: "LAMZU",
        model: "orcus-v2",
        name: "LAMZU ORCUS V2",
        transport: Transport::Wired,
        driver: DriverKind::Lamzu,
        verified: false,
    },
    // 同じ LM20 世代 (公式 env-models.json)。プロトコルは MAYA X V2 と共通 — 実機未確認。
    KnownDevice {
        vid: 0x37B0,
        pid: 0x0046,
        usage_page: Some(0xFFFF),
        usage: Some(0x00),
        kind: DeviceKind::Mouse,
        vendor: "LAMZU",
        model: "atlantis-mini-v2",
        name: "LAMZU ATLANTIS MINI V2",
        transport: Transport::Wired,
        driver: DriverKind::Lamzu,
        verified: false,
    },
    // ATLANTIS MINI V2 用の 8K 受信機 (公式 _8KDongle=0054)。ペア先は GetDevicePID で判定。
    KnownDevice {
        vid: 0x37B0,
        pid: 0x0054,
        usage_page: Some(0xFFFF),
        usage: Some(0x00),
        kind: DeviceKind::Mouse,
        vendor: "LAMZU",
        model: "lamzu-dongle",
        name: "LAMZU 8K レシーバー",
        transport: Transport::Wireless24G,
        driver: DriverKind::Lamzu,
        verified: false,
    },
    // ---------------------------------------------------------------- マウス (Logitech HID++ 2.0)
    // G HUB 置換。設定は HID++ long report (0x11, 20B) の vendor コレクション FF00/0002。
    // PID は libratbag のデバイス定義 (usb:046d:c09b / 無線 wpid 40a9) と
    // Linux hid-logitech-dj のパッチ (SL2 受信機 0xc54d) から — **実機未確認**。
    // 機能は HID++ の feature 問い合わせで決まるので、caps は開いた後に組み立てる。
    lg(
        0xC09B,
        "pro-x-superlight-2",
        "PRO X SUPERLIGHT 2",
        Transport::Wired,
    ),
    // SL2 同梱の Lightspeed 受信機 (device index 1 にマウス)。
    lg(
        0xC54D,
        "lightspeed-c54d",
        "PRO X SUPERLIGHT 2",
        Transport::Wireless24G,
    ),
    // 汎用 Lightspeed 受信機 (G502 X / PRO X SUPERLIGHT 等)。中身の名前は
    // feature 0x0005 (DeviceName) で取り直す。
    lg(
        0xC547,
        "lightspeed-c547",
        "Logitech LIGHTSPEED",
        Transport::Wireless24G,
    ),
    // ---------------------------------------------------------------- キーボード (MonsGeek RY5088)
    // FUN60 Ultra は同じ PID を複数機種と共有している (公式デバイス DB の id
    // 2307/2352/2381/2387 が FUN60 Ultra)。PID 一致で開き、GET_USB_VERSION (0x8F)
    // の device id で機種名を確定する。config IF = 2 (usage FFFF:02, 64B feature)。
    mg(0x5029, "ry5088-5029"),
    mg(0x502D, "ry5088-502d"),
    mg(0x5030, "ry5088-5030"),
];

const fn lg(
    pid: u16,
    model: &'static str,
    name: &'static str,
    transport: Transport,
) -> KnownDevice {
    KnownDevice {
        vid: 0x046D,
        pid,
        usage_page: Some(0xFF00),
        usage: Some(0x02),
        kind: DeviceKind::Mouse,
        vendor: "Logitech",
        model,
        name,
        transport,
        driver: DriverKind::Logitech,
        verified: false,
    }
}

const fn mg(pid: u16, model: &'static str) -> KnownDevice {
    KnownDevice {
        vid: 0x3151,
        pid,
        usage_page: Some(0xFFFF),
        usage: Some(0x02),
        kind: DeviceKind::Keyboard,
        vendor: "MonsGeek",
        model,
        name: "MonsGeek FUN60 Ultra",
        transport: Transport::Wired,
        driver: DriverKind::Monsgeek,
        verified: false,
    }
}

/// FUN60 Ultra (公式 key layout `Common61_gk06`) の物理配置。
///
/// `id` = RY5088 のマトリクス位置 (0..128、列優先で 1 列 6 スロット:
/// `pos = col * 6 + row`)。per-key のトラベル・キーマップはこの位置で指定する。
/// 位置表は公式デバイス DB の既定キーマトリクス (layout fallback) から —
/// **実機で要確認** (Fn キーはマトリクスに出ないので含めない)。
pub fn fun60_layout() -> Vec<LayoutKey> {
    fn k(pos: u16, x: f32, y: f32, w: f32, label: &str) -> LayoutKey {
        LayoutKey {
            id: pos,
            x,
            y,
            w,
            h: 1.0,
            label: label.into(),
        }
    }
    vec![
        // Row 0
        k(1, 0.0, 0.0, 1.0, "Esc"),
        k(7, 1.0, 0.0, 1.0, "1"),
        k(13, 2.0, 0.0, 1.0, "2"),
        k(19, 3.0, 0.0, 1.0, "3"),
        k(25, 4.0, 0.0, 1.0, "4"),
        k(31, 5.0, 0.0, 1.0, "5"),
        k(37, 6.0, 0.0, 1.0, "6"),
        k(43, 7.0, 0.0, 1.0, "7"),
        k(49, 8.0, 0.0, 1.0, "8"),
        k(55, 9.0, 0.0, 1.0, "9"),
        k(61, 10.0, 0.0, 1.0, "0"),
        k(67, 11.0, 0.0, 1.0, "-"),
        k(73, 12.0, 0.0, 1.0, "="),
        k(79, 13.0, 0.0, 2.0, "Backspace"),
        // Row 1
        k(2, 0.0, 1.0, 1.5, "Tab"),
        k(8, 1.5, 1.0, 1.0, "Q"),
        k(14, 2.5, 1.0, 1.0, "W"),
        k(20, 3.5, 1.0, 1.0, "E"),
        k(26, 4.5, 1.0, 1.0, "R"),
        k(32, 5.5, 1.0, 1.0, "T"),
        k(38, 6.5, 1.0, 1.0, "Y"),
        k(44, 7.5, 1.0, 1.0, "U"),
        k(50, 8.5, 1.0, 1.0, "I"),
        k(56, 9.5, 1.0, 1.0, "O"),
        k(62, 10.5, 1.0, 1.0, "P"),
        k(68, 11.5, 1.0, 1.0, "["),
        k(74, 12.5, 1.0, 1.0, "]"),
        k(80, 13.5, 1.0, 1.5, "\\"),
        // Row 2
        k(3, 0.0, 2.0, 1.75, "Caps"),
        k(9, 1.75, 2.0, 1.0, "A"),
        k(15, 2.75, 2.0, 1.0, "S"),
        k(21, 3.75, 2.0, 1.0, "D"),
        k(27, 4.75, 2.0, 1.0, "F"),
        k(33, 5.75, 2.0, 1.0, "G"),
        k(39, 6.75, 2.0, 1.0, "H"),
        k(45, 7.75, 2.0, 1.0, "J"),
        k(51, 8.75, 2.0, 1.0, "K"),
        k(57, 9.75, 2.0, 1.0, "L"),
        k(63, 10.75, 2.0, 1.0, ";"),
        k(69, 11.75, 2.0, 1.0, "'"),
        k(81, 12.75, 2.0, 2.25, "Enter"),
        // Row 3
        k(4, 0.0, 3.0, 2.25, "LShift"),
        k(16, 2.25, 3.0, 1.0, "Z"),
        k(22, 3.25, 3.0, 1.0, "X"),
        k(28, 4.25, 3.0, 1.0, "C"),
        k(34, 5.25, 3.0, 1.0, "V"),
        k(40, 6.25, 3.0, 1.0, "B"),
        k(46, 7.25, 3.0, 1.0, "N"),
        k(52, 8.25, 3.0, 1.0, "M"),
        k(58, 9.25, 3.0, 1.0, ","),
        k(64, 10.25, 3.0, 1.0, "."),
        k(70, 11.25, 3.0, 1.0, "/"),
        k(76, 12.25, 3.0, 2.75, "RShift"),
        // Row 4 (Fn はマトリクス外)
        k(5, 0.0, 4.0, 1.25, "LCtrl"),
        k(17, 1.25, 4.0, 1.25, "LWin"),
        k(23, 2.5, 4.0, 1.25, "LAlt"),
        k(41, 3.75, 4.0, 6.25, "Space"),
        k(65, 10.0, 4.0, 1.25, "RAlt"),
        k(77, 11.25, 4.0, 1.25, "Menu"),
        k(83, 12.5, 4.0, 1.25, "RCtrl"),
    ]
}

/// FUN60 Ultra の既定キーマトリクス (位置 → HID usage)。layer 0 の
/// 「既定に戻す」と、位置表の自己検査 (テスト) に使う。
pub const FUN60_DEFAULT_MATRIX: [u8; 84] = [
    0, 41, 43, 57, 225, 224, 0, 30, 20, 4, 0, 0, 0, 31, 26, 22, 29, 227, 0, 32, 8, 7, 27, 226, 0,
    33, 21, 9, 6, 0, 0, 34, 23, 10, 25, 0, 0, 35, 28, 11, 5, 44, 0, 36, 24, 13, 17, 0, 0, 37, 12,
    14, 16, 0, 0, 38, 18, 15, 54, 0, 0, 39, 19, 51, 55, 230, 0, 45, 47, 52, 56, 0, 0, 46, 48, 0,
    229, 101, 0, 42, 49, 40, 0, 228,
];

/// 列挙結果 (VID/PID/usage) に合う既知デバイス。`0xFFFF` のプレースホルダは一致しない。
pub fn match_device(
    vid: u16,
    pid: u16,
    usage_page: u16,
    usage: u16,
) -> Option<&'static KnownDevice> {
    KNOWN.iter().find(|k| {
        k.vid == vid
            && k.pid == pid
            && k.vid != 0xFFFF
            && k.usage_page.is_none_or(|p| p == usage_page)
            && k.usage.is_none_or(|u| u == usage)
    })
}

// ------------------------------------------------------------------ レイアウト

/// su66max の物理レイアウト (実機マトリクスから導出: 15col×5row + 分割スペース)。
///
/// `index` = プロトコルのマトリクス番号 `(matrix_row << 4) | matrix_col`
/// (matrix_row は 1..=5、col は 0..=14 — 実機 dump で確定)。
/// 見た目の x/y/w は 65% 標準の推定配置。SU66MAX_MATRIX が index→(row,col) の対応表。
pub fn su66max_layout() -> Vec<LayoutKey> {
    fn k(row: u8, col: u8, x: f32, y: f32, w: f32, label: &str) -> LayoutKey {
        LayoutKey {
            id: ((row as u16) << 4) | col as u16,
            x,
            y,
            w,
            h: 1.0,
            label: label.into(),
        }
    }
    vec![
        // Row 0: Esc + 数字段 + Backspace (右列キー無し)
        k(1, 0, 0.0, 0.0, 1.0, "Esc"),
        k(1, 1, 1.0, 0.0, 1.0, "1"),
        k(1, 2, 2.0, 0.0, 1.0, "2"),
        k(1, 3, 3.0, 0.0, 1.0, "3"),
        k(1, 4, 4.0, 0.0, 1.0, "4"),
        k(1, 5, 5.0, 0.0, 1.0, "5"),
        k(1, 6, 6.0, 0.0, 1.0, "6"),
        k(1, 7, 7.0, 0.0, 1.0, "7"),
        k(1, 8, 8.0, 0.0, 1.0, "8"),
        k(1, 9, 9.0, 0.0, 1.0, "9"),
        k(1, 10, 10.0, 0.0, 1.0, "0"),
        k(1, 11, 11.0, 0.0, 1.0, "-"),
        k(1, 12, 12.0, 0.0, 1.0, "="),
        k(1, 13, 13.0, 0.0, 2.0, "Backspace"),
        // Row 1: Tab + QWERTY
        k(2, 0, 0.0, 1.0, 1.5, "Tab"),
        k(2, 1, 1.5, 1.0, 1.0, "Q"),
        k(2, 2, 2.5, 1.0, 1.0, "W"),
        k(2, 3, 3.5, 1.0, 1.0, "E"),
        k(2, 4, 4.5, 1.0, 1.0, "R"),
        k(2, 5, 5.5, 1.0, 1.0, "T"),
        k(2, 6, 6.5, 1.0, 1.0, "Y"),
        k(2, 7, 7.5, 1.0, 1.0, "U"),
        k(2, 8, 8.5, 1.0, 1.0, "I"),
        k(2, 9, 9.5, 1.0, 1.0, "O"),
        k(2, 10, 10.5, 1.0, 1.0, "P"),
        k(2, 11, 11.5, 1.0, 1.0, "["),
        k(2, 12, 12.5, 1.0, 1.0, "]"),
        k(2, 13, 13.5, 1.0, 1.5, "\\"),
        // Row 2: Caps + ASDF + Enter
        k(3, 0, 0.0, 2.0, 1.75, "Caps"),
        k(3, 1, 1.75, 2.0, 1.0, "A"),
        k(3, 2, 2.75, 2.0, 1.0, "S"),
        k(3, 3, 3.75, 2.0, 1.0, "D"),
        k(3, 4, 4.75, 2.0, 1.0, "F"),
        k(3, 5, 5.75, 2.0, 1.0, "G"),
        k(3, 6, 6.75, 2.0, 1.0, "H"),
        k(3, 7, 7.75, 2.0, 1.0, "J"),
        k(3, 8, 8.75, 2.0, 1.0, "K"),
        k(3, 9, 9.75, 2.0, 1.0, "L"),
        k(3, 10, 10.75, 2.0, 1.0, ";"),
        k(3, 11, 11.75, 2.0, 1.0, "'"),
        k(3, 13, 12.75, 2.0, 2.25, "Enter"),
        // Row 3: LShift + ZXCV + RShift + ↑ + Del — GH60 15u の矢印付き行
        // (実機と同じ 1.75 Shift / 1.25 RShift / 1u の右列)。
        k(4, 0, 0.0, 3.0, 1.75, "LShift"),
        k(4, 2, 1.75, 3.0, 1.0, "Z"),
        k(4, 3, 2.75, 3.0, 1.0, "X"),
        k(4, 4, 3.75, 3.0, 1.0, "C"),
        k(4, 5, 4.75, 3.0, 1.0, "V"),
        k(4, 6, 5.75, 3.0, 1.0, "B"),
        k(4, 7, 6.75, 3.0, 1.0, "N"),
        k(4, 8, 7.75, 3.0, 1.0, "M"),
        k(4, 9, 8.75, 3.0, 1.0, ","),
        k(4, 10, 9.75, 3.0, 1.0, "."),
        k(4, 11, 10.75, 3.0, 1.0, "/"),
        k(4, 12, 11.75, 3.0, 1.25, "RShift"),
        k(4, 13, 13.0, 3.0, 1.0, "↑"),
        k(4, 14, 14.0, 3.0, 1.0, "Del"),
        // Row 4: 修飾 + 分割 Space (実機仕様 2.25+1.75+2.25) + RAlt/Fn + ←↓→
        k(5, 0, 0.0, 4.0, 1.25, "LCtrl"),
        k(5, 1, 1.25, 4.0, 1.25, "LWin"),
        k(5, 2, 2.5, 4.0, 1.25, "LAlt"),
        k(5, 4, 3.75, 4.0, 2.25, "Space"),
        k(5, 6, 6.0, 4.0, 1.75, "Space"),
        k(5, 8, 7.75, 4.0, 2.25, "Space"),
        k(5, 9, 10.0, 4.0, 1.0, "RAlt"),
        k(5, 10, 11.0, 4.0, 1.0, "Fn"),
        k(5, 12, 12.0, 4.0, 1.0, "←"),
        k(5, 13, 13.0, 4.0, 1.0, "↓"),
        k(5, 14, 14.0, 4.0, 1.0, "→"),
    ]
}

/// su66max_layout の id → (matrix_row, matrix_col)。
/// キーコードストリーム上の位置は `row * 30 + col` (30B 行×最大 8 行)。
pub fn su66max_matrix_pos(id: u16) -> (u8, u8) {
    ((id >> 4) as u8, (id & 0x0f) as u8)
}
