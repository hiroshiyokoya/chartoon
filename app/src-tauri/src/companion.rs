//! コンパニオン同期サーバー（#324・設計書 splabo-viewer docs/design.md §4①・§5）。
//!
//! 同一 LAN 上のペア済みモバイルクライアント（splabo-viewer）へエクスポートファイルを
//! 配信するオプトイン式 HTTP サーバー。デフォルトは停止。設定トグルで起動する。
//!
//! 担うのは「起動 / 停止・ペアリング情報発行・ファイル配信・mDNS 広告」（#324）に加えて、
//! **②更新命令（任天堂から取り直せ）の受付（#326・設計書 §4②）** と
//! **ブキ / ステージアイコンの同期供給・差分配信（#327・設計書 §6）**。
//!
//! ## 構成
//! - HTTP サーバー: `tiny_http`（同期・軽量）を専用スレッドで駆動。tokio ランタイムと独立。
//!   - `GET /ping`         … トークン検証のみの疎通確認。
//!   - `GET /gear_db.bin`  … `app_data_dir()/data/gear_db.bin` を配信（不在は 404）。
//!                           ETag（内容 sha256）対応。`If-None-Match` 一致なら 304（#356）。
//!   - `GET /battle_db.bin`… `app_data_dir()/data/battle_db.bin` を配信（不在は 404）。
//!                           ETag（内容 sha256）対応。`If-None-Match` 一致なら 304（#356）。
//!   - `GET /images/...`   … 画像（.gti）を相対パスで配信。`..` 等は 403。**2 系統を振り分ける**:
//!                           - `images/{weapon,sub_weapon,special_weapon,stage,ability}/...`
//!                             → `app_data_dir()/images/`（バトルアイコン・`images.rs` が書く／#327）
//!                           - それ以外（`images/{gear,brand,skill}/...`）
//!                             → `app_data_dir()/data/images/`（ギア画像・`gear.rs` が書く／#324）
//!                           gear_db / battle_db の画像パスをそのまま GET できる。
//!                           ETag（内容 sha256）対応。`If-None-Match` 一致なら 304（再転送しない）。
//!   - `GET /icons/manifest` … バトルアイコンの差分同期マニフェスト（#327・設計書 §6）。
//!                           viewer は hash を突き合わせ、変わったものだけ `/images/...` を引く。
//!   - `POST /update`      … ②更新命令の受付。**受付のみで即応答**（202）し、実処理は裏で走る。
//!                           ボディ `{"target":"battle"|"gear"|"both"}` で対象を選べる（#395）。
//!   - `GET /update_status`… ②更新命令の進捗・完了・失敗を問い合わせる（ポーリング用）。
//!   - いずれも `Authorization: Bearer <token>` 検証必須。不一致・欠落は 401。
//!
//! ## ②更新命令（設計書 §4②）
//! - 「重い・数十秒」処理なので HTTP を掴んだまま待たせない。`POST /update` は受付可否だけを
//!   即返し、viewer は `GET /update_status` をポーリングして完了を待ち、完了後に①プルする。
//! - **対象を選べる（#395）**。viewer は「前面表示中の定期プルはバトルのみ / ギアは明示操作のみ」と
//!   同期の粒度を分けるため、`{"target":"battle"|"gear"|"both"}` を送る。任天堂 API への再取得は
//!   数十秒かかる重い処理なので、要らない側を取りに行かせないことに実利がある。
//!   **省略・不正値は `both`**（＝この対応以前の動作）に倒すので、古い viewer もそのまま動く。
//! - **認証済みトークン前提**。未ログインなら任天堂 API を一切叩かず `NOT_LOGGED_IN` を返す。
//! - **多重起動しない**。実行中の更新ジョブ、およびデスクトップ側のバトル取得
//!   （`FetchInProgress`・手動/起動時/スケジューラー共通）と重なる場合は新規に走らせず、
//!   進行中である旨を返す。
//! - モバイルは任天堂 API に触れない。再フェッチはこのデスクトップ側だけで完結する。
//! - mDNS 広告: `mdns-sd` でサービス型 `_splabo._tcp.local.` を LAN に告知（viewer の NSD 発見用）。
//!   - **ベストエフォート**。失敗しても HTTP サーバーは動かし続ける（TXT にトークンは載せない）。
//!
//! ## セキュリティ
//! - トークンは起動ごとにランダム（16 バイト = 32 hex）。未ペア端末（トークン不一致）は 401。
//! - LAN 想定。バインドは `0.0.0.0:<空きポート>`（同一 LAN の viewer から到達可能にするため）。

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use mdns_sd::{ServiceDaemon, ServiceInfo};

const SERVICE_TYPE: &str = "_splabo._tcp.local.";
const SERVICE_INSTANCE: &str = "splabo";
const SERVICE_HOST: &str = "splabo.local.";

/// 稼働中のサーバー一式（停止時に一括で畳む）。
struct RunningServer {
    port: u16,
    token: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    mdns: Option<ServiceDaemon>,
}

/// リクエスト処理に必要な一式（HTTP スレッドが保持する）。
struct ServerCtx {
    token: String,
    /// ギア画像（`gear.rs` が書く）・`gear_db.bin` / `battle_db.bin` の置き場（`app_data_dir()/data`）。
    data_dir: PathBuf,
    /// バトルアイコン（`images.rs` が書く）のキャッシュルート（`app_data_dir()/images`・#327）。
    /// `data_dir` とは別系統。`/images/...` の kind で振り分ける（`resolve_image_path`）。
    images_root: PathBuf,
    app: AppHandle,
    /// ②更新命令のジョブ状態（サーバー稼働中のみ保持。停止で捨てる）。
    job: Arc<Mutex<UpdateStatus>>,
}

/// コンパニオンサーバーの状態。デフォルト停止（オプトイン）。
#[derive(Default)]
pub struct CompanionState(Mutex<Option<RunningServer>>);

/// `companion_start` の戻り値（viewer のペアリング QR に載せる）。
#[derive(Debug, Serialize)]
pub struct CompanionInfo {
    /// LAN 側ホスト IP（IPv4・非ループバック）。viewer は到達可能なものを選ぶ。
    pub host_ips: Vec<String>,
    pub port: u16,
    pub token: String,
}

/// `companion_status` の戻り値。
#[derive(Debug, Serialize)]
pub struct CompanionStatus {
    pub running: bool,
    pub port: Option<u16>,
}

// ---------------------------------------------------------------------------
// ②更新命令（#326・設計書 §4②）
// ---------------------------------------------------------------------------

/// 更新ジョブの状態。viewer は `state` で分岐する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    /// 一度も命令を受けていない（サーバー起動直後）。
    Idle,
    /// 実行中（`step` に現在フェーズ）。
    Running,
    /// 正常終了（`result` に件数）。
    Done,
    /// 失敗（`error_code` / `error_message`）。
    Failed,
}

/// 更新ジョブのフェーズ。viewer の進捗表示用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStep {
    /// SplatNet3 からバトル再フェッチ中。
    Battles,
    /// SplatNet3 からギア再フェッチ中（gear_db.bin / .gti 再生成を含む）。
    Gear,
    /// battle_db.bin エクスポート再生成中。
    Export,
}

/// 更新ジョブの対象（#395）。`POST /update` のボディ `{"target": ...}` で指定する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateTarget {
    /// バトルのみ（＋ battle_db.bin の再生成）。viewer の前面表示中の定期プル用。
    Battle,
    /// ギアのみ（gear_db.bin / .gti 再生成）。viewer の設定タブ「ギア更新」用。
    Gear,
    /// 両方。**省略時・不正値のフォールバック**（＝#395 以前の動作）。
    Both,
}

impl UpdateTarget {
    fn includes_battle(self) -> bool {
        matches!(self, Self::Battle | Self::Both)
    }

    fn includes_gear(self) -> bool {
        matches!(self, Self::Gear | Self::Both)
    }

    /// ログ用の短い表記。
    fn label(self) -> &'static str {
        match self {
            Self::Battle => "battle",
            Self::Gear => "gear",
            Self::Both => "both",
        }
    }
}

/// 更新ジョブの結果件数。
///
/// **各件数は `Option`**（#395）。対象から外れて実行しなかったフェーズは `None`（JSON では null）に
/// なる。`0` で埋めると「取りに行ったが 0 件だった」と区別できず、viewer が「バトルは更新された（0 件）」
/// と誤って解釈してしまうため、実行しなかったことを型で表す。
/// `target` 省略（= `both`）なら全フィールドが `Some` になるので、古い viewer から見た形は変わらない。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct UpdateResult {
    /// 新規に取り込んだバトル数。
    pub battles: Option<usize>,
    /// 詳細を補完したバトル数。
    pub details: Option<usize>,
    /// gear_db.bin に載ったギア数（頭 + 服 + 靴）。
    pub gear: Option<usize>,
    /// battle_db.bin に載ったバトル行数。
    pub exported_battles: Option<usize>,
}

/// `POST /update` / `GET /update_status` が返す状態。
#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub state: UpdateState,
    /// `state == running` のときの現在フェーズ。それ以外は null。
    pub step: Option<UpdateStep>,
    /// 受付時刻（UNIX 秒）。
    pub started_at: Option<i64>,
    /// 完了・失敗時刻（UNIX 秒）。
    pub finished_at: Option<i64>,
    /// `state == failed` のときのみ。viewer はこれで UX を分岐する。
    pub error_code: Option<&'static str>,
    /// 人間可読の失敗理由（そのまま表示せずログ向け）。
    pub error_message: Option<String>,
    /// `state == done` のときのみ。
    pub result: Option<UpdateResult>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            state: UpdateState::Idle,
            step: None,
            started_at: None,
            finished_at: None,
            error_code: None,
            error_message: None,
            result: None,
        }
    }
}

impl UpdateStatus {
    fn running(started_at: i64, target: UpdateTarget) -> Self {
        Self {
            state: UpdateState::Running,
            // 最初のフェーズは対象で決まる。ギア単独のときに `battles` を出すと、viewer の進捗表示が
            // 「取りに行かないバトル」を待っているように見えてしまう（#395）。
            step: Some(if target.includes_battle() {
                UpdateStep::Battles
            } else {
                UpdateStep::Gear
            }),
            started_at: Some(started_at),
            ..Self::default()
        }
    }

    fn failed(code: &'static str, message: impl Into<String>, started_at: Option<i64>) -> Self {
        Self {
            state: UpdateState::Failed,
            step: None,
            started_at,
            finished_at: Some(now_unix()),
            error_code: Some(code),
            error_message: Some(message.into()),
            result: None,
        }
    }
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// エラーメッセージを viewer が分岐できる error_code に写像する。
///
/// 既存コードのエラーは `NOT_LOGGED_IN:` / `FETCH_IN_PROGRESS:` プリフィクス方針
/// （lib.rs `run_fetch_full` / gear.rs `fetch_gear_full`）に従うため、それを尊重する。
/// サイドカー由来の失敗は `nxapi::FailureKind` が付ける `UPSTREAM_UNAVAILABLE:` /
/// `AUTH_EXPIRED:` / `NETWORK:` プリフィクスを見る（#399）。
///
/// **「bullet token が取れなかった」という事実だけで TOKEN_EXPIRED にしない。**
/// znca-api が 500 を返しただけでも再ログインを促してしまい、ユーザーは直らない操作を
/// 繰り返すことになる（#399 の症状）。理由が分からないものは `FETCH_FAILED` に倒す。
///
/// 対応する error_code:
/// - `UPSTREAM_UNAVAILABLE` … **新設**。外部サービスの一時障害。viewer 側は splabo-viewer#112 で対応。
///   未知コードを受け取る古い viewer は「その他の失敗」として扱えばよい。
/// - `AUTH_EXPIRED` → 既存の `TOKEN_EXPIRED` を維持（viewer の既存分岐をそのまま使う）。
/// - `NETWORK` → 既存の `FETCH_FAILED` に寄せる。viewer 側から見れば「デスクトップが取得に失敗した・
///   再試行して」で挙動は同じなので、viewer に新コードを増やす価値が無い。
/// - `CORAL_UPGRADE` … 任天堂が NSO の版を上げ、認証側がまだ追いついていない（#765）。
///   再ログインも再試行も直らない。未知コードを受け取る古い viewer は「その他の失敗」扱い。
fn classify_error(message: &str) -> &'static str {
    if message.starts_with("NOT_LOGGED_IN") {
        "NOT_LOGGED_IN"
    } else if message.starts_with("FETCH_IN_PROGRESS") {
        "FETCH_IN_PROGRESS"
    } else if message.starts_with("UPSTREAM_UNAVAILABLE") {
        "UPSTREAM_UNAVAILABLE"
    } else if message.starts_with("AUTH_EXPIRED") {
        "TOKEN_EXPIRED"
    } else if message.starts_with("CORAL_UPGRADE") {
        "CORAL_UPGRADE"
    } else if message.starts_with("NETWORK") {
        "FETCH_FAILED"
    } else if message.contains("invalid_grant") {
        // プリフィクスの付かない経路（Rust 側の認証コード等）から来た失効。
        "TOKEN_EXPIRED"
    } else {
        "FETCH_FAILED"
    }
}

/// デスクトップ側のバトル取得（手動 / 起動時 / スケジューラー）が進行中か。
///
/// 既存の `FetchInProgress` をそのまま参照する（新しい並走フラグを作らない）。
fn desktop_fetch_in_progress(app: &AppHandle) -> bool {
    app.try_state::<crate::FetchInProgress>()
        .map(|f| f.0.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// 現在の状態から新規ジョブを開始してよいかを判定し、可なら `running` に遷移させる。
///
/// 実行中（`Running`）なら遷移せず false。`Idle` / `Done` / `Failed` からは再実行できる。
/// HTTP/Tauri に依存しない純粋関数として切り出し、多重起動防止をユニットテストする。
///
/// **対象が違っても実行中なら弾く**（#395）。バトルとギアで並走させると任天堂 API を同時に叩き、
/// `FetchInProgress` とも噛み合わなくなる。粒度を分ける狙いは「要らない側を取りに行かせない」ことで
/// あって「同時に走らせる」ことではない。
fn try_begin(status: &mut UpdateStatus, started_at: i64, target: UpdateTarget) -> bool {
    if status.state == UpdateState::Running {
        return false;
    }
    *status = UpdateStatus::running(started_at, target);
    true
}

/// `POST /update` のリクエストボディ（#395）。未知フィールドは無視する。
#[derive(Debug, Deserialize)]
struct UpdateCommandBody {
    #[serde(default)]
    target: Option<String>,
}

/// リクエストボディから更新対象を読み取る（#395）。
///
/// **ボディが空・JSON でない・`target` が無い・知らない値、いずれも `Both` に倒す。**
/// 400 を返さないのは意図的で、
/// - 古い viewer は `target` を送らない（＝現行どおり両方取る、が正しい挙動）
/// - 将来 viewer が増やした値をこちらが知らない場合も、「遅いが正しい」両方取得に落ちるほうが安全
/// だからである（イシュー #395 の後方互換方針）。
fn parse_update_target(body: &str) -> UpdateTarget {
    let target = serde_json::from_str::<UpdateCommandBody>(body)
        .ok()
        .and_then(|b| b.target);
    match target.as_deref() {
        Some("battle") => UpdateTarget::Battle,
        Some("gear") => UpdateTarget::Gear,
        Some("both") | None => UpdateTarget::Both,
        Some(other) => {
            log::warn!("[companion] 未知の更新対象 \"{other}\" → both として扱う");
            UpdateTarget::Both
        }
    }
}

/// 更新ジョブ本体: SplatNet3 再フェッチ → gear / battle エクスポート再生成。
///
/// モバイルからは触れないデスクトップ側の既存経路をそのまま呼ぶ:
/// - バトル: `crate::run_fetch_full`（`FetchInProgress` による多重起動防止つき）
/// - ギア  : `crate::gear::fetch_gear_full`（gear_db.bin / .gti を再生成）
/// - エクスポート: `crate::battle_export::export_battle_db`（battle_db.bin を再生成）
///
/// `target` で対象を絞る（#395）。**battle_db.bin のエクスポートはバトルを取り直したときだけ走らせる**。
/// ギア単独ならバトル DB の中身は変わらず、再生成しても同じファイルを作り直すだけで、viewer 側も
/// ETag 一致で 304 になるため意味が無い（`gear_db.bin` の再生成は `fetch_gear_full` の中で行われる）。
async fn run_update_job(
    app: &AppHandle,
    job: &Arc<Mutex<UpdateStatus>>,
    target: UpdateTarget,
) -> Result<UpdateResult, String> {
    let set_step = |step: UpdateStep| {
        if let Ok(mut s) = job.lock() {
            s.step = Some(step);
        }
    };

    // 実行しなかったフェーズは None のまま残す（Default = 全 None）。
    let mut result = UpdateResult::default();

    if target.includes_battle() {
        // --- バトル再フェッチ ---
        set_step(UpdateStep::Battles);
        let (battles, details, _uploaded) = {
            let pool = app.try_state::<crate::db::DbPool>().ok_or_else(|| {
                "DB がまだ初期化されていません。少し待って再試行してください。".to_string()
            })?;
            crate::run_fetch_full(app, &pool).await?
        };
        result.battles = Some(battles);
        result.details = Some(details);
    }

    if target.includes_gear() {
        // --- ギア再フェッチ（gear_db.bin / .gti 再生成まで含む） ---
        set_step(UpdateStep::Gear);
        let gear = crate::gear::fetch_gear_full(app.clone()).await?;
        result.gear = Some(gear.head + gear.clothing + gear.shoes);
    }

    if target.includes_battle() {
        // --- battle_db.bin エクスポート再生成 ---
        set_step(UpdateStep::Export);
        let export = {
            let pool = app
                .try_state::<crate::db::DbPool>()
                .ok_or_else(|| "DB がまだ初期化されていません。".to_string())?;
            crate::battle_export::export_battle_db(app.clone(), pool, None).await?
        };
        result.exported_battles = Some(export.battles);
    }

    Ok(result)
}

/// ログ表示用に件数を整形する。実行しなかったフェーズ（`None`）は `-`。
fn fmt_count(count: Option<usize>) -> String {
    count.map_or_else(|| "-".to_string(), |n| n.to_string())
}

/// `POST /update` を捌く。受付判定のみ同期で行い、実処理は裏で走らせて即応答する。
///
/// - 202: 受付（`state = running`）
/// - 409: 実行中 / 未ログイン / デスクトップ側取得と衝突（本文の `error_code` で分岐）
///
/// ボディで対象を指定できる（#395）。`{"target":"battle"|"gear"|"both"}`・省略時は `both`。
fn handle_update_command(mut request: tiny_http::Request, ctx: &ServerCtx) {
    // ボディは受付判定より先に読み切る（読まずに respond すると次のリクエストとずれる）。
    // 読めなかった場合もエラーにせず both に倒す（`parse_update_target` の方針と揃える）。
    let mut body = String::new();
    if let Err(e) = request.as_reader().read_to_string(&mut body) {
        log::warn!("[companion] ②更新命令のボディ読み取り失敗（both として扱う）: {e}");
        body.clear();
    }
    let target = parse_update_target(&body);

    // 未ログインなら任天堂 API を一切叩かずに返す（viewer は「デスクトップでログインして」を表示）。
    if !crate::auth::is_logged_in(&ctx.app) {
        let loc = crate::locale::app_locale(&ctx.app);
        let status = UpdateStatus::failed(
            "NOT_LOGGED_IN",
            loc.companion_not_logged_in(),
            None,
        );
        if let Ok(mut s) = ctx.job.lock() {
            *s = status.clone();
        }
        respond_json(request, 409, &status);
        return;
    }

    // デスクトップ側で取得が走っているなら並走させない（既存 FetchInProgress を尊重）。
    if desktop_fetch_in_progress(&ctx.app) {
        let loc = crate::locale::app_locale(&ctx.app);
        let status = UpdateStatus::failed(
            "FETCH_IN_PROGRESS",
            loc.companion_fetch_in_progress(),
            None,
        );
        respond_json(request, 409, &status);
        return;
    }

    let started_at = now_unix();
    let accepted = {
        let mut guard = match ctx.job.lock() {
            Ok(g) => g,
            Err(e) => {
                let status = UpdateStatus::failed("FETCH_FAILED", format!("状態ロック失敗: {e}"), None);
                respond_json(request, 500, &status);
                return;
            }
        };
        let ok = try_begin(&mut guard, started_at, target);
        let snapshot = guard.clone();
        (ok, snapshot)
    };

    match accepted {
        (false, snapshot) => {
            // 既に更新ジョブ実行中: 新たに走らせず「進行中」を返す。
            respond_json(request, 409, &snapshot);
        }
        (true, snapshot) => {
            respond_json(request, 202, &snapshot);
            log::info!("[companion] ②更新命令を受付。再フェッチ開始 target={}", target.label());
            let app = ctx.app.clone();
            let job = Arc::clone(&ctx.job);
            tauri::async_runtime::spawn(async move {
                let outcome = run_update_job(&app, &job, target).await;
                if let Ok(mut s) = job.lock() {
                    match outcome {
                        Ok(result) => {
                            log::info!(
                                "[companion] ②更新命令 完了 target={} バトル+{} 詳細+{} ギア{} エクスポート{}行",
                                target.label(),
                                fmt_count(result.battles),
                                fmt_count(result.details),
                                fmt_count(result.gear),
                                fmt_count(result.exported_battles)
                            );
                            s.state = UpdateState::Done;
                            s.step = None;
                            s.finished_at = Some(now_unix());
                            s.result = Some(result);
                        }
                        Err(e) => {
                            let code = classify_error(&e);
                            log::error!(
                                "[companion] ②更新命令 失敗 target={} [{code}]: {e}",
                                target.label()
                            );
                            let started_at = s.started_at;
                            *s = UpdateStatus::failed(code, e, started_at);
                        }
                    }
                }
            });
        }
    }
}

/// ランダムな共有トークン（16 バイト → 32 hex 文字）を生成する。
fn generate_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| std::char::from_digit(rng.gen_range(0..16), 16).unwrap())
        .collect()
}

/// 仮想 NIC（WSL / Hyper-V / Docker / VirtualBox / VMware 等）らしいインターフェース名か。
///
/// **除外の判定には使わない。並べ替え（優先度を下げる）だけに使う。**
/// 名前は OS 依存で当てにならない（ユーザーがアダプタ名を変更できる／ローカライズされる）ため、
/// これで弾くと本命を消してしまう恐れがある。誤検出しても「順番が後ろになるだけ」で済む用途に限る。
fn looks_virtual_iface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "vethernet", // Windows: vEthernet (WSL (Hyper-V firewall)) / vEthernet (Default Switch)
        "wsl",
        "hyper-v",
        "docker",
        "virtualbox",
        "vmware",
        "vmnet",
        "vboxnet",
        "tailscale",
        "zerotier",
        "utun",
        "tun",
        "tap",
    ]
    .iter()
    .any(|kw| n.contains(kw))
}

/// ペアリング QR に載せる IPv4 候補を絞り込み・並べ替える純ロジック（#369）。
///
/// 入力は `(インターフェース名, IPv4)` の列（`if_addrs` が返した順）。
///
/// ## 除外するもの（APIPA のみ）
/// - **リンクローカル 169.254.0.0/16（APIPA）**。DHCP に失敗した／未接続のアダプタが付ける
///   アドレスで、原理的に LAN の別端末からは到達できない。載せる意味が無い。
///
/// **除外はこれだけ**。「仮想 NIC らしい」「レンジが怪しい」は一切除外しない。誤除外は
/// 「繋がらない」に直結するのに対し、余分な候補は viewer 側の `/ping` で落ちるだけだからである。
///
/// ## 並べ替え（安定ソート・同点は `if_addrs` の順を保つ）
/// 0. 家庭用 LAN で最も一般的な 192.168.0.0/16 / 10.0.0.0/8 で、仮想 NIC 名でないもの
/// 1. 上記以外（グローバル IP 等）で仮想 NIC 名でないもの
/// 2. 172.16.0.0/12 で仮想 NIC 名でないもの
/// 3. 仮想 NIC らしい名前のインターフェース（レンジを問わず最後）
///
/// ### 172.16.0.0/12 を「除外」しない理由
/// WSL / Hyper-V の仮想スイッチはこのレンジを使うが、**172.16-31.x.x は正当なプライベート
/// アドレスでもあり、実際にこのレンジを使う LAN が存在する**。レンジだけで弾くとその環境では
/// 本命が消えて一切ペアリングできなくなる。よってここでは**順位を下げるだけ**にとどめ、
/// 候補としては必ず残す。仮想スイッチであれば実際にはインターフェース名でも後ろへ回るため、
/// 名前判定が効く環境では二重に沈む。
pub(crate) fn filter_host_ipv4s(candidates: Vec<(String, Ipv4Addr)>) -> Vec<IpAddr> {
    let mut kept: Vec<(u8, Ipv4Addr)> = candidates
        .into_iter()
        // 除外は APIPA だけ。
        .filter(|(_, ip)| !ip.is_link_local())
        .map(|(name, ip)| {
            let rank = if looks_virtual_iface(&name) {
                3
            } else if is_common_lan(&ip) {
                0
            } else if is_shared_private_172(&ip) {
                2
            } else {
                1
            };
            (rank, ip)
        })
        .collect();
    // sort_by_key は安定ソートなので、同ランク内の並びは if_addrs の列挙順のまま。
    kept.sort_by_key(|(rank, _)| *rank);
    kept.into_iter().map(|(_, ip)| IpAddr::V4(ip)).collect()
}

/// 家庭用 LAN で最も一般的なレンジ（192.168.0.0/16 / 10.0.0.0/8）か。
fn is_common_lan(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    (o[0] == 192 && o[1] == 168) || o[0] == 10
}

/// 172.16.0.0/12（正当なプライベートでもあり、仮想スイッチも好んで使うレンジ）か。
fn is_shared_private_172(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 172 && (16..=31).contains(&o[1])
}

/// LAN 側の IPv4・非ループバックアドレスを列挙する（APIPA 除外・到達しやすい順／#369）。
fn host_ipv4s() -> Vec<IpAddr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => {
            let candidates = ifaces
                .into_iter()
                .filter(|i| !i.is_loopback())
                .filter_map(|i| match i.ip() {
                    IpAddr::V4(v4) => Some((i.name, v4)),
                    IpAddr::V6(_) => None,
                })
                .collect();
            filter_host_ipv4s(candidates)
        }
        Err(e) => {
            log::warn!("[companion] ホスト IP 列挙に失敗: {e}");
            Vec::new()
        }
    }
}

/// 接続トラブルの自己診断（#363）。コンパニオンが有効なのに viewer から到達できない
/// 典型要因を、フロントが具体的に案内するための材料を返す。**判定のみ**で、ファイアウォール
/// 規則の変更は一切しない（管理者権限が要る／ユーザーのセキュリティ設定を勝手に変えない）。
#[derive(Debug, Serialize)]
pub struct CompanionDiagnostics {
    /// 実行 OS（`std::env::consts::OS`）。`network_category` を判定できるのは "windows" のときだけ。
    pub os: String,
    /// ネットワークプロファイル種別（Windows のみ判定）:
    /// "public"（受信ブロックの主因）/ "private" / "domain" / "unknown"、非 Windows は "unsupported"。
    pub network_category: String,
    /// LAN 側 IPv4 が 1 つでも見えているか（false なら Wi-Fi 未接続などで到達以前の問題）。
    pub has_lan_ip: bool,
}

/// 現在のネットワークプロファイル種別を返す（#363）。
///
/// Windows は Network List Manager（COM・`INetworkListManager`）で接続中ネットワークの
/// カテゴリを問い合わせる。Public が 1 つでもあれば受信ブロックの主因なので "public" を優先で返す。
/// PowerShell 等の外部プロセスは起動しない。Windows 以外は判定手段が無いため "unsupported"。
///
/// 返す値: "public" / "private" / "domain" / "unknown" / "unsupported"。
#[cfg(target_os = "windows")]
fn detect_network_category() -> String {
    use windows::Win32::Networking::NetworkListManager::{
        INetwork, INetworkListManager, NetworkListManager, NLM_ENUM_NETWORK_CONNECTED,
        NLM_NETWORK_CATEGORY_DOMAIN_AUTHENTICATED, NLM_NETWORK_CATEGORY_PRIVATE,
        NLM_NETWORK_CATEGORY_PUBLIC,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    };

    unsafe {
        // 既に別モードで初期化済みでも致命ではない（RPC_E_CHANGED_MODE 等は無視）。
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let manager: INetworkListManager =
            match CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("[companion] NetworkListManager 生成失敗: {e}");
                    return "unknown".to_string();
                }
            };

        let networks = match manager.GetNetworks(NLM_ENUM_NETWORK_CONNECTED) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("[companion] 接続中ネットワークの列挙失敗: {e}");
                return "unknown".to_string();
            }
        };

        // Public を最優先で返す（到達不能の主因）。Private / Domain は fallback。
        let mut fallback: Option<&str> = None;
        loop {
            let mut item: [Option<INetwork>; 1] = [None];
            let mut fetched = 0u32;
            if networks.Next(&mut item, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(network) = item[0].take() else { break };
            let cat = match network.GetCategory() {
                Ok(c) => c,
                Err(_) => continue,
            };
            if cat == NLM_NETWORK_CATEGORY_PUBLIC {
                return "public".to_string();
            } else if cat == NLM_NETWORK_CATEGORY_PRIVATE {
                fallback = Some("private");
            } else if cat == NLM_NETWORK_CATEGORY_DOMAIN_AUTHENTICATED {
                fallback = fallback.or(Some("domain"));
            }
        }
        fallback.unwrap_or("unknown").to_string()
    }
}

#[cfg(not(target_os = "windows"))]
fn detect_network_category() -> String {
    "unsupported".to_string()
}

/// `Authorization` ヘッダ値（無ければ None）が共有トークンと一致するかを判定する。
///
/// HTTP に依存しない純粋関数として切り出し、トークン検証をユニットテストする。
fn token_matches(auth_header: Option<&str>, token: &str) -> bool {
    match auth_header {
        Some(value) => value == format!("Bearer {token}"),
        None => false,
    }
}

/// `Authorization: Bearer <token>` を検証する。
fn authorized(request: &tiny_http::Request, token: &str) -> bool {
    let header = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str().to_string());
    token_matches(header.as_deref(), token)
}

/// JSON を指定ステータスコードで返す。
fn respond_json<T: Serialize>(request: tiny_http::Request, status: u16, body: &T) {
    let json = serde_json::to_string(body).unwrap_or_else(|e| {
        format!(r#"{{"state":"failed","error_code":"FETCH_FAILED","error_message":"JSON 生成失敗: {e}"}}"#)
    });
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    let response = tiny_http::Response::from_string(json)
        .with_header(header)
        .with_status_code(status);
    let _ = request.respond(response);
}

/// ファイルを ETag つきで `application/octet-stream` 配信する（不在は 404・`If-None-Match` 一致なら 304）。
///
/// ETag はファイル内容の sha256（= アイコンマニフェストの `hash` と同値）。この 1 関数で
/// **gear_db.bin / battle_db.bin（#356）と画像（.gti・#327）を共通に配信する**。viewer は保存済み
/// ハッシュを `If-None-Match` で送り、内容が変わっていなければ 304 を受けて再転送を省ける
/// （設計書 §6「変わったものだけ転送」の HTTP レイヤでの担保）。
fn respond_file(request: tiny_http::Request, path: &std::path::Path) {
    let Ok(bytes) = std::fs::read(path) else {
        let _ = request.respond(tiny_http::Response::from_string("not found").with_status_code(404));
        return;
    };
    let etag = crate::icon_manifest::hash_bytes(&bytes);

    let if_none_match = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("If-None-Match"))
        .map(|h| h.value.as_str().to_string());
    if crate::icon_manifest::etag_matches(if_none_match.as_deref(), &etag) {
        let _ = request.respond(tiny_http::Response::empty(304));
        return;
    }

    let ctype =
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/octet-stream"[..]).unwrap();
    let etag_header =
        tiny_http::Header::from_bytes(&b"ETag"[..], format!("\"{etag}\"").as_bytes()).unwrap();
    let response = tiny_http::Response::from_data(bytes)
        .with_header(ctype)
        .with_header(etag_header);
    let _ = request.respond(response);
}

/// `images/` 配下の相対パスを安全に解決する（パストラバーサル拒否）。
///
/// リクエストパス（先頭 `/` 込み）を受け取り、実ファイルパスを返す。
/// `images/` 始まりでない・`..` を含む・親を辿るものは `None`（＝配信拒否）。
///
/// 画像キャッシュは 2 系統に分かれているため、**先頭 kind セグメントで振り分ける**:
/// - バトルアイコン（`images.rs` が書く）: `images_root`（= `app_data_dir()/images`）
/// - ギア画像（`gear.rs` が書く）: `data_dir/images`（= `app_data_dir()/data/images`）
///
/// 両者はディレクトリ名が重複しない（weapon/stage… と gear/brand/skill）ので衝突しない。
fn resolve_image_path(
    req_path: &str,
    data_dir: &std::path::Path,
    images_root: &std::path::Path,
) -> Option<PathBuf> {
    let rel = req_path.trim_start_matches('/');
    // gear_db / battle_db が参照する画像パスは必ず `images/` 始まり。それ以外は拒否。
    if !rel.starts_with("images/") {
        return None;
    }
    // `..` や絶対パス・ルート要素を一切許さない（通常要素のみ許可）。
    let mut safe = PathBuf::new();
    for comp in std::path::Path::new(rel).components() {
        match comp {
            std::path::Component::Normal(c) => safe.push(c),
            _ => return None,
        }
    }
    // `images/<kind>/...` の kind を見て、バトルアイコンなら images_root 側へ回す。
    let kind = safe.iter().nth(1).and_then(|c| c.to_str()).unwrap_or("");
    // #360 で `ability` が BATTLE_ICON_KINDS に入ったので、個別の特別扱いは不要になった。
    if crate::icon_manifest::BATTLE_ICON_KINDS.contains(&kind) {
        // images_root は既に `images` を含むので、先頭の `images` セグメントを外して join する。
        let stripped: PathBuf = safe.iter().skip(1).collect();
        return Some(images_root.join(stripped));
    }
    Some(data_dir.join(safe))
}

/// バトルアイコンの差分同期マニフェストを返す（#327・設計書 §6）。
fn respond_icon_manifest(request: tiny_http::Request, images_root: &std::path::Path) {
    let generated_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let manifest = crate::icon_manifest::build_manifest(images_root, generated_at);
    let json = match serde_json::to_string(&manifest) {
        Ok(j) => j,
        Err(e) => {
            log::warn!("[companion] アイコンマニフェスト生成失敗: {e}");
            let _ = request
                .respond(tiny_http::Response::from_string("manifest error").with_status_code(500));
            return;
        }
    };
    let header =
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    let _ = request.respond(tiny_http::Response::from_string(json).with_header(header));
}

/// 1 リクエストを捌く。
///
/// 認証（Bearer）を通したうえで、状態を要する②更新命令系（`/update`・`/update_status`／#326）は
/// `ServerCtx`（`AppHandle` / ジョブ状態）を使って捌き、残りの静的配信ルート（ping / DB / 画像 /
/// アイコンマニフェスト）は `AppHandle` 非依存の `serve_asset` へ委譲する。
fn handle_request(request: tiny_http::Request, ctx: &ServerCtx) {
    // 疎通・認証チェック（全エンドポイント共通で Bearer 検証。②更新命令も同じ検証を通す）。
    if !authorized(&request, &ctx.token) {
        let _ = request.respond(tiny_http::Response::from_string("unauthorized").with_status_code(401));
        return;
    }

    // クエリを除いたパス部分で分岐。
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let method = request.method().clone();
    match (&method, path.as_str()) {
        // ②更新命令の受付（#326）。受付可否を即返し、実処理は裏で走る。
        (tiny_http::Method::Post, "/update") => handle_update_command(request, ctx),
        // ②更新命令の進捗・完了・失敗の問い合わせ（viewer はこれをポーリングする）。
        (tiny_http::Method::Get, "/update_status") => {
            let status = ctx
                .job
                .lock()
                .map(|s| s.clone())
                .unwrap_or_else(|e| UpdateStatus::failed("FETCH_FAILED", format!("状態ロック失敗: {e}"), None));
            respond_json(request, 200, &status);
        }
        // 静的配信（ping / DB / 画像 / アイコンマニフェスト）は AppHandle 非依存の経路へ。
        _ => serve_asset(request, &method, &path, &ctx.data_dir, &ctx.images_root),
    }
}

/// 認証済みリクエストを静的配信ルートで捌く（#324 / #327・`AppHandle` 非依存）。
///
/// ここに来るのは Bearer 検証済みで、②更新命令系（`/update`・`/update_status`）ではないリクエスト。
/// `AppHandle` を要さないため、#327 の HTTP テストは `authorized` と本関数を直接駆動して
/// 画像振り分け・ETag/304・アイコンマニフェストを検証する。
fn serve_asset(
    request: tiny_http::Request,
    method: &tiny_http::Method,
    path: &str,
    data_dir: &std::path::Path,
    images_root: &std::path::Path,
) {
    match (method, path) {
        (tiny_http::Method::Get, "/ping") => {
            let _ = request.respond(tiny_http::Response::from_string("pong"));
        }
        (tiny_http::Method::Get, "/gear_db.bin") => {
            respond_file(request, &data_dir.join("gear_db.bin"))
        }
        (tiny_http::Method::Get, "/battle_db.bin") => {
            respond_file(request, &data_dir.join("battle_db.bin"))
        }
        // バトルアイコンの差分同期マニフェスト（#327）。
        (tiny_http::Method::Get, "/icons/manifest") => respond_icon_manifest(request, images_root),
        // 画像（.gti）を相対パスで配信。viewer は gear_db / battle_db の画像パスをそのまま GET する。
        (tiny_http::Method::Get, p) if p.starts_with("/images/") => {
            match resolve_image_path(p, data_dir, images_root) {
                Some(file) => respond_file(request, &file),
                None => {
                    let _ = request
                        .respond(tiny_http::Response::from_string("forbidden").with_status_code(403));
                }
            }
        }
        _ => {
            let _ = request.respond(tiny_http::Response::from_string("not found").with_status_code(404));
        }
    }
}

/// mDNS 広告を開始する（ベストエフォート）。失敗しても None を返してサーバーは継続。
fn start_mdns(port: u16, ips: &[IpAddr]) -> Option<ServiceDaemon> {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("[companion] mDNS デーモン起動失敗（広告なしで継続）: {e}");
            return None;
        }
    };
    // TXT レコードはブロードキャストされるためトークンは載せない（バージョンのみ）。
    let props: [(&str, &str); 1] = [("v", "1")];
    let service = match ServiceInfo::new(
        SERVICE_TYPE,
        SERVICE_INSTANCE,
        SERVICE_HOST,
        ips,
        port,
        &props[..],
    ) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("[companion] mDNS ServiceInfo 構築失敗（広告なしで継続）: {e}");
            return None;
        }
    };
    match daemon.register(service) {
        Ok(_) => {
            log::info!("[companion] mDNS 広告開始 {SERVICE_TYPE} port={port}");
            Some(daemon)
        }
        Err(e) => {
            log::warn!("[companion] mDNS 登録失敗（広告なしで継続）: {e}");
            None
        }
    }
}

/// `app_data_dir()/data` を解決する（存在しなければ作成）。
fn resolve_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("アプリデータディレクトリ解決失敗: {e}"))?
        .join("data");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// コンパニオンサーバーを起動する。
///
/// ランダムトークン発行 → 空きポートで HTTP サーバー起動 → mDNS 広告 →
/// `{host_ips, port, token}` を返す（ペアリング QR 用）。
/// 既に稼働中なら現行の情報をそのまま返す（多重起動しない）。
#[tauri::command]
pub fn companion_start(
    app: AppHandle,
    state: State<'_, CompanionState>,
) -> Result<CompanionInfo, String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;

    // 既に稼働中: 現行情報を返す。
    if let Some(running) = guard.as_ref() {
        return Ok(CompanionInfo {
            host_ips: host_ipv4s().iter().map(|ip| ip.to_string()).collect(),
            port: running.port,
            token: running.token.clone(),
        });
    }

    let data_dir = resolve_data_dir(&app)?;
    // バトルアイコンのキャッシュルート（`app_data_dir()/images`・data_dir とは別系統／#327）。
    // 未キャッシュでも起動を妨げない（マニフェストが空になるだけ）。
    let images_root = crate::icon_manifest::resolve_images_root(&app)?;

    // 空きポートで全インターフェースにバインド（LAN の viewer から到達可能に）。
    let server = tiny_http::Server::http("0.0.0.0:0")
        .map_err(|e| format!("HTTP サーバー起動失敗: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .ok_or_else(|| "バインドポートの解決に失敗".to_string())?;

    let token = generate_token();
    let stop = Arc::new(AtomicBool::new(false));

    // 配信スレッド: recv_timeout でポーリングし、stop フラグで抜ける。
    let server = Arc::new(server);
    let handle = {
        let server = Arc::clone(&server);
        let stop = Arc::clone(&stop);
        let ctx = ServerCtx {
            token: token.clone(),
            data_dir: data_dir.clone(),
            images_root: images_root.clone(),
            app: app.clone(),
            job: Arc::new(Mutex::new(UpdateStatus::default())),
        };
        std::thread::spawn(move || {
            log::info!("[companion] HTTP サーバー稼働 0.0.0.0:{port}");
            while !stop.load(Ordering::SeqCst) {
                match server.recv_timeout(Duration::from_millis(500)) {
                    Ok(Some(request)) => handle_request(request, &ctx),
                    Ok(None) => {} // タイムアウト: stop フラグを再チェック
                    Err(e) => {
                        log::warn!("[companion] リクエスト受信エラー: {e}");
                        break;
                    }
                }
            }
            log::info!("[companion] HTTP サーバー停止");
        })
    };

    let ips = host_ipv4s();
    let mdns = start_mdns(port, &ips);

    *guard = Some(RunningServer {
        port,
        token: token.clone(),
        stop,
        handle: Some(handle),
        mdns,
    });

    Ok(CompanionInfo {
        host_ips: ips.iter().map(|ip| ip.to_string()).collect(),
        port,
        token,
    })
}

/// コンパニオンサーバーを停止する（HTTP スレッド join + mDNS 広告停止）。
#[tauri::command]
pub fn companion_stop(state: State<'_, CompanionState>) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if let Some(mut running) = guard.take() {
        running.stop.store(true, Ordering::SeqCst);
        if let Some(daemon) = running.mdns.take() {
            let _ = daemon.shutdown();
        }
        if let Some(handle) = running.handle.take() {
            let _ = handle.join();
        }
        log::info!("[companion] 停止完了");
    }
    Ok(())
}

/// 稼働状態を返す。
#[tauri::command]
pub fn companion_status(state: State<'_, CompanionState>) -> Result<CompanionStatus, String> {
    let guard = state.0.lock().map_err(|e| e.to_string())?;
    Ok(match guard.as_ref() {
        Some(running) => CompanionStatus {
            running: true,
            port: Some(running.port),
        },
        None => CompanionStatus {
            running: false,
            port: None,
        },
    })
}

/// 接続トラブルの自己診断材料を返す（#363）。フロントはこれを見て、Windows の
/// ネットワークプロファイルが Public のとき「プライベートに変更してください」等を案内する。
#[tauri::command]
pub fn companion_diagnostics() -> CompanionDiagnostics {
    CompanionDiagnostics {
        os: std::env::consts::OS.to_string(),
        network_category: detect_network_category(),
        has_lan_ip: !host_ipv4s().is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};

    /// バトルアイコンのキャッシュルート（`app_data_dir()/images`）。
    fn images_root() -> &'static Path {
        Path::new("/app/images")
    }

    /// テスト用: `(iface 名, "a.b.c.d")` の列を `filter_host_ipv4s` に渡し、文字列で受け取る。
    fn filtered(pairs: &[(&str, &str)]) -> Vec<String> {
        let input = pairs
            .iter()
            .map(|(name, ip)| (name.to_string(), ip.parse::<Ipv4Addr>().unwrap()))
            .collect();
        filter_host_ipv4s(input)
            .iter()
            .map(|ip| ip.to_string())
            .collect()
    }

    #[test]
    fn drops_apipa_link_local() {
        // #369: 169.254.0.0/16 は原理的に LAN の別端末から到達できないので載せない。
        let got = filtered(&[
            ("イーサネット", "192.168.3.8"),
            ("Wi-Fi", "169.254.121.53"),
            ("Bluetooth ネットワーク接続", "169.254.65.55"),
        ]);
        assert_eq!(got, vec!["192.168.3.8"]);
    }

    #[test]
    fn keeps_all_ordinary_private_ranges() {
        // #369 の最重要事項: 172.16-31.x.x を含む正当なプライベート IP を絶対に除外しない。
        for ip in [
            "192.168.3.8",
            "192.168.0.1",
            "10.0.0.5",
            "10.255.255.254",
            "172.16.0.1",
            "172.20.10.1",
            "172.31.255.254",
        ] {
            assert_eq!(filtered(&[("イーサネット", ip)]), vec![ip], "{ip} が消えた");
        }
    }

    #[test]
    fn keeps_172_even_on_virtual_iface() {
        // 仮想 NIC 名でも「除外」はしない（後ろに回すだけ）。誤除外で繋がらなくなるのを防ぐ。
        let got = filtered(&[("vEthernet (WSL (Hyper-V firewall))", "172.31.144.1")]);
        assert_eq!(got, vec!["172.31.144.1"]);
    }

    #[test]
    fn ranks_common_lan_before_172_and_virtual() {
        // 実機（#369 の実測）に近い並び。本命 192.168.3.8 が先頭に来ること。
        let got = filtered(&[
            ("vEthernet (WSL (Hyper-V firewall))", "172.31.144.1"),
            ("vEthernet (Default Switch)", "172.23.32.1"),
            ("Wi-Fi", "169.254.88.185"),
            ("イーサネット", "192.168.3.8"),
        ]);
        assert_eq!(got, vec!["192.168.3.8", "172.31.144.1", "172.23.32.1"]);
    }

    #[test]
    fn ranks_physical_172_before_virtual_ifaces() {
        // 172 系を使う実 LAN の環境: 物理 NIC の 172 が仮想 NIC より前に来る。
        let got = filtered(&[
            ("vEthernet (Default Switch)", "172.23.32.1"),
            ("イーサネット", "172.20.10.9"),
        ]);
        assert_eq!(got, vec!["172.20.10.9", "172.23.32.1"]);
    }

    #[test]
    fn keeps_global_ip_without_panicking() {
        // グローバル IP が混ざっていても落ちない。192.168 / 10 系より後ろ、172 系より前。
        let got = filtered(&[
            ("イーサネット", "172.16.5.5"),
            ("ppp0", "203.0.113.7"),
            ("Wi-Fi", "192.168.1.2"),
        ]);
        assert_eq!(got, vec!["192.168.1.2", "203.0.113.7", "172.16.5.5"]);
    }

    #[test]
    fn stable_within_same_rank() {
        // 同ランクなら if_addrs の列挙順を保つ（無用な並び替えをしない）。
        let got = filtered(&[
            ("イーサネット", "192.168.3.8"),
            ("Wi-Fi", "10.0.0.5"),
            ("イーサネット 2", "192.168.4.9"),
        ]);
        assert_eq!(got, vec!["192.168.3.8", "10.0.0.5", "192.168.4.9"]);
    }

    #[test]
    fn empty_input_yields_empty() {
        // 候補ゼロでも panic しない。has_lan_ip=false の既存警告経路がそのまま働く。
        assert!(filtered(&[]).is_empty());
    }

    #[test]
    fn all_apipa_yields_empty_not_panic() {
        assert!(filtered(&[("Wi-Fi", "169.254.1.2"), ("イーサネット", "169.254.9.9")]).is_empty());
    }

    #[test]
    fn resolves_valid_image_path() {
        let data = Path::new("/data");
        // ギア画像は従来どおり data_dir 側（#324 の挙動を変えない）。
        let got = resolve_image_path("/images/gear/head/abc.gti", data, images_root());
        assert_eq!(got, Some(data.join("images/gear/head/abc.gti")));
    }

    #[test]
    fn routes_gear_kinds_to_data_dir() {
        let data = Path::new("/data");
        for kind in ["gear", "brand", "skill"] {
            let got = resolve_image_path(&format!("/images/{kind}/x.gti"), data, images_root());
            assert_eq!(got, Some(data.join(format!("images/{kind}/x.gti"))));
        }
    }

    #[test]
    fn routes_battle_icon_kinds_to_images_root() {
        // #327: バトルアイコンは `app_data_dir()/images/<kind>/<sha256(name)>.gti` にある。
        // battle_db の name から解決した `images/weapon/<hash>.gti` がそのまま引けること。
        let data = Path::new("/data");
        for kind in ["weapon", "sub_weapon", "special_weapon", "stage", "ability"] {
            let got = resolve_image_path(&format!("/images/{kind}/abc.gti"), data, images_root());
            assert_eq!(got, Some(images_root().join(format!("{kind}/abc.gti"))));
        }
    }

    #[test]
    fn rejects_non_images_prefix() {
        let data = Path::new("/data");
        // gear_db.bin 等の直接パスや任意ファイルは画像エンドポイントでは配信しない。
        assert_eq!(resolve_image_path("/gear_db.bin", data, images_root()), None);
        assert_eq!(resolve_image_path("/secrets.txt", data, images_root()), None);
    }

    #[test]
    fn rejects_path_traversal() {
        let data = Path::new("/data");
        assert_eq!(
            resolve_image_path("/images/../../etc/passwd", data, images_root()),
            None
        );
        assert_eq!(
            resolve_image_path("/images/../gear_db.bin", data, images_root()),
            None
        );
        // バトルアイコン側へ振り分けられる kind でも脱出は許さない。
        assert_eq!(
            resolve_image_path("/images/weapon/../../secrets.txt", data, images_root()),
            None
        );
    }

    // --- HTTP レイヤ（実ソケットで handle_request を駆動する） ---

    fn temp_root(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "splabo_companion_test_{}_{}_{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// テスト用サーバーを立て、1 リクエストだけ捌いて生レスポンスを返す。
    ///
    /// 静的配信ルート（#324 / #327）は `AppHandle` 非依存なので、本番と同じ `authorized` で
    /// Bearer を検証し `serve_asset` へ委譲する（②更新命令系は `AppHandle` を要すため対象外）。
    fn serve_once(raw_request: &str, data_dir: PathBuf, images_root: PathBuf) -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let handle = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            if !authorized(&request, "tok") {
                let _ = request
                    .respond(tiny_http::Response::from_string("unauthorized").with_status_code(401));
                return;
            }
            let url = request.url().to_string();
            let path = url.split('?').next().unwrap_or("").to_string();
            let method = request.method().clone();
            serve_asset(request, &method, &path, &data_dir, &images_root);
        });

        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream.write_all(raw_request.as_bytes()).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        handle.join().unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    fn get(path: &str, extra_headers: &str, data_dir: PathBuf, images_root: PathBuf) -> String {
        let raw = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer tok\r\n{extra_headers}Connection: close\r\n\r\n"
        );
        serve_once(&raw, data_dir, images_root)
    }

    #[test]
    fn serves_battle_icon_from_images_root() {
        let data_dir = temp_root("http_data");
        let images_root = temp_root("http_images");
        std::fs::create_dir_all(images_root.join("weapon")).unwrap();
        std::fs::write(images_root.join("weapon").join("abc.gti"), b"WEAPON-BYTES").unwrap();

        let res = get("/images/weapon/abc.gti", "", data_dir, images_root);
        assert!(res.starts_with("HTTP/1.1 200 OK"), "unexpected: {res}");
        assert!(res.contains("WEAPON-BYTES"), "body missing: {res}");
        // ETag は内容の sha256（マニフェストの hash と同値）。
        let etag = crate::icon_manifest::hash_bytes(b"WEAPON-BYTES");
        assert!(res.contains(&format!("ETag: \"{etag}\"")), "etag missing: {res}");
    }

    #[test]
    fn returns_304_when_etag_matches() {
        let data_dir = temp_root("http_data");
        let images_root = temp_root("http_images");
        std::fs::create_dir_all(images_root.join("stage")).unwrap();
        std::fs::write(images_root.join("stage").join("s1.gti"), b"STAGE-BYTES").unwrap();

        let etag = crate::icon_manifest::hash_bytes(b"STAGE-BYTES");
        let res = get(
            "/images/stage/s1.gti",
            &format!("If-None-Match: \"{etag}\"\r\n"),
            data_dir,
            images_root,
        );
        // 変わっていないので再転送しない（設計書 §6）。
        assert!(res.starts_with("HTTP/1.1 304"), "unexpected: {res}");
        assert!(!res.contains("STAGE-BYTES"), "body should not be sent: {res}");
    }

    #[test]
    fn serves_gear_db_with_etag() {
        // #356: gear_db.bin もアイコンと同じく内容 sha256 の ETag を付けて配信する。
        let data_dir = temp_root("http_data");
        let images_root = temp_root("http_images");
        std::fs::write(data_dir.join("gear_db.bin"), b"GEAR-DB-BYTES").unwrap();

        let res = get("/gear_db.bin", "", data_dir, images_root);
        assert!(res.starts_with("HTTP/1.1 200 OK"), "unexpected: {res}");
        assert!(res.contains("GEAR-DB-BYTES"), "body missing: {res}");
        assert!(res.contains("application/octet-stream"), "content-type: {res}");
        // ETag は内容の sha256（画像・マニフェストの hash と同じ算出）。
        let etag = crate::icon_manifest::hash_bytes(b"GEAR-DB-BYTES");
        assert!(res.contains(&format!("ETag: \"{etag}\"")), "etag missing: {res}");
    }

    #[test]
    fn returns_304_when_db_etag_matches() {
        // #356: viewer が保存済みハッシュを If-None-Match で送り、内容が同じなら 304・本文なし。
        let data_dir = temp_root("http_data");
        let images_root = temp_root("http_images");
        std::fs::write(data_dir.join("battle_db.bin"), b"BATTLE-DB-BYTES").unwrap();

        let etag = crate::icon_manifest::hash_bytes(b"BATTLE-DB-BYTES");
        let res = get(
            "/battle_db.bin",
            &format!("If-None-Match: \"{etag}\"\r\n"),
            data_dir,
            images_root,
        );
        // 変わっていないので再転送しない（DB ファイルの転送そのものを省く・viewer #47）。
        assert!(res.starts_with("HTTP/1.1 304"), "unexpected: {res}");
        assert!(!res.contains("BATTLE-DB-BYTES"), "body should not be sent: {res}");
    }

    #[test]
    fn serves_db_when_etag_differs() {
        // If-None-Match が古い（不一致）なら通常どおり 200 + 本文 + 現行 ETag。
        let data_dir = temp_root("http_data");
        let images_root = temp_root("http_images");
        std::fs::write(data_dir.join("gear_db.bin"), b"GEAR-DB-V2").unwrap();

        let res = get(
            "/gear_db.bin",
            "If-None-Match: \"stale-hash\"\r\n",
            data_dir,
            images_root,
        );
        assert!(res.starts_with("HTTP/1.1 200 OK"), "unexpected: {res}");
        assert!(res.contains("GEAR-DB-V2"), "body missing: {res}");
        let etag = crate::icon_manifest::hash_bytes(b"GEAR-DB-V2");
        assert!(res.contains(&format!("ETag: \"{etag}\"")), "etag missing: {res}");
    }

    #[test]
    fn serves_icon_manifest() {
        let data_dir = temp_root("http_data");
        let images_root = temp_root("http_images");
        std::fs::create_dir_all(images_root.join("weapon")).unwrap();
        std::fs::write(images_root.join("weapon").join("abc.gti"), b"W").unwrap();

        let res = get("/icons/manifest", "", data_dir, images_root);
        assert!(res.starts_with("HTTP/1.1 200 OK"), "unexpected: {res}");
        assert!(res.contains("application/json"), "content-type: {res}");
        assert!(res.contains("icon-manifest-v1"), "schema: {res}");
        assert!(res.contains("images/weapon/abc.gti"), "path: {res}");
        assert!(
            res.contains(&crate::icon_manifest::hash_bytes(b"W")),
            "hash: {res}"
        );
    }

    #[test]
    fn requires_token_for_icon_endpoints() {
        let data_dir = temp_root("http_data");
        let images_root = temp_root("http_images");
        let raw = "GET /icons/manifest HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let res = serve_once(raw, data_dir, images_root);
        // 未ペア端末にはアイコンもマニフェストも渡さない。
        assert!(res.starts_with("HTTP/1.1 401"), "unexpected: {res}");
    }

    // --- トークン検証（②更新命令も同じ検証を通る） ---

    #[test]
    fn accepts_matching_bearer_token() {
        assert!(token_matches(Some("Bearer abc123"), "abc123"));
    }

    #[test]
    fn rejects_wrong_or_missing_token() {
        assert!(!token_matches(Some("Bearer wrong"), "abc123"));
        assert!(!token_matches(None, "abc123"));
        // スキームなし・小文字スキーム・前後の差異は許さない。
        assert!(!token_matches(Some("abc123"), "abc123"));
        assert!(!token_matches(Some("bearer abc123"), "abc123"));
        assert!(!token_matches(Some("Bearer abc123 "), "abc123"));
        // 空トークン設定でも「Bearer 」だけで通ってしまわないこと。
        assert!(!token_matches(Some("Bearer"), ""));
    }

    // --- 多重起動防止 ---

    #[test]
    fn begins_job_from_idle() {
        let mut status = UpdateStatus::default();
        assert!(try_begin(&mut status, 100, UpdateTarget::Both));
        assert_eq!(status.state, UpdateState::Running);
        assert_eq!(status.step, Some(UpdateStep::Battles));
        assert_eq!(status.started_at, Some(100));
    }

    #[test]
    fn rejects_second_job_while_running() {
        let mut status = UpdateStatus::default();
        assert!(try_begin(&mut status, 100, UpdateTarget::Both));
        // 実行中に再度命令が来ても走らせない（進行中の状態を保つ）。
        assert!(!try_begin(&mut status, 200, UpdateTarget::Both));
        assert_eq!(status.state, UpdateState::Running);
        assert_eq!(status.started_at, Some(100));
    }

    #[test]
    fn rejects_second_job_even_with_different_target() {
        // #395: 対象を分けても並走はさせない（任天堂 API を同時に叩かせない）。
        let mut status = UpdateStatus::default();
        assert!(try_begin(&mut status, 100, UpdateTarget::Battle));
        assert!(!try_begin(&mut status, 200, UpdateTarget::Gear));
        assert_eq!(status.started_at, Some(100));
        assert_eq!(status.step, Some(UpdateStep::Battles));
    }

    #[test]
    fn allows_rerun_after_done_or_failed() {
        let mut status = UpdateStatus::default();
        assert!(try_begin(&mut status, 100, UpdateTarget::Both));
        status.state = UpdateState::Done;
        assert!(try_begin(&mut status, 200, UpdateTarget::Both));
        assert_eq!(status.started_at, Some(200));

        let mut failed = UpdateStatus::failed("NOT_LOGGED_IN", "未ログイン", None);
        assert!(try_begin(&mut failed, 300, UpdateTarget::Both));
        assert_eq!(failed.state, UpdateState::Running);
        // 前回の失敗情報は引きずらない。
        assert_eq!(failed.error_code, None);
    }

    // --- #395: 更新対象の指定 ---

    #[test]
    fn parses_explicit_targets() {
        assert_eq!(parse_update_target(r#"{"target":"battle"}"#), UpdateTarget::Battle);
        assert_eq!(parse_update_target(r#"{"target":"gear"}"#), UpdateTarget::Gear);
        assert_eq!(parse_update_target(r#"{"target":"both"}"#), UpdateTarget::Both);
    }

    #[test]
    fn falls_back_to_both_for_missing_or_invalid_body() {
        // 古い viewer（ボディ無し）・空ボディ・JSON でないもの・未知の値は、すべて現行動作の both。
        for body in [
            "",
            "{}",
            r#"{"target":null}"#,
            r#"{"target":"weapons"}"#,
            r#"{"target":123}"#,
            "not json",
        ] {
            assert_eq!(parse_update_target(body), UpdateTarget::Both, "body={body:?}");
        }
    }

    #[test]
    fn ignores_unknown_fields_in_body() {
        // viewer が将来フィールドを増やしても target だけ読めればよい。
        assert_eq!(
            parse_update_target(r#"{"target":"gear","since":"2026-01-01"}"#),
            UpdateTarget::Gear
        );
    }

    #[test]
    fn first_step_follows_target() {
        // ギア単独なら、取りに行かない battles を進捗に出さない。
        assert_eq!(
            UpdateStatus::running(1, UpdateTarget::Gear).step,
            Some(UpdateStep::Gear)
        );
        assert_eq!(
            UpdateStatus::running(1, UpdateTarget::Battle).step,
            Some(UpdateStep::Battles)
        );
        assert_eq!(
            UpdateStatus::running(1, UpdateTarget::Both).step,
            Some(UpdateStep::Battles)
        );
    }

    #[test]
    fn target_selects_phases() {
        assert!(UpdateTarget::Battle.includes_battle() && !UpdateTarget::Battle.includes_gear());
        assert!(UpdateTarget::Gear.includes_gear() && !UpdateTarget::Gear.includes_battle());
        assert!(UpdateTarget::Both.includes_battle() && UpdateTarget::Both.includes_gear());
    }

    #[test]
    fn skipped_phase_counts_are_null() {
        // #395: 実行しなかった側は 0 ではなく null。viewer が「更新して 0 件」と誤読しないため。
        let result = UpdateResult {
            gear: Some(10),
            ..UpdateResult::default()
        };
        let json = serde_json::to_value(result).unwrap();
        assert_eq!(json["gear"], 10);
        assert_eq!(json["battles"], serde_json::Value::Null);
        assert_eq!(json["details"], serde_json::Value::Null);
        assert_eq!(json["exported_battles"], serde_json::Value::Null);
    }

    // --- エラー分類（viewer の異常系 UX 分岐） ---

    #[test]
    fn classifies_errors_for_viewer() {
        assert_eq!(
            classify_error("NOT_LOGGED_IN: Nintendo アカウントでログインしていません。"),
            "NOT_LOGGED_IN"
        );
        assert_eq!(
            classify_error("FETCH_IN_PROGRESS: 既にバトル取得が進行中です。"),
            "FETCH_IN_PROGRESS"
        );
        assert_eq!(classify_error("bullet token 取得失敗: invalid_grant"), "TOKEN_EXPIRED");
        assert_eq!(classify_error("HTTP クライアント構築失敗: dns"), "FETCH_FAILED");
    }

    /// #399: znca-api の一時障害を TOKEN_EXPIRED にしない。
    ///
    /// 実際に出たログ（2026-07-22）が UPSTREAM_UNAVAILABLE になること、
    /// 401/403・invalid_grant は従来どおり TOKEN_EXPIRED のままであることを固定する。
    #[test]
    fn classifies_upstream_failure_separately_from_auth() {
        assert_eq!(
            classify_error(
                "UPSTREAM_UNAVAILABLE: bullet token 取得失敗: [znca-api] Non-200 status code: 500 (status=500 upstream_error=timeout)"
            ),
            "UPSTREAM_UNAVAILABLE"
        );
        assert_eq!(
            classify_error("AUTH_EXPIRED: bullet token 取得失敗: [znca-api] Non-200 status code: 401 (status=401 upstream_error=-)"),
            "TOKEN_EXPIRED"
        );
        // ローカルのネットワーク断は既存の汎用コードに寄せる（viewer に新コードを増やさない）
        assert_eq!(
            classify_error("NETWORK: bullet token 取得失敗: fetch failed (status=- upstream_error=-)"),
            "FETCH_FAILED"
        );
        assert_eq!(
            classify_error("CORAL_UPGRADE: bullet token 取得失敗: [znc] Upgrade required. (status=200 upstream_error=-)"),
            "CORAL_UPGRADE"
        );
        // 理由不明の bullet token 失敗を「失効」と決めつけない（本 Issue の核心）
        assert_eq!(
            classify_error("bullet token 取得失敗: 何かよく分からない失敗"),
            "FETCH_FAILED"
        );
    }

    /// Rust の分類（nxapi::FailureKind）と companion の error_code が結線されていること。
    #[test]
    fn nxapi_failure_kind_maps_to_expected_error_code() {
        let cases: &[(Option<u16>, Option<&str>, &str, &str)] = &[
            (Some(500), Some("timeout"), "[znca-api] Non-200 status code: 500", "UPSTREAM_UNAVAILABLE"),
            (Some(503), None, "[znca-api] Non-200 status code: 503", "UPSTREAM_UNAVAILABLE"),
            (Some(401), None, "unauthorized", "TOKEN_EXPIRED"),
            (Some(403), None, "forbidden", "TOKEN_EXPIRED"),
            (Some(400), Some("invalid_grant"), "bad request", "TOKEN_EXPIRED"),
            (None, None, "fetch failed", "FETCH_FAILED"),
            (Some(200), None, "[znc] Upgrade required.", "CORAL_UPGRADE"),
        ];
        for (status, upstream, message, expected) in cases {
            let kind = crate::nxapi::classify_failure(*status, *upstream, message);
            let rendered = match kind.code() {
                Some(code) => format!("{code}: bullet token 取得失敗: {message}"),
                None => format!("bullet token 取得失敗: {message}"),
            };
            assert_eq!(&classify_error(&rendered), expected, "message={message}");
        }
    }

    // --- JSON 形（viewer #34 が読む契約） ---

    #[test]
    fn serializes_status_shape() {
        let mut status = UpdateStatus::running(1700000000, UpdateTarget::Both);
        status.state = UpdateState::Done;
        status.step = None;
        status.finished_at = Some(1700000042);
        // target 省略（= both）のときは全件数が埋まるので、古い viewer から見た形は #395 前と同じ。
        status.result = Some(UpdateResult {
            battles: Some(3),
            details: Some(2),
            gear: Some(10),
            exported_battles: Some(50),
        });
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["state"], "done");
        assert_eq!(json["step"], serde_json::Value::Null);
        assert_eq!(json["started_at"], 1700000000_i64);
        assert_eq!(json["finished_at"], 1700000042_i64);
        assert_eq!(json["result"]["battles"], 3);
        assert_eq!(json["result"]["exported_battles"], 50);

        let failed = UpdateStatus::failed("NOT_LOGGED_IN", "未ログイン", None);
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["state"], "failed");
        assert_eq!(json["error_code"], "NOT_LOGGED_IN");

        let running = UpdateStatus::running(1, UpdateTarget::Both);
        let json = serde_json::to_value(&running).unwrap();
        assert_eq!(json["state"], "running");
        assert_eq!(json["step"], "battles");
    }

    // --- #363: 接続トラブルの自己診断 ---

    #[test]
    fn diagnostics_reports_os_and_nonempty_category() {
        // フロント（#363）が読む JSON 契約の固定。os は実行 OS、network_category は
        // どの OS でも空文字にはならない（非 Windows は "unsupported"）。
        let d = companion_diagnostics();
        assert_eq!(d.os, std::env::consts::OS);
        assert!(!d.network_category.is_empty());

        let json = serde_json::to_value(&d).unwrap();
        assert!(json.get("os").is_some());
        assert!(json.get("network_category").is_some());
        assert!(json.get("has_lan_ip").is_some());
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn network_category_is_unsupported_off_windows() {
        // 判定は Windows(NLM) 専用。非 Windows は誤判定せず "unsupported" を返す。
        assert_eq!(detect_network_category(), "unsupported");
    }
}
