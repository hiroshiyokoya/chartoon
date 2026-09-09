#!/usr/bin/env node
/**
 * nxapi-wrapper — chartoon Tauri サイドカー
 *
 * 使い方 (CLI / sidecar):
 *   node wrapper.js setup <session_token> <data_dir>
 *   node wrapper.js get_bullet_token <data_dir>
 *   node wrapper.js check_login <data_dir>
 *   node wrapper.js weapon_records <data_dir>
 *
 * 結果は stdout に 1行の JSON で出力する。
 * エラー時は
 *   {"ok": false, "error": "<message>", "status": <HTTP ステータス|null>, "upstream_error": "<body の error|null>"}
 * を stdout に出力し、exit code 1 で終了する（#399）。
 * `status` / `upstream_error` は **Rust 側が失敗理由を分類するための構造**であり、
 * 文字列 `error` のパターンマッチに頼らせないために用意している。
 */

import path from 'path';

// ── nxapi 内部モジュール（相対パスで直接インポート、exports 制限を回避）
import { init as initGlobals } from './node_modules/nxapi/dist/common/globals.js';
import { embedded_nxapi_auth_cli_client_id, pkg } from './node_modules/nxapi/dist/util/product.js';
import { initStorage } from './node_modules/nxapi/dist/util/storage.js';
import { NxapiClientAssertionProvider, setClientAssertionProvider } from './node_modules/nxapi/dist/util/nxapi-auth.js';
import { addUserAgent } from './node_modules/nxapi/dist/util/useragent.js';

// ── nxapi 初期化 ────────────────────────────────────────────
initGlobals();
addUserAgent('chartoon-sidecar/0.1.0');

// nxapi-znca-api クライアント ID を設定（f-token 生成に必要）
function setupClientAssertion() {
  if (embedded_nxapi_auth_cli_client_id) {
    setClientAssertionProvider(
      new NxapiClientAssertionProvider(
        embedded_nxapi_auth_cli_client_id,
        undefined,
        'ca:gf ca:er ca:dr ca:na',
      ),
    );
  } else if (pkg.__nxapi_auth?.cli?.client_id) {
    setClientAssertionProvider(
      new NxapiClientAssertionProvider(
        pkg.__nxapi_auth.cli.client_id,
        undefined,
        'ca:gf ca:er ca:dr ca:na',
      ),
    );
  } else if (process.env.NXAPI_AUTH_CLIENT_ID) {
    setClientAssertionProvider(
      new NxapiClientAssertionProvider(
        process.env.NXAPI_AUTH_CLIENT_ID,
        undefined,
        process.env.NXAPI_AUTH_SCOPE ?? 'ca:gf ca:er ca:dr',
      ),
    );
  } else {
    respond({ ok: false, error: 'nxapi クライアント ID が見つかりません' });
    process.exit(1);
  }
}

// ── エントリポイント ────────────────────────────────────────
const [, , cmd, ...args] = process.argv;

async function main() {
  setupClientAssertion();

  switch (cmd) {
    case 'setup':
      await cmdSetup(args);
      break;
    case 'get_bullet_token':
      await cmdGetBulletToken(args);
      break;
    case 'check_login':
      await cmdCheckLogin(args);
      break;
    case 'weapon_records':
      await cmdWeaponRecords(args);
      break;
    default:
      respond({ ok: false, error: `不明なコマンド: ${cmd ?? '(なし)'}` });
      process.exit(1);
  }
}

main().catch((e) => {
  respond(describeFailure(e));
  process.exit(1);
});

// ── コマンド実装 ────────────────────────────────────────────

/**
 * setup <session_token> <data_dir>
 * session_token を nxapi のストレージ形式で保存する。
 */
/**
 * 一時エラー（znca-api の 5xx / タイムアウト / ネットワーク断）に対して指数バックオフで再試行する。
 * 恒久エラー（未ログイン・4xx 等）は即座に投げる。nxapi の znca-api 呼び出しが断続的に
 * 500{"error":"timeout"} を返す事象への耐性（#272）。
 */
/**
 * 1 回のネットワーク操作の上限時間（ミリ秒・#402）。
 *
 * nxapi の getBulletToken / getWeaponRecords は内部で複数の HTTP を叩くが、
 * どれも undici(global fetch) のデフォルトは実質無制限に近く、
 * 「接続はできるが応答が返らない」上流（stat.ink / znca-api / Cloudflare エッジ）に
 * 当たるとサイドカーが永久にハングする → Rust 側の await も返らず「取得中」が固まる。
 * ここで 1 操作ごとに時間上限を設け、超えたら**必ず**エラーとして返す。
 */
const NETWORK_TIMEOUT_MS = 30000;

/**
 * 認証（bulletToken 取得）だけの上限時間（ミリ秒・#611）。
 *
 * 🔴 **認証は 1 回で 12 本前後の往復がある。** 30 秒では足りずに落ちていた。
 *
 * ```
 * id_token → f-token(znca-api) → Coral login → getWebServiceToken → bullet_token
 * ```
 *
 * に加えて nxapi の `splatnet3.js` が毎回 7 本の追加 API を並列で叩く
 * （friendList / chats / webServices / activeEvent / media / announcements / currentUser）。
 *
 * ここで落ちると**枠だけ減ってキャッシュは空のまま**になり、次の試行も最初からやり直す。
 * 1 時間 4 回の枠しかないので、4 回落ちれば締め出される。
 * 「取れないことが多い」の実体はこれだった（実データで 3 分間に 4 回の再認証を確認）。
 *
 * ハングを防ぐという当初の目的（#402）は保ちつつ、往復の本数に見合う余裕を取る。
 */
const AUTH_TIMEOUT_MS = 90000;

/**
 * `promise` に時間上限を設ける（#402）。
 *
 * global fetch へ AbortSignal を差し込む口が getBulletToken 等には無いため、
 * Promise.race で打ち切る。元の promise 自体はキャンセルできないが、サイドカーは
 * 応答後すぐ process.exit するプロセスなので、宙に浮いた fetch はプロセス終了で回収される。
 * タイムアウト時のメッセージには "timeout"/"ETIMEDOUT" を含め、Rust 側 classify_failure と
 * 本ファイルの withRetry が「一時エラー」として拾えるようにする。
 */
function withTimeout(promise, ms = NETWORK_TIMEOUT_MS, label = 'network') {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(
      () => reject(new Error(`[${label}] request timeout (ETIMEDOUT): ${ms}ms 以内に応答がありません`)),
      ms,
    );
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

async function withRetry(fn, { attempts = 3, baseDelayMs = 800, label = 'znca-api' } = {}) {
  let lastErr;
  for (let i = 1; i <= attempts; i++) {
    try {
      return await fn();
    } catch (e) {
      lastErr = e;
      const msg = String((e && e.message) || e);
      // 構造（ステータス / body の error）を優先して判定し、無ければ文字列で拾う（#399）。
      const status = extractStatus(e, msg);
      const upstream = extractUpstreamError(e, msg);
      const transient =
        (typeof status === 'number' && status >= 500 && status < 600) ||
        (typeof upstream === 'string' && /timeout|unavailable/i.test(upstream)) ||
        /Non-200 status code:\s*5\d\d|timeout|ETIMEDOUT|ECONNRESET|ECONNREFUSED|EAI_AGAIN|ENOTFOUND|socket hang up|fetch failed|network/i.test(msg);
      if (!transient || i === attempts) throw e;
      const wait = baseDelayMs * i;
      process.stderr.write(`[${label}] 一時エラー (${i}/${attempts}): ${msg} — ${wait}ms 後に再試行\n`);
      await new Promise((r) => setTimeout(r, wait));
    }
  }
  throw lastErr;
}

async function cmdSetup([sessionToken, dataDir]) {
  if (!sessionToken || !dataDir) {
    throw new Error('usage: setup <session_token> <data_dir>');
  }

  const nsid = parseJwtSub(sessionToken);

  const storage = await initStorage(dataDir);
  await storage.setItem('SelectedUser', nsid);
  await storage.setItem('NintendoAccountToken.' + nsid, sessionToken);

  respond({ ok: true, nsid });
}

/**
 * get_bullet_token <data_dir>
 * SplatNet3 の bulletToken を取得して返す。
 */
async function cmdGetBulletToken([dataDir]) {
  if (!dataDir) throw new Error('usage: get_bullet_token <data_dir>');

  const storage = await initStorage(dataDir);
  const nsid = await storage.getItem('SelectedUser');
  if (!nsid) throw new Error('未ログインです（先に setup を実行してください）');

  const sessionToken = await storage.getItem('NintendoAccountToken.' + nsid);
  if (!sessionToken) throw new Error('session_token が見つかりません');

  // キャッシュに当たるのか、認証をやり直すのかを**先に**出す（#611）。
  // 区別が付かないと「なぜ枠が減るのか」がストレージを掘るまで分からない。
  // 実際、3 分間に 4 回も再認証していたのに気付けなかった。
  const state = await tokenState(storage, sessionToken);
  process.stderr.write(`bulletToken を取得中... (${state})\n`);

  // 🔴 認証しに行くと分かっているなら、**枠を先に見る**（#615）。
  // 尽きているのに試すと「認証します」→「失敗」と見せるだけで、
  // 何が起きたかも、いつ再開できるかも伝わらない。呼ぶ前なら両方言える。
  if (state.includes('認証します')) {
    const budget = await tightestBudget(storage, nsid);
    if (budget.remaining === 0) {
      const when = budget.freeAt ? `${hhmm(budget.freeAt)} 以降に` : 'しばらくしてから';
      throw new Error(
        `認証の回数制限に達しています（${budget.key}・1 時間に ${AUTH_LIMIT_REQUESTS} 回まで）。` +
          `${when}再試行できます。\n` +
          '失敗した認証も回数に数えられます。上流が不安定な時間帯が続くと枠を使い切ります。',
      );
    }
  }
  // splatnet3.js は coral の top-level await を含むため、ここで動的インポート
  const { getBulletToken } = await import('./node_modules/nxapi/dist/common/auth/splatnet3.js');

  // 🔴 **ここはリトライしない**（#596）。
  // nxapi は認証を 1 時間に 4 回までに制限している（LIMIT_REQUESTS / LIMIT_PERIOD）。
  // サイドカーは端末から起動されないので制限は有効側に倒れる。
  // 一方 bulletToken は約 2 時間もち、nxapi がキャッシュするので、
  // 正常なら 1 時間に 1 回も取り直さない。
  // それでも枠を使い切っていたのは、**失敗のたびに認証をやり直していた**ため。
  // 再試行しても失効していなければキャッシュが返るだけで、状況は改善せず枠だけ減る。
  // 一時エラーには**枠を見ながら 1 回だけ**再試行する（下の authWithBudget を参照）。
  let data;
  try {
    ({ data } = await authWithBudget(
      () => withTimeout(
        getBulletToken(storage, sessionToken, undefined, true),
        AUTH_TIMEOUT_MS,
        'get_bullet_token',
      ),
      storage,
      nsid,
    ));
  } catch (e) {
    e.message = await withAuthContext(storage, nsid, e.message);
    throw e;
  }

  respond({
    ok: true,
    bullet_token: data.bullet_token.bulletToken,
    country: data.country,
    language: data.bullet_token.lang,
  });
}

/**
 * check_login <data_dir>
 * ストレージにログイン情報があるか確認する。
 */
async function cmdCheckLogin([dataDir]) {
  if (!dataDir) throw new Error('usage: check_login <data_dir>');

  const storage = await initStorage(dataDir);
  const nsid = await storage.getItem('SelectedUser');
  if (!nsid) {
    respond({ ok: true, logged_in: false });
    return;
  }
  const token = await storage.getItem('NintendoAccountToken.' + nsid);
  respond({ ok: true, logged_in: !!token, nsid });
}

/**
 * weapon_records <data_dir>
 * SplatNet 3 の WeaponRecordQuery を実行し、レスポンス全体を返す。
 *
 * レスポンスの形 (data.weaponRecords.nodes[]):
 *   { name, image2d{url}, image3d{url}, weaponId, stats{level,paint,win,vibes,...},
 *     subWeapon{name,image{url}}, specialWeapon{name,image{url}}, weaponCategory{...} }
 *
 * 武器熟練度 (stats.level)・通算勝利数 (stats.win)・総塗りポイント (stats.paint) はここで取れる。
 * ブキチャレパワー / ビッグラン熟練度は WeaponRecordQuery には含まれない（別クエリの領域）。
 */
async function cmdWeaponRecords([dataDir]) {
  if (!dataDir) throw new Error('usage: weapon_records <data_dir>');

  const storage = await initStorage(dataDir);
  const nsid = await storage.getItem('SelectedUser');
  if (!nsid) throw new Error('未ログインです（先に setup を実行してください）');

  const sessionToken = await storage.getItem('NintendoAccountToken.' + nsid);
  if (!sessionToken) throw new Error('session_token が見つかりません');

  process.stderr.write(
    `WeaponRecordQuery を実行中... (${await tokenState(storage, sessionToken)})\n`,
  );
  const { getBulletToken } = await import('./node_modules/nxapi/dist/common/auth/splatnet3.js');
  // 第 4 引数 allow_fetch_token=true により bullet_token が無ければ自動取得する。
  // 🔴 認証はリトライしない（#596）。理由は cmdGetBulletToken のコメントを参照。
  let splatnet;
  try {
    ({ splatnet } = await withTimeout(
      getBulletToken(storage, sessionToken, undefined, true),
      AUTH_TIMEOUT_MS,
      'get_bullet_token',
    ));
  } catch (e) {
    e.message = await withAuthContext(storage, nsid, e.message);
    throw e;
  }

  // GraphQL の呼び出し側は一時エラーで再試行してよい（認証枠を消費しない）。
  const result = await withRetry(
    () => withTimeout(splatnet.getWeaponRecords(), NETWORK_TIMEOUT_MS, 'weapon_records'),
    { label: 'weapon_records' },
  );
  // result は { data, ... }。data.weaponRecords.nodes が本体。
  respond({ ok: true, data: result.data });
}

// ── ユーティリティ ─────────────────────────────────────────

function respond(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

/** nxapi の認証レート制限（`common/auth/util.js`）。1 時間に 4 回まで。 */
const AUTH_LIMIT_REQUESTS = 4;
const AUTH_LIMIT_PERIOD_MS = 60 * 60 * 1000;

/**
 * 認証の残り枠と回復時刻の説明を作る（#596）。
 *
 * `Too many attempts to authenticate` だけでは、いつ再試行できるのか分からない。
 * nxapi は試行時刻を `RateLimitAttempts-<key>.<user>` に残しているので、そこから出す。
 * 取れなければ null（説明が付かないだけで、元のエラーは失われない）。
 */
/** 認証の再試行までの待ち時間（ミリ秒）。znca-api が落ち着く猶予を取る。 */
const AUTH_RETRY_DELAY_MS = 5000;

/**
 * 認証を実行し、**上流の一時障害のときだけ枠を見て 1 回だけ**やり直す（#613）。
 *
 * # なぜ「1 回だけ」で「枠を見て」なのか
 *
 * znca-api は断続的に `500 {"error":"timeout"}` を返す（#272）。上流の一時障害なので
 * やり直せば通ることが多い。
 *
 * 一方、#596 では**失敗のたびに 3 回やり直して枠を使い切っていた**。
 * nxapi は認証を種類ごとに 1 時間 4 回までに制限している。
 * 実データでは、この失敗で `coral` と `na` の枠が 1 つずつ減っていた。
 *
 * どちらの失敗も「再試行が多すぎる / 少なすぎる」の両極だった。
 * **残り枠が 2 つ以上あるときに限り 1 回だけ**やり直す、が両方を避ける線。
 */
async function authWithBudget(run, storage, nsid) {
  const started = Date.now();
  try {
    return await run();
  } catch (e) {
    const msg = String((e && e.message) || e);
    const status = extractStatus(e, msg);
    const upstream = extractUpstreamError(e, msg);
    const transient =
      (typeof status === 'number' && status >= 500 && status < 600) ||
      (typeof upstream === 'string' && /timeout|unavailable/i.test(upstream));
    const elapsed = Math.round((Date.now() - started) / 1000);

    if (!transient) {
      process.stderr.write(`認証に失敗（${elapsed} 秒）: やり直しても直らない種類のエラーです\n`);
      throw e;
    }
    // 枠を使い切ると 1 時間締め出される。余裕が無いなら再試行しない。
    const room = (await tightestBudget(storage, nsid)).remaining;
    if (room < 2) {
      process.stderr.write(
        `上流の一時障害（${elapsed} 秒）。残り枠が ${room} なので再試行しません\n`,
      );
      throw e;
    }
    process.stderr.write(
      `上流の一時障害（${elapsed} 秒）: ${msg}\n` +
        `${AUTH_RETRY_DELAY_MS / 1000} 秒後に 1 回だけやり直します（残り枠 ${room}）\n`,
    );
    await new Promise((r) => setTimeout(r, AUTH_RETRY_DELAY_MS));
    return await run();
  }
}

/**
 * nxapi が枠を分けている認証の種類（#615）。
 *
 * **それぞれ独立に 1 時間 4 回。** どれか 1 つでも尽きれば認証は通らない。
 * `splatnet3` だけを見ていたため、`coral` が尽きたときに
 * 「空きがある」と誤判定して案内が出ていなかった。
 */
const AUTH_LIMIT_KEYS = ['na', 'coral', 'splatnet3'];

/** 種類ごとの残り枠と回復時刻。読めない種類は「残り 0」に倒す（安全側）。 */
async function authBudget(storage, nsid, key) {
  try {
    const raw = (await storage.getItem(`RateLimitAttempts-${key}.${nsid}`)) ?? [];
    const recent = raw
      .map((a) => (typeof a === 'number' ? a : a && a.time))
      .filter((t) => typeof t === 'number' && t >= Date.now() - AUTH_LIMIT_PERIOD_MS)
      .sort((a, b) => a - b);
    return {
      key,
      used: recent.length,
      remaining: Math.max(0, AUTH_LIMIT_REQUESTS - recent.length),
      // 尽きているときだけ意味を持つ。いちばん古い試行が 1 時間で流れた時刻。
      freeAt: recent.length ? new Date(recent[0] + AUTH_LIMIT_PERIOD_MS) : null,
    };
  } catch {
    return { key, used: AUTH_LIMIT_REQUESTS, remaining: 0, freeAt: null };
  }
}

/** すべての種類の枠。 */
async function authBudgets(storage, nsid) {
  return await Promise.all(AUTH_LIMIT_KEYS.map((k) => authBudget(storage, nsid, k)));
}

/** いちばん逼迫している枠（残りが最小のもの）。 */
async function tightestBudget(storage, nsid) {
  const all = await authBudgets(storage, nsid);
  return all.reduce((a, b) => (b.remaining < a.remaining ? b : a));
}

function hhmm(d) {
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

/**
 * キャッシュされた bulletToken の状態を一言で返す（#611）。
 *
 * 「キャッシュに当たる」のか「認証をやり直す」のかが分からないと、
 * 枠が減る理由を追えない。実際 3 分間に 4 回も再認証していたのに気付けなかった。
 *
 * nxapi はキー `BulletToken.<session_token>` に `expires_at`（ミリ秒）付きで持つ。
 */
async function tokenState(storage, sessionToken) {
  try {
    const t = await storage.getItem('BulletToken.' + sessionToken);
    if (!t || !t.expires_at) return 'キャッシュなし → 認証します';
    const left = Math.round((t.expires_at - Date.now()) / 60000);
    return left > 0 ? `キャッシュ有効（残り ${left} 分）` : 'キャッシュ失効 → 認証します';
  } catch {
    return 'キャッシュ状態は不明';
  }
}

/**
 * 認証の失敗メッセージに文脈を足す。
 *
 * 足すのは 2 つ。
 *
 * 1. **どこで詰まったか。** タイムアウトは上流（nxapi-znca-api / 任天堂）が
 *    応答を返さなかったということで、こちらの設定や回数制限とは関係がない。
 *    区別しないと「自分の使いすぎだ」と誤解される。
 * 2. **認証の残り枠。** 上限に達しているときだけ「◯時以降に」と言う。
 */
async function withAuthContext(storage, nsid, message) {
  const parts = [message];
    if (/timeout|ETIMEDOUT/i.test(message)) {
    parts.push(
      '任天堂側または nxapi-znca-api が応答しませんでした。' +
        'こちらの設定や回数制限の問題ではないので、少し待ってからもう一度お試しください。',
    );
  }
  if (/upgrade required/i.test(message)) {
    parts.push(
      '任天堂が Nintendo Switch Online アプリの版を上げたため、いまの認証では通れません。' +
        'ログインし直しても、すぐ再試行しても直りません。認証の回数だけ減ります。',
    );
  }
  const hint = await rateLimitHint(storage, nsid, message);
  if (hint) parts.push(hint);
  return parts.join('\n');
}

async function rateLimitHint(storage, nsid, message = '') {
  try {
    // 🔴 **エラーが名指ししている種類を優先する**（#615）。
    // `Too many attempts to authenticate (coral)` のように括弧で入っている。
    // splatnet3 決め打ちだったので、coral が尽きたとき「空きあり」と誤判定し、
    // 案内が 1 行も出なかった。
    const named = message.match(/Too many attempts to authenticate \(([a-z0-9_]+)\)/i);
    const budget = named && AUTH_LIMIT_KEYS.includes(named[1])
      ? await authBudget(storage, nsid, named[1])
      : await tightestBudget(storage, nsid);

    if (budget.remaining > 0) {
      // 枠が残っているなら「◯時以降」とは言わない。待つ必要が無いのに待たせてしまう。
      if (budget.used === 0) return null;
      return `直近 1 時間の認証は ${budget.used} 回です（上限 ${AUTH_LIMIT_REQUESTS} 回）。あと ${budget.remaining} 回試せます。`;
    }
    const when = budget.freeAt ? `${hhmm(budget.freeAt)} 以降に` : 'しばらくしてから';
    return `直近 1 時間の認証が上限（${AUTH_LIMIT_REQUESTS} 回・任天堂側の負荷を避けるための nxapi の制限）に達しました。${when}再試行できます。`;
  } catch {
    return null;
  }
}

/**
 * 例外を「構造を保った失敗レスポンス」に変換する（#399）。
 *
 * nxapi の `ErrorResponse` は元の `Response`（`.response`）とパース済み body（`.data`）を
 * 保持しているので、そこから **HTTP ステータス** と **body の `error` フィールド** を取り出す。
 * znca-api の 500 + `{"error":"timeout"}` を「トークン失効」と誤診しないために、
 * 分類に必要な材料をここで落とさず Rust へ渡すのが要点。
 */
function describeFailure(e) {
  const error = String((e && e.message) || e);
  return {
    ok: false,
    error,
    status: extractStatus(e, error),
    upstream_error: extractUpstreamError(e, error),
  };
}

/** HTTP ステータスを取り出す。構造が無ければ message から拾う（保険）。 */
function extractStatus(e, message) {
  const status = e?.response?.status ?? e?.status;
  if (typeof status === 'number' && Number.isFinite(status)) return status;
  const m = /Non-200 status code:\s*(\d{3})/.exec(message);
  return m ? Number(m[1]) : null;
}

/** レスポンス body の `error` フィールド（znca-api なら "timeout" / "invalid_grant" 等）。 */
function extractUpstreamError(e, message) {
  if (e?.data && typeof e.data === 'object' && typeof e.data.error === 'string') {
    return e.data.error;
  }
  if (typeof e?.body === 'string' && e.body) {
    try {
      const parsed = JSON.parse(e.body);
      if (parsed && typeof parsed.error === 'string') return parsed.error;
    } catch { /* JSON でなければ無視 */ }
  }
  const m = /"error"\s*:\s*"([^"]*)"/.exec(message);
  return m ? m[1] : null;
}

function parseJwtSub(jwt) {
  const parts = jwt.split('.');
  if (parts.length < 2) throw new Error('session_token が JWT 形式ではありません');
  const payload = JSON.parse(Buffer.from(parts[1], 'base64url').toString('utf-8'));
  if (!payload.sub) throw new Error('JWT payload に sub フィールドがありません');
  return payload.sub;
}
