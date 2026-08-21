---
name: plex
description: "Query Plex Media Server to discover movies, shows, episodes, and collections. Formats results as etv-station channel YAML entries."
user_invocable: true
---

# /plex — Query Plex Media Server

## When to use this skill

Trigger when the user says things like:
- "find on Plex", "check Plex for", "query Plex", "search Plex"
- "Plex library", "what's on Plex", "list my Plex movies/shows"
- "build a channel from Plex", "make a channel using my Plex collection"
- "all [genre] movies from Plex", "Plex collection [name]"

## Required environment variables

| Variable | Purpose |
|---|---|
| `PLEX_URL` | Base URL of the Plex server, e.g. `http://100.x.x.x:32400` |
| `PLEX_TOKEN` | Plex authentication token |
| `MEDIA_PATH_FROM` | *(optional)* Path prefix Plex reports, e.g. `/media` |
| `MEDIA_PATH_TO` | *(optional)* Path prefix etv-station sees, e.g. `/data/media` |

If `PLEX_URL` or `PLEX_TOKEN` are missing, ask the user to add them to the project `.env` before proceeding.

## Path translation

Plex returns file paths as its container sees them. etv-station may mount the same files under a different prefix. Set `MEDIA_PATH_FROM` and `MEDIA_PATH_TO` to remap. If neither is set, paths pass through unchanged.

## Procedure

### Step 1 — Check env vars

Confirm `PLEX_URL` and `PLEX_TOKEN` are set. If missing, stop and ask the user.

### Step 2 — Discover library sections (if needed)

If you don't know the section ID, list all libraries first:

```python
import os, json
from urllib.request import Request, urlopen

url = os.environ["PLEX_URL"]
token = os.environ["PLEX_TOKEN"]

req = Request(f"{url}/library/sections", headers={"X-Plex-Token": token, "Accept": "application/json"})
data = json.loads(urlopen(req).read())
for s in data["MediaContainer"]["Directory"]:
    print(f"id={s['key']} type={s['type']} title={s['title']}")
```

### Step 3 — Run the query

Use `ctx_execute` with the appropriate Python template below.

### Step 4 — Format as YAML

Shape each result into an entry under `rule.blocks[].entries` using the field mapping table.

**For large result sets (10+ items): hand the formatting to a Haiku sub-agent.** Pass it the digested results from `ctx_execute` and the field-mapping table from this skill, and have it return the YAML entries. This keeps the long render output (multi-KB of repeated YAML) off the parent's context and runs on a cheaper model tier. The parent then shows the YAML to the user or writes it to a file.

---

## Query templates

### All movies in a library section

```python
import os, json, re
from urllib.request import Request, urlopen

url = os.environ["PLEX_URL"]
token = os.environ["PLEX_TOKEN"]
path_from = os.environ.get("MEDIA_PATH_FROM", "")
path_to = os.environ.get("MEDIA_PATH_TO", "")
SECTION_ID = "1"  # replace with actual section key

def translate(p):
    if path_from and p.startswith(path_from):
        return path_to + p[len(path_from):]
    return p

def plex_get(endpoint):
    req = Request(f"{url}{endpoint}", headers={"X-Plex-Token": token, "Accept": "application/json"})
    return json.loads(urlopen(req).read())

def slugify(s):
    return re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")

data = plex_get(f"/library/sections/{SECTION_ID}/all?type=1")
for item in data["MediaContainer"].get("Metadata", []):
    try:
        file_path = translate(item["Media"][0]["Part"][0]["file"])
    except (KeyError, IndexError):
        continue
    genres = [g["tag"] for g in item.get("Genre", [])]
    year = item.get("year", "")
    slug = slugify(f"{item['title']}-{year}" if year else item["title"])
    print(f"---")
    print(f"id={slug} | title={item['title']} | year={year}")
    print(f"file={file_path}")
    print(f"rating={item.get('contentRating','')} | genres={genres}")
    print(f"summary={item.get('summary','')[:120]}")
    print(f"ratingKey={item['ratingKey']}")
```

### Movies by genre filter

Add after fetching metadata:

```python
TARGET_GENRE = "Action"  # replace as needed
filtered = [m for m in data["MediaContainer"].get("Metadata", [])
            if any(g["tag"] == TARGET_GENRE for g in m.get("Genre", []))]
```

### All episodes of a TV show

```python
import os, json, re
from urllib.request import Request, urlopen

url = os.environ["PLEX_URL"]
token = os.environ["PLEX_TOKEN"]
path_from = os.environ.get("MEDIA_PATH_FROM", "")
path_to = os.environ.get("MEDIA_PATH_TO", "")
SHOW_RATING_KEY = "456"  # ratingKey of the show (from listing shows with type=2)

def translate(p):
    if path_from and p.startswith(path_from):
        return path_to + p[len(path_from):]
    return p

def plex_get(endpoint):
    req = Request(f"{url}{endpoint}", headers={"X-Plex-Token": token, "Accept": "application/json"})
    return json.loads(urlopen(req).read())

def slugify(s):
    return re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")

show_data = plex_get(f"/library/metadata/{SHOW_RATING_KEY}")
show_title = show_data["MediaContainer"]["Metadata"][0]["title"]
show_slug = slugify(show_title)

episodes = plex_get(f"/library/metadata/{SHOW_RATING_KEY}/allLeaves")
for ep in episodes["MediaContainer"].get("Metadata", []):
    try:
        file_path = translate(ep["Media"][0]["Part"][0]["file"])
    except (KeyError, IndexError):
        continue
    season = ep.get("parentIndex", 0)
    episode = ep.get("index", 0)
    ep_id = f"{show_slug}-s{season:02d}e{episode:02d}"
    print(f"id={ep_id} | S{season:02d}E{episode:02d} | {ep['title']}")
    print(f"  file={file_path}")
    print(f"  summary={ep.get('summary','')[:100]}")
```

### Items in a Plex collection

```python
import os, json
from urllib.request import Request, urlopen

url = os.environ["PLEX_URL"]
token = os.environ["PLEX_TOKEN"]
COLLECTION_ID = "789"  # ratingKey of the collection

def plex_get(endpoint):
    req = Request(f"{url}{endpoint}", headers={"X-Plex-Token": token, "Accept": "application/json"})
    return json.loads(urlopen(req).read())

data = plex_get(f"/library/collections/{COLLECTION_ID}/children")
for item in data["MediaContainer"].get("Metadata", []):
    print(f"{item['title']} ({item.get('year','')}) — ratingKey={item['ratingKey']}")
```

---

## Field mapping: Plex → etv-station YAML

| Plex API field | YAML field | Notes |
|---|---|---|
| `title` | `program.title` | movie: the film. TV: see `grandparentTitle` below |
| `grandparentTitle` | `program.title` | TV only — the series |
| `title` | `program.sub_title` | TV only — the episode's own name |
| `summary` | `program.description` | truncate if very long |
| `year` | `program.year` | integer |
| `contentRating` | `program.content_rating` | e.g. `"R"` |
| `Genre[].tag` | `program.categories` | array; prepend `"Movie"` or `"TV"` |
| `Media[0].Part[0].file` | `source.path` | apply path translation |
| `ratingKey` | used to build `artwork_url` | see below |
| `parentIndex` | `program.season` | TV only |
| `index` | `program.episode` | TV only |

On an episode Plex's `title` is the episode and `grandparentTitle` is the series,
which is the reverse of what a naive mapping does. `title` is the series and
`sub_title` is the episode — that is the split ETV-next's own `ProgramMetadata`
makes (`vendor/etv-next/crates/ersatztv-playout/src/playout.rs`), and the guide
templates (#158) read them as separate `{title}` / `{sub_title}` fields. A name
not in that struct is silently dropped at load rather than refused.

Artwork URL pattern:
```
{PLEX_URL}/library/metadata/{ratingKey}/thumb/1?X-Plex-Token={token}
```

⚠️ **That URL carries a live `PLEX_TOKEN`.** Writing it into a channel file puts a
working credential in a file on disk, and anyone who can read the file can read the
library. Prefer leaving `artwork_url` unset and letting the catalog ingester supply
artwork (`crates/etv-station/src/catalog/ingest/plex.rs`), which reads the token
from the environment at run time. If a literal URL is genuinely needed, tell the
user it contains their token before writing it.

---

## YAML output template (movie)

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
```

## YAML output template (TV episode)

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
            description: "Walter White, a chemistry teacher, cooks meth with a former student..."
            categories: ["TV", "Drama"]
            year: 2008
            season: 1
            episode: 1
```

**Never author an `id`.** An item's identity is derived from its source at
resolution time (`crates/etv-station/src/config/entry.rs:48`), which is what lets
two entries pointing at the same file collapse. An `id:` key is not part of
`ItemEntry` and is dropped.

Omit any field where the API returned null or empty string.
