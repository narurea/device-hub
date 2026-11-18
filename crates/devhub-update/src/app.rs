//! Device HUB 本体の更新確認 (読み取り専用 — ダウンロードや自己更新はしない)。
//!
//! 確認先は GitHub の公開リポジトリ:
//!
//! 1. `GET api.github.com/repos/<repo>/releases/latest` — Release を出していればそのタグ
//!    (`v0.2.7` など) と Release ページ・リリースノート。
//! 2. Release が 1 つも無い (404) ときは、既定ブランチの `crates/device-hub/Cargo.toml` の
//!    `version` を読む。開発中はバージョンを上げて push するだけで「更新あり」になる。
//!
//! 送るのは汎用の User-Agent だけ (個人情報・端末情報は送らない)。失敗は `UpdateError`。

use crate::UpdateError;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(6);
const UA: &str = "DeviceHub-UpdateCheck";

/// 確認結果。
#[derive(Clone, Debug, PartialEq)]
pub struct AppUpdate {
    /// 公開されている最新版 (先頭の `v` は外した表記)。
    pub latest: String,
    /// `latest` が実行中の版より新しい。
    pub newer: bool,
    /// 開くページ (Release ページ、無ければリポジトリ)。
    pub url: String,
    /// リリースノート (Release があるときだけ。長ければ切り詰め済み)。
    pub notes: Option<String>,
    /// Release から取れた (false = ブランチの Cargo.toml から)。
    pub from_release: bool,
}

/// `repo` = `owner/name`、`branch` = 既定ブランチ、`current` = 実行中の版。
pub fn check_app(repo: &str, branch: &str, current: &str) -> Result<AppUpdate, UpdateError> {
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let api = format!("https://api.github.com/repos/{repo}/releases/latest");
    match agent
        .get(&api)
        .set("User-Agent", UA)
        .set("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(r) => {
            let v: serde_json::Value = r
                .into_json()
                .map_err(|e| UpdateError(format!("release の解析に失敗: {e}")))?;
            from_release_json(&v, repo, current)
        }
        // Release がまだ無い → ブランチのバージョンを見る。
        Err(ureq::Error::Status(404, _)) => {
            let raw = format!(
                "https://raw.githubusercontent.com/{repo}/{branch}/crates/device-hub/Cargo.toml"
            );
            let text = agent
                .get(&raw)
                .set("User-Agent", UA)
                .call()
                .map_err(|e| UpdateError(format!("バージョン情報を取得できません: {e}")))?
                .into_string()
                .map_err(|e| UpdateError(e.to_string()))?;
            let latest = package_version(&text)
                .ok_or_else(|| UpdateError("Cargo.toml に version がありません".into()))?;
            Ok(AppUpdate {
                newer: crate::version_newer(&latest, current),
                latest,
                url: format!("https://github.com/{repo}/commits/{branch}"),
                notes: None,
                from_release: false,
            })
        }
        Err(ureq::Error::Status(code, _)) => {
            Err(UpdateError(format!("GitHub が {code} を返しました")))
        }
        Err(e) => Err(UpdateError(format!("接続できません: {e}"))),
    }
}

fn from_release_json(
    v: &serde_json::Value,
    repo: &str,
    current: &str,
) -> Result<AppUpdate, UpdateError> {
    let tag = v["tag_name"]
        .as_str()
        .ok_or_else(|| UpdateError("release に tag_name がありません".into()))?;
    let latest = tag.trim_start_matches(['v', 'V']).to_string();
    let url = v["html_url"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://github.com/{repo}/releases"));
    let notes = v["body"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut n: String = s.chars().take(600).collect();
            if s.chars().count() > 600 {
                n.push('…');
            }
            n
        });
    Ok(AppUpdate {
        newer: crate::version_newer(&latest, current),
        latest,
        url,
        notes,
        from_release: true,
    })
}

/// `[package]` 節の `version = "x.y.z"` を取り出す (workspace 継承は対象外)。
fn package_version(toml: &str) -> Option<String> {
    let mut in_pkg = false;
    for line in toml.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_pkg = l == "[package]";
            continue;
        }
        if in_pkg {
            if let Some(rest) = l.strip_prefix("version") {
                let rest = rest.trim_start();
                if let Some(v) = rest.strip_prefix('=') {
                    let v = v.trim().trim_matches('"');
                    if !v.is_empty() && v.contains('.') {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_package_version_only() {
        let t = "[package]\nname = \"device-hub\"\nversion = \"0.2.7\"\n\n[dependencies]\nversion = \"9\"\n";
        assert_eq!(package_version(t).as_deref(), Some("0.2.7"));
        assert_eq!(package_version("[dependencies]\nversion = \"1.0\"\n"), None);
    }

    #[test]
    fn release_json() {
        let v = serde_json::json!({
            "tag_name": "v0.3.0",
            "html_url": "https://github.com/o/r/releases/tag/v0.3.0",
            "body": "  Logitech 対応  "
        });
        let u = from_release_json(&v, "o/r", "0.2.6").unwrap();
        assert_eq!(u.latest, "0.3.0");
        assert!(u.newer && u.from_release);
        assert_eq!(u.notes.as_deref(), Some("Logitech 対応"));
        let same = from_release_json(&v, "o/r", "0.3.0").unwrap();
        assert!(!same.newer);
    }

    /// 実際の GitHub に問い合わせる (ネットワークが要るので既定では走らせない)。
    /// `cargo test -p devhub-update -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_check() {
        let u = check_app("narurea/device-hub", "main", "0.0.1").unwrap();
        println!("{u:?}");
        assert!(u.newer);
    }
}
