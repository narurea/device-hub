//! ファームウェア更新通知 (読み取り専用 — 書き込み/DFU は行わない)。
//! アプリ本体の更新確認は `app` (GitHub Releases / ブランチの版)。
//!
//! - mouse.fit: `Config/<ui_folder>/FWLINK.txt` → zip の ETag/Last-Modified/Length が指紋
//! - Sparklink: `getKeyboardFirmware` (board_id/vid/pid + Xs-API-Key) → バージョン比較
//!
//! ネットワーク断・タイムアウト・サーバーエラーは全部 `Err(UpdateError)` で静かに
//! 返すだけ — アプリ側は失敗を無視してよい。

use devhub_core::model::FirmwareFeed;

/// Device HUB 本体の更新確認 (GitHub)。
pub mod app;
pub use app::{check_app, AppUpdate};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);
const SPARKLINK_API: &str = "https://api.sparklinkplayjoy.com/api/v1/getKeyboardFirmware";
// hub.xsyd.top の同梱 SDK が送る公開キー (firmware 参照のみに使われる)。
const SPARKLINK_KEY: &str = "860DE3DB584158536BC89030D8640E29";
const MOUSEFIT_BASE: &str = "https://www.mouse.fit";

/// 1 回の確認結果。`fingerprint` は次回比較用に保存する。
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateStatus {
    /// 新しいファームウェアが公開されている。
    /// 初回観測 (基準値なし) は必ず false — いきなり通知は出さない。
    pub new_available: bool,
    /// 分かれば新バージョンの表示用文字列。
    pub version: Option<String>,
    /// 「公式ページを開く」ボタンの遷移先。
    pub url: Option<String>,
    /// 配布物の変化を検知する指紋 (ETag+日時+サイズ、またはバージョン文字列)。
    pub fingerprint: String,
    /// 確認時刻 (UNIX 秒)。
    pub checked_at: u64,
}

/// 失敗は全部この 1 型 — 呼び出し側は単に今回分を捨てればよい。
#[derive(Debug)]
pub struct UpdateError(pub String);

/// 公式の配布元を 1 回確認する。
///
/// `last_fp`: 前回保存した指紋 (空 = 初回)。`current_fw`: デバイスの現在バージョン。
/// 成功したら必ず fingerprint を保存する (初回は基準値確立 = new_available: false)。
pub fn check(
    feed: &FirmwareFeed,
    current_fw: Option<&str>,
    last_fp: &str,
) -> Result<UpdateStatus, UpdateError> {
    let checked_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let mut st = match feed {
        FirmwareFeed::MouseFit { ui_folder } => mousefit(&agent, ui_folder)?,
        FirmwareFeed::Sparklink { vid, pid, board_id } => {
            sparklink(&agent, *vid, *pid, *board_id, current_fw)?
        }
    };
    st.checked_at = checked_at;
    // 指紋の変化は「バージョン比較ができなかった系統」でのみ新しい扱いにする —
    // バージョンが取れて「同じ/古い」と分かるなら、配布物のヘッダ変更だけで通知しない。
    st.new_available =
        st.new_available || (st.version.is_none() && is_new(&st.fingerprint, last_fp));
    Ok(st)
}

/// `a.b.c` 形式の版比較 — `dist > cur` のときだけ true。
/// 両方とも数値の点列に parse できなければ比較不能 → 新しいとは扱わない。
fn version_newer(dist: &str, cur: &str) -> bool {
    fn parse(s: &str) -> Option<Vec<u32>> {
        s.trim_start_matches(['v', 'V'])
            .split('.')
            .map(|p| p.parse::<u32>())
            .collect::<Result<_, _>>()
            .ok()
    }
    let (Some(mut d), Some(mut c)) = (parse(dist), parse(cur)) else {
        return false;
    };
    let n = d.len().max(c.len());
    d.resize(n, 0);
    c.resize(n, 0);
    d > c
}

/// 指紋が「新しい」か。初回 (last_fp 空) は必ず false = 基準値を作るだけ。
/// 指紋が取れない応答 (Sparklink の空 data 等) でも false。
fn is_new(fp: &str, last_fp: &str) -> bool {
    !fp.is_empty() && !last_fp.is_empty() && fp != last_fp
}

/// mouse.fit: FWLINK.txt が指す zip の HTTP ヘッダを指紋にする。
fn mousefit(agent: &ureq::Agent, ui_folder: &str) -> Result<UpdateStatus, UpdateError> {
    let feed_url = format!("{MOUSEFIT_BASE}/Config/{ui_folder}/FWLINK.txt");
    let body = agent
        .get(&feed_url)
        .call()
        .map_err(|e| UpdateError(format!("FWLINK: {e}")))?
        .into_string()
        .map_err(|e| UpdateError(format!("FWLINK read: {e}")))?;
    let zip = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .ok_or_else(|| UpdateError("FWLINK is empty".into()))?;
    let zip_url = if zip.starts_with("http") {
        zip.to_string()
    } else {
        format!("{MOUSEFIT_BASE}/{}", zip.trim_start_matches('/'))
    };
    // zip は 14MB 級なので HEAD だけ。
    let head = agent
        .head(&zip_url)
        .call()
        .map_err(|e| UpdateError(format!("zip HEAD: {e}")))?;
    let etag = head.header("ETag").unwrap_or("");
    let lastmod = head.header("Last-Modified").unwrap_or("");
    let len = head.header("Content-Length").unwrap_or("");
    let fingerprint = format!("{etag}|{lastmod}|{len}");
    // バージョンらしき数字列をファイル名から拾う (…/3955/xxx_3955JM.zip → "3955")。
    let version = zip_url
        .rsplit('/')
        .next()
        .and_then(|f| f.strip_suffix(".zip"))
        .map(|f| f.to_string());
    Ok(UpdateStatus {
        new_available: false,
        version,
        url: Some("https://www.mouse.fit/".into()),
        fingerprint,
        checked_at: 0,
    })
}

/// Sparklink: getKeyboardFirmware → data[] のバージョンを現在値と比較する。
fn sparklink(
    agent: &ureq::Agent,
    vid: u16,
    pid: u16,
    board_id: Option<u32>,
    current_fw: Option<&str>,
) -> Result<UpdateStatus, UpdateError> {
    let board_id = board_id.ok_or_else(|| UpdateError("no board_id".into()))?;
    let url = format!(
        "{SPARKLINK_API}?board_id={board_id}&vid={}&pid={}",
        fmt_vidpid(vid),
        fmt_vidpid(pid)
    );
    let res = agent
        .get(&url)
        .set("Xs-API-Key", SPARKLINK_KEY)
        .call()
        .map_err(|e| UpdateError(format!("api: {e}")))?;
    let json: serde_json::Value = res
        .into_json()
        .map_err(|e| UpdateError(format!("json: {e}")))?;
    let code = json.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    // 415「固件未上传」は正常な「公開物なし」。他の非 0 も保守的に無し扱い。
    if code != 0 && code != 200 {
        return Ok(UpdateStatus {
            new_available: false,
            version: None,
            url: Some("https://hub.xsyd.top/".into()),
            fingerprint: String::new(),
            checked_at: 0,
        });
    }
    let data = json
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    // 応答のキー名は公開物が無いため確定していない — ありそうな名前を順に拾う。
    let pick = |keys: &[&str]| -> Option<String> {
        data.iter().find_map(|item| {
            keys.iter().find_map(|k| {
                item.get(*k)
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Number(n) => n.to_string(),
                        _ => String::new(),
                    })
                    .filter(|s| !s.is_empty())
            })
        })
    };
    let version = pick(&[
        "version",
        "firmwareVersion",
        "fw_version",
        "fwVersion",
        "ver",
    ]);
    let file = pick(&[
        "url",
        "file",
        "fileUrl",
        "firmwareUrl",
        "downloadUrl",
        "path",
    ]);
    // 指紋は「バージョン + ファイル URL」。バージョン比較でも更新検知する。
    let fingerprint = match (&version, &file) {
        (None, None) => String::new(),
        (v, f) => format!(
            "{}|{}",
            v.clone().unwrap_or_default(),
            f.clone().unwrap_or_default()
        ),
    };
    // 比較できる版同士だけ「新しい」とする — 表記違い・現在値不明・配布の方が
    // 古い場合は新規と言わない (順序を定義できない版は「不明」)。
    let newer = match (&version, current_fw) {
        (Some(v), Some(cur)) => version_newer(v, cur),
        _ => false,
    };
    Ok(UpdateStatus {
        new_available: newer,
        version,
        url: Some("https://hub.xsyd.top/".into()),
        fingerprint,
        checked_at: 0,
    })
}

/// API は vid/pid を 10 進で受ける実績があるが、4 桁 16 進でも通した実績がある。
/// ここでは公式サイトの表記に合わせて 16 進 4 桁で送る。
fn fmt_vidpid(v: u16) -> String {
    format!("{v:04X}")
}

/// Sparklink の軸名データベース (`getAxisListV3`)。公式 HUB はこれで軸ライブラリの
/// u16 id → 名称 (例「磁白軸」) を引いている。失敗は `Err` で静かに返すだけ。
pub fn sparklink_axis_names(
    board_id: u32,
    vid: u16,
    pid: u16,
) -> Result<std::collections::BTreeMap<u16, String>, UpdateError> {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let url = format!(
        "https://api.sparklinkplayjoy.com/api/v1/getAxisListV3?board_id={board_id}&vid={}&pid={}&t={t}",
        fmt_vidpid(vid),
        fmt_vidpid(pid)
    );
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let res = agent
        .get(&url)
        .set("Xs-API-Key", SPARKLINK_KEY)
        .call()
        .map_err(|e| UpdateError(format!("axis api: {e}")))?;
    let json: serde_json::Value = res
        .into_json()
        .map_err(|e| UpdateError(format!("axis json: {e}")))?;
    let mut out = std::collections::BTreeMap::new();
    // data: [{list:[{axis_id, axis_name}, …]}, …]
    for group in json
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default()
    {
        for a in group
            .get("list")
            .and_then(|l| l.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let id = a.get("axis_id").and_then(|v| v.as_u64()).map(|v| v as u16);
            let name = a
                .get("axis_name")
                .or_else(|| a.get("axisName"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let (Some(id), Some(name)) = (id, name) {
                out.entry(id).or_insert(name);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_new_rules() {
        assert!(!is_new("a|b|1", "")); // 初回は基準値だけ
        assert!(!is_new("", "old")); // 指紋が取れない
        assert!(!is_new("same", "same"));
        assert!(is_new("a|b|2", "a|b|1")); // 変化 = 新しい
    }

    #[test]
    fn vidpid_hex() {
        assert_eq!(fmt_vidpid(0x1CA6), "1CA6");
        assert_eq!(fmt_vidpid(0x3009), "3009");
    }
}
