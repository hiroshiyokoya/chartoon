use serde::Deserialize;
use tauri::{AppHandle, Manager};
use tauri_plugin_shell::ShellExt;

#[derive(Deserialize)]
pub struct BulletTokenResult {
    pub bullet_token: String,
    pub country: String,
    pub language: String,
}

/// サイドカー（nxapi）呼び出しが失敗した「理由」（#399）。
///
/// 以前は失敗をすべて文字列に潰していたため、znca-api が 500 を返しただけでも
/// 「トークンが失効した・ログインし直せ」と案内していた。**何が起きたか**を型で持ち、
/// 上位（companion の `error_code` / フロントの通知文言）はこれを見て分岐する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// znca-api など外部サービスの一時障害（5xx / body が `error:"timeout"`）。
    /// **再ログインを促してはいけない**。トークンは生きている。
    UpstreamUnavailable,
    /// 認証情報の失効（401 / 403 / `invalid_grant`）。再ログインが必要。
    AuthExpired,
    /// ローカル側のネットワーク断・接続タイムアウト。
    Network,
    /// 認証の回数制限に達した（nxapi の 1 時間 4 回・種類ごと）。
    ///
    /// **待つ以外にできることが無い。** 再ログインを促しても、再試行しても意味がないので、
    /// 失敗として騒ぎ立てずに回復時刻を伝える（#616）。
    RateLimited,
    /// 任天堂が NSO アプリの版を上げ、認証側（znca-api）がまだ追いついていない（#765）。
    ///
    /// `[znc] Upgrade required` は HTTP 200 で来る。ログインし直しても、すぐ再試行しても
    /// 直らない。枠だけ減るので再試行を案内しない。
    CoralUpgrade,
    /// 上のいずれとも判定できないもの。憶測で認証エラー扱いしない。
    Unknown,
}

impl FailureKind {
    /// エラーメッセージ先頭に付ける機械可読プリフィクス。
    ///
    /// 既存の `NOT_LOGGED_IN:` / `FETCH_IN_PROGRESS:` と同じ流儀。`Unknown` は
    /// プリフィクスを付けない（付けると「分類できた」ように見えてしまう）。
    pub fn code(self) -> Option<&'static str> {
        match self {
            FailureKind::UpstreamUnavailable => Some("UPSTREAM_UNAVAILABLE"),
            FailureKind::AuthExpired => Some("AUTH_EXPIRED"),
            FailureKind::Network => Some("NETWORK"),
            FailureKind::RateLimited => Some("RATE_LIMITED"),
            FailureKind::CoralUpgrade => Some("CORAL_UPGRADE"),
            FailureKind::Unknown => None,
        }
    }
}

/// サイドカー呼び出しの失敗。理由（`kind`）と、ログ・表示用の詳細を持つ。
#[derive(Debug, Clone)]
pub struct NxapiError {
    pub kind: FailureKind,
    /// 「bullet token 取得失敗: ...」のような人間向け詳細（プリフィクス無し）。
    pub detail: String,
    pub status: Option<u16>,
    pub upstream_error: Option<String>,
}

impl NxapiError {
    fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self { kind, detail: detail.into(), status: None, upstream_error: None }
    }

    /// サイドカーが分類不能な形で落ちたとき（起動失敗・出力破損など）。
    fn opaque(detail: impl Into<String>) -> Self {
        Self::new(FailureKind::Unknown, detail)
    }
}

impl std::fmt::Display for NxapiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind.code() {
            Some(code) => write!(f, "{code}: {}", self.detail),
            None => f.write_str(&self.detail),
        }
    }
}

impl std::error::Error for NxapiError {}

/// 既存の `Result<_, String>` な呼び出し側がそのまま `?` で受けられるようにする。
/// 文字列化しても先頭のプリフィクスで理由が残るのが要点。
impl From<NxapiError> for String {
    fn from(e: NxapiError) -> Self {
        e.to_string()
    }
}

/// ローカル側のネットワーク不調を示す語（小文字比較）。
const NETWORK_HINTS: &[&str] = &[
    "fetch failed",
    "econnrefused",
    "econnreset",
    "etimedout",
    "enotfound",
    "eai_again",
    "socket hang up",
    "getaddrinfo",
    "timed out",
    "network",
    "dns",
];

/// 「HTTP ステータス」「body の error」「メッセージ」から失敗理由を決める（#399）。
///
/// 判定順が重要:
/// 1. **5xx / upstream timeout を先に見る**。ここを後回しにすると、外部サービスの一時障害が
///    「認証失効」に吸われて再ログインを促してしまう（本 Issue の症状そのもの）。
/// 2. 401 / 403 / `invalid_grant` のときだけ認証失効とする。
///    znca-api の `invalid_token` は**サイドカー自身のクライアント資格情報**の話であって
///    ユーザーの Nintendo アカウントとは関係ないので、ここには含めない。
/// 3. 残りをローカルのネットワーク断として拾い、それも違えば `Unknown`。
pub fn classify_failure(
    status: Option<u16>,
    upstream_error: Option<&str>,
    message: &str,
) -> FailureKind {
    let status = status.or_else(|| status_from_message(message));
    let upstream = upstream_error.map(|s| s.to_ascii_lowercase());
    let upstream = upstream.as_deref();
    let lower = message.to_ascii_lowercase();

    // 0. 認証の回数制限（#616）。
    // 待つ以外にできることが無いので、他のどれよりも先に判定する。
    // 5xx に混ざると「一時障害だから再試行」と読まれ、無駄に叩いて枠の回復を遅らせる。
    if lower.contains("too many attempts to authenticate") || message.contains("認証の回数制限") {
        return FailureKind::RateLimited;
    }

    // 0.5 任天堂が NSO の版を上げた（#765）。HTTP 200 で来るので 5xx より先でなくてよいが、
    // 401 に倒すと再ログインを促す。枠を減らす再試行も案内しない。
    if lower.contains("upgrade required") {
        return FailureKind::CoralUpgrade;
    }
    if matches!(status, Some(s) if (500..600).contains(&s)) {
        return FailureKind::UpstreamUnavailable;
    }
    if matches!(upstream, Some(u) if u.contains("timeout") || u.contains("unavailable") || u.contains("temporarily"))
    {
        return FailureKind::UpstreamUnavailable;
    }

    // 2. 認証失効
    if matches!(status, Some(401) | Some(403)) {
        return FailureKind::AuthExpired;
    }
    if matches!(upstream, Some(u) if u.contains("invalid_grant")) || lower.contains("invalid_grant")
    {
        return FailureKind::AuthExpired;
    }

    // 3. ローカルのネットワーク
    if NETWORK_HINTS.iter().any(|hint| lower.contains(hint)) {
        return FailureKind::Network;
    }

    FailureKind::Unknown
}

/// 構造が失われた（＝古いサイドカー等）場合の保険。`Non-200 status code: 500` を拾う。
fn status_from_message(message: &str) -> Option<u16> {
    const MARKER: &str = "Non-200 status code:";
    let idx = message.find(MARKER)?;
    let rest = message[idx + MARKER.len()..].trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u16>().ok()
}

/// サイドカーの失敗 JSON（`{ok:false, error, status, upstream_error}`）を `NxapiError` にする。
fn sidecar_failure(prefix: &str, result: &serde_json::Value) -> NxapiError {
    let err = result["error"].as_str().unwrap_or("不明なエラー");
    let status = result["status"].as_u64().and_then(|v| u16::try_from(v).ok());
    let upstream_error = result["upstream_error"].as_str().map(str::to_string);
    let kind = classify_failure(status, upstream_error.as_deref(), err);

    // ログで原因を追えるよう、構造が取れているときは併記する。
    let detail = match (status, upstream_error.as_deref()) {
        (None, None) => format!("{prefix}: {err}"),
        _ => format!(
            "{prefix}: {err} (status={} upstream_error={})",
            status.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string()),
            upstream_error.as_deref().unwrap_or("-")
        ),
    };

    NxapiError { kind, detail, status, upstream_error }
}

/// サイドカーの stderr をアプリのログへ流す（#611）。
///
/// 🔴 **stderr は今まで誰も読んでいなかった。** サイドカーは進捗や診断を stderr に
/// 書いているのに、`.output()` で捨てていたので**どこにも出ていなかった**。
/// 「bulletToken がキャッシュに当たったのか、認証をやり直したのか」も同じく消えていた。
///
/// 認証まわりの不具合はこの一行が見えるかどうかで追跡の手間が変わる。
fn log_sidecar_stderr(stderr: &[u8], prefix: &str) {
    let text = String::from_utf8_lossy(stderr);
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        log::info!("[{prefix}] {line}");
    }
}

/// サイドカーの stdout を JSON として読み、`ok:true` でなければ `NxapiError` にする。
fn parse_sidecar_output(stdout: &[u8], prefix: &str) -> Result<serde_json::Value, NxapiError> {
    let text = String::from_utf8_lossy(stdout);
    let result: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|e| NxapiError::opaque(format!("{prefix} 出力解析失敗: {e}\nstdout: {text}")))?;

    if result["ok"].as_bool() != Some(true) {
        return Err(sidecar_failure(prefix, &result));
    }
    Ok(result)
}

/// session_token を nxapi ストレージに保存する。
/// handle_auth_redirect 後に呼び出す。
pub async fn nxapi_setup(app: &AppHandle, session_token: &str) -> Result<(), NxapiError> {
    let data_dir = nxapi_data_dir(app)?;

    let output = app
        .shell()
        .sidecar("nxapi-sidecar")
        .map_err(|e| NxapiError::opaque(format!("サイドカー起動失敗: {e}")))?
        .args(["setup", session_token, &data_dir])
        .output()
        .await
        .map_err(|e| NxapiError::opaque(format!("サイドカー実行失敗: {e}")))?;

    log_sidecar_stderr(&output.stderr, "nxapi setup");
    parse_sidecar_output(&output.stdout, "nxapi setup").map(|_| ())
}

/// nxapi ストレージから bullet token を取得する。
///
/// 失敗時は `NxapiError` を返す。**「取得できなかった」という事実だけで認証失効と決めつけない**（#399）。
pub async fn nxapi_get_bullet_token(app: &AppHandle) -> Result<BulletTokenResult, NxapiError> {
    let data_dir = nxapi_data_dir(app)?;

    let output = app
        .shell()
        .sidecar("nxapi-sidecar")
        .map_err(|e| NxapiError::opaque(format!("サイドカー起動失敗: {e}")))?
        .args(["get_bullet_token", &data_dir])
        .output()
        .await
        .map_err(|e| NxapiError::opaque(format!("サイドカー実行失敗: {e}")))?;

    log_sidecar_stderr(&output.stderr, "bullet token");
    let result = parse_sidecar_output(&output.stdout, "bullet token 取得失敗")?;

    let field = |name: &str| -> Result<String, NxapiError> {
        result[name]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| NxapiError::opaque(format!("{name} フィールドが見つかりません")))
    };

    Ok(BulletTokenResult {
        bullet_token: field("bullet_token")?,
        country: field("country")?,
        language: field("language")?,
    })
}

/// nxapi-sidecar 経由で WeaponRecordQuery を実行し、`data` フィールドを返す (#49)。
///
/// 本番の取得は `splatnet3::fetch_and_store_weapon_records` が現行ハッシュで GraphQL 直叩きする（#674）。
/// サイドカー同梱の古いハッシュは Nintendo 側で無効（#162）。この関数は互換のため残す。
/// 戻り値はおおよそ `{ "weaponRecords": { "nodes": [...] } }` の serde_json::Value。
pub async fn nxapi_fetch_weapon_records(app: &AppHandle) -> Result<serde_json::Value, NxapiError> {
    let data_dir = nxapi_data_dir(app)?;

    let output = app
        .shell()
        .sidecar("nxapi-sidecar")
        .map_err(|e| NxapiError::opaque(format!("サイドカー起動失敗: {e}")))?
        .args(["weapon_records", &data_dir])
        .output()
        .await
        .map_err(|e| NxapiError::opaque(format!("サイドカー実行失敗: {e}")))?;

    log_sidecar_stderr(&output.stderr, "weapon records");
    let result = parse_sidecar_output(&output.stdout, "WeaponRecordQuery 失敗")?;
    Ok(result["data"].clone())
}

fn nxapi_data_dir(app: &AppHandle) -> Result<String, NxapiError> {
    app.path()
        .app_data_dir()
        .map(|p| p.join("nxapi-data").to_string_lossy().to_string())
        .map_err(|e| NxapiError::opaque(format!("データディレクトリ取得失敗: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(json: serde_json::Value) -> NxapiError {
        sidecar_failure("bullet token 取得失敗", &json)
    }

    // --- 認証の回数制限（#616）---

    /// 回数制限は**待つ以外にできることが無い**ので、他と区別できること。
    ///
    /// 一時障害（5xx）に混ざると「再試行すれば直る」と読まれ、無駄に叩いて
    /// 枠の回復を遅らせる。認証失効に混ざると再ログインを促してしまう。
    #[test]
    fn 回数制限は待つしかない失敗として分類される() {
        for msg in [
            "Too many attempts to authenticate (coral)",
            "Too many attempts to authenticate (splatnet3)",
            "認証の回数制限に達しています（coral・1 時間に 4 回まで）。01:51 以降に再試行できます。",
        ] {
            assert_eq!(
                classify_failure(None, None, msg),
                FailureKind::RateLimited,
                "{msg}"
            );
        }
        // 文字列化したときに前置きが付き、上位が見分けられること。
        let e = NxapiError::new(FailureKind::RateLimited, "bullet token 取得失敗: Too many attempts");
        assert!(e.to_string().starts_with("RATE_LIMITED:"), "{e}");
    }

    /// HTTP 200 の `[znc] Upgrade required` は再ログインでも再試行でも直らない（#765）。
    #[test]
    fn upgrade_required_is_coral_upgrade_not_auth() {
        let e = failure(serde_json::json!({
            "ok": false,
            "error": "[znc] Upgrade required.",
            "status": 200,
            "upstream_error": null,
        }));
        assert_eq!(e.kind, FailureKind::CoralUpgrade);
        assert!(e.to_string().starts_with("CORAL_UPGRADE: bullet token 取得失敗:"));
        assert_eq!(
            classify_failure(Some(200), None, "bullet token 取得失敗: [znc] Upgrade required."),
            FailureKind::CoralUpgrade,
        );
    }

    /// 500 と一緒に来ても回数制限を優先すること（判定順を守る）。
    #[test]
    fn 回数制限は一時障害より先に判定される() {
        assert_eq!(
            classify_failure(Some(500), Some("timeout"), "Too many attempts to authenticate (coral)"),
            FailureKind::RateLimited,
        );
    }

    // --- 本 Issue の再現ケース: znca-api の 500 + {"error":"timeout"} ---

    #[test]
    fn znca_500_timeout_is_upstream_not_auth() {
        let e = failure(serde_json::json!({
            "ok": false,
            "error": "[znca-api] Non-200 status code: 500",
            "status": 500,
            "upstream_error": "timeout",
        }));
        assert_eq!(e.kind, FailureKind::UpstreamUnavailable);
        assert!(e.to_string().starts_with("UPSTREAM_UNAVAILABLE: bullet token 取得失敗:"));
        assert!(e.to_string().contains("status=500"));
        assert!(e.to_string().contains("upstream_error=timeout"));
    }

    #[test]
    fn any_5xx_is_upstream() {
        for status in [500u16, 502, 503, 504] {
            assert_eq!(
                classify_failure(Some(status), None, "[znca-api] Non-200 status code"),
                FailureKind::UpstreamUnavailable,
                "status={status}"
            );
        }
    }

    #[test]
    fn upstream_timeout_without_status_is_upstream() {
        assert_eq!(
            classify_failure(None, Some("timeout"), "[znca-api] エラー"),
            FailureKind::UpstreamUnavailable
        );
    }

    // --- 認証失効 ---

    #[test]
    fn unauthorized_and_forbidden_are_auth_expired() {
        assert_eq!(classify_failure(Some(401), None, "unauthorized"), FailureKind::AuthExpired);
        assert_eq!(classify_failure(Some(403), None, "forbidden"), FailureKind::AuthExpired);
    }

    #[test]
    fn invalid_grant_is_auth_expired() {
        assert_eq!(
            classify_failure(Some(400), Some("invalid_grant"), "[znca-api] Non-200 status code: 400"),
            FailureKind::AuthExpired
        );
        // 構造が無く文字列にしか残っていない場合も拾う
        assert_eq!(classify_failure(None, None, "invalid_grant"), FailureKind::AuthExpired);
    }

    // --- ローカルのネットワーク ---

    #[test]
    fn local_network_errors_are_network() {
        for msg in ["fetch failed", "connect ECONNREFUSED 1.2.3.4:443", "getaddrinfo EAI_AGAIN api.example"] {
            assert_eq!(classify_failure(None, None, msg), FailureKind::Network, "msg={msg}");
        }
    }

    // --- 決めつけない ---

    #[test]
    fn unknown_stays_unknown_and_has_no_prefix() {
        let e = failure(serde_json::json!({ "ok": false, "error": "未ログインです" }));
        assert_eq!(e.kind, FailureKind::Unknown);
        assert_eq!(e.to_string(), "bullet token 取得失敗: 未ログインです");
    }

    #[test]
    fn status_falls_back_to_message_when_structure_is_missing() {
        assert_eq!(status_from_message("[znca-api] Non-200 status code: 503 body=..."), Some(503));
        assert_eq!(status_from_message("ふつうのエラー"), None);
        // 古いサイドカーが status を返さなくても 5xx を読み取れる
        assert_eq!(
            classify_failure(None, None, "[znca-api] Non-200 status code: 500 body={\"error\":\"timeout\"}"),
            FailureKind::UpstreamUnavailable
        );
    }
}
