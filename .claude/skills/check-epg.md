---
name: check-epg
description: "Fetch and validate the XMLTV EPG from a running dev integration. Use when the user asks if the EPG is correct, if guide data is showing up, or to verify programme titles/times."
---

# Check EPG

## Endpoint

```
http://127.0.0.1:8409/xmltv.xml
```

This requires `./tools/dev-run.sh` to be running. If the server is not up, tell the user to start it and stop here.

## Procedure

### 1. Fetch the XMLTV feed

```
curl -s -o /tmp/etv-epg.xml -w "%{http_code}" http://127.0.0.1:8409/xmltv.xml
```

If HTTP code is not 200, the server is down or returned an error — check `tmp/dev.*.log` with the `read-logs` skill.

### 2. Read the EPG XML

```
cat /tmp/etv-epg.xml
```

Parse what you see. A valid XMLTV document:
- Root element is `<tv>`
- Contains one `<channel>` element per channel with `id` attribute and `<display-name>` child
- Contains `<programme>` elements with `start`, `stop`, `channel` attributes and at least a `<title>` child

### 3. Validate content

Check for:

| Thing to check | What it should look like |
|----------------|--------------------------|
| Channel count | One `<channel>` per entry in `lineup.json` (currently 2) |
| Channel IDs | Match the channel number from `lineup.json` |
| Programme coverage | `<programme>` elements for the current time window; no gaps |
| Programme titles | Match the content in the playout JSON files under `examples/output/` |
| `start`/`stop` format | `YYYYMMDDHHmmss +0000` (UTC offset) |

### 4. Cross-check with playout JSON

The playout JSON files live under `examples/output/<channel-folder>/`. Each file is named `{start}_{finish}.json` (ISO 8601 UTC). The EPG `<programme>` titles should match the `title` fields in those JSON files for the current broadcast window.

To find the current playout JSON:

```
ls -t examples/output/test/*.json
ls -t examples/output/diehard/*.json
```

Read the most recent one and confirm its titles appear in the EPG.

## Reporting

- **Channel count correct**: yes/no
- **Programme coverage**: are there programmes covering the current time?
- **Titles match playout JSON**: yes/no, with specifics if mismatched
- **Any malformed elements**: list them with their line numbers
