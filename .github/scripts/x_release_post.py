#!/usr/bin/env python3
"""CHANGELOG から X 告知文を組む (#648 / #763)。

GitHub リリース Publish 後に Slack へ流す文面。280 加重字に収める。
非 ASCII は 2、ASCII は 1。投稿本文に URL は入れない（自動投稿のコスト）。
誘導は「ダウンロードはプロフィールからどうぞ。」。箇条書きの上は「更新内容」。
Query 名は出さない。末尾のタグは #スプラトゥーン3 #Splatoon3 #SpLabo で固定。
組み立てたあとで添削し、校正チェックを通す。
"""
from __future__ import annotations

import argparse
import re
import sys
import unicodedata

DOWNLOAD_PAGE = "https://splaboon.pages.dev/"
HASHTAGS = "#スプラトゥーン3 #Splatoon3 #SpLabo"
PROFILE_CTA = "ダウンロードはプロフィールからどうぞ。"
CHANGES_HEADING = "更新内容"
MAX_WEIGHT = 280
MAX_BULLETS = 3
QUERY_NAME_RE = re.compile(
    r"、?現行の\s*(?:\*\*)?\w+Query(?:\*\*)?(?:\s*/\s*(?:\*\*)?\w+Query(?:\*\*)?)*\s*で"
)

VERSION_RE = re.compile(r"^## \[([^\]]+)\]")
URL_RE = re.compile(r"https?://[^\s]+")


def tweet_weight(text: str) -> int:
    """X の加重文字数。本文に URL は入れない前提。残っていれば t.co 23。"""
    n = 0
    pos = 0
    for m in URL_RE.finditer(text):
        n += _run_weight(text[pos:m.start()])
        n += 23
        pos = m.end()
    n += _run_weight(text[pos:])
    return n


def _run_weight(s: str) -> int:
    w = 0
    for ch in s:
        if ch == "\n" or ch == "\r":
            w += 1
            continue
        # East Asian Wide/Fullwidth とそれ以外の非 Latin-1 は 2。
        ea = unicodedata.east_asian_width(ch)
        w += 2 if ea in ("W", "F") or ord(ch) > 0x10FF else 1
    return w


def extract_bullets(changelog: str, version: str) -> list[str]:
    lines = changelog.splitlines()
    in_section = False
    bullets: list[str] = []
    for line in lines:
        m = VERSION_RE.match(line)
        if m:
            if in_section:
                break
            in_section = m.group(1) == version
            continue
        if not in_section:
            continue
        if line.startswith("- "):
            bullets.append(line[2:].strip())
    return bullets


_FINISHED = re.compile(r"(しました|です|ます)$")
_NAKED_SHITA = re.compile(r"(?<!まし)した$")
# 辞書形・可能形で切れた述語。短くしたあとに「出せる／飛べる」で終わらないようにする。
_BARE_PREDICATE = re.compile(
    r"(できる|される|られる|れる|える|(?<!まし)する|[うくぐすつぬぶむる])$"
)


def shorten_bullet(raw: str) -> str:
    """マークアップと補足を落とす。文末の「しました」は残す。"""
    s = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", raw)
    s = URL_RE.sub("", s)
    s = QUERY_NAME_RE.sub("", s)
    s = re.sub(r"\b\w+Query\b", "", s)
    s = s.replace("**", "")
    s = re.sub(r"[（(][^）)]*[）)]", "", s)
    s = re.sub(r"\s{2,}", " ", s)
    return s.rstrip("。").strip()


def polish_bullet(s: str) -> str:
    """節落としや短縮のあと、投稿として読める一文にする。"""
    s = s.strip()
    s = re.sub(r"^[、。・\s]+", "", s)
    s = re.sub(r"^[をがに]\s*", "", s)
    s = re.sub(r"\s{2,}", " ", s)
    s = s.rstrip("。")
    if not s:
        return s
    if _FINISHED.search(s):
        return s
    if _NAKED_SHITA.search(s):
        return s[:-2] + "しました"
    if s.endswith("ように"):
        return s + "しました"
    if _BARE_PREDICATE.search(s):
        return s + "ようにしました"
    return s


def _opening_topic(s: str) -> str | None:
    # ひらがな助詞の手前まで。カタカナ・漢字・英数の塊を主題にする。
    m = re.match(r"[A-Za-z0-9一-龥ァ-ヶー]+", s)
    if not m:
        return None
    topic = m.group(0)
    return topic if len(topic) >= 2 else None


def _drop_leading_clause(s: str) -> str | None:
    """長い行の先頭節を落とす。主題が消えたら先頭語を付け直す。"""
    topic = _opening_topic(s)

    def finish(rest: str) -> str | None:
        polished = polish_bullet(rest)
        if not polished:
            return None
        if topic and topic not in polished:
            glued = polish_bullet(f"{topic}で{polished}")
            if glued and tweet_weight(glued) < tweet_weight(s):
                return glued
            return None
        if tweet_weight(polished) < tweet_weight(s):
            return polished
        return None

    for sep in ("、", "や"):
        if sep in s:
            got = finish(s.split(sep, 1)[1].strip())
            if got:
                return got
    if "が" in s:
        rest = s.split("が", 1)[1].strip()
        if rest and tweet_weight(rest) >= 24:
            return finish(rest)
    return None


def proofread_post(text: str) -> str:
    """投稿全体の箇条書きを添削する。定型句は触らない。"""
    lines: list[str] = []
    for line in text.splitlines():
        if line.startswith("・"):
            lines.append("・" + polish_bullet(line[1:]))
        else:
            lines.append(line)
    return "\n".join(lines)


def proofread_issues(text: str) -> list[str]:
    """校正。投稿できない形が残っていたら理由を返す。"""
    issues: list[str] = []
    if tweet_weight(text) > MAX_WEIGHT:
        issues.append(f"加重 {tweet_weight(text)} > {MAX_WEIGHT}")
    if "**" in text or "Query" in text:
        issues.append("マークアップが残っている")
    if URL_RE.search(text):
        issues.append("URL が入っている")
    if re.search(r"のを直$", text, re.M):
        issues.append("「直しました」が途中切れ")
    for line in text.splitlines():
        if not line.startswith("・"):
            continue
        body = line[1:]
        if re.match(r"[、。をがに]", body):
            issues.append(f"箇条書きの先頭が途中から: {body[:12]}")
        if re.search(r"直した$", body) and not body.endswith("直しました"):
            issues.append("「直しました」が「直した」になっている")
        if body and not _FINISHED.search(body):
            issues.append(f"文末が途中切れ: {body[-16:]}")
    if PROFILE_CTA not in text:
        issues.append("誘導文が無い")
    if HASHTAGS not in text:
        issues.append("ハッシュタグが無い")
    return issues


def build_post(version: str, bullets: list[str]) -> str:
    short = [polish_bullet(shorten_bullet(b)) for b in bullets]
    chosen = [s for s in short if s][:MAX_BULLETS]

    def compose(items: list[str]) -> str:
        parts = [f"SpLabo v{version} をリリースしました。", ""]
        if items:
            parts.append(CHANGES_HEADING)
            parts.extend(f"・{b}" for b in items)
            parts.append("")
        parts.extend([PROFILE_CTA, "", HASHTAGS])
        return "\n".join(parts)

    text = compose(chosen)
    while tweet_weight(text) > MAX_WEIGHT and chosen:
        # 溢れたら末尾の項目を短くする。短くできなければ落とす。
        last = chosen[-1]
        trimmed = _drop_leading_clause(last)
        if trimmed is not None and tweet_weight(trimmed) < tweet_weight(last):
            chosen[-1] = trimmed
            text = compose(chosen)
            continue
        chosen = chosen[:-1]
        text = compose(chosen)
    text = proofread_post(text)
    while tweet_weight(text) > MAX_WEIGHT and chosen:
        chosen = chosen[:-1]
        text = proofread_post(compose(chosen))
    leftover = proofread_issues(text)
    if leftover:
        raise AssertionError("校正チェック失敗: " + "; ".join(leftover))
    return text


def render(changelog: str, version: str) -> str:
    return build_post(version, extract_bullets(changelog, version))


def self_test() -> None:
    sample = """# Changelog

## [Unreleased]

## [0.10.4] — 2026-08-14

### Changed

- 散布図のツールチップに、ブキの**サブとスペシャル**のアイコンを出すようにしました（ダッシュボード・環境分析。保存 HTML も同じ）
- ヒートマップの**ブキ軸ラベル**をホバーすると、名前とサブ／スペシャルのアイコンが出るようにしました（ダッシュボード・環境分析）
- 自動取得に失敗したときの通知に**時刻**を出すようにしました（何度も溜まったとき、いつ失敗したか分かるように）

### Fixed

- ステージ図鑑で **平均塗り** で並び替えできるようにしました（一覧ヘッダ・パネルの並び順）

## [0.10.3] — 2026-08-13

- これは混ざらない
"""
    text = render(sample, "0.10.4")
    assert "0.10.3" not in text
    assert "SpLabo v0.10.4 をリリースしました。" in text
    assert PROFILE_CTA in text
    assert CHANGES_HEADING in text
    assert DOWNLOAD_PAGE not in text
    assert "http" not in text
    assert HASHTAGS in text
    assert "**" not in text
    assert tweet_weight(text) <= MAX_WEIGHT, tweet_weight(text)
    assert "散布図" in text
    assert "ヒートマップ" in text
    assert "ようにしました" in text
    assert re.search(r"出すようにしました", text)
    queryish = """# Changelog

## [0.10.5] — 2026-08-15

- 公式アプリのブキ記録を、現行の **WeaponQuery** / **StageRecordQuery** で取れるようにしました（設定から）
- ダッシュボードの棒グラフで公式の熟練度を軸に選べるようにしました
"""
    qtext = render(queryish, "0.10.5")
    assert "Query" not in qtext
    assert "ブキ記録を取れるようにしました" in qtext
    assert HASHTAGS in qtext
    assert not proofread_issues(qtext)
    empty = render("# Changelog\n\n## [1.0.0]\n\n### Changed\n\n", "1.0.0")
    assert empty.startswith("SpLabo v1.0.0")
    assert tweet_weight(empty) <= MAX_WEIGHT
    missing = render("# Changelog\n", "9.9.9")
    assert "SpLabo v9.9.9" in missing
    assert shorten_bullet("グラフの黒い枠が出ていたのを直しました") == "グラフの黒い枠が出ていたのを直しました"
    assert polish_bullet("ダウンロードページへ正しく飛べる") == "ダウンロードページへ正しく飛べるようにしました"
    assert polish_bullet("アイコンを出す") == "アイコンを出すようにしました"
    assert polish_bullet("黒い枠が出ていたのを直した") == "黒い枠が出ていたのを直しました"
    assert polish_bullet("黒い枠が出ていたのを直しました") == "黒い枠が出ていたのを直しました"
    cut = """# Changelog

## [0.11.1] — 2026-09-01

### Fixed

- シーズンが暦の 0 時に切り替わっていたのを直しました（日付と同じ JST 9時）
- 日本語表示で、カスタムグラフの散布図設定（点の見た目・画像の大きさ）や散布図・カレンダーのサイズ凡例見出しが翻訳されず英語キーのまま出ていたのを直しました
"""
    ctext = render(cut, "0.11.1")
    assert tweet_weight(ctext) <= MAX_WEIGHT, tweet_weight(ctext)
    assert "のを直しました" in ctext
    assert not re.search(r"のを直$", ctext, re.M)
    assert "シーズン" in ctext
    assert "英語キー" in ctext
    assert ctext.count("\n・") == 2
    assert "…" not in ctext
    assert not proofread_issues(text)
    assert not proofread_issues(ctext)
    assert not re.search(r"(出す|出る|飛べる|取れる|選べる)$", text, re.M)
    fly = """# Changelog

## [0.11.2] — 2026-09-06

### Fixed

- シーズンや期間の終了日のバトルが、その日の絞り込みに入らないのを直しました
- GitHub の README からダウンロードページへ正しく飛べるようにしました
- ブキ図鑑のブキチャレパワー（現在）が、今シーズン使っていないブキでも前シーズンの値のまま出ていたのを直しました
"""
    ftext = render(fly, "0.11.2")
    assert tweet_weight(ftext) <= MAX_WEIGHT
    assert "のを直しました" in ftext
    assert "飛べるようにしました" in ftext
    assert "飛べる\n" not in ftext
    assert not proofread_issues(ftext)
    print(text)
    print("--- weight:", tweet_weight(text), "/", MAX_WEIGHT)
    print(ctext)
    print("--- 0.11.1 weight:", tweet_weight(ctext), "/", MAX_WEIGHT)
    print(ftext)
    print("--- 0.11.2 weight:", tweet_weight(ftext), "/", MAX_WEIGHT)


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--changelog", help="CHANGELOG.md のパス")
    p.add_argument("--version", help="0.10.4 形式（splabo-v 無し）")
    p.add_argument("--self-test", action="store_true")
    args = p.parse_args()
    if args.self_test:
        self_test()
        return 0
    if not args.changelog or not args.version:
        print("--changelog と --version が必要です", file=sys.stderr)
        return 2
    version = args.version.removeprefix("splabo-v")
    changelog = open(args.changelog, encoding="utf-8").read()
    sys.stdout.write(render(changelog, version))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
