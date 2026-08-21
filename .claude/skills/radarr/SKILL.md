---
name: radarr
description: "Query Radarr to discover movies with file paths and metadata. Formats results as etv-station channel YAML entries."
user_invocable: true
---

# /radarr — Query Radarr

## When to use this skill

Trigger when the user says things like:
- "find on Radarr", "check Radarr for", "query Radarr"
- "Radarr library", "what movies does Radarr have"
- "build a channel from Radarr", "make a channel using Radarr movies"
- "all [genre] movies from Radarr", "movies tagged [tag] in Radarr"
- Using Radarr metadata (genres, certification, TMDB poster) for a channel config

## Required environment variables

| Variable | Purpose |
|---|---|
| `RADARR_URL` | Base URL of the Radarr server, e.g. `http://100.x.x.x:7878` |
| `RADARR_API_KEY` | Radarr API key (Settings → General → API Key) |
| `MEDIA_PATH_FROM` | *(optional)* Path prefix Radarr reports, e.g. `/media` |
| `MEDIA_PATH_TO` | *(optional)* Path prefix etv-station sees, e.g. `/data/media` |

If `RADARR_URL` or `RADARR_API_KEY` are missing, ask the user to add them to the project `.env` before proceeding.

## Path translation

Radarr returns file paths as its container sees them. etv-station may mount the same files under a different prefix. Set `MEDIA_PATH_FROM` and `MEDIA_PATH_TO` to remap. If neither is set, paths pass through unchanged.

## Procedure

### Step 1 — Check env vars

Confirm `RADARR_URL` and `RADARR_API_KEY` are set. If missing, stop and ask the user.

### Step 2 — Run the query via ctx_execute

Use the appropriate Python template below.

### Step 3 — Format as YAML

Shape each result into an entry under `rule.blocks[].entries` using the field mapping table. Skip any movie where `hasFile` is false.

**For large result sets (10+ items): hand the formatting to a Haiku sub-agent.** Pass it the digested results from `ctx_execute` and the field-mapping table from this skill, and have it return the YAML entries. Keeps the long render output off the parent's context and runs cheaper.

---

## Query templates

### All downloaded movies

```python
import os, json, re
from urllib.request import Request, urlopen

url = os.environ["RADARR_URL"]
key = os.environ["RADARR_API_KEY"]
path_from = os.environ.get("MEDIA_PATH_FROM", "")
path_to = os.environ.get("MEDIA_PATH_TO", "")

def translate(p):
    if path_from and p.startswith(path_from):
        return path_to + p[len(path_from):]
    return p

def radarr_get(endpoint):
    req = Request(f"{url}{endpoint}", headers={"X-Api-Key": key})
    return json.loads(urlopen(req).read())

def slugify(s):
    return re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")

movies = [m for m in radarr_get("/api/v3/movie") if m.get("hasFile")]
for m in movies:
    file_path = translate(m["movieFile"]["path"])
    poster = next((i["remoteUrl"] for i in m.get("images", []) if i["coverType"] == "poster"), "")
    year = m.get("year", "")
    slug = slugify(f"{m['title']}-{year}" if year else m["title"])
    print(f"id={slug} | {m['title']} ({year})")
    print(f"  file={file_path}")
    print(f"  cert={m.get('certification','')} | genres={m.get('genres',[])} | runtime={m.get('runtime','')}min")
    print(f"  overview={m.get('overview','')[:120]}")
    print(f"  poster={poster}")
```

### Movies by genre

Add after fetching all movies:

```python
TARGET_GENRE = "Action"  # replace as needed
filtered = [m for m in movies if TARGET_GENRE in m.get("genres", [])]
```

### Movies by tag name

```python
import os, json, re
from urllib.request import Request, urlopen

url = os.environ["RADARR_URL"]
key = os.environ["RADARR_API_KEY"]
path_from = os.environ.get("MEDIA_PATH_FROM", "")
path_to = os.environ.get("MEDIA_PATH_TO", "")

def translate(p):
    if path_from and p.startswith(path_from):
        return path_to + p[len(path_from):]
    return p

def radarr_get(endpoint):
    req = Request(f"{url}{endpoint}", headers={"X-Api-Key": key})
    return json.loads(urlopen(req).read())

TARGET_TAG = "christmas"  # replace as needed

tags = {t["id"]: t["label"] for t in radarr_get("/api/v3/tag")}
tag_id = next((tid for tid, label in tags.items() if label.lower() == TARGET_TAG.lower()), None)

if tag_id is None:
    print(f"Tag '{TARGET_TAG}' not found. Available tags: {list(tags.values())}")
else:
    movies = [m for m in radarr_get("/api/v3/movie") if m.get("hasFile") and tag_id in m.get("tags", [])]
    for m in movies:
        print(f"{m['title']} ({m.get('year','')}) — {translate(m['movieFile']['path'])}")
```

### Single movie lookup

```python
import os, json
from urllib.request import Request, urlopen

url = os.environ["RADARR_URL"]
key = os.environ["RADARR_API_KEY"]
MOVIE_ID = 42  # Radarr internal ID

req = Request(f"{url}/api/v3/movie/{MOVIE_ID}", headers={"X-Api-Key": key})
m = json.loads(urlopen(req).read())
print(json.dumps(m, indent=2))
```

---

## Field mapping: Radarr → etv-station YAML

| Radarr API field | YAML field | Notes |
|---|---|---|
| `title` | `program.title` | required |
| `overview` | `program.description` | |
| `year` | `program.year` | integer |
| `certification` | `program.content_rating` | e.g. `"R"` |
| `genres[]` | `program.categories` | array; prepend `"Movie"` |
| `movieFile.path` | `source.path` | apply path translation |
| `images[coverType=="poster"].remoteUrl` | `program.artwork_url` | TMDB URL |

Every `program.*` name above is a field of ETV-next's own `ProgramMetadata`
(`vendor/etv-next/crates/ersatztv-playout/src/playout.rs`), so a name not in that
struct is silently dropped at load rather than refused. Check there before adding one.

---

## YAML output template

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
            path: "/data/media/Movies/Die Hard (1988)/Die.Hard.mkv"
          program:
            title: "Die Hard"
            description: "Off-duty NYPD detective John McClane fights terrorists..."
            categories: ["Movie", "Action", "Thriller"]
            year: 1988
            content_rating: "R"
            artwork_url: "https://image.tmdb.org/t/p/original/yFihWxQcmqcaBR31QM6Y8gT6aYV.jpg"
```

**Never author an `id`.** An item's identity is derived from its source at
resolution time (`crates/etv-station/src/config/entry.rs:48`), which is what lets
two entries pointing at the same file collapse. An `id:` key is not part of
`ItemEntry` and is dropped.

Omit any field where the API returned null or empty string. Always filter to `hasFile: true` — don't reference movies Radarr hasn't downloaded yet.
