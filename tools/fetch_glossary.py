# -*- coding: utf-8 -*-
"""从 PokeAPI 拉取宝可梦术语（英->简中），生成 assets/glossary/*.tsv。

用法（项目根目录）:
    python tools/fetch_glossary.py

数据源: https://pokeapi.co (免费公开 API，无需 Key)
输出:   assets/glossary/{pokemon,moves,abilities,items}.tsv，每行 "英文\t中文"
说明:   缺 zh-hans 时回退 zh-hant（并在行尾标记 [HANT]，便于人工校对后移除标记）。
"""
import json
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

BASE = "https://pokeapi.co/api/v2"
OUT_DIR = Path(__file__).resolve().parent.parent / "assets" / "glossary"
WORKERS = 6
RETRIES = 5


def fetch_json(url: str):
    for attempt in range(RETRIES):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "xiangxu-glossary/1.0"})
            with urllib.request.urlopen(req, timeout=30) as r:
                return json.load(r)
        except Exception:
            if attempt == RETRIES - 1:
                return None
            time.sleep(2.0 + attempt * 2.0)


def get_count(endpoint: str) -> int:
    d = fetch_json(f"{BASE}/{endpoint}?limit=1")
    return d["count"] if d else 0


def pick_name(names: list) -> tuple[str | None, bool]:
    """返回 (中文名, 是否繁体回退)。优先 zh-hans，回退 zh-hant。"""
    hant = None
    for n in names:
        lang = n["language"]["name"]
        if lang == "zh-hans":
            return n["name"], False
        if lang == "zh-hant" and hant is None:
            hant = n["name"]
    return hant, True


def fetch_entry(endpoint: str, idx: int):
    d = fetch_json(f"{BASE}/{endpoint}/{idx}")
    if not d:
        return None
    en = d.get("name") or ""
    zh, is_hant = pick_name(d.get("names") or [])
    if not en or not zh:
        return None
    return (idx, en, zh, is_hant)


def fetch_all(endpoint: str, label: str, out_name: str) -> None:
    total = get_count(endpoint)
    if total == 0:
        print(f"[{label}] 无法获取总数，跳过")
        return
    print(f"[{label}] 共 {total} 条，开始拉取...", flush=True)
    results: dict[int, tuple[str, str, bool]] = {}
    failed: list[int] = []
    done = 0
    with ThreadPoolExecutor(max_workers=WORKERS) as ex:
        futures = {ex.submit(fetch_entry, endpoint, i): i for i in range(1, total + 1)}
        for f in as_completed(futures):
            done += 1
            if done % 200 == 0:
                print(f"  [{label}] {done}/{total}", flush=True)
            r = f.result()
            if r:
                idx, en, zh, is_hant = r
                results[idx] = (en, zh, is_hant)
            else:
                failed.append(futures[f])

    hant_count = sum(1 for _, (_, _, h) in results.items() if h)
    # 第一轮失败的条目串行补拉一轮（避开限流），仍失败才放弃。
    for idx in sorted(failed):
        r = fetch_entry(endpoint, idx)
        if r:
            _, en, zh, is_hant = r
            results[idx] = (en, zh, is_hant)
    failed = [i for i in failed if i not in results]
    out = OUT_DIR / out_name
    with open(out, "w", encoding="utf-8", newline="\n") as fp:
        fp.write("# 由 tools/fetch_glossary.py 从 PokeAPI 生成，勿手工编辑；补充请用 custom.tsv\n")
        for idx in sorted(results):
            en, zh, is_hant = results[idx]
            mark = "\t[HANT]" if is_hant else ""
            fp.write(f"{en}\t{zh}{mark}\n")
    print(f"[{label}] 完成: {len(results)} 条 (简中缺失回退繁体 {hant_count} 条)，写入 {out}")
    if failed:
        print(f"[{label}] 失败 {len(failed)} 条: {sorted(failed)[:20]}...")


def main() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    for endpoint, label, out_name in [
        ("pokemon-species", "宝可梦", "pokemon.tsv"),
        ("move", "技能", "moves.tsv"),
        ("ability", "特性", "abilities.tsv"),
        ("item", "道具", "items.tsv"),
    ]:
        fetch_all(endpoint, label, out_name)
    print("全部完成")


if __name__ == "__main__":
    sys.exit(main())
