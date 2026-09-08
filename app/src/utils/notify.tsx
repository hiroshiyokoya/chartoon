import { createContext, useCallback, useContext, useMemo, useState, type ReactNode } from 'react'
import i18n from '../i18n'

export type NotifyKind = 'error' | 'info' | 'success' | 'warning'

export interface Notice {
  id: number
  kind: NotifyKind
  title: string
  /** 本文。省略可。 */
  message?: string
  /** ms 単位。0 で永続（手動で閉じるまで残る）。デフォルト 6000ms。 */
  durationMs: number
  /** クリックで実行されるアクション（例: 「設定を開く」「再試行」）。 */
  action?: {
    label: string
    onClick: () => void
  }
}

interface NotifyContextValue {
  notices: Notice[]
  notify: (n: Omit<Notice, 'id' | 'durationMs'> & { durationMs?: number }) => number
  dismiss: (id: number) => void
  dismissAll: () => void
}

const NotifyContext = createContext<NotifyContextValue | null>(null)

let nextId = 1

/**
 * アプリ全体で使うグローバル通知（トースト）プロバイダ。
 *
 * `main.tsx` で `<App />` をラップしておくと、子のどこからでも `useNotify()` で呼べる。
 * 通知は配列で持ち、`durationMs` 経過後に自動で消える。`durationMs: 0` で永続。
 */
export function NotifyProvider({ children }: { children: ReactNode }) {
  const [notices, setNotices] = useState<Notice[]>([])

  const dismiss = useCallback((id: number) => {
    setNotices(curr => curr.filter(n => n.id !== id))
  }, [])

  const dismissAll = useCallback(() => setNotices([]), [])

  const notify: NotifyContextValue['notify'] = useCallback((n) => {
    const id         = nextId++
    const durationMs = n.durationMs ?? 6000
    setNotices(curr => [...curr, { ...n, id, durationMs }])
    if (durationMs > 0) {
      window.setTimeout(() => dismiss(id), durationMs)
    }
    return id
  }, [dismiss])

  const value = useMemo<NotifyContextValue>(
    () => ({ notices, notify, dismiss, dismissAll }),
    [notices, notify, dismiss, dismissAll],
  )

  return <NotifyContext.Provider value={value}>{children}</NotifyContext.Provider>
}

/** 通知を発行するための hook。Provider 外で呼ぶとエラー。 */
export function useNotify(): NotifyContextValue {
  const ctx = useContext(NotifyContext)
  if (!ctx) throw new Error('useNotify must be called within <NotifyProvider>')
  return ctx
}

// ---------------------------------------------------------------------------
// エラーパース：Rust から返ってくる String エラーから意味のある分類を取り出す
// ---------------------------------------------------------------------------

export type FetchErrorKind =
  | 'not_logged_in'
  /** 外部サービス（znca-api 等）の一時障害。再ログインしても直らない（#399）。 */
  | 'upstream_unavailable'
  | 'network'
  | 'auth_expired'
  | 'fetch_in_progress'
  | 'gear_fetch_in_progress'
  | 'statink_no_api_key'
  | 'rate_limited'
  | 'coral_upgrade'
  | 'unknown'

export interface FetchError {
  kind:      FetchErrorKind
  title:     string
  message:   string
  /** ユーザー向けに「次にどうすればよいか」のサジェスト。 */
  hint?:     string
}

/** バックエンドが付ける機械可読プリフィクス（`app/src-tauri/src/nxapi.rs` の `FailureKind::code`）。 */
const ERROR_CODE_PREFIX = /^(NOT_LOGGED_IN|UPSTREAM_UNAVAILABLE|AUTH_EXPIRED|NETWORK|FETCH_IN_PROGRESS|GEAR_FETCH_IN_PROGRESS|STATINK_NO_API_KEY|RATE_LIMITED|CORAL_UPGRADE)\s*:\s*/

/**
 * Rust 側のエラー文字列をフロント向けに分類する。
 *
 * 分類は**バックエンドが付けたプリフィクス**で行う。以前は `bullet token|f-token|znca` の
 * 文字列マッチで「認証期限切れ」に倒していたが、znca-api が 500 を返しただけの一時障害でも
 * 再ログインを促してしまい、ユーザーは直らない操作を繰り返すことになっていた（#399）。
 * 「取得に失敗した」という事実ではなく「なぜ失敗したか」で文言を出し分ける。
 *
 * プリフィクスが無い（＝分類できなかった）ものは、憶測で認証エラーにせず素直に扱う。
 */
export function parseFetchError(raw: unknown): FetchError {
  const text = typeof raw === 'string' ? raw : raw instanceof Error ? raw.message : String(raw)
  const code = ERROR_CODE_PREFIX.exec(text)?.[1]
  /** 表示用。機械可読プリフィクスはユーザーに見せない。 */
  const detail = text.replace(ERROR_CODE_PREFIX, '')

  if (code === 'NOT_LOGGED_IN' || text.startsWith('NOT_LOGGED_IN')) {
    return {
      kind:    'not_logged_in',
      title:   i18n.t('errors.notLoggedInTitle'),
      message: i18n.t('errors.notLoggedInMessage'),
      hint:    'settings',
    }
  }

  if (code === 'FETCH_IN_PROGRESS') {
    return {
      kind:    'fetch_in_progress',
      title:   i18n.t('errors.fetchInProgressTitle'),
      message: i18n.t('errors.fetchInProgressMessage'),
    }
  }

  if (code === 'GEAR_FETCH_IN_PROGRESS') {
    return {
      kind:    'gear_fetch_in_progress',
      title:   i18n.t('errors.gearFetchInProgressTitle'),
      message: i18n.t('errors.gearFetchInProgressMessage'),
    }
  }

  if (code === 'STATINK_NO_API_KEY' || text.includes('stat.ink API キーが設定されていません')) {
    return {
      kind:    'statink_no_api_key',
      title:   i18n.t('errors.statinkNoApiKeyTitle'),
      message: i18n.t('errors.statinkNoApiKeyMessage'),
    }
  }

  if (code === 'RATE_LIMITED') {
    return {
      kind:    'rate_limited',
      title:   i18n.t('errors.rateLimitedTitle'),
      message: i18n.t('errors.rateLimitedMessage', { detail }),
      hint:    'retry',
    }
  }

  // 任天堂が NSO の版を上げ、認証側がまだ追いついていない（#765）。
  // 再ログインも再試行も直らない。枠だけ減る。
  if (code === 'CORAL_UPGRADE') {
    return {
      kind:    'coral_upgrade',
      title:   i18n.t('errors.coralUpgradeTitle'),
      message: i18n.t('errors.coralUpgradeMessage'),
      hint:    'wait',
    }
  }

  // 外部サービス（znca-api 等）の一時障害。**再ログインを促さない** — トークンは生きている。
  if (code === 'UPSTREAM_UNAVAILABLE') {
    return {
      kind:    'upstream_unavailable',
      title:   i18n.t('errors.upstreamTitle'),
      message: i18n.t('errors.upstreamMessage', { detail }),
      hint:    'retry',
    }
  }

  // 認証情報の失効（401 / 403 / invalid_grant）。ここだけが再ログイン案内。
  if (code === 'AUTH_EXPIRED') {
    return {
      kind:    'auth_expired',
      title:   i18n.t('errors.authExpiredTitle'),
      message: i18n.t('errors.authExpiredMessage', { detail }),
      hint:    'retry_or_login',
    }
  }

  if (code === 'NETWORK') {
    return {
      kind:    'network',
      title:   i18n.t('errors.networkTitle'),
      message: i18n.t('errors.networkMessage', { detail }),
      hint:    'retry',
    }
  }

  // 分類プリフィクスが付かない経路でも、任天堂の版上げは再ログイン案内にしない（#765）。
  if (/upgrade required/i.test(detail)) {
    return {
      kind:    'coral_upgrade',
      title:   i18n.t('errors.coralUpgradeTitle'),
      message: i18n.t('errors.coralUpgradeMessage'),
      hint:    'wait',
    }
  }

  // 分類プリフィクスが付かない経路（Rust 内部の reqwest エラー等）はメッセージから判断する。
  if (/timed?\s*out|connection|dns|reqwest|tls|certificate|sendrequest|HTTP\s*client/i.test(detail)) {
    return {
      kind:    'network',
      title:   i18n.t('errors.networkTitle'),
      message: i18n.t('errors.networkMessage', { detail }),
      hint:    'retry',
    }
  }

  return {
    kind:    'unknown',
    title:   i18n.t('errors.unknownTitle'),
    message: detail,
  }
}

/**
 * Rust `invoke()` の失敗をインライン表示用の 1 行文字列に変換する（Settings 等）。
 * 機械可読プリフィクス付きエラーは `errors.*` の文言に置き換える。
 */
export function formatInvokeError(raw: unknown): string {
  const fe = parseFetchError(raw)
  if (fe.kind === 'unknown') {
    return fe.message || fe.title
  }
  return fe.message || fe.title
}
