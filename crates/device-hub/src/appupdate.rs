//! アプリ本体の更新確認 (GitHub)。通信は devhub-update、ここは UI への配線だけ。
//!
//! - 自動: 起動直後と以後 24 時間ごと (常駐中も 2 秒ティックで期限を見る)。新しい版が
//!   見つかったらトースト + Windows 通知 (同じ版では 1 回だけ)。
//! - 手動: アプリ設定の「更新を確認」。結果を必ずトーストで返す。
//!
//! ダウンロード・自己更新はしない — 「ダウンロードページを開く」でブラウザに渡すだけ。

use crate::app::Shell;
use gpui_kit::*;

/// 確認先のリポジトリ (公開) と既定ブランチ。
pub const REPO: &str = "narurea/device-hub";
pub const BRANCH: &str = "main";
/// 自動確認の間隔。
const EVERY_SECS: u64 = 24 * 60 * 60;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Shell {
    /// 更新を確認する。`manual` = ボタンから (間引きを飛ばし、結果を必ず知らせる)。
    pub fn check_app_update(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.app_update_busy {
            return;
        }
        let t = now();
        if !manual {
            if !self.prefs.app_update_check
                || t.saturating_sub(self.prefs.app_update_last) < EVERY_SECS
            {
                return;
            }
            self.prefs.app_update_last = t;
            self.prefs.save();
        }
        self.app_update_busy = true;
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let r =
                devhub_update::check_app(REPO, BRANCH, env!("CARGO_PKG_VERSION")).map_err(|e| e.0);
            let _ = tx.send(r);
        });
        cx.spawn(async move |this: WeakEntity<Shell>, cx: &mut AsyncApp| {
            let res = rx.await.unwrap_or_else(|_| Err("確認が中断されました".into()));
            let _ = this.update(cx, |this, cx| {
                this.app_update_busy = false;
                match res {
                    Ok(u) => {
                        if u.newer {
                            this.toast(
                                format!("Device HUB の新しいバージョン v{} が公開されています", u.latest),
                                false,
                                cx,
                            );
                            if !manual && this.prefs.app_update_notified != u.latest {
                                this.prefs.app_update_notified = u.latest.clone();
                                this.prefs.save();
                                let _ = notify_rust::Notification::new()
                                    .summary("Device HUB")
                                    .body(&format!(
                                        "新しいバージョン v{} があります。アプリ設定 → アプリの更新から開けます",
                                        u.latest
                                    ))
                                    .show();
                            }
                        } else if manual {
                            this.toast(
                                format!("最新版です (v{})", env!("CARGO_PKG_VERSION")),
                                false,
                                cx,
                            );
                        }
                        this.app_update = Some(u);
                    }
                    Err(e) => {
                        if manual {
                            this.toast(format!("更新を確認できませんでした: {e}"), true, cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
