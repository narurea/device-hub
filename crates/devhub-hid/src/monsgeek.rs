//! MonsGeek / Akko 磁気軸キーボード (RongYuan RY5088 ファーム) — FUN60 Ultra 向け。
//!
//! # 解析の出典
//!
//! - 公式 Web ドライバ (app.monsgeek.com) の `CommonKBRY5088` クラス (コマンド番号・
//!   フレーム・チェックサム・磁気軸テーブルのページ形式・キーマトリクスの 4 バイト表現)。
//!   ベンダー JS は echtzeit-solutions/monsgeek-akko-linux が保存・解析したものを参照。
//! - 同プロジェクトの `docs/PROTOCOL.md` (GPL — 事実のみ参照、コードは独自実装)。
//! - 公式デバイス DB (FUN60 Ultra = device id 2307/2352/2381/2387、key layout
//!   `Common61_gk06`、PID 0x5029/0x502D/0x5030)。
//!
//! 詳細は `docs/PROTOCOL_NOTES.md` §7。**実機未検証**。
//!
//! # ワイヤ形式
//!
//! 設定 IF (interface 2, usage FFFF:02) の 64B feature report (report ID 0)。
//! 送信 `[cmd, p1..p6, ck, payload…]`、ck = `255 - (Σ f[0..7] & 0xFF)` (Bit7)。
//! ライティング SET (0x07) だけ ck を f[8] に置く (Bit8: Σ f[0..8])。
//! 応答は GET_FEATURE で読む。多くは f[0] にコマンドのエコーがあるが、
//! キーマトリクス (0x8A) と磁気軸テーブル (0xE5) はエコー無しの生 64B。
//!
//! # 磁気軸テーブル (0x65 SET / 0xE5 GET)
//!
//! キーはマトリクス位置 (0..128、列優先 6 行) で並ぶ。sub-command ごとに
//! u16 LE ×128 (4 ページ) か u8 ×128 (2 ページ):
//!
//! | sub | 内容 | 型 |
//! |---|---|---|
//! | 0x00 | 作動点 (press) | u16 |
//! | 0x01 | リリース点 (lift) | u16 |
//! | 0x02 / 0x03 | RT 押し / 戻し感度 | u16 |
//! | 0x06 | 下端デッドゾーン | u16 |
//! | 0x07 | キーモード (bit7 = RT、下位 = Normal/DKS/MT/TGL/SnapTap) | u8 |
//! | 0xFB | 上端デッドゾーン (fw ≥ 1024) | u8 |
//!
//! 値 = mm × 精度係数 (fw < 768 → 10、768–1279 → 100、≥1280 → 200)。
//! 一括書き込みは `[0x65, sub, 1, page, commit, 0, 0, ck, 56B…]` — commit は
//! バッチの最後のページだけ 1 (そこでフラッシュ保存が走る)。

use devhub_core::backend::*;
use devhub_core::model::*;
use devhub_core::registry;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

/// コマンド番号 (公式 JS の FEA_CMD_*)。
pub mod cmd {
    pub const SET_RESET: u8 = 0x01;
    pub const SET_REPORT: u8 = 0x03;
    pub const SET_PROFILE: u8 = 0x04;
    pub const SET_DEBOUNCE: u8 = 0x06;
    pub const SET_LEDPARAM: u8 = 0x07;
    pub const SET_KBOPTION: u8 = 0x09;
    pub const SET_KEYMATRIX: u8 = 0x0A;
    pub const SET_MAG_CAL_MIN: u8 = 0x1C;
    pub const SET_MAG_CAL_MAX: u8 = 0x1E;
    pub const SET_MULTI_MAGNETISM: u8 = 0x65;
    pub const GET_REPORT: u8 = 0x83;
    pub const GET_PROFILE: u8 = 0x84;
    pub const GET_DEBOUNCE: u8 = 0x86;
    pub const GET_LEDPARAM: u8 = 0x87;
    pub const GET_KBOPTION: u8 = 0x89;
    pub const GET_KEYMATRIX: u8 = 0x8A;
    pub const GET_USB_VERSION: u8 = 0x8F;
    pub const GET_MULTI_MAGNETISM: u8 = 0xE5;
}

/// 磁気軸テーブルの sub-command。
pub mod mag {
    pub const PRESS: u8 = 0x00;
    pub const LIFT: u8 = 0x01;
    pub const RT_PRESS: u8 = 0x02;
    pub const RT_LIFT: u8 = 0x03;
    pub const BOTTOM_DZ: u8 = 0x06;
    pub const KEY_MODE: u8 = 0x07;
    pub const TOP_DZ: u8 = 0xFB;
}

/// マトリクス位置の数 (128 = 32 列 × 4 … 実際は 6 行 × 21 列 + 余り)。
pub const POSITIONS: usize = 128;
/// 1 ページあたりの書き込みバイト数。
const PAGE_W: usize = 56;
/// RT フラグ (キーモードの bit7)。
const RT_FLAG: u8 = 0x80;

/// FUN60 Ultra の公式 device id (GET_USB_VERSION の u32)。
const FUN60_ULTRA_IDS: &[u32] = &[2307, 2352, 2381, 2387];
/// 同じ 61 キー配置 (Common61_gk06) の FUN60 系。
const FUN60_FAMILY: &[(u32, &str)] = &[
    (2299, "MonsGeek FUN60"),
    (2304, "MonsGeek FUN60 PRO"),
    (2305, "MonsGeek FUN60 PRO"),
    (2306, "MonsGeek FUN60 MAX"),
    (2464, "MonsGeek FUN60 PRO"),
    (2600, "MonsGeek FUN60 PRO"),
    (2785, "MonsGeek FUN60 PRO"),
    (3299, "MonsGeek FUN60 MAX"),
];

// ==================================================================== 純関数 (テスト対象)

/// 64B フレームを組み立てる (Bit7 チェックサム: f[7] = 255 - Σf[0..7])。
pub fn frame(c: u8, params: &[u8], payload: &[u8]) -> [u8; 64] {
    let mut f = [0u8; 64];
    f[0] = c;
    let n = params.len().min(6);
    f[1..1 + n].copy_from_slice(&params[..n]);
    let m = payload.len().min(56);
    f[8..8 + m].copy_from_slice(&payload[..m]);
    let s: u32 = f[..7].iter().map(|&b| b as u32).sum();
    f[7] = 255 - (s & 0xFF) as u8;
    f
}

/// ライティング用 (Bit8: f[8] = 255 - Σf[0..8])。params は f[1..8] の 7 バイト。
pub fn frame_bit8(c: u8, params: &[u8]) -> [u8; 64] {
    let mut f = [0u8; 64];
    f[0] = c;
    let n = params.len().min(7);
    f[1..1 + n].copy_from_slice(&params[..n]);
    let s: u32 = f[..8].iter().map(|&b| b as u32).sum();
    f[8] = 255 - (s & 0xFF) as u8;
    f
}

/// ファーム版 → mm あたりの単位数。
pub fn precision(fw: u16) -> f32 {
    match fw {
        0..=767 => 10.,
        768..=1279 => 100.,
        _ => 200.,
    }
}

/// ポーリング: 0=8000 … 6=125。
pub const RATES: [i64; 7] = [8000, 4000, 2000, 1000, 500, 250, 125];

/// u16 LE テーブルを一括書き込みのページ列 (commit は最後だけ) にする。
pub fn paged_frames(sub: u8, data: &[u8], commit_last: bool) -> Vec<[u8; 64]> {
    let pages = data.len().div_ceil(PAGE_W);
    (0..pages)
        .map(|p| {
            let chunk = &data[p * PAGE_W..((p + 1) * PAGE_W).min(data.len())];
            let commit = commit_last && p == pages - 1;
            frame(
                cmd::SET_MULTI_MAGNETISM,
                &[sub, 1, p as u8, commit as u8],
                chunk,
            )
        })
        .collect()
}

/// キーマトリクス 4 バイト ⇄ `KeyAction`。
///
/// コード空間: 基本キーは HID usage そのまま (`[0,0,usage,0]`)。それ以外は
/// 4 バイトを BE で詰めた u32 (先頭バイト = 種別: 1 マウス, 3 コンシューマ, 10 Fn …)。
pub fn decode_key(b: &[u8]) -> KeyAction {
    match (b[0], b[1], b[2], b[3]) {
        (0, 0, 0, 0) => KeyAction::None,
        (0, 0, u, 0) => KeyAction::Code(u as u32),
        (0, m, u, 0) if u != 0 => KeyAction::Combo {
            mods: m,
            code: u as u32,
        },
        _ => KeyAction::Code(u32::from_be_bytes([b[0], b[1], b[2], b[3]])),
    }
}

pub fn encode_key(a: &KeyAction) -> HubResult<[u8; 4]> {
    Ok(match a {
        KeyAction::None => [0, 0, 0, 0],
        KeyAction::Code(c) if *c < 0x100 => [0, 0, *c as u8, 0],
        KeyAction::Code(c) => c.to_be_bytes(),
        KeyAction::Combo { mods, code } if *code < 0x100 => [0, *mods, *code as u8, 0],
        KeyAction::Transparent => {
            return Err(HubError::Invalid("メインレイヤーに透過は使えません".into()))
        }
        _ => return Err(HubError::Invalid("この割り当てには対応していません".into())),
    })
}

fn pack(b: [u8; 4]) -> u32 {
    u32::from_be_bytes(b)
}

/// 割り当て候補 (公式 JS の mouseKeyTable / specialBasicFnc から)。
fn catalog() -> Vec<KeyGroup> {
    let g = |name: &str, v: &[(u32, &str, &str)]| KeyGroup {
        name: name.into(),
        keys: v
            .iter()
            .map(|(c, l, s)| KeyChoice {
                code: *c,
                label: (*l).into(),
                short: Some((*s).into()),
            })
            .collect(),
    };
    vec![
        devhub_core::keys::basic_group(),
        g(
            "マウス",
            &[
                (pack([1, 0, 240, 0]), "マウス左", "M左"),
                (pack([1, 0, 241, 0]), "マウス右", "M右"),
                (pack([1, 0, 242, 0]), "マウス中", "M中"),
                (pack([1, 0, 243, 0]), "マウス戻る", "M戻"),
                (pack([1, 0, 244, 0]), "マウス進む", "M進"),
                (pack([1, 0, 245, 1]), "ホイール上", "W↑"),
                (pack([1, 0, 245, 255]), "ホイール下", "W↓"),
            ],
        ),
        g(
            "メディア",
            &[
                (pack([3, 0, 205, 0]), "再生/一時停止", "⏯"),
                (pack([3, 0, 183, 0]), "停止", "⏹"),
                (pack([3, 0, 181, 0]), "次の曲", "⏭"),
                (pack([3, 0, 182, 0]), "前の曲", "⏮"),
                (pack([3, 0, 233, 0]), "音量 +", "🔊"),
                (pack([3, 0, 234, 0]), "音量 −", "🔉"),
                (pack([3, 0, 226, 0]), "ミュート", "🔇"),
                (pack([3, 0, 146, 1]), "電卓", "Calc"),
                (pack([3, 0, 148, 1]), "マイコンピュータ", "PC"),
                (pack([3, 0, 138, 1]), "メール", "Mail"),
                (pack([3, 0, 111, 0]), "画面の明るさ +", "☀+"),
                (pack([3, 0, 112, 0]), "画面の明るさ −", "☀−"),
            ],
        ),
        g(
            "システム",
            &[
                (pack([10, 1, 0, 0]), "Fn", "Fn"),
                (pack([2, 0, 130, 0]), "スリープ", "Zz"),
            ],
        ),
    ]
}

/// ライティングのエフェクト (0x87/0x07 の mode)。13/20/21/22/25 はホスト駆動・
/// 画像系なので出さない。
fn effects() -> Vec<LightEffect> {
    vec![
        LightEffect::new(1, "常時点灯").color().still(),
        LightEffect::new(2, "呼吸").color(),
        LightEffect::new(3, "ネオン"),
        LightEffect::new(4, "ウェーブ").color().dir(),
        LightEffect::new(5, "リップル").color(),
        LightEffect::new(6, "雨粒").color(),
        LightEffect::new(7, "スネーク").color().dir(),
        LightEffect::new(8, "押下で点灯").color(),
        LightEffect::new(9, "収束").color(),
        LightEffect::new(10, "サインウェーブ").color(),
        LightEffect::new(11, "万華鏡").color().dir(),
        LightEffect::new(12, "ラインウェーブ").color().dir(),
        LightEffect::new(14, "レーザー").color(),
        LightEffect::new(15, "サークルウェーブ").color().dir(),
        LightEffect::new(16, "レインボー").color(),
        LightEffect::new(17, "レインダウン").color(),
        LightEffect::new(18, "流星").color(),
        LightEffect::new(19, "押下で消灯").color(),
        LightEffect::new(23, "トレイン").color(),
        LightEffect::new(24, "花火").color(),
    ]
}

/// GET_LEDPARAM 応答 → (mode, speed 0..4, brightness 0..4, direction, rainbow, rgb)。
pub fn decode_led(r: &[u8]) -> (u8, u8, u8, u8, bool, Rgb) {
    let mode = r[1];
    let speed = 4u8.saturating_sub(r[2].min(4));
    let bright = r[3].min(4);
    let dir = r[4] >> 4;
    let rainbow = r[4] & 0x0F == 8;
    let mut rgb = Rgb::new(r[5], r[6], r[7]);
    // 公式は白 0xFFFFFF を 0xFAFFFA で送る (読み戻しは白として扱う)。
    if rgb == Rgb::hex(0xFAFFFA) {
        rgb = Rgb::hex(0xFFFFFF);
    }
    (mode, speed, bright, dir, rainbow, rgb)
}

pub fn encode_led(mode: u8, speed: u8, bright: u8, dir: u8, rainbow: bool, rgb: Rgb) -> [u8; 64] {
    let opt = ((dir & 0x0F) << 4) | if rainbow { 8 } else { 7 };
    let rgb = if rgb == Rgb::hex(0xFFFFFF) {
        Rgb::hex(0xFAFFFA)
    } else {
        rgb
    };
    frame_bit8(
        cmd::SET_LEDPARAM,
        &[
            mode,
            4 - speed.min(4),
            bright.min(4),
            opt,
            rgb.r,
            rgb.g,
            rgb.b,
        ],
    )
}

// ==================================================================== テーブル

/// 磁気軸の全テーブル (マトリクス位置順、生の値)。
#[derive(Clone, Debug, PartialEq)]
pub struct MagTables {
    pub modes: Vec<u8>,
    pub press: Vec<u16>,
    pub lift: Vec<u16>,
    pub rt_press: Vec<u16>,
    pub rt_lift: Vec<u16>,
    pub bottom_dz: Vec<u16>,
    /// fw < 1024 では None。
    pub top_dz: Option<Vec<u8>>,
}

fn u16s(b: &[u8]) -> Vec<u16> {
    let mut v: Vec<u16> = b
        .chunks(2)
        .map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)]))
        .collect();
    v.resize(POSITIONS, 0);
    v
}

fn le_bytes(v: &[u16]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

impl MagTables {
    /// 位置 `pos` の設定を `KeyPerf` (mm) にする。
    pub fn perf(&self, pos: usize, f: f32) -> KeyPerf {
        let mm = |v: u16| v as f32 / f;
        let act = mm(self.press[pos]);
        let rel = mm(self.lift[pos]);
        KeyPerf {
            mode: if self.modes[pos] & RT_FLAG != 0 {
                TriggerMode::Rapid
            } else {
                TriggerMode::Normal
            },
            actuation: act,
            release: ((rel - act).abs() > 0.5 / f).then_some(rel),
            rt_first: act,
            rt_press: mm(self.rt_press[pos]),
            rt_release: mm(self.rt_lift[pos]),
            top_deadzone: self
                .top_dz
                .as_ref()
                .map(|t| t[pos] as f32 / f)
                .unwrap_or(0.),
            bottom_deadzone: mm(self.bottom_dz[pos]),
            continuous: false,
            switch_type: 0,
        }
    }

    /// `keys` に `p` を書き込んだ結果のテーブル。
    pub fn with(&self, keys: &[usize], p: &KeyPerf, f: f32, travel: f32) -> MagTables {
        let mut t = self.clone();
        let raw = |mm: f32| (mm.clamp(0., travel) * f).round().clamp(0., 65535.) as u16;
        let step = 1.0 / f;
        for &k in keys {
            if k >= POSITIONS {
                continue;
            }
            let act = p.actuation.max(step);
            t.press[k] = raw(act);
            t.lift[k] = raw(p.release.unwrap_or(act).max(step));
            t.rt_press[k] = raw(p.rt_press.max(step));
            t.rt_lift[k] = raw(p.rt_release.max(step));
            t.bottom_dz[k] = raw(p.bottom_deadzone);
            if let Some(td) = t.top_dz.as_mut() {
                td[k] = (p.top_deadzone.clamp(0., travel) * f)
                    .round()
                    .clamp(0., 255.) as u8;
            }
            let base = t.modes[k] & !RT_FLAG;
            t.modes[k] = match p.mode {
                TriggerMode::Rapid => base | RT_FLAG,
                TriggerMode::Normal => base,
            };
        }
        t
    }

    /// `self` → `new` で変わったテーブルの書き込みフレーム列 (最後のページだけ commit)。
    pub fn diff_frames(&self, new: &MagTables) -> Vec<[u8; 64]> {
        let mut batches: Vec<(u8, Vec<u8>)> = vec![];
        if self.modes != new.modes {
            batches.push((mag::KEY_MODE, new.modes.clone()));
        }
        for (sub, a, b) in [
            (mag::PRESS, &self.press, &new.press),
            (mag::LIFT, &self.lift, &new.lift),
            (mag::RT_PRESS, &self.rt_press, &new.rt_press),
            (mag::RT_LIFT, &self.rt_lift, &new.rt_lift),
            (mag::BOTTOM_DZ, &self.bottom_dz, &new.bottom_dz),
        ] {
            if a != b {
                batches.push((sub, le_bytes(b)));
            }
        }
        if let (Some(a), Some(b)) = (&self.top_dz, &new.top_dz) {
            if a != b {
                batches.push((mag::TOP_DZ, b.clone()));
            }
        }
        let n = batches.len();
        batches
            .into_iter()
            .enumerate()
            .flat_map(|(i, (sub, data))| paged_frames(sub, &data, i == n - 1))
            .collect()
    }
}

// ==================================================================== ドライバ

pub struct MonsgeekDriver {
    dev: Mutex<hidapi::HidDevice>,
    info: Mutex<DeviceInfo>,
    /// ファーム版 (精度係数・上端デッドゾーンの有無)。
    fw: u16,
    layout: Vec<LayoutKey>,
    /// 既定キーマトリクス (位置 → HID usage)。FUN60 系のみ。
    defaults: Option<Vec<u8>>,
    travel: f32,
    mags: Mutex<Option<MagTables>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn open(dev: hidapi::HidDevice, info: DeviceInfo) -> HubResult<MonsgeekDriver> {
    let mut d = MonsgeekDriver {
        dev: Mutex::new(dev),
        info: Mutex::new(info),
        fw: 0,
        layout: vec![],
        defaults: None,
        travel: 3.4,
        mags: Mutex::new(None),
    };
    let (id, fw) = d
        .query(cmd::GET_USB_VERSION, &[])
        .map(|r| {
            (
                u32::from_le_bytes([r[1], r[2], r[3], r[4]]),
                u16::from_le_bytes([r[7], r[8]]),
            )
        })
        .unwrap_or((0, 0));
    d.fw = fw;
    let family = FUN60_ULTRA_IDS
        .contains(&id)
        .then_some("MonsGeek FUN60 Ultra")
        .or_else(|| FUN60_FAMILY.iter().find(|x| x.0 == id).map(|x| x.1));
    {
        let mut i = lock(&d.info);
        if fw != 0 {
            i.firmware = Some(format!("{}.{:02}", fw >> 8, fw & 0xFF));
        }
        i.board_id = (id != 0).then_some(id);
        if let Some(n) = family {
            i.name = n.into();
            i.model = if FUN60_ULTRA_IDS.contains(&id) {
                "fun60-ultra".into()
            } else {
                "fun60".into()
            };
        }
    }
    if family.is_some() {
        d.layout = registry::fun60_layout();
        d.defaults = Some(registry::FUN60_DEFAULT_MATRIX.to_vec());
    } else {
        // 未知の RY5088 機: 実機のキーマトリクスから並びだけ作る (位置 = 列×6+行)。
        let m = d.read_keymatrix(0, 0).unwrap_or_default();
        d.layout = (0..POSITIONS)
            .filter(|&p| m.get(p * 4..p * 4 + 4).is_some_and(|b| b != [0, 0, 0, 0]))
            .map(|p| {
                let label = match decode_key(&m[p * 4..p * 4 + 4]) {
                    KeyAction::Code(c) if c < 0x100 => devhub_core::keys::hid_short(c as u16),
                    _ => format!("#{p}"),
                };
                LayoutKey {
                    id: p as u16,
                    x: (p / 6) as f32,
                    y: (p % 6) as f32,
                    w: 1.0,
                    h: 1.0,
                    label,
                }
            })
            .collect();
    }
    Ok(d)
}

impl MonsgeekDriver {
    fn factor(&self) -> f32 {
        precision(self.fw)
    }

    fn has_top_dz(&self) -> bool {
        self.fw >= 1024
    }

    fn send(&self, f: &[u8; 64]) -> HubResult<()> {
        let mut buf = [0u8; 65];
        buf[1..].copy_from_slice(f);
        lock(&self.dev)
            .send_feature_report(&buf)
            .map_err(|e| HubError::Io(format!("monsgeek feature write: {e}")))
    }

    fn get(&self) -> HubResult<[u8; 64]> {
        let mut buf = [0u8; 65];
        let n = lock(&self.dev)
            .get_feature_report(&mut buf)
            .map_err(|e| HubError::Io(format!("monsgeek feature read: {e}")))?;
        if n < 2 {
            return Err(HubError::Protocol("monsgeek: empty feature report".into()));
        }
        let mut r = [0u8; 64];
        r.copy_from_slice(&buf[1..]);
        Ok(r)
    }

    /// エコー付き GET (応答 f[0] == cmd を待つ。最大 2 回送り直す)。
    fn query(&self, c: u8, params: &[u8]) -> HubResult<[u8; 64]> {
        let f = frame(c, params, &[]);
        for _ in 0..3 {
            self.send(&f)?;
            for _ in 0..4 {
                sleep(Duration::from_millis(12));
                let r = self.get()?;
                if r[0] == c {
                    return Ok(r);
                }
            }
        }
        Err(HubError::Protocol(format!(
            "monsgeek: no reply to {c:#04x}"
        )))
    }

    /// エコー無しの GET (キーマトリクス・磁気軸テーブル)。
    fn query_raw(&self, f: &[u8; 64]) -> HubResult<[u8; 64]> {
        self.send(f)?;
        sleep(Duration::from_millis(15));
        self.get()
    }

    /// SET (ACK は返らない — 送信成功で受理とみなす)。
    fn set(&self, f: &[u8; 64], settle_ms: u64) -> HubResult<()> {
        self.send(f)?;
        sleep(Duration::from_millis(settle_ms));
        Ok(())
    }

    fn mag_table(&self, sub: u8, pages: u8) -> HubResult<Vec<u8>> {
        let mut out = Vec::with_capacity(pages as usize * 64);
        for p in 0..pages {
            out.extend_from_slice(&self.query_raw(&frame(
                cmd::GET_MULTI_MAGNETISM,
                &[sub, 1, p],
                &[],
            ))?);
        }
        Ok(out)
    }

    fn read_mags(&self) -> HubResult<MagTables> {
        let mut modes = self.mag_table(mag::KEY_MODE, 2)?;
        modes.resize(POSITIONS, 0);
        let t = MagTables {
            modes,
            press: u16s(&self.mag_table(mag::PRESS, 4)?),
            lift: u16s(&self.mag_table(mag::LIFT, 4)?),
            rt_press: u16s(&self.mag_table(mag::RT_PRESS, 4)?),
            rt_lift: u16s(&self.mag_table(mag::RT_LIFT, 4)?),
            bottom_dz: u16s(&self.mag_table(mag::BOTTOM_DZ, 4)?),
            top_dz: if self.has_top_dz() {
                self.mag_table(mag::TOP_DZ, 2).ok().map(|mut v| {
                    v.resize(POSITIONS, 0);
                    v
                })
            } else {
                None
            },
        };
        *lock(&self.mags) = Some(t.clone());
        Ok(t)
    }

    /// キーマトリクス (profile, layer) 512B = 128 位置 × 4。
    fn read_keymatrix(&self, profile: u8, layer: u8) -> HubResult<Vec<u8>> {
        let mut out = Vec::with_capacity(512);
        for page in 0..8u8 {
            let f = frame(cmd::GET_KEYMATRIX, &[profile, 0xFF, page, layer], &[]);
            out.extend_from_slice(&self.query_raw(&f)?);
        }
        Ok(out)
    }

    fn write_key(&self, profile: u8, pos: u8, b: [u8; 4]) -> HubResult<()> {
        // [0x0A, profile, pos, 0, 0, enabled=1, layer=0, ck, b0..b3]
        let f = frame(cmd::SET_KEYMATRIX, &[profile, pos, 0, 0, 1, 0], &b);
        self.set(&f, 40)
    }

    fn profile(&self) -> u8 {
        self.query(cmd::GET_PROFILE, &[])
            .map(|r| r[1].min(3))
            .unwrap_or(0)
    }

    fn led(&self) -> HubResult<[u8; 64]> {
        self.query(cmd::GET_LEDPARAM, &[])
    }

    fn kb_option(&self) -> HubResult<[u8; 64]> {
        self.query(cmd::GET_KBOPTION, &[])
    }

    /// KB オプションを読み → 1 バイト差し替え → 書く (公式もフィールド単位の SET は無い)。
    fn set_kb_option(&self, idx: usize, v: u8) -> HubResult<()> {
        let r = self.kb_option()?;
        // SET は [9, 0(OS は接続から再判定), fn, 誤操作防止, RT 安定 level, WASD]。
        let mut p = [0u8, r[2], r[3], r[4].min(5), r[5]];
        p[idx - 1] = v;
        self.set(&frame(cmd::SET_KBOPTION, &p, &[]), 120)
    }

    fn write_led(&self, f: impl FnOnce(&mut (u8, u8, u8, u8, bool, Rgb))) -> HubResult<()> {
        let mut s = decode_led(&self.led()?);
        f(&mut s);
        self.set(&encode_led(s.0, s.1, s.2, s.3, s.4, s.5), 80)
    }

    fn perf_caps(&self) -> PerfCaps {
        let step = 1.0 / self.factor();
        PerfCaps {
            travel: self.travel,
            min: step.max(0.01),
            step,
            rapid_trigger: true,
            separate_release: true,
            rt_first: false,
            deadzones: true,
            continuous_rt: false,
            switch_types: vec![],
            switch_ids: vec![],
            per_key: true,
            live_travel: false,
        }
    }
}

impl DeviceDriver for MonsgeekDriver {
    fn info(&self) -> DeviceInfo {
        let mut i = lock(&self.info).clone();
        i.supported = true;
        i
    }

    fn caps(&self) -> Capabilities {
        use section::*;
        let opts = |v: &[(i64, &str)]| v.iter().map(|(a, b)| ChoiceOpt::new(*a, *b)).collect();
        Capabilities {
            settings: vec![
                SettingSpec::choice(
                    "polling",
                    PERFORMANCE,
                    "ポーリングレート",
                    opts(&[
                        (125, "125"),
                        (250, "250"),
                        (500, "500"),
                        (1000, "1K"),
                        (2000, "2K"),
                        (4000, "4K"),
                        (8000, "8K"),
                    ]),
                )
                .help("キーボードの報告頻度 (Hz)"),
                SettingSpec::slider("debounce", KEYBOARD, "デバウンス", 0., 20., 1., "ms")
                    .help("チャタリング防止の待ち時間"),
                SettingSpec::choice(
                    "rt_stability",
                    KEYBOARD,
                    "RT 安定化",
                    opts(&[
                        (0, "オフ"),
                        (25, "25ms"),
                        (50, "50ms"),
                        (75, "75ms"),
                        (100, "100ms"),
                        (125, "125ms"),
                    ]),
                )
                .help("ラピッドトリガーの誤反応を抑える待ち時間"),
                SettingSpec::toggle("anti_mistouch", KEYBOARD, "誤操作防止")
                    .help("意図しない軽い接触を無視します"),
                SettingSpec::toggle("wasd_swap", KEYBOARD, "WASD ⇄ 矢印キー入れ替え")
                    .help("Fn+W と同じ切替"),
                SettingSpec::toggle("led_rainbow", LIGHTING, "レインボーカラー")
                    .help("エフェクトの色を虹色で循環させます (公式の「炫彩」)"),
            ],
            actions: vec![
                ActionSpec::new("calib_start", PERFORMANCE, "磁軸キャリブレーション開始")
                    .help("キーから手を離して実行 → 約 2 秒後、全キーを底まで押してから「終了」を押してください")
                    .confirm("キーから手を離した状態で開始してください"),
                ActionSpec::new("calib_end", PERFORMANCE, "磁軸キャリブレーション終了")
                    .help("全キーを底まで押し終えたら実行して保存します"),
                ActionSpec::new("factory_reset", SYSTEM, "工場出荷時に戻す")
                    .danger("キーマップ・ライティング・トリガー設定がすべて初期化されます"),
            ],
            profiles: Some(ProfileCaps {
                count: 4,
                renamable: false,
            }),
            mouse: None,
            keyboard: Some(KeyboardCaps {
                layout: self.layout.clone(),
                layers: vec!["メイン".into()],
                catalog: catalog(),
                combos: true,
                perf: Some(self.perf_caps()),
                advanced: vec![],
                macros: None,
            }),
            lighting: vec![LightArea {
                id: 0,
                name: "キーボード".into(),
                effects: effects(),
                brightness_max: 4,
                speed_max: 4,
                directions: vec![
                    "方向 1 (右 / 外 / 反時計)".into(),
                    "方向 2 (左 / 内 / 時計)".into(),
                    "方向 3 (下)".into(),
                    "方向 4 (上)".into(),
                ],
                switchable: true,
                zones: vec![],
                per_key: false,
                direct_drive: false,
                live_readback: false,
            }],
            battery: false,
            peq: None,
            firmware_feed: None,
        }
    }

    fn read(&self) -> HubResult<DeviceState> {
        let mut settings = BTreeMap::new();
        if let Ok(r) = self.query(cmd::GET_REPORT, &[]) {
            if let Some(&hz) = RATES.get(r[2] as usize) {
                settings.insert("polling".into(), SettingValue::Choice(hz));
            }
        }
        if let Ok(r) = self.query(cmd::GET_DEBOUNCE, &[]) {
            settings.insert("debounce".into(), SettingValue::Num(r[1] as f32));
        }
        if let Ok(r) = self.kb_option() {
            settings.insert("anti_mistouch".into(), SettingValue::Bool(r[3] != 0));
            let st = if r[4] > 5 { 0 } else { r[4] as i64 * 25 };
            settings.insert("rt_stability".into(), SettingValue::Choice(st));
            settings.insert("wasd_swap".into(), SettingValue::Bool(r[5] == 1));
        }
        let mut lighting = BTreeMap::new();
        if let Ok(r) = self.led() {
            let (mode, speed, bright, dir, rainbow, rgb) = decode_led(&r);
            settings.insert("led_rainbow".into(), SettingValue::Bool(rainbow));
            lighting.insert(
                0u8,
                LightingState {
                    on: mode != 0,
                    effect: if mode == 0 { 1 } else { mode },
                    color: rgb,
                    brightness: bright,
                    speed,
                    direction: dir,
                    per_key: BTreeMap::new(),
                    zone_on: vec![],
                    palette: vec![],
                    palette_sel: 0,
                    color_correction: None,
                },
            );
        }
        let profile = self.profile();
        let f = self.factor();
        let mags = self.read_mags()?;
        let perf = self
            .layout
            .iter()
            .map(|k| mags.perf(k.id as usize, f))
            .collect();
        let km = self.read_keymatrix(profile, 0)?;
        let keymap = vec![self
            .layout
            .iter()
            .map(|k| {
                let p = k.id as usize * 4;
                decode_key(&km[p..p + 4])
            })
            .collect()];
        Ok(DeviceState {
            profile: ProfileState {
                active: profile,
                names: vec![],
            },
            settings,
            mouse: None,
            keyboard: Some(KeyboardState {
                keymap,
                perf,
                advanced: vec![],
                macros: vec![],
            }),
            lighting,
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::Setting { id, value } => match id.as_str() {
                "polling" => {
                    let code = RATES
                        .iter()
                        .position(|&x| x == value.as_choice())
                        .ok_or_else(|| HubError::Invalid("polling rate".into()))?;
                    // レートは f[2] (f[1] は予約で 0)。
                    self.set(&frame(cmd::SET_REPORT, &[0, code as u8], &[]), 150)
                }
                "debounce" => self.set(
                    &frame(
                        cmd::SET_DEBOUNCE,
                        &[value.as_num().clamp(0., 50.) as u8],
                        &[],
                    ),
                    120,
                ),
                "anti_mistouch" => self.set_kb_option(3, value.as_bool() as u8),
                "rt_stability" => self.set_kb_option(4, (value.as_choice() / 25).clamp(0, 5) as u8),
                "wasd_swap" => self.set_kb_option(5, value.as_bool() as u8),
                "led_rainbow" => self.write_led(|s| s.4 = value.as_bool()),
                _ => Err(HubError::Unsupported("setting")),
            },
            Change::Lighting { area: 0, state } => self.write_led(|s| {
                s.0 = if state.on { state.effect.max(1) } else { 0 };
                s.1 = state.speed.min(4);
                s.2 = state.brightness.min(4);
                s.3 = state.direction.min(3);
                s.5 = state.color;
            }),
            Change::Profile(p) => {
                self.set(&frame(cmd::SET_PROFILE, &[(*p).min(3)], &[]), 300)?;
                *lock(&self.mags) = None;
                Ok(())
            }
            Change::KeyPerf { keys, perf } => {
                let cur = match lock(&self.mags).clone() {
                    Some(t) => t,
                    None => self.read_mags()?,
                };
                let pos: Vec<usize> = keys
                    .iter()
                    .filter_map(|&i| self.layout.get(i).map(|k| k.id as usize))
                    .collect();
                let new = cur.with(&pos, perf, self.factor(), self.travel);
                let frames = cur.diff_frames(&new);
                let n = frames.len();
                for (i, f) in frames.iter().enumerate() {
                    // 最後 (commit) の後はフラッシュ保存待ち。読み戻しが化けないよう長めに。
                    self.set(f, if i + 1 == n { 500 } else { 30 })?;
                }
                *lock(&self.mags) = Some(new);
                Ok(())
            }
            Change::KeyMap {
                layer: 0,
                key,
                action,
            } => {
                let k = self
                    .layout
                    .get(*key)
                    .ok_or_else(|| HubError::Invalid("key".into()))?;
                let b = encode_key(action)?;
                self.write_key(self.profile(), k.id as u8, b)
            }
            Change::KeyMapReset { layer: 0 } => {
                let d = self
                    .defaults
                    .as_ref()
                    .ok_or(HubError::Unsupported("この機種の既定キーマップ"))?;
                let prof = self.profile();
                for k in &self.layout {
                    let u = d.get(k.id as usize).copied().unwrap_or(0);
                    self.write_key(prof, k.id as u8, [0, 0, u, 0])?;
                }
                Ok(())
            }
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            "calib_start" => {
                // 最小位置 (離した状態) を 2 秒測ってから最大位置の測定を始める。
                self.set(&frame(cmd::SET_MAG_CAL_MIN, &[1], &[]), 2000)?;
                self.set(&frame(cmd::SET_MAG_CAL_MIN, &[0], &[]), 100)?;
                self.set(&frame(cmd::SET_MAG_CAL_MAX, &[1], &[]), 50)?;
                Ok(ActionOutcome {
                    message: Some(
                        "測定中: 全キーを 1 回ずつ底まで押してから「キャリブレーション終了」を押してください"
                            .into(),
                    ),
                    reload: false,
                })
            }
            "calib_end" => {
                self.set(&frame(cmd::SET_MAG_CAL_MAX, &[0], &[]), 500)?;
                Ok(ActionOutcome {
                    message: Some("キャリブレーションを保存しました".into()),
                    reload: false,
                })
            }
            "factory_reset" => {
                self.set(&frame(cmd::SET_RESET, &[], &[]), 2000)?;
                *lock(&self.mags) = None;
                Ok(ActionOutcome {
                    message: Some("工場出荷状態に戻しました".into()),
                    reload: true,
                })
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }
}

/// 診断用: 版・LED・ポーリング・磁気軸テーブルの先頭を表示 (読み取りのみ)。
pub fn probe(d: &MonsgeekDriver) -> String {
    let mut s = String::new();
    let i = lock(&d.info).clone();
    s.push_str(&format!(
        "name: {}  fw: {:?} (factor {})  device id: {:?}\n",
        i.name,
        i.firmware,
        d.factor(),
        i.board_id
    ));
    for (c, n) in [
        (cmd::GET_REPORT, "GET_REPORT"),
        (cmd::GET_PROFILE, "GET_PROFILE"),
        (cmd::GET_DEBOUNCE, "GET_DEBOUNCE"),
        (cmd::GET_LEDPARAM, "GET_LEDPARAM"),
        (cmd::GET_KBOPTION, "GET_KBOPTION"),
    ] {
        match d.query(c, &[]) {
            Ok(r) => s.push_str(&format!("{n:<14} {:02x?}\n", &r[..16])),
            Err(e) => s.push_str(&format!("{n:<14} {e}\n")),
        }
    }
    match d.read_mags() {
        Ok(t) => {
            let f = d.factor();
            for k in d.layout.iter().take(6) {
                s.push_str(&format!(
                    "  {:<6} {:?}\n",
                    k.label,
                    t.perf(k.id as usize, f)
                ));
            }
        }
        Err(e) => s.push_str(&format!("magnetism: {e}\n")),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_bit7_matches_doc() {
        // 公式 doc の例: F7 → ck 0x08。
        let f = frame(0xF7, &[], &[]);
        assert_eq!(f[7], 0x08);
        let f = frame(0x8F, &[], &[]);
        assert_eq!(f[7], 0x70);
        let f = frame(0x11, &[1], &[]);
        assert_eq!(f[7], 0xED);
    }

    #[test]
    fn led_roundtrip() {
        let f = encode_led(4, 3, 2, 1, true, Rgb::hex(0xFFFFFF));
        let s: u32 = f[..8].iter().map(|&b| b as u32).sum();
        assert_eq!(f[8], 255 - (s & 0xFF) as u8);
        assert_eq!(&f[..8], &[7, 4, 1, 2, 0x18, 0xFA, 0xFF, 0xFA]);
        let mut r = [0u8; 64];
        r[..8].copy_from_slice(&[0x87, 4, 1, 2, 0x18, 0xFA, 0xFF, 0xFA]);
        assert_eq!(decode_led(&r), (4, 3, 2, 1, true, Rgb::hex(0xFFFFFF)));
    }

    #[test]
    fn precision_by_fw() {
        assert_eq!(precision(0x0200), 10.);
        assert_eq!(precision(0x0300), 100.);
        assert_eq!(precision(0x0500), 200.);
    }

    #[test]
    fn paging_and_commit() {
        let data = vec![1u8; 256];
        let fr = paged_frames(mag::PRESS, &data, true);
        assert_eq!(fr.len(), 5);
        assert_eq!(&fr[0][..5], &[0x65, 0, 1, 0, 0]);
        assert_eq!(&fr[4][..5], &[0x65, 0, 1, 4, 1]);
        // 最後のページは 256-224 = 32 バイト。
        assert_eq!(fr[4][8 + 31], 1);
        assert_eq!(fr[4][8 + 32], 0);
    }

    fn tables() -> MagTables {
        MagTables {
            modes: vec![0; POSITIONS],
            press: vec![200; POSITIONS],
            lift: vec![200; POSITIONS],
            rt_press: vec![30; POSITIONS],
            rt_lift: vec![30; POSITIONS],
            bottom_dz: vec![0; POSITIONS],
            top_dz: Some(vec![0; POSITIONS]),
        }
    }

    #[test]
    fn perf_edit_and_diff() {
        let t = tables();
        let p = t.perf(9, 100.);
        assert_eq!(p.mode, TriggerMode::Normal);
        assert!((p.actuation - 2.0).abs() < 1e-6);
        assert_eq!(p.release, None);
        let mut q = p;
        q.mode = TriggerMode::Rapid;
        q.actuation = 0.5;
        q.rt_press = 0.1;
        let n = t.with(&[9, 15], &q, 100., 3.4);
        assert_eq!(n.press[9], 50);
        assert_eq!(n.lift[15], 50);
        assert_eq!(n.modes[9], 0x80);
        assert_eq!(n.rt_press[15], 10);
        assert_eq!(n.rt_lift[9], 30);
        let fr = t.diff_frames(&n);
        // modes(3p) + press(5) + lift(5) + rt_press(5) = 18、最後だけ commit。
        assert_eq!(fr.len(), 18);
        assert!(fr[..17].iter().all(|f| f[4] == 0));
        assert_eq!(fr[17][4], 1);
        assert_eq!(fr[0][1], mag::KEY_MODE);
    }

    #[test]
    fn key_codec() {
        for a in [
            KeyAction::None,
            KeyAction::Code(0x04),
            KeyAction::Combo {
                mods: mods::LCTRL,
                code: 0x06,
            },
            KeyAction::Code(pack([1, 0, 240, 0])),
            KeyAction::Code(pack([3, 0, 233, 0])),
        ] {
            assert_eq!(decode_key(&encode_key(&a).unwrap()), a);
        }
    }

    #[test]
    fn fun60_layout_matches_matrix() {
        let l = registry::fun60_layout();
        assert_eq!(l.len(), 60);
        let m = registry::FUN60_DEFAULT_MATRIX;
        let mut seen = std::collections::BTreeSet::new();
        for k in &l {
            assert!(seen.insert(k.id), "dup {}", k.id);
            let u = m[k.id as usize];
            assert_ne!(u, 0, "{} at {}", k.label, k.id);
        }
        // 位置表の自己検査: 刻印と既定 usage が一致すること (代表キー)。
        let usage = |label: &str| m[l.iter().find(|k| k.label == label).unwrap().id as usize];
        assert_eq!(usage("Esc"), 0x29);
        assert_eq!(usage("Q"), 0x14);
        assert_eq!(usage("A"), 0x04);
        assert_eq!(usage("Space"), 0x2C);
        assert_eq!(usage("Enter"), 0x28);
        assert_eq!(usage("RCtrl"), 0xE4);
    }
}
