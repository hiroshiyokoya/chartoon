import { useState, useEffect, useRef } from 'react'
import { useTranslation } from 'react-i18next'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { Dashboard } from './components/Dashboard'
import { BattleLog } from './components/BattleLog'
import { FilterBar } from './components/FilterBar'
import { WeaponBook } from './components/WeaponBook'
import { StageBook } from './components/StageBook'
import { AiAnalysis } from './components/AiAnalysis'
import { EnvAnalysis } from './components/EnvAnalysis'
import { Settings } from './components/Settings'
import { About } from './components/About'
import { GearSection } from './gear/GearSection'
import { ViewToggle } from './components/ViewToggle'
import type { ViewToggleOption } from './components/ViewToggle'
import type { Tab, BattlesView, SettingsTab, AppSettings, Filters } from './types'
import { DEFAULT_FILTERS } from './types'
import { loadViewPrefs, saveViewPrefs } from './utils/viewPrefs'
import { initAppSettings } from './utils/appSettings'
import { useNotify, parseFetchError } from './utils/notify'
import {
  SETTINGS_KEY,
  LAST_FETCHED_KEY,
  LAST_WEAPONS_FETCH_KEY as LAST_WEAPONS_FETCH_K,
  lsGet,
  mirrorToStore,
  importFromStoreIfNewer,
} from './utils/settingsStore'
import './App.css'

initAppSettings()

const DEFAULT_SETTINGS: AppSettings = {
  ai: { provider: 'openai', apiKey: '', model: '' },
  autoFetchEnabled: false,
  /**
   * 自動取得の間隔（分）。既定は 1 時間（#616）。
   *
   * 🔴 **長くするほど不利になる。** SplatNet の bulletToken は約 2 時間で失効し、
   * 失効していると取得のたびに認証をやり直す。認証は znca-api 経由で 12 往復ほどあり、
   * ここが最も失敗しやすい（`tools/nxapi-wrapper/wrapper.js` の AUTH_TIMEOUT_MS を参照）。
   *
   * | 間隔 | トークン | 認証 |
   * |---|---|---|
   * | 120 分 | 取りに行くたびちょうど失効 | 毎回やり直し |
   * | 60 分 | 2 回に 1 回は生きている | 半分で済む |
   *
   * さらに nxapi は認証を種類ごとに 1 時間 4 回までに制限しており、
   * **失敗した認証も回数に数える**。上流が不安定な時間帯に長い間隔で当たると、
   * 数回の失敗で 1 時間締め出される。トークンの寿命より短い間隔にしておくと、
   * キャッシュに当たる回数が増えて認証そのものが減る。
   */
  autoFetchIntervalMin: 60,
  statink: { apiKey: '', autoUpload: false, screenName: null },
}
/** ブキマスターを再取得するインターバル(ミリ秒)。24 時間。 */
const WEAPONS_FETCH_INTERVAL_MS = 24 * 60 * 60 * 1000

/** 「バトル」タブ内のビュー切替(#296)。 */

function formatLastFetched(raw: string, lang: string): string {
  const ms = Date.parse(raw)
  const d = Number.isFinite(ms) ? new Date(ms) : null
  const locale = lang.startsWith('en') ? 'en-US' : 'ja-JP'
  if (d) {
    return d.toLocaleString(locale, { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' })
  }
  return raw
}

function loadSettings(): AppSettings {
  try {
    const raw = lsGet(SETTINGS_KEY)
    if (!raw) return DEFAULT_SETTINGS
    const saved = JSON.parse(raw)
    return {
      ...DEFAULT_SETTINGS,
      ...saved,
      statink: { ...DEFAULT_SETTINGS.statink, ...(saved.statink ?? {}) },
    }
  } catch {
    return DEFAULT_SETTINGS
  }
}

export default function App() {
  const { t, i18n } = useTranslation()
  const battlesViews: readonly ViewToggleOption<BattlesView>[] = [
    { key: 'dashboard', label: t('battles.dashboard'), icon: '📊' },
    { key: 'list',      label: t('battles.list'),      icon: '📋' },
  ]
  const [tab, setTab] = useState<Tab>('battles')
  // 設定タブへ飛ばすときに開くサブタブの指定(#428)。nonce を毎回上げることで、
  // 既に設定タブにいても・同じサブタブでも Settings 側の useEffect が発火する。
  const [settingsFocus, setSettingsFocus] = useState<{ tab: SettingsTab; nonce: number } | null>(null)
  /** 設定タブを開き、任意で特定サブタブに着地させる。未指定なら前回の選択のまま。 */
  const openSettings = (subTab?: SettingsTab) => {
    setTab('settings')
    if (subTab) setSettingsFocus(f => ({ tab: subTab, nonce: (f?.nonce ?? 0) + 1 }))
  }
  // 「バトル」タブ内のビュー。前回選択を localStorage から復元する(#296)。
  const [battlesView, setBattlesViewState] = useState<BattlesView>(() => loadViewPrefs().battles)
  const [settings, setSettings] = useState<AppSettings>(loadSettings)
  const [loginVersion, setLoginVersion] = useState(0)
  const [filters, setFilters] = useState<Filters>(DEFAULT_FILTERS)
  const [showAbout, setShowAbout] = useState(false)
  const [lastFetchedAt, setLastFetchedAt] = useState<string | null>(
    () => lsGet(LAST_FETCHED_KEY)
  )
  const { notify } = useNotify()
  // バトル/ブキ/ステージ上部の絞り込み＋見出しを sticky にするとき、
  // ブキ・ステージの見出し行が FilterBar の下に来るよう高さを測る(#450)。
  const stickyChromeRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    const el = stickyChromeRef.current
    if (!el) {
      document.documentElement.style.removeProperty('--content-sticky-chrome-height')
      return
    }
    const update = () => {
      const h = Math.ceil(el.getBoundingClientRect().height)
      document.documentElement.style.setProperty('--content-sticky-chrome-height', `${h}px`)
    }
    update()
    const ro = new ResizeObserver(update)
    ro.observe(el)
    return () => {
      ro.disconnect()
      document.documentElement.style.removeProperty('--content-sticky-chrome-height')
    }
  }, [tab, battlesView])

  // 起動時: store(settings.json)が localStorage より新しければ取り込む(識別子変更後の復元経路 #241)。
  // 取り込んだら設定 state・派生 state を localStorage の最新値で更新する。
  useEffect(() => {
    let cancelled = false
    importFromStoreIfNewer().then(imported => {
      if (imported && !cancelled) {
        setSettings(loadSettings())
        setLastFetchedAt(lsGet(LAST_FETCHED_KEY))
        // テーマは module ロード時に initAppSettings() で既定適用済みなので、
        // 取り込み後の localStorage 値で再適用する。
        initAppSettings()
      }
    })
    return () => { cancelled = true }
  }, [])

  // 起動直後・設定変更時・store 取り込み後に、自動取得と stat.ink 設定を
  // バックエンドへ同期する(#322)。以前は設定タブ(Settings)を開いたときにしか
  // 同期されず、開かずに閉じると「自動取得 ON でもトレイに残らず終了」「スケジューラーが
  // 動かない」状態になっていた。settings は本コンポーネントが持つので、mount 時
  // (loadSettings の値)と import 後(setSettings)で必ず同期される。
  useEffect(() => {
    invoke('set_scheduler_config', {
      enabled: settings.autoFetchEnabled,
      intervalMin: settings.autoFetchIntervalMin,
    }).catch(console.error)
    invoke('set_statink_config', {
      autoUpload: settings.statink.autoUpload,
      apiKey: settings.statink.apiKey,
    }).catch(console.error)
  }, [
    settings.autoFetchEnabled,
    settings.autoFetchIntervalMin,
    settings.statink.autoUpload,
    settings.statink.apiKey,
  ])

  /** フェッチ失敗を共通の流儀でトーストに変換する。
   *  ・未ログイン → 設定タブへ誘導するボタン付き
   *  ・期限切れ・ネットワーク → 再試行ボタン付き
   *  ・それ以外 → エラー詳細を出して再試行ボタン付き */
  function reportFetchError(err: unknown, retry?: () => void) {
    const fe = parseFetchError(err)
    const action =
      fe.hint === 'settings' ? { label: t('common.openSettings'), onClick: () => openSettings('link') } :
      fe.hint === 'wait'     ? undefined :
      retry                  ? { label: t('common.retry'),     onClick: retry } :
      undefined
    // 未ログイン・外部サービスの一時障害・NSO 版待ちは「アプリが壊れた」ではないので warning 止まり。
    const soft = fe.kind === 'not_logged_in' || fe.kind === 'upstream_unavailable' || fe.kind === 'coral_upgrade'
    notify({ kind: soft ? 'warning' : 'error', title: fe.title, message: fe.message, action, durationMs: 0 })
  }

  // 認証完了後にデータ取得を実行
  useEffect(() => {
    if (loginVersion > 0) {
      invoke('fetch_battles_full').catch(err => reportFetchError(err, () => invoke('fetch_battles_full').catch(e => reportFetchError(e))))
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loginVersion])

  // バトル取得中フラグ。サイドバーの「最新データを取得」ボタン、Dashboard 空状態ボタン等が参照する。
  // 手動ボタン経由は handleFetchFull が直接 set。
  // 起動時取得・スケジューラ取得は Rust 側の fetch_start/fetch_finish イベントで設定される。
  const [fetching, setFetching] = useState(false)

  useEffect(() => {
    const unlistenPromise = listen('fetch_complete', () => {
      const nowIso = new Date().toISOString()
      setLastFetchedAt(nowIso)
      localStorage.setItem(LAST_FETCHED_KEY, nowIso)
      void mirrorToStore()
    })
    return () => { unlistenPromise.then(fn => fn()) }
  }, [])

  // 起動時取得・スケジューラ取得でも「取得中…」を出すため、Rust 側の fetch_start/fetch_finish を listen し、
  // さらに mount 時点で進行中なら即座に取得中表示にする(起動時取得は React マウント前に始まり得る race 対策)。
  useEffect(() => {
    let disposed  = false
    // 実イベントを一度でも受け取ったら、あとから解決する is_fetching スナップショットで
    // 上書きしない(古い値でのクロバー防止)。
    let sawEvent  = false
    const startP  = listen('fetch_start',  () => { sawEvent = true; setFetching(true) })
    const finishP = listen('fetch_finish', () => { sawEvent = true; setFetching(false) })
    // listener 登録が完了してから is_fetching を読む。こうすると「登録の隙間に
    // fetch_finish を取りこぼして『取得中』が残る」race を塞げる(#402 任意4):
    // 登録前に emit された finish は、登録後のこの再確認が false を読んで解除する。
    Promise.all([startP, finishP]).then(() => {
      invoke<boolean>('is_fetching')
        .then(v => { if (!disposed && !sawEvent) setFetching(v) })
        .catch(() => {})
    })
    return () => {
      disposed = true
      startP.then(fn => fn())
      finishP.then(fn => fn())
    }
  }, [])

  // stat.ink 自動アップロードの失敗をユーザーに見せる(#402 必須2)。
  // 以前は warn ログに畳んで握りつぶしていたため「なぜか送られない」だけが残っていた。
  // 未送信バトルは DB に残り次回自動で再送されるので、データは失われない旨も伝える。
  useEffect(() => {
    const unlistenPromise = listen<string>('statink_upload_failed', (event) => {
      const fe     = parseFetchError(event.payload)
      const isAuth = fe.kind === 'auth_expired'
      notify({
        kind:    'warning',
        title:   isAuth ? t('notify.statinkAuthTitle') : t('notify.statinkFailTitle'),
        message: isAuth
          ? t('notify.statinkAuthMessage')
          : t('notify.statinkFailMessage'),
        durationMs: 8000,
      })
    })
    return () => { unlistenPromise.then(fn => fn()) }
  }, [notify])

  // stat.ink の screen_name を初回アップロード時に自動取得して設定に保存
  useEffect(() => {
    const unlistenPromise = listen<string>('statink_screen_name_detected', (event) => {
      const name = event.payload
      setSettings(prev => {
        if (prev.statink.screenName === name) return prev
        const next = { ...prev, statink: { ...prev.statink, screenName: name } }
        localStorage.setItem(SETTINGS_KEY, JSON.stringify(next))
        void mirrorToStore()
        return next
      })
    })
    return () => { unlistenPromise.then(fn => fn()) }
  }, [])

  // 起動時、screen_name が未取得 & API キーありなら既存アップロード済みバトルから逆引き
  useEffect(() => {
    if (settings.statink.screenName) return
    if (!settings.statink.apiKey) return
    invoke<string | null>('detect_statink_screen_name', { apiKey: settings.statink.apiKey }).then(name => {
      if (!name) return
      setSettings(prev => {
        if (prev.statink.screenName === name) return prev
        const next = { ...prev, statink: { ...prev.statink, screenName: name } }
        localStorage.setItem(SETTINGS_KEY, JSON.stringify(next))
        void mirrorToStore()
        return next
      })
    }).catch(console.error)
  }, [settings.statink.apiKey])

  // 起動時にブキマスターの自動チェック。前回取得から 24h 経過していれば裏で再取得。
  // 失敗してもサイレント(UI ブロックしない)。
  useEffect(() => {
    const last = Number(localStorage.getItem(LAST_WEAPONS_FETCH_K) ?? 0)
    if (Date.now() - last < WEAPONS_FETCH_INTERVAL_MS) return
    invoke<number>('fetch_weapons')
      .then(count => {
        localStorage.setItem(LAST_WEAPONS_FETCH_K, String(Date.now()))
        console.log(`[startup] ブキマスター ${count} 件取得`)
      })
      .catch(err => console.warn('[startup] ブキマスター取得失敗:', err))
  }, [loginVersion])

  useEffect(() => {
    const unlistenPromise = listen<string>('deep-link-received', async (event) => {
      const url = event.payload
      if (url.startsWith('npf71b963c1b7b6d119://')) {
        try {
          await invoke('handle_auth_redirect', { url })
          setLoginVersion(v => v + 1)
        } catch (e) {
          console.error('認証リダイレクト処理失敗:', e)
        }
      }
    })
    return () => { unlistenPromise.then(fn => fn()) }
  }, [])

  // 移行が発火した初回起動時、旧バージョン(v0.7 以前の chartoon / geartoon)の
  // アンインストールを案内する。別 identifier のため single-instance では共存を防げず、
  // 旧版と同時起動してしまうのを案内で解消する(#279)。
  useEffect(() => {
    const unlistenPromise = listen('migration_completed', () => {
      notify({
        kind: 'info',
        title: t('notify.migrationTitle'),
        message: t('notify.migrationMessage'),
        durationMs: 0,
      })
    })
    return () => { unlistenPromise.then(fn => fn()) }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  function saveSettings(s: AppSettings) {
    setSettings(s)
    localStorage.setItem(SETTINGS_KEY, JSON.stringify(s))
    // store(settings.json)へミラー。識別子変更で localStorage が失われても復元できるように(#241)。
    void mirrorToStore()
  }

  /** ビュー切替。選択を localStorage に保存して次回起動時にも復元する(#296)。 */
  function setBattlesView(next: BattlesView) {
    setBattlesViewState(next)
    saveViewPrefs({ ...loadViewPrefs(), battles: next })
  }

  // サイドバーから呼ばれる「最新データを取得」処理(バトル → ギア best-effort)
  async function handleFetchFull() {
    if (fetching) return
    setFetching(true)
    try {
      await invoke('fetch_battles_full')
      // 統合: サイドバーの取得でバトルに続けてギアも取得する。
      // ギアの失敗はバトル取得と独立に通知し、バトル取得の成功は保つ。
      try {
        await invoke('fetch_gear_full')
      } catch (gearErr) {
        console.error('gear fetch failed:', gearErr)
        notify({ kind: 'warning', title: t('notify.gearFailTitle'), message: t('notify.gearFailMessage'), durationMs: 6000 })
      }
    } catch (e) {
      reportFetchError(e, handleFetchFull)
    } finally {
      setFetching(false)
    }
  }

  return (
    <div className="app">
      <nav className="sidebar">
        <button className="logo" onClick={() => setShowAbout(true)} aria-label={t('nav.aboutAria')}>
          <img src="/splabo-logo.png" alt="splabo" />
        </button>
        {/* 並び順: バトル → ブキ → ステージ → ギア → 環境分析 → AI分析 → 設定
            扱う対象をそのまま名前にする。旧アプリ由来の 2 つ(バトル / ギア)は
            メニューでだけ旧名を併記する(#419)。 */}
        <NavItem id="battles"   icon="⚔️" label={t('nav.battles')} legacyName="chartoon" active={tab} onClick={setTab} />
        <NavItem id="weapons"   icon="🔫" label={t('nav.weapons')}           active={tab} onClick={setTab} />
        <NavItem id="stages"    icon="🗺️" label={t('nav.stages')}       active={tab} onClick={setTab} />
        <NavItem id="gear"      icon="👕" label={t('nav.gear')} legacyName="geartoon" active={tab} onClick={setTab} />
        <NavItem id="env"       icon="🌍" label={t('nav.env')}       active={tab} onClick={setTab} />
        <NavItem id="ai"        icon="🧙" label={t('nav.ai')}         active={tab} onClick={setTab} />
        <NavItem id="settings"  icon="⚙️" label={t('nav.settings')}           active={tab} onClick={setTab} />
        <button
          className="btn-primary sidebar-fetch-btn"
          onClick={handleFetchFull}
          disabled={fetching}
          title={t('nav.fetchTitle')}
        >
          {fetching ? t('nav.fetching') : t('nav.fetch')}
        </button>
        <div className="sidebar-last-fetched">
          {lastFetchedAt
            ? t('nav.lastFetched', { time: formatLastFetched(lastFetchedAt, i18n.language) })
            : t('nav.neverFetched')}
        </div>
      </nav>

      <main className="content">
        {tab === 'battles' && (
          <>
            {/* ブキ・ステージ(FilterBar → 見出し行内の ViewToggle)と並びを揃えるため、
                切替は絞り込みの下に置く。見出しはダッシュボード / 一覧の両方に共通なので、
                各ビューの中ではなくここに 1 つだけ置く。
                絞り込み＋見出しはスクロール中も常時表示する(#450)。 */}
            <div className="content-sticky-chrome" ref={stickyChromeRef}>
              <FilterBar filters={filters} onChange={setFilters} />
              <div className="battles-header">
                <h2>{t('battles.title')}</h2>
                <ViewToggle
                  options={battlesViews}
                  value={battlesView}
                  onChange={setBattlesView}
                  ariaLabel={t('battles.viewAria')}
                />
              </div>
            </div>
            {battlesView === 'dashboard' ? (
              <Dashboard
                filters={filters}
                onFetchRequest={handleFetchFull}
                onOpenSettings={() => openSettings('link')}
                fetching={fetching}
              />
            ) : (
              <BattleLog filters={filters} statinkScreenName={settings.statink.screenName} />
            )}
          </>
        )}
        {/* ブキ・ステージタブ(#298): 期間・ロビー・ルール・結果を集計に反映する。
            ブキ/ステージ絞り込みは自己言及的なので hideTargetFilters で隠す。
            FilterBar は sticky。見出し行は各 Book 内で chrome 高さ分ずらして sticky(#450)。 */}
        {(tab === 'weapons' || tab === 'stages') && (
          <div className="content-sticky-chrome" ref={stickyChromeRef}>
            <FilterBar filters={filters} onChange={setFilters} hideTargetFilters />
          </div>
        )}
        {tab === 'weapons'   && <WeaponBook filters={filters} />}
        {tab === 'stages'    && <StageBook  filters={filters} />}
        {tab === 'env'       && <EnvAnalysis />}
        {tab === 'gear'      && <GearSection />}
        {tab === 'ai' && <AiAnalysis settings={settings} />}
        {tab === 'settings' && <Settings settings={settings} onSave={saveSettings} loginVersion={loginVersion} focus={settingsFocus} />}
      </main>

      {showAbout && <About onClose={() => setShowAbout(false)} />}
    </div>
  )
}

function NavItem({ id, icon, label, legacyName, active, onClick }: { id: Tab; icon: string; label: string; legacyName?: string; active: Tab; onClick: (t: Tab) => void }) {
  return (
    <button className={`nav-item ${active === id ? 'active' : ''}`} onClick={() => onClick(id)}>
      <span className="nav-item-icon" aria-hidden="true">{icon}</span>
      <span className="nav-item-label">
        {label}
        {/* 旧アプリ由来のタブ(バトル＝Chartoon / ギア＝Geartoon)に添える旧アプリ名。
            タブの中は絞り込み窓をブキ・ステージと揃えている都合で置き場所が無いため、メニューに一本化した。
            aria からは外す(読み上げでは日本語名だけで十分)。 */}
        {legacyName && <span className="nav-item-legacy" aria-hidden="true">({legacyName})</span>}
      </span>
    </button>
  )
}
