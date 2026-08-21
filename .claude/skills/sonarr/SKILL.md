---
name: sonarr
description: "Query Sonarr to discover TV series and episodes with file paths and metadata. Formats results as etv-station channel YAML entries."
user_invocable: true
---

# /sonarr — Query Sonarr

## When to use this skill

Trigger when the user says things like:
- "find on Sonarr", "check Sonarr for", "query Sonarr"
- "Sonarr library", "what shows does Sonarr have"
- "build a channel from Sonarr", "make a TV channel using Sonarr"
- "all episodes of [show] from Sonarr", "TV episodes for channel"
- Using Sonarr episode file paths or series metadata for a channel config

## Required environment variables

| Variable | Purpose |
|---|---|
| `SONARR_URL` | Base URL of the Sonarr server, e.g. `http://100.x.x.x:8989` |
| `SONARR_API_KEY` | Sonarr API key (Settings → General → API Key) |
| `MEDIA_PATH_FROM` | *(optional)* Path prefix Sonarr reports, e.g. `/media` |
| `MEDIA_PATH_TO` | *(optional)* Path prefix etv-station sees, e.g. `/data/media` |

If `SONARR_URL` or `SONARR_API_KEY` are missing, ask the user to add them to the project `.env` before proceeding.

## Path translation

Sonarr returns file paths as its container sees them. etv-station may mount the same files under a different prefix. Set `MEDIA_PATH_FROM` and `MEDIA_PATH_TO` to remap. If neither is set, paths pass through unchanged.

## Procedure

### Step 1 — Check env vars

Confirm `SONARR_URL` and `SONARR_API_KEY` are set. If missing, stop and ask the user.

### Step 2 — Run the query via ctx_execute

Use the appropriate Python template below.

### Step 3 — Format as YAML

Shape each episode result into an entry under `rule.blocks[].entries` using the field mapping table. Skip episodes with no file (`episodeFileId` is 0 or absent).

**For large result sets (10+ items): hand the formatting to a Haiku sub-agent.** Pass it the digested results from `ctx_execute` and the field-mapping table from this skill, and have it return the YAML entries. Keeps the long render output off the parent's context and runs cheaper.

---

## Query templates

### All series (listing)

```python
import os, json
from urllib.request import Request, urlopen

url = os.environ["SONARR_URL"]
key = os.environ["SONARR_API_KEY"]

req = Request(f"{url}/api/v3/series", headers={"X-Api-Key": key})
series_list = json.loads(urlopen(req).read())
for s in sorted(series_list, key=lambda x: x["title"]):
    print(f"id={s['id']} | {s['title']} ({s.get('year','')}) | status={s.get('status','')}")
```

### All episodes of a series (with file paths)

This is the most common query — fetches series info, episodes, and episode files in one `ctx_execute` call, then joins them.

```python
import os, json, re
from urllib.request import Request, urlopen

url = os.environ["SONARR_URL"]
key = os.environ["SONARR_API_KEY"]
path_from = os.environ.get("MEDIA_PATH_FROM", "")
path_to = os.environ.get("MEDIA_PATH_TO", "")
SEARCH_TITLE = "breaking bad"  # replace with target show (case-insensitive)

def translate(p):
    if path_from and p.startswith(path_from):
        return path_to + p[len(path_from):]
    return p

def sonarr_get(endpoint):
    req = Request(f"{url}{endpoint}", headers={"X-Api-Key": key})
    return json.loads(urlopen(req).read())

def slugify(s):
    return re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")

all_series = sonarr_get("/api/v3/series")
series = next((s for s in all_series if SEARCH_TITLE.lower() in s["title"].lower()), None)
if not series:
    print(f"Series not found: {SEARCH_TITLE}")
    print("Available:", [s["title"] for s in all_series[:20]])
else:
    sid = series["id"]
    show_slug = slugify(series["title"])
    poster = next((i["remoteUrl"] for i in series.get("images", []) if i["coverType"] == "poster"), "")

    episodes = sonarr_get(f"/api/v3/episode?seriesId={sid}")
    files = {f["id"]: translate(f["path"]) for f in sonarr_get(f"/api/v3/episodefile?seriesId={sid}")}

    has_file = [ep for ep in episodes if ep.get("episodeFileId") and ep["episodeFileId"] in files]
    for ep in sorted(has_file, key=lambda e: (e["seasonNumber"], e["episodeNumber"])):
        season = ep["seasonNumber"]
        episode = ep["episodeNumber"]
        ep_id = f"{show_slug}-s{season:02d}e{episode:02d}"
        file_path = files[ep["episodeFileId"]]
        print(f"id={ep_id}")
        print(f"  S{season:02d}E{episode:02d} — {ep['title']}")
        print(f"  file={file_path}")
        print(f"  overview={ep.get('overview','')[:100]}")
        print(f"  poster={poster}")
        print()
```

### Specific season only

Add a filter after `has_file`:

```python
TARGET_SEASON = 1
has_file = [ep for ep in has_file if ep["seasonNumber"] == TARGET_SEASON]
```

### All series that have downloaded episodes

```python
import os, json
from urllib.request import Request, urlopen

url = os.environ["SONARR_URL"]
key = os.environ["SONARR_API_KEY"]

def sonarr_get(endpoint):
    req = Request(f"{url}{endpoint}", headers={"X-Api-Key": key})
    return json.loads(urlopen(req).read())

series_list = sonarr_get("/api/v3/series")
for s in series_list:
    stats = s.get("statistics", {})
    if stats.get("episodeFileCount", 0) > 0:
        print(f"{s['title']} — {stats['episodeFileCount']} files / {stats.get('episodeCount',0)} episodes")
```

---

## Field mapping: Sonarr → etv-station YAML

| Sonarr API field | YAML field | Notes |
|---|---|---|
| `series.title` | `program.title` | use series title for all episodes |
| `episode.title` | `program.sub_title` | the episode's own name |
| `episode.overview` | `program.description` | episode-level description |
| `series.year` | `program.year` | integer |
| `series.genres[]` | `program.categories` | prepend `"TV"` |
| `episodefile.path` | `source.path` | apply path translation |
| `episode.seasonNumber` | `program.season` | integer |
| `episode.episodeNumber` | `program.episode` | integer |
| `series.images[coverType=="poster"].remoteUrl` | `program.artwork_url` | series poster |

`title` is the series and `sub_title` is the episode — that is the split ETV-next's
own `ProgramMetadata` makes (`vendor/etv-next/crates/ersatztv-playout/src/playout.rs`),
and the guide templates (#158) read them as separate `{title}` / `{sub_title}` fields.
A name not in that struct is silently dropped at load rather than refused, so check
there before adding one.

There is no content rating in Sonarr's API — omit `content_rating` unless you have it from another source.

---

## YAML output template (TV episode)

An item entry lives in a block, and a channel is a list of blocks — there is no
top-level item list. Append to an existing block's `entries:` when the channel
already has one.

```yaml
rule:
  blocks:
    - mode: "all"
      order: "manual"
      entries:
        - kind: item
          source:
            kind: local
            path: "/data/media/TV/Breaking Bad/Season 01/Breaking.Bad.S01E01.mkv"
          program:
            title: "Breaking Bad"
            sub_title: "Pilot"
            description: "A chemistry teacher diagnosed with inoperable lung cancer turns to crime..."
            categories: ["TV", "Drama", "Crime"]
            year: 2008
            season: 1
            episode: 1
            artwork_url: "https://artworks.thetvdb.com/banners/posters/..."
```

**Never author an `id`.** An item's identity is derived from its source at
resolution time (`crates/etv-station/src/config/entry.rs:48`), which is what lets
two entries pointing at the same file collapse. An `id:` key is not part of
`ItemEntry` and is dropped.

Omit any field where the API returned null or empty string. Always filter to episodes that have a downloaded file (`episodeFileId` present and in the files map).
