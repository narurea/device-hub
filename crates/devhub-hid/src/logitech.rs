//! Logitech G HUB 置換 — HID++ 2.0 ドライバ (PRO X SUPERLIGHT 2 を主対象)。
//!
//! # 解析の出典
//!
//! G HUB 本体は解析していない。HID++ 2.0 は公開資料と OSS 実装が揃っているので、
//! そこから仕様を起こして独自に実装した (コードの流用はしていない):
//!
//! - libratbag `src/hidpp20.c` / `driver-hidpp20.c` (MIT) — 0x8100 オンボードプロファイルの
//!   セクタ形式・ボタン割当の 4 バイト表現・CRC・ディレクトリ/ROM の扱い。
//! - Solaar `lib/logitech_receiver/hidpp20.py` / `settings_templates.py` (GPL) — 0x8061 拡張
//!   レポートレート、0x1004 統合バッテリー、0x0003 ファーム情報の読み方 (事実のみ参照)。
//! - libratbag のデバイス定義 (`usb:046d:c09b`, 無線 wpid `40a9`) と Linux
//!   `hid-logitech-dj` のパッチ (SL2 受信機 `0xc54d`)。
//!
//! 詳細は `docs/PROTOCOL_NOTES.md` §6。**実機未検証** — 書き込みは公開実装と同じ手順
//! (読み → 変更 → CRC → 書き → ディレクトリ) に揃え、壊れたセクタは ROM へ戻せるようにしてある。
//!
//! # ワイヤ形式
//!
//! long report `0x11` (20B): `[0x11, dev_idx, feat_idx, func<<4 | sw_id, params×16]`。
//! エラー: `[0x11, dev_idx, 0xFF, feat_idx, func<<4|sw_id, code, …]`。
//! dev_idx は有線 0xFF、Lightspeed 受信機経由は 1 (1..6 を ping で探す)。
//! feature index は ROOT (index 0) の `getFeature(id)` で引く。
//!
//! # 設定の置き場所
//!
//! G HUB を使わない運用では本体の **オンボードメモリ (0x8100)** が設定の実体。
//! DPI ステージ・ボタン割当・(1000Hz 以下の) レポートレートはアクティブな
//! プロファイルのセクタ (既定 256B) に入っている:
//!
//! ```text
//!   0 report_rate (ms)   1 default_dpi idx   2 shift_dpi idx   3..13 dpi[5] LE16
//!  13 color[3]          16 power_mode        17 angle_snap      28 powersave LE16
//!  30 poweroff LE16     32 buttons[16]×4     96 gshift[16]×4   160 name UTF-16LE[24]
//! 208 leds …           size-2 CRC-CCITT (BE)
//! ```
//!
//! 2000Hz 以上は 0x8061 (拡張レポートレート) でしか表せないので、そちらを優先して使う。

use devhub_core::backend::*;
use devhub_core::model::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LONG: u8 = 0x11;
const LONG_LEN: usize = 20;
/// 自分の要求を識別する software id (1..15、0 は通知用)。
const SW_ID: u8 = 0x0B;

/// HID++ 2.0 の feature id。
pub mod feat {
    pub const ROOT: u16 = 0x0000;
    pub const FW_VERSION: u16 = 0x0003;
    pub const NAME: u16 = 0x0005;
    pub const BATTERY_STATUS: u16 = 0x1000;
    pub const UNIFIED_BATTERY: u16 = 0x1004;
    pub const ADJUSTABLE_DPI: u16 = 0x2201;
    /// SL2 など新しいゲーミングマウスの DPI (X/Y・LOD・本体プロファイルの DPI 段)。
    pub const EXT_DPI: u16 = 0x2202;
    pub const REPORT_RATE: u16 = 0x8060;
    pub const EXT_REPORT_RATE: u16 = 0x8061;
    pub const ONBOARD_PROFILES: u16 = 0x8100;
}

/// 0x8100 のファンクション (上位ニブル = function id)。
mod ob {
    pub const GET_DESC: u8 = 0x0;
    pub const SET_MODE: u8 = 0x1;
    pub const GET_MODE: u8 = 0x2;
    pub const SET_PROFILE: u8 = 0x3;
    pub const GET_PROFILE: u8 = 0x4;
    pub const MEM_READ: u8 = 0x5;
    pub const MEM_ADDR_WRITE: u8 = 0x6;
    pub const MEM_WRITE: u8 = 0x7;
    pub const MEM_WRITE_END: u8 = 0x8;
    pub const GET_DPI_INDEX: u8 = 0xB;
    pub const SET_DPI_INDEX: u8 = 0xC;
    pub const MODE_ONBOARD: u8 = 1;
}

/// ROM (工場出荷) プロファイルのセクタ基点。
const ROM_BASE: u16 = 0x0100;

// ==================================================================== 純関数 (テスト対象)

/// CRC-16/CCITT-FALSE (init 0xFFFF, poly 0x1021)。プロファイルセクタ末尾 2B (BE)。
pub fn crc_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// セクタの CRC が正しいか。
pub fn sector_valid(data: &[u8]) -> bool {
    data.len() >= 2 && {
        let n = data.len();
        crc_ccitt(&data[..n - 2]) == u16::from_be_bytes([data[n - 2], data[n - 1]])
    }
}

/// セクタ末尾へ CRC を書く。
pub fn seal_sector(data: &mut [u8]) {
    let n = data.len();
    let c = crc_ccitt(&data[..n - 2]);
    data[n - 2..].copy_from_slice(&c.to_be_bytes());
}

/// 0x8100 getDescription の応答。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObDesc {
    pub memory_model: u8,
    pub profile_format: u8,
    pub macro_format: u8,
    pub profile_count: u8,
    pub rom_profile_count: u8,
    pub button_count: u8,
    pub sector_count: u8,
    pub sector_size: u16,
}

impl ObDesc {
    pub fn parse(p: &[u8]) -> Option<Self> {
        if p.len() < 9 {
            return None;
        }
        Some(Self {
            memory_model: p[0],
            profile_format: p[1],
            macro_format: p[2],
            profile_count: p[3],
            rom_profile_count: p[4],
            button_count: p[5],
            sector_count: p[6],
            sector_size: u16::from_be_bytes([p[7], p[8]]),
        })
    }
    /// libratbag/Solaar と同じ判定: G402 系メモリ (1) とプロファイル形式 1..=5 だけ扱う。
    pub fn supported(&self) -> bool {
        self.memory_model == 1
            && (1..=5).contains(&self.profile_format)
            && self.sector_size >= 64
            && self.profile_count > 0
    }
}

/// プロファイルディレクトリ (セクタ 0 / ROM は 0x0100): `[addr BE16, enabled, 0]` の列、
/// `FFFF` で終わる。無効 (全 0 / 全 FF / CRC 不正) なら None。
pub fn parse_directory(data: &[u8], max: usize) -> Option<Vec<(u16, bool)>> {
    if data.len() < 4 || data[..4] == [0, 0, 0, 0] || data[..4] == [0xFF; 4] {
        return None;
    }
    let mut out = vec![];
    for i in 0..max {
        let o = i * 4;
        if o + 4 > data.len() {
            break;
        }
        let addr = u16::from_be_bytes([data[o], data[o + 1]]);
        if addr == 0xFFFF {
            break;
        }
        out.push((addr, data[o + 2] != 0));
    }
    (!out.is_empty()).then_some(out)
}

/// ユーザー領域のディレクトリセクタを作る。
pub fn build_directory(entries: &[(u16, bool)], size: usize) -> Vec<u8> {
    let mut d = vec![0xFFu8; size];
    let mut o = 0;
    for (addr, en) in entries {
        d[o..o + 2].copy_from_slice(&addr.to_be_bytes());
        d[o + 2] = *en as u8;
        d[o + 3] = 0;
        o += 4;
    }
    d[o..o + 4].copy_from_slice(&[0xFF, 0xFF, 0, 0]);
    seal_sector(&mut d);
    d
}

/// プロファイル内オフセット。
pub mod off {
    pub const REPORT_RATE: usize = 0;
    pub const DEFAULT_DPI: usize = 1;
    pub const SHIFT_DPI: usize = 2;
    pub const DPI: usize = 3;
    pub const BUTTONS: usize = 32;
    pub const NAME: usize = 160;
}

/// プロファイルの DPI 5 枠 (0 / 0xFFFF は未使用)。
pub fn profile_dpis(p: &[u8]) -> [u16; 5] {
    let mut o = [0u16; 5];
    for (i, d) in o.iter_mut().enumerate() {
        let a = off::DPI + i * 2;
        *d = u16::from_le_bytes([p[a], p[a + 1]]);
    }
    o
}

pub fn set_profile_dpis(p: &mut [u8], dpis: &[u32]) {
    for i in 0..5 {
        let v = dpis.get(i).copied().unwrap_or(0).min(0xFFFE) as u16;
        let a = off::DPI + i * 2;
        p[a..a + 2].copy_from_slice(&v.to_le_bytes());
    }
}

/// プロファイル名 (UTF-16LE 24 文字、未設定は全 FF)。
pub fn profile_name(p: &[u8]) -> String {
    if p.len() < off::NAME + 48 || p[off::NAME..off::NAME + 48].iter().all(|&b| b == 0xFF) {
        return String::new();
    }
    let u: Vec<u16> = p[off::NAME..off::NAME + 48]
        .chunks(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0 && c != 0xFFFF)
        .collect();
    String::from_utf16_lossy(&u)
}

pub fn set_profile_name(p: &mut [u8], name: &str) {
    let r = &mut p[off::NAME..off::NAME + 48];
    if name.is_empty() {
        r.fill(0xFF);
        return;
    }
    r.fill(0);
    for (i, c) in name.encode_utf16().take(23).enumerate() {
        r[i * 2..i * 2 + 2].copy_from_slice(&c.to_le_bytes());
    }
}

/// レポートレート (profile byte = 1 報告あたりの ms)。
pub fn rate_from_ms(ms: u8) -> Option<i64> {
    match ms {
        1 => Some(1000),
        2 => Some(500),
        4 => Some(250),
        8 => Some(125),
        _ => None,
    }
}

/// 0x8061 のレート index (0=125Hz … 6=8000Hz)。
pub const EXT_RATES: [i64; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];

// ---- ボタン (プロファイル内 4 バイト)
//
// [0x80, 1, mask BE16]       マウスボタン (bit0 左, 1 右, 2 中, 3 戻る, 4 進む, 6/7 チルト)
// [0x80, 2, mods, usage]     キー
// [0x80, 3, usage BE16]      コンシューマ (メディア)
// [0x80, 0, …]               何もしない
// [0x90, special, 0, prof]   特殊 (DPI 送り・プロファイル送り等)
// [0x0n, …]                  マクロ (アプリでは往復保存のみ)
// [0xFF, …]                  無効

const MEDIA_USAGES: &[(MediaFn, u16)] = &[
    (MediaFn::PlayPause, 0x00CD),
    (MediaFn::Stop, 0x00B7),
    (MediaFn::Next, 0x00B5),
    (MediaFn::Prev, 0x00B6),
    (MediaFn::VolumeUp, 0x00E9),
    (MediaFn::VolumeDown, 0x00EA),
    (MediaFn::Mute, 0x00E2),
    (MediaFn::Calculator, 0x0192),
    (MediaFn::MyComputer, 0x0194),
    (MediaFn::Browser, 0x0196),
    (MediaFn::Mail, 0x018A),
    (MediaFn::BrightnessUp, 0x006F),
    (MediaFn::BrightnessDown, 0x0070),
    (MediaFn::Refresh, 0x0227),
];

const MOUSE_BITS: &[(MouseFn, u16)] = &[
    (MouseFn::Left, 0x0001),
    (MouseFn::Right, 0x0002),
    (MouseFn::Middle, 0x0004),
    (MouseFn::Back, 0x0008),
    (MouseFn::Forward, 0x0010),
    (MouseFn::ScrollLeft, 0x0040),
    (MouseFn::ScrollRight, 0x0080),
];

/// 4 バイト → 割当。`dpis` は DPI シフト先の表示用 (shift index を DPI に直す)。
pub fn decode_button(b: &[u8], dpis: &[u16; 5], shift_idx: u8) -> ButtonAction {
    let raw = || ButtonAction::Unsupported {
        ty: b[0],
        params: b[1..4].to_vec(),
    };
    match b[0] {
        0xFF => ButtonAction::Disabled,
        0x80 => match b[1] {
            0 => ButtonAction::Disabled,
            1 => {
                let m = u16::from_be_bytes([b[2], b[3]]);
                MOUSE_BITS
                    .iter()
                    .find(|(_, bit)| *bit == m)
                    .map(|(f, _)| ButtonAction::Mouse(*f))
                    .unwrap_or_else(raw)
            }
            2 => ButtonAction::Key(KeyStroke {
                mods: b[2],
                usage: b[3] as u16,
            }),
            3 => {
                let u = u16::from_be_bytes([b[2], b[3]]);
                MEDIA_USAGES
                    .iter()
                    .find(|(_, x)| *x == u)
                    .map(|(f, _)| ButtonAction::Media(*f))
                    .unwrap_or_else(raw)
            }
            _ => raw(),
        },
        0x90 => match b[1] {
            0x01 => ButtonAction::Mouse(MouseFn::ScrollLeft),
            0x02 => ButtonAction::Mouse(MouseFn::ScrollRight),
            0x03 => ButtonAction::Dpi(DpiFn::Up),
            0x04 => ButtonAction::Dpi(DpiFn::Down),
            0x05 => ButtonAction::Dpi(DpiFn::Cycle),
            0x07 => {
                let d = dpis.get(shift_idx as usize).copied().unwrap_or(0);
                ButtonAction::Dpi(DpiFn::Shift(d as u32))
            }
            0x0A => ButtonAction::ProfileCycle,
            0x10 => ButtonAction::Mouse(MouseFn::ScrollDown),
            0x11 => ButtonAction::Mouse(MouseFn::ScrollUp),
            _ => raw(),
        },
        _ => raw(),
    }
}

/// 割当 → 4 バイト。DPI シフトは「プロファイルの DPI 枠のどれか」を指す方式なので、
/// 同じ DPI のステージが無ければ空き枠 (stages の後ろ) へ入れて index を返す。
/// 返り値: (4 バイト, Some(新しい shift index と dpi 枠))。
pub type EncodedButton = ([u8; 4], Option<(u8, u16)>);

pub fn encode_button(
    a: &ButtonAction,
    dpis: &[u16; 5],
    stage_count: usize,
) -> HubResult<EncodedButton> {
    let b = match a {
        ButtonAction::Disabled => [0xFF, 0xFF, 0xFF, 0xFF],
        ButtonAction::Mouse(MouseFn::ScrollUp) => [0x90, 0x11, 0, 0],
        ButtonAction::Mouse(MouseFn::ScrollDown) => [0x90, 0x10, 0, 0],
        ButtonAction::Mouse(f) => {
            let bit = MOUSE_BITS
                .iter()
                .find(|(m, _)| m == f)
                .map(|(_, b)| *b)
                .ok_or_else(|| HubError::Invalid("このマウス操作は割り当てできません".into()))?;
            let be = bit.to_be_bytes();
            [0x80, 1, be[0], be[1]]
        }
        ButtonAction::Key(k) => {
            if k.usage > 0xFF {
                return Err(HubError::Invalid("キーコードが範囲外です".into()));
            }
            [0x80, 2, k.mods, k.usage as u8]
        }
        ButtonAction::Media(m) => {
            let u = MEDIA_USAGES
                .iter()
                .find(|(x, _)| x == m)
                .map(|(_, u)| *u)
                .ok_or_else(|| HubError::Invalid("メディアキー".into()))?;
            let be = u.to_be_bytes();
            [0x80, 3, be[0], be[1]]
        }
        ButtonAction::Dpi(DpiFn::Up) => [0x90, 0x03, 0, 0],
        ButtonAction::Dpi(DpiFn::Down) => [0x90, 0x04, 0, 0],
        ButtonAction::Dpi(DpiFn::Cycle) => [0x90, 0x05, 0, 0],
        ButtonAction::Dpi(DpiFn::Shift(d)) => {
            let d16 = (*d).clamp(1, 0xFFFE) as u16;
            let idx = match dpis.iter().position(|&x| x == d16) {
                Some(i) => i,
                None if stage_count < 5 => stage_count,
                None => return Err(HubError::Invalid(
                    "DPI シフト先は既存ステージの DPI を選ぶか、ステージを 4 個以下にしてください"
                        .into(),
                )),
            };
            return Ok(([0x90, 0x07, 0, 0], Some((idx as u8, d16))));
        }
        ButtonAction::ProfileCycle => [0x90, 0x0A, 0, 0],
        ButtonAction::Unsupported { ty, params } => {
            let mut b = [*ty, 0, 0, 0];
            for (i, p) in params.iter().take(3).enumerate() {
                b[i + 1] = *p;
            }
            b
        }
        _ => return Err(HubError::Invalid("この割り当てには対応していません".into())),
    };
    Ok((b, None))
}

/// 0x2201 getSensorDpiList の応答 → (min, max, step)。
/// 値の列で、`0xE000 | step` は「直前の値から step 刻みで次の値まで」を表す。
pub fn parse_dpi_list(p: &[u8]) -> Option<(u32, u32, u32)> {
    let mut vals = vec![];
    let mut step = None;
    for c in p.get(1..)?.chunks(2) {
        if c.len() < 2 {
            break;
        }
        let v = u16::from_be_bytes([c[0], c[1]]);
        if v == 0 {
            break;
        }
        if v >> 13 == 0b111 {
            step = Some((v & 0x1FFF) as u32);
        } else {
            vals.push(v as u32);
        }
    }
    let min = *vals.iter().min()?;
    let max = *vals.iter().max()?;
    let step = step.unwrap_or_else(|| {
        // 列挙型 (刻みなし): 隣り合う差の最小値。
        vals.windows(2)
            .map(|w| w[1].abs_diff(w[0]))
            .filter(|&d| d > 0)
            .min()
            .unwrap_or(50)
    });
    Some((min, max, step.max(1)))
}

/// 0x0003 getFwInfo → "RQM 01.02.B0034" 風の表示。
pub fn format_fw(p: &[u8]) -> Option<String> {
    if p.len() < 8 {
        return None;
    }
    let name: String = p[1..4]
        .iter()
        .filter(|c| c.is_ascii_graphic())
        .map(|&c| c as char)
        .collect();
    let build = u16::from_be_bytes([p[6], p[7]]);
    let mut s = format!("{name} {:02X}.{:02X}", p[4], p[5]);
    if build != 0 {
        s.push_str(&format!(".B{build:04X}"));
    }
    Some(s.trim().to_string())
}

fn hidpp_err(code: u8) -> &'static str {
    match code {
        1 => "unknown",
        2 => "invalid argument",
        3 => "out of range",
        4 => "hardware error",
        5 => "internal error",
        6 => "invalid feature index",
        7 => "invalid function",
        8 => "busy",
        9 => "unsupported",
        _ => "error",
    }
}

// ==================================================================== 0x2202 / 0x8061 の純関数

/// 0x2202 getSensorDpiRanges の連結ストリーム (各ページの 3 バイト目以降) →
/// (min, max, step)。値は BE16、`0xE000 | step` は前後の値の間を step 刻みで埋める印。
pub fn parse_dpi_ranges(stream: &[u8]) -> Option<(u32, u32, u32)> {
    let mut vals = vec![];
    let mut step: Option<u32> = None;
    for c in stream.chunks(2) {
        if c.len() < 2 {
            break;
        }
        let v = u16::from_be_bytes([c[0], c[1]]);
        if v == 0 {
            break;
        }
        if v >> 13 == 0b111 {
            let s = (v & 0x1FFF) as u32;
            step = Some(step.map_or(s, |x| x.min(s)));
        } else {
            vals.push(v as u32);
        }
    }
    let min = *vals.iter().min()?;
    let max = *vals.iter().max()?;
    Some((min, max, step.unwrap_or(50).max(1)))
}

/// 0x2202 getSensorDpiList (`[sensor, dir, BE16…, 0]`) → 本体プロファイルの DPI 段。
pub fn parse_dpi_levels(p: &[u8]) -> Vec<u32> {
    p.get(2..)
        .unwrap_or(&[])
        .chunks(2)
        .take_while(|c| c.len() == 2 && (c[0] != 0 || c[1] != 0))
        .map(|c| u16::from_be_bytes([c[0], c[1]]) as u32)
        .filter(|&d| d >> 13 != 0b111)
        .collect()
}

/// 0x8061 fn0/fn1 の応答 → 使えるレート (Hz)。マスクは **byte 1 だけ** (Options+ と同じ)。
pub fn ext_rate_mask(p: &[u8]) -> Vec<i64> {
    let m = p.get(1).copied().unwrap_or(0);
    (0..7)
        .filter(|i| m & (1 << i) != 0)
        .map(|i| EXT_RATES[i])
        .collect()
}

/// 既定のボタン割り当て (ホストモード・プロファイル未対応時の表示用)。
fn default_buttons(n: usize) -> Vec<ButtonAction> {
    let d = [
        MouseFn::Left,
        MouseFn::Right,
        MouseFn::Middle,
        MouseFn::Back,
        MouseFn::Forward,
    ];
    (0..n)
        .map(|i| {
            d.get(i)
                .map_or(ButtonAction::Disabled, |f| ButtonAction::Mouse(*f))
        })
        .collect()
}

// ==================================================================== ホストモードの保存

/// ホストモードの設定 (本体は電源断でオンボードに戻るので、アプリが覚えて再適用する)。
/// `%APPDATA%\DeviceHub\logitech.json` に DeviceId ごと。
#[derive(Clone, Debug, Default, PartialEq)]
struct HostPrefs {
    host: bool,
    stages: Vec<u32>,
    active: usize,
    rate: Option<i64>,
}

fn host_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("DeviceHub").join("logitech.json"))
}

fn load_host(id: &str) -> HostPrefs {
    let Some(v) = host_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    else {
        return HostPrefs::default();
    };
    let e = &v[id];
    HostPrefs {
        host: e["host"].as_bool().unwrap_or(false),
        stages: e["stages"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_u64())
                    .map(|x| x as u32)
                    .collect()
            })
            .unwrap_or_default(),
        active: e["active"].as_u64().unwrap_or(0) as usize,
        rate: e["rate"].as_i64(),
    }
}

fn save_host(id: &str, h: &HostPrefs) {
    let Some(p) = host_file() else { return };
    let mut v = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    v[id] = serde_json::json!({
        "host": h.host,
        "stages": h.stages,
        "active": h.active,
        "rate": h.rate,
    });
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(&v) {
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

// ==================================================================== ドライバ

/// オンボードメモリの状態 (開いた後にキャッシュ)。
#[derive(Clone, Debug)]
struct Onboard {
    desc: ObDesc,
    /// ユーザーディレクトリ (無効なら ROM ディレクトリの内容)。
    dir: Vec<(u16, bool)>,
    /// `dir` が ROM 由来 (ユーザー領域が未初期化)。
    dir_is_rom: bool,
}

/// 0x2202 のセンサー能力。
#[derive(Clone, Copy, Debug, Default)]
struct Dpi2202 {
    y: bool,
    lod: bool,
}

pub struct LogitechDriver {
    dev: Mutex<hidapi::HidDevice>,
    info: Mutex<DeviceInfo>,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
    /// HID++ device index (有線 0xFF / 受信機 1..6)。
    index: Mutex<u8>,
    /// feature id → index (None = 持たない)。
    features: Mutex<HashMap<u16, Option<u8>>>,
    onboard: Mutex<Option<Onboard>>,
    /// オンボードプロファイルを扱えない理由 (診断表示用)。
    onboard_err: Mutex<Option<String>>,
    caps: Mutex<Option<Capabilities>>,
}

pub fn open(
    dev: hidapi::HidDevice,
    info: DeviceInfo,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
) -> HubResult<LogitechDriver> {
    dev.set_blocking_mode(false).ok();
    let wired = info.transport == Transport::Wired;
    let d = LogitechDriver {
        dev: Mutex::new(dev),
        info: Mutex::new(info),
        events,
        index: Mutex::new(if wired { 0xFF } else { 1 }),
        features: Mutex::new(HashMap::new()),
        onboard: Mutex::new(None),
        onboard_err: Mutex::new(None),
        caps: Mutex::new(None),
    };
    if !wired {
        d.find_index();
    }
    d.wake_setup();
    Ok(d)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl LogitechDriver {
    fn emit(&self, e: HubEvent) {
        if let Some(ev) = &self.events {
            ev(e);
        }
    }

    fn id(&self) -> String {
        lock(&self.info).id.0.clone()
    }

    fn set_asleep(&self, asleep: bool) {
        let mut i = lock(&self.info);
        if i.transport == Transport::Wired || i.asleep == asleep {
            return;
        }
        i.asleep = asleep;
        let info = i.clone();
        let id = info.id.clone();
        drop(i);
        self.emit(HubEvent::Info(id.clone(), Box::new(info)));
        if !asleep {
            // 起きた: 名前・機能を取り直し、ホストモード設定を再適用して読み直させる。
            self.wake_setup();
            self.emit(HubEvent::StateChanged(id));
        }
    }

    /// 1 往復。`retry` は読み取り系 (冪等) のみ true — メモリ書き込みは再送しない。
    fn request_ex(
        &self,
        fidx: u8,
        func: u8,
        params: &[u8],
        timeout_ms: u64,
        retry: bool,
    ) -> HubResult<Vec<u8>> {
        let idx = *lock(&self.index);
        let mut f = [0u8; LONG_LEN];
        f[0] = LONG;
        f[1] = idx;
        f[2] = fidx;
        f[3] = (func << 4) | SW_ID;
        let n = params.len().min(16);
        f[4..4 + n].copy_from_slice(&params[..n]);
        let dev = lock(&self.dev);
        let tries = if retry { 2 } else { 1 };
        for _ in 0..tries {
            dev.write(&f)
                .map_err(|e| HubError::Io(format!("hid++ write: {e}")))?;
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let mut buf = [0u8; 64];
                let n = match dev.read_timeout(&mut buf, left.as_millis().clamp(1, 100) as i32) {
                    Ok(n) => n,
                    Err(e) => return Err(HubError::Io(format!("hid++ read: {e}"))),
                };
                if n < 7 || buf[0] != LONG || buf[1] != idx {
                    continue;
                }
                if buf[2] == 0xFF && buf[3] == fidx && buf[4] == f[3] {
                    drop(dev);
                    self.set_asleep(false);
                    return Err(HubError::Protocol(format!(
                        "HID++ feature {fidx:#04x} fn {func}: {}",
                        hidpp_err(buf[5])
                    )));
                }
                if buf[2] == fidx && buf[3] == f[3] {
                    drop(dev);
                    self.set_asleep(false);
                    let mut out = buf[4..n.min(LONG_LEN)].to_vec();
                    out.resize(16, 0);
                    return Ok(out);
                }
            }
        }
        drop(dev);
        self.set_asleep(true);
        Err(HubError::Protocol(
            "HID++: 応答がありません (マウスがスリープ中なら動かして起こしてください)".into(),
        ))
    }

    fn request(&self, fidx: u8, func: u8, params: &[u8]) -> HubResult<Vec<u8>> {
        self.request_ex(fidx, func, params, 700, true)
    }

    fn write_req(&self, fidx: u8, func: u8, params: &[u8]) -> HubResult<Vec<u8>> {
        self.request_ex(fidx, func, params, 1000, false)
    }

    /// 受信機経由: 1..6 を ping してマウスの device index を探す。
    fn find_index(&self) {
        for idx in 1..=6u8 {
            *lock(&self.index) = idx;
            // ROOT ping (fn1) の 3 バイト目はエコーされる。
            if let Ok(r) = self.request_ex(0, 1, &[0, 0, 0x5A], 250, false) {
                if r.get(2) == Some(&0x5A) {
                    return;
                }
            }
        }
        *lock(&self.index) = 1;
    }

    /// 0x8061 の接続種別 (0 = 有線, 1 = ゲーミング無線)。device index で決まる。
    fn conn_type(&self) -> u8 {
        if *lock(&self.index) == 0xFF {
            0
        } else {
            1
        }
    }

    /// feature id → index (キャッシュ)。通信できなければ Err (キャッシュしない)。
    fn feature(&self, id: u16) -> HubResult<Option<u8>> {
        if id == feat::ROOT {
            return Ok(Some(0));
        }
        if let Some(v) = lock(&self.features).get(&id) {
            return Ok(*v);
        }
        let be = id.to_be_bytes();
        let r = self.request(0, 0, &[be[0], be[1]])?;
        let v = r.first().copied().filter(|&i| i != 0);
        lock(&self.features).insert(id, v);
        Ok(v)
    }

    fn has(&self, id: u16) -> bool {
        matches!(self.feature(id), Ok(Some(_)))
    }

    fn need(&self, id: u16, what: &'static str) -> HubResult<u8> {
        self.feature(id)?.ok_or(HubError::Unsupported(what))
    }

    /// 起動/覚醒時: 名前を取り、ユーザーがホストモードを選んでいれば再適用する。
    /// (G HUB と違い、本体のモードを勝手には変えない。)
    fn wake_setup(&self) {
        if lock(&self.info).asleep {
            return;
        }
        if let Ok(name) = self.read_name() {
            let mut i = lock(&self.info);
            if !name.is_empty() && i.name != name {
                i.name = name;
                let info = i.clone();
                drop(i);
                self.emit(HubEvent::Info(info.id.clone(), Box::new(info)));
            }
        }
        let h = load_host(&self.id());
        if h.host && self.has(feat::ONBOARD_PROFILES) {
            let _ = self.apply_host(&h);
        }
        let _ = self.load_onboard();
    }

    fn read_name(&self) -> HubResult<String> {
        let fi = self.need(feat::NAME, "device name")?;
        let len = *self.request(fi, 0, &[])?.first().unwrap_or(&0) as usize;
        let mut s = Vec::new();
        while s.len() < len.min(64) {
            let r = self.request(fi, 1, &[s.len() as u8])?;
            if r.iter().all(|&b| b == 0) {
                break;
            }
            s.extend(r.iter().take(len - s.len()));
        }
        Ok(String::from_utf8_lossy(&s)
            .trim_matches(char::from(0))
            .trim()
            .to_string())
    }

    fn read_fw(&self) -> Option<String> {
        let fi = self.feature(feat::FW_VERSION).ok()??;
        let n = *self.request(fi, 0, &[]).ok()?.first()?;
        for e in 0..n {
            let r = self.request(fi, 1, &[e]).ok()?;
            if r.first().map(|t| t & 0x0F) == Some(0) {
                return format_fw(&r);
            }
        }
        None
    }

    // ---------------------------------------------------------------- モード

    /// 本体がオンボードモードか (0x8100 が無ければ false = 常にホスト扱い)。
    fn is_onboard_mode(&self) -> bool {
        let Ok(Some(fi)) = self.feature(feat::ONBOARD_PROFILES) else {
            return false;
        };
        self.request(fi, ob::GET_MODE, &[])
            .map(|r| r[0] == ob::MODE_ONBOARD)
            .unwrap_or(true)
    }

    fn set_mode(&self, onboard: bool) -> HubResult<()> {
        let fi = self.ob_index()?;
        self.write_req(fi, ob::SET_MODE, &[if onboard { 1 } else { 2 }])
            .map(|_| ())
    }

    /// ホストモードへ切り替えて、覚えている DPI とレートを適用する。
    fn apply_host(&self, h: &HostPrefs) -> HubResult<()> {
        self.set_mode(false)?;
        if let Some(&d) = h.stages.get(h.active.min(h.stages.len().saturating_sub(1))) {
            self.write_dpi(d)?;
        }
        if let Some(r) = h.rate {
            let _ = self.write_ext_rate(r);
        }
        Ok(())
    }

    /// プロファイルを編集できる状態か (オンボードモードで形式も対応)。
    fn profile_editable(&self) -> bool {
        self.is_onboard_mode()
            && self.load_onboard().is_ok_and(|o| {
                let i = self.active_profile(&o);
                self.read_profile(&o, i)
                    .is_ok_and(|p| self.check_profile(&p).is_ok())
            })
    }

    // ---------------------------------------------------------------- DPI (0x2202 / 0x2201)

    fn dpi2202(&self) -> Option<(u8, Dpi2202)> {
        let fi = self.feature(feat::EXT_DPI).ok()??;
        let r = self.request(fi, 1, &[0]).ok()?;
        Some((
            fi,
            Dpi2202 {
                y: r[2] & 0x01 != 0,
                lod: r[2] & 0x02 != 0,
            },
        ))
    }

    /// (min, max, step)。
    fn dpi_range(&self) -> (u32, u32, u32) {
        if let Ok(Some(fi)) = self.feature(feat::EXT_DPI) {
            let mut stream = vec![];
            for page in 0..16u8 {
                let Ok(r) = self.request(fi, 2, &[0, 0, page]) else {
                    break;
                };
                if r[2] != page {
                    break;
                }
                stream.extend_from_slice(&r[3..16]);
                if stream
                    .chunks(2)
                    .any(|c| c.len() == 2 && c[0] == 0 && c[1] == 0)
                {
                    break;
                }
            }
            if let Some(v) = parse_dpi_ranges(&stream) {
                return v;
            }
        }
        if let Ok(Some(fi)) = self.feature(feat::ADJUSTABLE_DPI) {
            if let Some(v) = self
                .request(fi, 1, &[0])
                .ok()
                .and_then(|r| parse_dpi_list(&r))
            {
                return v;
            }
        }
        (100, 32000, 50)
    }

    /// 今の DPI と (0x2202 なら) LOD。
    fn read_dpi(&self) -> HubResult<(u32, Option<u8>)> {
        if let Some((fi, c)) = self.dpi2202() {
            let r = self.request(fi, 5, &[0])?;
            let x = u16::from_be_bytes([r[1], r[2]]) as u32;
            return Ok((x, c.lod.then_some(r[9])));
        }
        let fi = self.need(feat::ADJUSTABLE_DPI, "dpi")?;
        let r = self.request(fi, 2, &[0])?;
        Ok((u16::from_be_bytes([r[1], r[2]]) as u32, None))
    }

    fn write_dpi_lod(&self, dpi: u32, lod: Option<u8>) -> HubResult<()> {
        let d = (dpi.clamp(1, 0xFFFF) as u16).to_be_bytes();
        if let Some((fi, c)) = self.dpi2202() {
            let cur_lod = if c.lod {
                lod.or_else(|| self.read_dpi().ok().and_then(|x| x.1))
                    .unwrap_or(2)
            } else {
                0
            };
            let y = if c.y { d } else { [0, 0] };
            return self
                .write_req(fi, 6, &[0, d[0], d[1], y[0], y[1], cur_lod])
                .map(|_| ());
        }
        let fi = self.need(feat::ADJUSTABLE_DPI, "dpi")?;
        self.write_req(fi, 3, &[0, d[0], d[1]]).map(|_| ())
    }

    fn write_dpi(&self, dpi: u32) -> HubResult<()> {
        self.write_dpi_lod(dpi, None)
    }

    /// オンボードモードで本体プロファイルの DPI 段 (0x2202 fn3)。
    fn onboard_levels(&self) -> Vec<u32> {
        let Some((fi, _)) = self.dpi2202() else {
            return vec![];
        };
        self.request(fi, 3, &[0, 0])
            .map(|r| parse_dpi_levels(&r))
            .unwrap_or_default()
    }

    /// プロファイルの解釈が本体と合っているかの照合: 0x2202 が返す本体の DPI 段と、
    /// こちらがセクタから読んだ DPI 段が一致すること。合わなければ形式の解釈違いとみなし
    /// 表示・書き込みに使わない (壊れたプロファイルを書き戻さないため)。
    fn check_profile(&self, p: &[u8]) -> HubResult<()> {
        let levels = self.onboard_levels();
        if levels.is_empty() {
            return Ok(());
        }
        let mine: Vec<u32> = profile_dpis(p)
            .iter()
            .take_while(|&&d| d != 0 && d != 0xFFFF)
            .map(|&d| d as u32)
            .collect();
        if mine == levels {
            Ok(())
        } else {
            let msg =
                format!("プロファイルの解釈が本体と一致しません (本体 {levels:?} / 解釈 {mine:?})");
            *lock(&self.onboard_err) = Some(msg.clone());
            Err(HubError::Protocol(msg))
        }
    }

    // ---------------------------------------------------------------- オンボードメモリ

    fn ob_index(&self) -> HubResult<u8> {
        self.need(feat::ONBOARD_PROFILES, "onboard profiles")
    }

    fn read_sector(&self, sector: u16, size: u16) -> HubResult<Vec<u8>> {
        let fi = self.ob_index()?;
        let size = size as usize;
        let mut data = vec![0u8; size];
        let mut o = 0usize;
        while o < size {
            // 末尾は size-16 から読む (それ以降を指すと INVALID_ARGUMENT)。
            let at = o.min(size.saturating_sub(16));
            let s = sector.to_be_bytes();
            let a = (at as u16).to_be_bytes();
            let r = self.request(fi, ob::MEM_READ, &[s[0], s[1], a[0], a[1]])?;
            let n = r.len().min(16).min(size - at);
            data[at..at + n].copy_from_slice(&r[..n]);
            o = at + 16;
        }
        Ok(data)
    }

    fn write_sector(&self, sector: u16, data: &[u8]) -> HubResult<()> {
        let fi = self.ob_index()?;
        let s = sector.to_be_bytes();
        let l = (data.len() as u16).to_be_bytes();
        self.write_req(fi, ob::MEM_ADDR_WRITE, &[s[0], s[1], 0, 0, l[0], l[1]])?;
        for c in data.chunks(16) {
            let mut b = [0xFFu8; 16];
            b[..c.len()].copy_from_slice(c);
            self.write_req(fi, ob::MEM_WRITE, &b)?;
        }
        self.request_ex(fi, ob::MEM_WRITE_END, &[], 1500, false)?;
        Ok(())
    }

    /// 説明・ディレクトリを読んでキャッシュする。失敗理由は診断用に残す。
    fn load_onboard(&self) -> HubResult<Onboard> {
        if let Some(o) = lock(&self.onboard).clone() {
            return Ok(o);
        }
        let r = self.load_onboard_inner();
        match &r {
            Ok(o) => {
                *lock(&self.onboard) = Some(o.clone());
                *lock(&self.onboard_err) = None;
            }
            Err(e) => *lock(&self.onboard_err) = Some(e.to_string()),
        }
        r
    }

    fn load_onboard_inner(&self) -> HubResult<Onboard> {
        let fi = self.ob_index()?;
        let raw = self.request(fi, ob::GET_DESC, &[])?;
        let desc = ObDesc::parse(&raw)
            .ok_or_else(|| HubError::Protocol("onboard: short description".into()))?;
        if !desc.supported() {
            return Err(HubError::Protocol(format!(
                "未対応のオンボード形式 (memory {}, profile format {}, sector {}B)",
                desc.memory_model, desc.profile_format, desc.sector_size
            )));
        }
        let max = desc.profile_count as usize;
        let user = self.read_sector(0, desc.sector_size)?;
        let (dir, dir_is_rom) = match sector_valid(&user)
            .then(|| parse_directory(&user, max))
            .flatten()
        {
            Some(d) => (d, false),
            None => {
                let rom = self.read_sector(ROM_BASE, desc.sector_size)?;
                let d = parse_directory(&rom, max).unwrap_or_else(|| vec![(ROM_BASE + 1, true)]);
                (d, true)
            }
        };
        Ok(Onboard {
            desc,
            dir,
            dir_is_rom,
        })
    }

    /// 現在のプロファイル番号 (0 始まり)。getCurrentProfile の byte1 = 1 始まりの番号。
    fn active_profile(&self, o: &Onboard) -> usize {
        let Ok(fi) = self.ob_index() else { return 0 };
        let Ok(r) = self.request(fi, ob::GET_PROFILE, &[]) else {
            return 0;
        };
        let addr = u16::from_be_bytes([r[0], r[1]]);
        o.dir
            .iter()
            .position(|(a, _)| *a == addr || (*a & 0xFF) == addr & 0xFF)
            .unwrap_or_else(|| ((addr & 0xFF) as usize).saturating_sub(1))
            .min(o.dir.len().saturating_sub(1))
    }

    /// プロファイル `i` の中身 (CRC 不正なら ROM の同じ番号へ落とす)。
    fn read_profile(&self, o: &Onboard, i: usize) -> HubResult<Vec<u8>> {
        let addr = o.dir.get(i).map(|e| e.0).unwrap_or(1 + i as u16);
        let d = self.read_sector(addr, o.desc.sector_size)?;
        if sector_valid(&d) {
            return Ok(d);
        }
        let rom = ROM_BASE + 1 + (i as u16).min(o.desc.rom_profile_count.saturating_sub(1) as u16);
        self.read_sector(rom, o.desc.sector_size)
    }

    /// ユーザー領域が未初期化 (ROM ディレクトリ) なら、ROM のプロファイルを
    /// ユーザーセクタへ写してディレクトリを書く (libratbag の commit と同じ形)。
    fn ensure_user(&self) -> HubResult<Onboard> {
        let o = self.load_onboard()?;
        if !o.dir_is_rom {
            return Ok(o);
        }
        let n = o.desc.profile_count as usize;
        let mut entries = vec![];
        for i in 0..n {
            let mut p = self.read_profile(&o, i.min(o.dir.len().saturating_sub(1)))?;
            seal_sector(&mut p);
            let sector = 1 + i as u16;
            self.write_sector(sector, &p)?;
            let en = o.dir.get(i).map(|e| e.1).unwrap_or(i == 0);
            entries.push((sector, en));
        }
        if !entries.iter().any(|e| e.1) {
            entries[0].1 = true;
        }
        self.write_sector(0, &build_directory(&entries, o.desc.sector_size as usize))?;
        let o2 = Onboard {
            desc: o.desc,
            dir: entries,
            dir_is_rom: false,
        };
        *lock(&self.onboard) = Some(o2.clone());
        Ok(o2)
    }

    /// アクティブプロファイルを読み → `edit` → 書き戻し → 再選択で反映。
    fn edit_active(&self, edit: impl FnOnce(&mut Vec<u8>) -> HubResult<()>) -> HubResult<()> {
        // 解釈が本体と合わないなら、ROM → ユーザー領域の初期化も含めて一切書かない。
        if !self.profile_editable() {
            return Err(self.not_editable("この設定"));
        }
        let o = self.ensure_user()?;
        let i = self.active_profile(&o);
        let mut p = self.read_profile(&o, i)?;
        self.check_profile(&p)?;
        let before = p.clone();
        edit(&mut p)?;
        if p == before {
            return Ok(());
        }
        seal_sector(&mut p);
        let addr = o.dir.get(i).map(|e| e.0).unwrap_or(1 + i as u16);
        self.write_sector(addr, &p)?;
        // 書いたセクタを本体に読み直させる (同じプロファイルを選び直す)。
        let fi = self.ob_index()?;
        let a = addr.to_be_bytes();
        let _ = self.request_ex(fi, ob::SET_PROFILE, &[a[0], a[1]], 1000, false);
        Ok(())
    }

    fn set_dpi_index(&self, idx: u8) -> HubResult<()> {
        let fi = self.ob_index()?;
        self.write_req(fi, ob::SET_DPI_INDEX, &[idx]).map(|_| ())
    }

    /// プロファイルを書けないときの説明。
    fn not_editable(&self, what: &str) -> HubError {
        if !self.is_onboard_mode() {
            return HubError::Invalid(format!(
                "{what}はオンボードメモリモードでのみ本体に保存できます (「オンボードメモリを使う」を ON に)"
            ));
        }
        let why = lock(&self.onboard_err)
            .clone()
            .unwrap_or_else(|| "読み込みに失敗".into());
        HubError::Invalid(format!(
            "この機種のオンボードプロファイルはまだ書き換えられません ({why})。DPI とレートは「オンボードメモリを使う」を OFF (ホストモード) にすると変更できます。「診断情報を保存」の結果を送ってもらえれば対応します"
        ))
    }

    // ---------------------------------------------------------------- レポートレート

    fn rate_options(&self) -> Vec<i64> {
        if let Ok(Some(fi)) = self.feature(feat::EXT_REPORT_RATE) {
            // fn1 getActualReportRateList = 今の接続で使えるもの。
            if let Ok(r) = self.request(fi, 1, &[]) {
                let v = ext_rate_mask(&r);
                if !v.is_empty() {
                    return v;
                }
            }
        }
        if let Ok(Some(fi)) = self.feature(feat::REPORT_RATE) {
            if let Ok(r) = self.request(fi, 0, &[]) {
                let flags = r[0];
                let v: Vec<i64> = [8u8, 4, 2, 1]
                    .iter()
                    .filter(|&&ms| flags & (1 << (ms - 1)) != 0)
                    .filter_map(|&ms| rate_from_ms(ms))
                    .collect();
                if !v.is_empty() {
                    return v;
                }
            }
        }
        vec![125, 250, 500, 1000]
    }

    fn read_rate(&self, profile: Option<&[u8]>) -> Option<i64> {
        if let Ok(Some(fi)) = self.feature(feat::EXT_REPORT_RATE) {
            if let Ok(r) = self.request(fi, 2, &[self.conn_type()]) {
                if let Some(&hz) = EXT_RATES.get(r[0] as usize) {
                    return Some(hz);
                }
            }
        }
        if let Ok(Some(fi)) = self.feature(feat::REPORT_RATE) {
            if let Ok(r) = self.request(fi, 1, &[]) {
                if let Some(hz) = rate_from_ms(r[0]) {
                    return Some(hz);
                }
            }
        }
        profile.and_then(|p| rate_from_ms(p[off::REPORT_RATE]))
    }

    /// 0x8061 / 0x8060 で今のレートを変える (オンボードモードでは本体が無視することがある)。
    fn write_ext_rate(&self, hz: i64) -> HubResult<()> {
        if let Ok(Some(fi)) = self.feature(feat::EXT_REPORT_RATE) {
            let i = EXT_RATES
                .iter()
                .position(|&x| x == hz)
                .ok_or_else(|| HubError::Invalid("report rate".into()))?;
            return self.write_req(fi, 3, &[i as u8]).map(|_| ());
        }
        if let Ok(Some(fi)) = self.feature(feat::REPORT_RATE) {
            let ms = (1000 / hz.max(1)) as u8;
            return self.write_req(fi, 2, &[ms]).map(|_| ());
        }
        Err(HubError::Unsupported("report rate"))
    }

    fn write_rate(&self, hz: i64) -> HubResult<()> {
        let onboard = self.is_onboard_mode();
        if !onboard {
            self.write_ext_rate(hz)?;
            let id = self.id();
            let mut h = load_host(&id);
            h.rate = Some(hz);
            save_host(&id, &h);
            return Ok(());
        }
        // オンボード: 1000Hz 以下はプロファイルに書く (電源断後も保持)。
        if hz <= 1000 && self.profile_editable() {
            let ms = (1000 / hz.max(1)) as u8;
            self.edit_active(|p| {
                p[off::REPORT_RATE] = ms;
                Ok(())
            })?;
            let _ = self.write_ext_rate(hz);
            return Ok(());
        }
        // それ以外は直接書いて、効いたか読み戻す (本体プロファイルが優先だと無視される)。
        self.write_ext_rate(hz)?;
        if self.read_rate(None) == Some(hz) {
            Ok(())
        } else {
            Err(HubError::Invalid(
                "オンボードメモリモードではレポートレートが本体プロファイルで固定されています。「オンボードメモリを使う」を OFF (ホストモード) にすると変更できます".into(),
            ))
        }
    }

    // ---------------------------------------------------------------- caps

    fn build_caps(&self) -> Capabilities {
        use section::*;
        let rates = self.rate_options();
        let (dpi_min, dpi_max, dpi_step) = self.dpi_range();
        let ob = self.load_onboard().ok();
        let has_8100 = self.has(feat::ONBOARD_PROFILES);
        let nbtn = ob
            .as_ref()
            .map(|o| (o.desc.button_count as usize).clamp(1, 16))
            .unwrap_or(5);
        let names = [
            "左ボタン",
            "右ボタン",
            "ホイールクリック",
            "サイド (戻る)",
            "サイド (進む)",
            "ボタン 6",
            "ボタン 7",
            "ボタン 8",
        ];
        let buttons = (0..nbtn)
            .map(|i| ButtonSlot {
                name: names
                    .get(i)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("ボタン {}", i + 1)),
            })
            .collect();
        let mut settings = vec![SettingSpec::choice(
            "polling",
            PERFORMANCE,
            "レポートレート",
            rates
                .iter()
                .map(|&hz| {
                    ChoiceOpt::new(
                        hz,
                        if hz >= 1000 {
                            format!("{}K", hz / 1000)
                        } else {
                            hz.to_string()
                        },
                    )
                })
                .collect(),
        )
        .help("マウスの報告頻度 (Hz)")];
        if has_8100 {
            settings.push(
                SettingSpec::toggle("onboard_mode", PERFORMANCE, "オンボードメモリを使う")
                    .help("ON: 本体に保存した設定で動く (アプリを閉じても有効・ボタン割り当てはこちら)。OFF: G HUB と同じホストモード — DPI・レートをアプリが適用し、マウスの電源を入れ直すたびに常駐アプリが掛け直します"),
            );
        }
        if self.dpi2202().is_some_and(|(_, c)| c.lod) {
            settings.push(
                SettingSpec::choice(
                    "lod",
                    SENSOR,
                    "リフトオフ距離",
                    vec![
                        ChoiceOpt::new(1, "低"),
                        ChoiceOpt::new(2, "中"),
                        ChoiceOpt::new(3, "高"),
                    ],
                )
                .help("持ち上げてカーソルが止まる高さ"),
            );
        }
        let mut actions = vec![];
        if ob.is_some() {
            actions.push(
                ActionSpec::new("restore_rom", SYSTEM, "オンボード設定を工場出荷に戻す")
                    .help(
                        "アクティブなプロファイルを本体 ROM の既定値 (DPI・ボタン) で上書きします",
                    )
                    .danger("現在のプロファイルの DPI とボタン割り当てが既定値に戻ります"),
            );
        }
        actions.push(
            ActionSpec::new("diag_export", SYSTEM, "診断情報を保存")
                .help("HID++ の機能一覧・オンボードメモリの形式などをテキストに書き出します (読み取りのみ)。不具合報告に添付してください")
                .save_file(&["txt"]),
        );
        let has_batt = self.has(feat::UNIFIED_BATTERY)
            || self.has(feat::BATTERY_STATUS)
            || lock(&self.info).transport != Transport::Wired;
        Capabilities {
            settings,
            actions,
            profiles: ob
                .as_ref()
                .filter(|o| o.desc.profile_count > 1)
                .map(|o| ProfileCaps {
                    count: o.desc.profile_count,
                    renamable: true,
                }),
            mouse: Some(MouseCaps {
                dpi_min,
                dpi_max,
                dpi_step,
                min_stages: 1,
                max_stages: 5,
                xy_split: false,
                stage_colors: false,
                stage_toggle: false,
                buttons,
                button_kinds: vec![
                    ButtonActionKind::Mouse,
                    ButtonActionKind::Key,
                    ButtonActionKind::Media,
                    ButtonActionKind::Dpi,
                    ButtonActionKind::ProfileCycle,
                    ButtonActionKind::Disabled,
                ],
                require_left_click: true,
                macros: None,
                live_buttons: vec![],
                adaptive_dpi: None,
            }),
            keyboard: None,
            lighting: vec![],
            battery: has_batt,
            peq: None,
            firmware_feed: None,
        }
    }

    /// プロファイルを読めるときの状態。
    fn read_profile_state(&self, o: &Onboard) -> HubResult<(ProfileState, MouseState, Vec<u8>)> {
        let i = self.active_profile(o);
        let p = self.read_profile(o, i)?;
        self.check_profile(&p)?;
        let dpis = profile_dpis(&p);
        let stages: Vec<DpiStage> = dpis
            .iter()
            .take_while(|&&d| d != 0 && d != 0xFFFF)
            .map(|&d| stage(d as u32))
            .collect();
        let fi = self.ob_index()?;
        let active = self
            .request(fi, ob::GET_DPI_INDEX, &[])
            .map(|r| r[0])
            .unwrap_or(p[off::DEFAULT_DPI]) as usize;
        let nbtn = (o.desc.button_count as usize).clamp(1, 16);
        let buttons = (0..nbtn)
            .map(|b| {
                let a = off::BUTTONS + b * 4;
                decode_button(&p[a..a + 4], &dpis, p[off::SHIFT_DPI])
            })
            .collect();
        let mut names = vec![String::new(); o.desc.profile_count as usize];
        if let Some(n) = names.get_mut(i) {
            *n = profile_name(&p);
        }
        if stages.is_empty() {
            return Err(HubError::Protocol("profile: DPI が空".into()));
        }
        Ok((
            ProfileState {
                active: i as u8,
                names,
            },
            MouseState {
                active_stage: active.min(stages.len() - 1),
                stages,
                buttons,
                macros: vec![],
                adaptive: None,
            },
            p,
        ))
    }
}

fn stage(x: u32) -> DpiStage {
    DpiStage {
        x,
        y: None,
        color: Rgb::default(),
        enabled: true,
    }
}

impl DeviceDriver for LogitechDriver {
    fn info(&self) -> DeviceInfo {
        let fw = self.read_fw();
        let batt = self.battery().ok();
        let mut i = lock(&self.info);
        if fw.is_some() {
            i.firmware = fw;
        }
        if batt.is_some() {
            i.battery = batt;
        }
        i.supported = true;
        i.clone()
    }

    fn caps(&self) -> Capabilities {
        if let Some(c) = lock(&self.caps).clone() {
            return c;
        }
        let c = self.build_caps();
        // 休眠中に作った仮の caps はキャッシュしない (起きたら作り直す)。
        if !lock(&self.info).asleep {
            *lock(&self.caps) = Some(c.clone());
        }
        c
    }

    fn read(&self) -> HubResult<DeviceState> {
        let mut settings = BTreeMap::new();
        let onboard = self.is_onboard_mode();
        let has_8100 = self.has(feat::ONBOARD_PROFILES);
        if has_8100 {
            settings.insert("onboard_mode".into(), SettingValue::Bool(onboard));
        }
        // 1) オンボードモードでプロファイルを読める → 本体プロファイルが正。
        let from_profile = if onboard {
            self.load_onboard()
                .ok()
                .and_then(|o| self.read_profile_state(&o).ok())
        } else {
            None
        };
        let (profile_state, mouse, prof) = match from_profile {
            Some((ps, ms, p)) => (ps, ms, Some(p)),
            None => {
                // 2) ホストモード / 未対応形式: 今の DPI (0x2202/0x2201) を基準に組み立てる。
                let (cur, lod) = self.read_dpi()?;
                if let Some(l) = lod {
                    settings.insert("lod".into(), SettingValue::Choice(l as i64));
                }
                let nbtn = self
                    .load_onboard()
                    .map(|o| (o.desc.button_count as usize).clamp(1, 16))
                    .unwrap_or(5);
                let h = load_host(&self.id());
                let mut levels = if onboard {
                    self.onboard_levels()
                } else {
                    h.stages.clone()
                };
                if levels.is_empty() {
                    levels = vec![cur];
                }
                let ob_idx = if onboard {
                    self.ob_index()
                        .ok()
                        .and_then(|fi| self.request(fi, ob::GET_DPI_INDEX, &[]).ok())
                        .map(|r| r[0] as usize)
                        .filter(|&i| i < levels.len() && levels[i] == cur)
                } else {
                    None
                };
                let active = ob_idx
                    .or_else(|| levels.iter().position(|&d| d == cur))
                    .unwrap_or_else(|| {
                        if onboard {
                            0
                        } else {
                            h.active.min(levels.len() - 1)
                        }
                    });
                if !levels.contains(&cur) && !onboard {
                    levels[active] = cur;
                }
                (
                    ProfileState::default(),
                    MouseState {
                        stages: levels.iter().map(|&d| stage(d)).collect(),
                        active_stage: active,
                        buttons: default_buttons(nbtn),
                        macros: vec![],
                        adaptive: None,
                    },
                    None,
                )
            }
        };
        if !settings.contains_key("lod") {
            if let Ok((_, Some(l))) = self.read_dpi() {
                settings.insert("lod".into(), SettingValue::Choice(l as i64));
            }
        }
        if let Some(hz) = self.read_rate(prof.as_deref()) {
            settings.insert("polling".into(), SettingValue::Choice(hz));
        }
        Ok(DeviceState {
            profile: profile_state,
            settings,
            mouse: Some(mouse),
            keyboard: None,
            lighting: BTreeMap::new(),
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::Setting { id, value } if id == "polling" => self.write_rate(value.as_choice()),
            Change::Setting { id, value } if id == "lod" => {
                let (cur, _) = self.read_dpi()?;
                self.write_dpi_lod(cur, Some(value.as_choice().clamp(1, 3) as u8))
            }
            Change::Setting { id, value } if id == "onboard_mode" => {
                let on = value.as_bool();
                let sid = self.id();
                let mut h = load_host(&sid);
                if on {
                    self.set_mode(true)?;
                    h.host = false;
                    save_host(&sid, &h);
                } else {
                    // ホストへ: 今の値を引き継いでから切り替える。
                    let (cur, _) = self.read_dpi().unwrap_or((800, None));
                    if h.stages.is_empty() {
                        let lv = self.onboard_levels();
                        h.stages = if lv.is_empty() { vec![cur] } else { lv };
                        h.active = h.stages.iter().position(|&d| d == cur).unwrap_or(0);
                    }
                    if h.rate.is_none() {
                        h.rate = self.read_rate(None);
                    }
                    h.host = true;
                    save_host(&sid, &h);
                    self.apply_host(&h)?;
                }
                // 表示中の DPI・ボタンが変わるので読み直させる。
                self.emit(HubEvent::StateChanged(DeviceId(sid)));
                Ok(())
            }
            Change::DpiStages { stages, active } => {
                let n = stages.len().clamp(1, 5);
                let dpis: Vec<u32> = stages[..n].iter().map(|s| s.x).collect();
                let act = (*active).min(n - 1);
                if !self.is_onboard_mode() {
                    let sid = self.id();
                    let mut h = load_host(&sid);
                    h.stages = dpis.clone();
                    h.active = act;
                    h.host = true;
                    save_host(&sid, &h);
                    return self.write_dpi(dpis[act]);
                }
                if !self.profile_editable() {
                    return Err(self.not_editable("DPI ステージ"));
                }
                self.edit_active(|p| {
                    // DPI シフト用に後ろの枠を使っていたら残す。
                    let old = profile_dpis(p);
                    let shift = p[off::SHIFT_DPI] as usize;
                    let mut v = dpis.clone();
                    if shift >= n && shift < 5 && old[shift] != 0 {
                        v.resize(shift, 0);
                        v.push(old[shift] as u32);
                    }
                    set_profile_dpis(p, &v);
                    p[off::DEFAULT_DPI] = act as u8;
                    Ok(())
                })?;
                self.set_dpi_index(act as u8)
            }
            Change::ActiveStage(a) => {
                if !self.is_onboard_mode() {
                    let sid = self.id();
                    let mut h = load_host(&sid);
                    let d = *h
                        .stages
                        .get(*a)
                        .ok_or_else(|| HubError::Invalid("stage".into()))?;
                    h.active = *a;
                    save_host(&sid, &h);
                    return self.write_dpi(d);
                }
                self.set_dpi_index((*a).min(4) as u8)
            }
            Change::Button { slot, action } => {
                if !self.profile_editable() {
                    return Err(self.not_editable("ボタン割り当て"));
                }
                let o = self.load_onboard()?;
                if *slot >= (o.desc.button_count as usize).clamp(1, 16) {
                    return Err(HubError::Invalid("button slot".into()));
                }
                self.edit_active(|p| {
                    let dpis = profile_dpis(p);
                    let stages = dpis.iter().take_while(|&&d| d != 0 && d != 0xFFFF).count();
                    let (b, shift) = encode_button(action, &dpis, stages)?;
                    if let Some((idx, dpi)) = shift {
                        let a = off::DPI + idx as usize * 2;
                        p[a..a + 2].copy_from_slice(&dpi.to_le_bytes());
                        p[off::SHIFT_DPI] = idx;
                    }
                    let a = off::BUTTONS + slot * 4;
                    p[a..a + 4].copy_from_slice(&b);
                    Ok(())
                })
            }
            Change::Profile(i) => {
                if !self.profile_editable() {
                    return Err(self.not_editable("プロファイル切替"));
                }
                let o = self.ensure_user()?;
                let addr = o
                    .dir
                    .get(*i as usize)
                    .map(|e| e.0)
                    .ok_or_else(|| HubError::Invalid("profile".into()))?;
                let fi = self.ob_index()?;
                let a = addr.to_be_bytes();
                self.write_req(fi, ob::SET_PROFILE, &[a[0], a[1]])
                    .map(|_| ())
            }
            Change::ProfileName { index, name } => {
                if !self.profile_editable() {
                    return Err(self.not_editable("プロファイル名"));
                }
                let o = self.ensure_user()?;
                if *index as usize != self.active_profile(&o) {
                    return Err(HubError::Invalid(
                        "名前を変えるプロファイルを先に選択してください".into(),
                    ));
                }
                self.edit_active(|p| {
                    set_profile_name(p, name);
                    Ok(())
                })
            }
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn run_action(&self, id: &str, file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            "diag_export" => {
                let path = file.ok_or_else(|| HubError::Invalid("保存先".into()))?;
                std::fs::write(path, probe(self))
                    .map_err(|e| HubError::Io(format!("保存できません: {e}")))?;
                Ok(ActionOutcome {
                    message: Some(format!("診断情報を保存しました: {}", path.display())),
                    reload: false,
                })
            }
            "restore_rom" => {
                if !self.profile_editable() {
                    return Err(self.not_editable("工場出荷への復元"));
                }
                let o = self.ensure_user()?;
                let i = self.active_profile(&o);
                let rom = ROM_BASE
                    + 1
                    + (i as u16).min(o.desc.rom_profile_count.saturating_sub(1) as u16);
                let mut p = self.read_sector(rom, o.desc.sector_size)?;
                if !sector_valid(&p) {
                    return Err(HubError::Protocol(
                        "ROM プロファイルの CRC が不正です".into(),
                    ));
                }
                seal_sector(&mut p);
                let addr = o.dir.get(i).map(|e| e.0).unwrap_or(1 + i as u16);
                self.write_sector(addr, &p)?;
                let fi = self.ob_index()?;
                let a = addr.to_be_bytes();
                let _ = self.request_ex(fi, ob::SET_PROFILE, &[a[0], a[1]], 1000, false);
                Ok(ActionOutcome {
                    message: Some("工場出荷時の DPI・ボタン設定に戻しました".into()),
                    reload: true,
                })
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        if let Some(fi) = self.feature(feat::UNIFIED_BATTERY)? {
            // get_status: [残量%, レベル bit, 充電状態, 外部電源]
            let r = self.request(fi, 1, &[])?;
            let soc = r[0];
            let pct = if soc != 0 {
                soc
            } else {
                match r[1] {
                    8.. => 90,
                    4..=7 => 50,
                    2..=3 => 20,
                    1 => 5,
                    _ => 0,
                }
            };
            return Ok(Battery {
                percent: Some(pct.min(100)),
                charging: matches!(r[2], 1 | 2 | 4),
            });
        }
        if let Some(fi) = self.feature(feat::BATTERY_STATUS)? {
            let r = self.request(fi, 0, &[])?;
            return Ok(Battery {
                percent: Some(r[0].min(100)),
                charging: matches!(r[2], 1 | 2 | 4),
            });
        }
        Err(HubError::Unsupported("battery"))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 診断用: 機能一覧・各機能の生応答・オンボードメモリを文字列で返す (読み取りのみ)。
/// アプリの「診断情報を保存」と `cargo run -p devhub-hid --example lgprobe` が使う。
pub fn probe(drv: &LogitechDriver) -> String {
    let mut s = String::new();
    let info = lock(&drv.info).clone();
    s.push_str(&format!(
        "Device HUB {} — Logitech HID++ 診断\n{:04x}:{:04x} {:?} name={}\n",
        env!("CARGO_PKG_VERSION"),
        info.vid,
        info.pid,
        info.transport,
        info.name
    ));
    s.push_str(&format!("device index: {:#04x}\n", *lock(&drv.index)));
    match drv.read_name() {
        Ok(n) => s.push_str(&format!("name(0x0005): {n}\n")),
        Err(e) => s.push_str(&format!("name(0x0005): {e}\n")),
    }
    s.push_str(&format!("firmware: {:?}\n", drv.read_fw()));
    s.push_str("features:\n");
    for (id, n) in [
        (feat::FW_VERSION, "FW_VERSION"),
        (feat::NAME, "DEVICE_NAME"),
        (feat::BATTERY_STATUS, "BATTERY_STATUS"),
        (feat::UNIFIED_BATTERY, "UNIFIED_BATTERY"),
        (feat::ADJUSTABLE_DPI, "ADJUSTABLE_DPI"),
        (feat::EXT_DPI, "EXTENDED_ADJUSTABLE_DPI"),
        (feat::REPORT_RATE, "REPORT_RATE"),
        (feat::EXT_REPORT_RATE, "EXTENDED_REPORT_RATE"),
        (0x8090, "MODE_STATUS"),
        (feat::ONBOARD_PROFILES, "ONBOARD_PROFILES"),
        (0x1B04, "REPROG_CONTROLS_V4"),
        (0x8110, "MOUSE_BUTTON_SPY"),
    ] {
        s.push_str(&format!("  {id:#06x} {n:<24} {:?}\n", drv.feature(id)));
    }
    let raw = |fid: u16, func: u8, params: &[u8]| -> String {
        match drv.feature(fid) {
            Ok(Some(fi)) => match drv.request(fi, func, params) {
                Ok(r) => hex(&r),
                Err(e) => e.to_string(),
            },
            Ok(None) => "-".into(),
            Err(e) => e.to_string(),
        }
    };
    s.push_str(&format!(
        "0x2202 fn1 caps     : {}\n",
        raw(feat::EXT_DPI, 1, &[0])
    ));
    s.push_str(&format!(
        "0x2202 fn2 ranges p0: {}\n",
        raw(feat::EXT_DPI, 2, &[0, 0, 0])
    ));
    s.push_str(&format!(
        "0x2202 fn3 list     : {}\n",
        raw(feat::EXT_DPI, 3, &[0, 0])
    ));
    s.push_str(&format!(
        "0x2202 fn5 params   : {}\n",
        raw(feat::EXT_DPI, 5, &[0])
    ));
    s.push_str(&format!(
        "0x2201 fn2 dpi      : {}\n",
        raw(feat::ADJUSTABLE_DPI, 2, &[0])
    ));
    s.push_str(&format!(
        "0x8061 fn1 list     : {}\n",
        raw(feat::EXT_REPORT_RATE, 1, &[])
    ));
    s.push_str(&format!(
        "0x8061 fn2 rate     : {}\n",
        raw(feat::EXT_REPORT_RATE, 2, &[drv.conn_type()])
    ));
    s.push_str(&format!(
        "0x8100 fn0 desc     : {}\n",
        raw(feat::ONBOARD_PROFILES, 0, &[])
    ));
    s.push_str(&format!(
        "0x8100 fn2 mode     : {}\n",
        raw(feat::ONBOARD_PROFILES, 2, &[])
    ));
    s.push_str(&format!(
        "0x8100 fn4 profile  : {}\n",
        raw(feat::ONBOARD_PROFILES, 4, &[])
    ));
    s.push_str(&format!(
        "0x8100 fnB dpi idx  : {}\n",
        raw(feat::ONBOARD_PROFILES, 0xB, &[])
    ));
    for (sector, label) in [
        (0u16, "user dir"),
        (ROM_BASE, "rom dir"),
        (1, "sector 1"),
        (ROM_BASE + 1, "rom 1"),
    ] {
        let sb = sector.to_be_bytes();
        let mut line = String::new();
        for off16 in [0u16, 16, 32, 48] {
            let o = off16.to_be_bytes();
            line.push_str(&raw(
                feat::ONBOARD_PROFILES,
                ob::MEM_READ,
                &[sb[0], sb[1], o[0], o[1]],
            ));
            line.push_str(" | ");
        }
        s.push_str(&format!("0x8100 read {label:<8} {line}\n"));
    }
    match drv.load_onboard() {
        Ok(o) => {
            s.push_str(&format!(
                "onboard: {:?}\n  dir (rom={}): {:?}\n",
                o.desc, o.dir_is_rom, o.dir
            ));
            let i = drv.active_profile(&o);
            if let Ok(p) = drv.read_profile(&o, i) {
                s.push_str(&format!(
                    "  active #{i}: crc_ok={} rate_ms={} dpis={:?} default={} shift={}\n",
                    sector_valid(&p),
                    p[0],
                    profile_dpis(&p),
                    p[1],
                    p[2]
                ));
                for b in 0..(o.desc.button_count as usize).min(16) {
                    let a = off::BUTTONS + b * 4;
                    s.push_str(&format!("  button {b}: {:02x?}\n", &p[a..a + 4]));
                }
            }
        }
        Err(e) => s.push_str(&format!("onboard: {e}\n")),
    }
    s.push_str(&format!("rates: {:?}\n", drv.rate_options()));
    s.push_str(&format!("dpi range: {:?}\n", drv.dpi_range()));
    s.push_str(&format!("dpi now: {:?}\n", drv.read_dpi()));
    s.push_str(&format!("battery: {:?}\n", drv.battery()));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_ccitt_false() {
        assert_eq!(crc_ccitt(b"123456789"), 0x29B1);
    }

    fn sample_profile() -> Vec<u8> {
        let mut p = vec![0xFFu8; 256];
        p[0] = 1; // 1000Hz
        p[1] = 1; // default stage
        p[2] = 4; // shift idx
        set_profile_dpis(&mut p, &[400, 800, 1600]);
        let btn: [[u8; 4]; 5] = [
            [0x80, 1, 0, 1],
            [0x80, 1, 0, 2],
            [0x80, 1, 0, 4],
            [0x80, 1, 0, 8],
            [0x90, 0x05, 0, 0],
        ];
        for (i, b) in btn.iter().enumerate() {
            p[32 + i * 4..36 + i * 4].copy_from_slice(b);
        }
        seal_sector(&mut p);
        p
    }

    #[test]
    fn profile_roundtrip() {
        let p = sample_profile();
        assert!(sector_valid(&p));
        assert_eq!(profile_dpis(&p), [400, 800, 1600, 0, 0]);
        assert_eq!(rate_from_ms(p[0]), Some(1000));
        let d = profile_dpis(&p);
        assert_eq!(
            decode_button(&p[32..36], &d, 4),
            ButtonAction::Mouse(MouseFn::Left)
        );
        assert_eq!(
            decode_button(&p[44..48], &d, 4),
            ButtonAction::Mouse(MouseFn::Back)
        );
        assert_eq!(
            decode_button(&p[48..52], &d, 4),
            ButtonAction::Dpi(DpiFn::Cycle)
        );
        let mut q = p.clone();
        q[100] ^= 1;
        assert!(!sector_valid(&q));
    }

    #[test]
    fn button_codec_roundtrip() {
        let dpis = [400, 800, 1600, 0, 0];
        for a in [
            ButtonAction::Mouse(MouseFn::Right),
            ButtonAction::Mouse(MouseFn::Forward),
            ButtonAction::Mouse(MouseFn::ScrollUp),
            ButtonAction::Key(KeyStroke {
                mods: mods::LCTRL,
                usage: 0x06,
            }),
            ButtonAction::Media(MediaFn::VolumeUp),
            ButtonAction::Dpi(DpiFn::Up),
            ButtonAction::ProfileCycle,
            ButtonAction::Disabled,
        ] {
            let (b, shift) = encode_button(&a, &dpis, 3).unwrap();
            assert!(shift.is_none());
            assert_eq!(decode_button(&b, &dpis, 0), a, "{a:?} -> {b:02x?}");
        }
        // DPI シフト: 既存ステージと同じ値ならその index、無ければ空き枠。
        let (b, s) = encode_button(&ButtonAction::Dpi(DpiFn::Shift(800)), &dpis, 3).unwrap();
        assert_eq!((b, s), ([0x90, 7, 0, 0], Some((1, 800))));
        let (_, s) = encode_button(&ButtonAction::Dpi(DpiFn::Shift(400 + 50)), &dpis, 3).unwrap();
        assert_eq!(s, Some((3, 450)));
        // マクロ等は生のまま往復する。
        let raw = [0x00, 0x05, 0x00, 0x20];
        let a = decode_button(&raw, &dpis, 0);
        assert_eq!(encode_button(&a, &dpis, 3).unwrap().0, raw);
    }

    #[test]
    fn directory_roundtrip() {
        let d = build_directory(&[(1, true), (2, false)], 256);
        assert!(sector_valid(&d));
        assert_eq!(parse_directory(&d, 5), Some(vec![(1, true), (2, false)]));
        assert_eq!(parse_directory(&[0u8; 256], 5), None);
    }

    #[test]
    fn dpi_list_range() {
        // [sensor, 100, 0xE000|50, 32000, 0]
        let mut p = vec![0u8];
        for v in [100u16, 0xE000 | 50, 32000, 0] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(parse_dpi_list(&p), Some((100, 32000, 50)));
    }

    #[test]
    fn names_and_fw() {
        let mut p = sample_profile();
        set_profile_name(&mut p, "FPS");
        assert_eq!(profile_name(&p), "FPS");
        set_profile_name(&mut p, "");
        assert_eq!(profile_name(&p), "");
        let fw = [0u8, b'R', b'Q', b'M', 0x01, 0x02, 0x00, 0x34];
        assert_eq!(format_fw(&fw).as_deref(), Some("RQM 01.02.B0034"));
    }

    #[test]
    fn ext_dpi_ranges_and_levels() {
        // 100..(step 1)..44000 の範囲表現 + 終端。
        let mut st = vec![];
        for v in [100u16, 0xE000 | 1, 44000, 0] {
            st.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(parse_dpi_ranges(&st), Some((100, 44000, 1)));
        // fn3: [sensor, dir, 400, 800, 1600, 0…]
        let mut p = vec![0u8, 0];
        for v in [400u16, 800, 1600, 0] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        p.resize(16, 0);
        assert_eq!(parse_dpi_levels(&p), vec![400, 800, 1600]);
    }

    #[test]
    fn ext_rate_mask_uses_byte1_only() {
        // byte0 は無関係、byte1 = 125 | 1000 | 8000。
        assert_eq!(ext_rate_mask(&[0xFF, 0b0100_1001]), vec![125, 1000, 8000]);
        assert_eq!(ext_rate_mask(&[0x00, 0x7F]).len(), 7);
    }
}
