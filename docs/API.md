# SJNmusic Daemon API

`sjnmusicd` exposes a small JSON-over-HTTP API. The CLI (`sjnmusic`) and the GUI
are both clients of it; anything they can do, you can do with `curl`.

- **Base URL:** `http://127.0.0.1:14567` by default (`[server] host` / `port` in
  `~/.sjn/music/config.toml`)
- **Content type:** `application/json` on every response
- **No versioning:** routes are plain paths, no `/v1` prefix

## Authentication

Authentication is optional and token-based.

- If no token is configured, every peer is trusted.
- If a token is configured, requests from loopback addresses (`127.0.0.1`,
  `::1`) are still trusted. Every other peer must send
  `Authorization: Bearer <token>`.
- Missing/invalid credentials on a protected route return `401` with
  `WWW-Authenticate: Bearer realm="sjnmusicd"`.
- `GET /auth/token` and `POST /auth/pair` hand the token to a **localhost**
  caller only; from any other address they return `403`. When authentication is
  off they return `404` instead of inventing a token.

## Conventions

- Trailing slashes are ignored (`/queue/` == `/queue`).
- Query strings are URL-decoded, including `+` as a space.
- Endpoints that take a search string accept it either as `?q=...` or in a JSON
  body using any of the keys `query`, `name`, `song`, `q`.
- Queue and playlist positions are **1-based**.
- An empty request body is treated as JSON `null`, so `POST /skip` needs no
  body at all.
- Errors have one shape: `{"error": "message"}`. Status codes used:

| Status | Meaning |
| ------ | ------- |
| `200` | Success |
| `202` | Work accepted asynchronously (downloads, scans, recognition) |
| `400` | Bad input (missing query, unknown song, invalid JSON, bad position) |
| `401` | Missing/invalid bearer token |
| `403` | Localhost-only endpoint called from off-host |
| `404` | Unknown route or unknown song/id |
| `409` | Conflicting state (a scan is already running) |
| `413` | Queue is full (`max_queue_size` reached) |

## Routes

### Introspection

| Method | Path | Description |
| ------ | ---- | ----------- |
| `GET` | `/` or `/help` | List every endpoint as strings |

### Library

| Method | Path | Description |
| ------ | ---- | ----------- |
| `GET` | `/songs?q=` | All songs, sorted by name. Optional `q` is a SQL `LIKE` substring filter |
| `GET` | `/songs/{id}` | One song with full metadata and original source |
| `POST` | `/import` | Copy local audio files into the library. Body: `{"path": "..."}` or `{"paths": ["...", "..."]}` (also accepts `file` / `files`) |
| `POST` | `/del` | Delete a song from the database, disk, and queue. Body/query names the song |

### Downloads and search

| Method | Path | Description |
| ------ | ---- | ----------- |
| `POST` | `/init` | Queue a download. Body `{"song": "..."}`, `{"name": "..."}`, or `{"url": "..."}`. Runs asynchronously |
| `POST` | `/init/batch` | Queue several downloads. Body `{"songs": ["...", "..."]}` |
| `GET` | `/downloads` | All in-flight and recent download jobs |
| `GET` | `/search?q=` / `POST /search` | Best single library match (exact, then fuzzy) |
| `GET` | `/search/all?q=` | Every library match above the fuzzy threshold, best first |
| `GET` | `/search/yt?q=&limit=N` | Raw `yt-dlp` candidates for a picker |
| `GET` | `/search/yt/ranked?q=&limit=N` | The same candidates, scored and sorted |
| `GET` | `/pick?q=&limit=N&margin=N` | Auto-pick a candidate when confident, otherwise return the shortlist |

`/init` accepts a direct audio-file URL as well as a video page/`ytsearch`
expression. A direct URL is probed, downloaded, validated, and stored once —
re-submitting the same URL returns the existing library row.

### Queue and playback

| Method | Path | Description |
| ------ | ---- | ----------- |
| `POST` | `/play` | Put a matched song at the head of the queue and start it |
| `POST` | `/add` | Append a matched song to the queue (honours the queue size limit) |
| `POST` | `/play/all` | Replace the queue with the whole library in random order |
| `POST` | `/skip` | Stop the current track (it is not written to history) |
| `GET` | `/queue` | Queue, current track, and repeat mode |
| `GET` | `/now-playing` | Current track plus live playback state |
| `POST` | `/queue/shuffle` | Shuffle the queue in place |
| `POST` | `/queue/clear` | Empty the queue |
| `POST` | `/queue/remove` | Remove one entry. Body `{"pos": N}` |
| `POST` | `/queue/jump` | Play entry `N` now and drop everything before it. Body `{"pos": N}` |
| `POST` | `/queue/reorder` | Move `from` to `to` (1-based). Body `{"from": N, "to": M}` |
| `POST` | `/pause` | Pause playback |
| `POST` | `/resume` | Resume playback |
| `POST` | `/seek` | Seek. Body `{"secs": 12.5}` (also `seconds`, `pos`, `position`) |
| `GET` | `/repeat` | Current repeat mode |
| `POST` | `/repeat` | Set mode. Body `{"mode": "off" \| "one" \| "all"}` |
| `GET` | `/volume` | Volume plus playing/paused flags |
| `POST` | `/volume` | Set volume. Body `{"volume": 0.8}` (also `vol`, `v`) |

Notes:

- `/play` and `/play/all` only queue; the daemon's playback loop starts the
  first entry as soon as the audio engine is idle.
- `"one"` requeues the same track when it ends; `"all"` reloads the queue
  snapshot once the queue drains.

### Playlists

| Method | Path | Description |
| ------ | ---- | ----------- |
| `GET` | `/playlists` | All playlists with their songs |
| `POST` | `/playlists` | Create. Body `{"name": "..."}` |
| `GET` | `/playlists/{name}` | One playlist |
| `PATCH` | `/playlists/{name}` | Rename. Body `{"name": "new name"}` |
| `DELETE` | `/playlists/{name}` | Delete the playlist (files are untouched) |
| `POST` | `/playlists/{name}/add` | Add a matched song. Body `{"query": "..."}` |
| `POST` | `/playlists/{name}/play` | Replace the queue with this playlist's songs |
| `DELETE` | `/playlists/{name}/songs/{id}` | Remove song `id` from the playlist |
| `POST` | `/playlists/{name}/reorder` | Body `{"from": N, "to": M}` |
| `POST` | `/playlists/{name}/duplicate` | Copy to a new name. Body `{"name": "..."}` |

### History, stats, metadata

| Method | Path | Description |
| ------ | ---- | ----------- |
| `GET` | `/history?limit=N` | Recent plays, default 50 |
| `GET` | `/stats` | Total plays/seconds and per-song leaders |
| `GET` | `/metadata/status` | Which services are configured/available, plus coverage counts |
| `GET` | `/metadata/settings` | Current `[metadata]` configuration |
| `POST` | `/metadata/settings` | Replace the `[metadata]` configuration with the request body |
| `POST` | `/metadata/test` | Probe a service. Body `{"service": "acoustid" \| "musicbrainz"}` |
| `POST` | `/metadata/scan` | Start a library scan. Body `{"force": true}` to redo complete songs |
| `GET` | `/metadata/scan` | Live progress / final result |
| `POST` | `/metadata/scan/cancel` | Ask a running scan to stop after the current song |
| `POST` | `/metadata/recognize` | Fingerprint + recognize one song. Body `{"song_id": N}` or `{"query": "..."}` |
| `POST` | `/metadata/sync` | Write database metadata back into the file's tags. Same body as above |

`/metadata/test` returns `200` even for a failed probe — check `ok` in the body.

## Response objects

### Song

Returned by `/songs`, `/songs/{id}`, `/search`, `/search/all`, `/play`, `/add`,
`/del`, `/import`, `/init` (as `song_id`), and inside playlists and queues.

```json
{
  "id": 7,
  "name": "Radiohead - Everything In Its Right Place",
  "path": "/home/user/.sjn/music/songs/Radiohead - Everything In Its Right Place.mp3",
  "duration_secs": 240.2,
  "title": "Everything In Its Right Place",
  "artist": "Radiohead",
  "album": "Kid A",
  "release_year": 2000,
  "track_number": 1,
  "genre": "Electronic",
  "metadata_status": "recognized",
  "metadata_source": "acoustid",
  "metadata_confidence": 0.94,
  "source_type": "youtube",
  "source_platform": "youtube",
  "source_url": "https://www.youtube.com/watch?v=...",
  "source_identifier": "dQw4w9WgXcQ",
  "source_title": "Radiohead - Everything In Its Right Place",
  "source_created_at": 1750000000
}
```

`id`, `name`, and `path` are always present; `duration_secs` is omitted until
it has been probed. Every other field is optional and null/absent when unknown.
The optional groups are:

- **MusicBrainz:** `musicbrainz_recording_id`, `musicbrainz_release_id`,
  `musicbrainz_release_group_id`, `musicbrainz_artist_id`,
  `musicbrainz_release_artist_id`, `musicbrainz_work_id`,
  `musicbrainz_track_id`
- **AcoustID:** `acoustid_id`, `acoustid_score`, `acoustid_fingerprint`,
  `acoustid_last_checked_at`
- **Credits/release:** `album_artist`, `composer`, `songwriter`, `arranger`,
  `performer`, `release_date`, `original_release_date`, `track_total`,
  `disc_number`, `disc_total`, `tags`, `style`, `recording_location`,
  `language`, `isrc`, `label`, `catalog_number`, `barcode`, `release_type`,
  `release_country`, `bpm`, `musical_key`
- **Metadata bookkeeping:** `metadata_last_updated_at`,
  `metadata_last_checked_at`, `metadata_error`
- **Original source:** `source_path` (for imports), `source_metadata` (JSON
  string with the original request/filename/timestamps)

`source_*` fields record where a song came from and are never rewritten by
metadata recognition.

### Lists

```json
// GET /songs
{ "songs": [ Song, ... ] }

// GET /search/all
{ "matches": [ Song, ... ] }

// GET /downloads
{ "downloads": [ DownloadJob, ... ] }

// GET /history
{ "history": [ HistoryEntry, ... ] }

// GET /playlists
[ Playlist, ... ]
```

### Playlist

```json
{
  "id": 3,
  "name": "favourites",
  "songs": [ Song, ... ]
}
```

### Queue and playback state

```json
// GET /queue
{
  "queue": [ Song, ... ],
  "current": Song | null,
  "repeat": "off"
}
```

```json
// GET /now-playing
{
  "current": Song | null,
  "queue_len": 12,
  "elapsed_secs": 42.5,
  "duration_secs": 240.2,
  "paused": false,
  "playing": true,
  "volume": 0.8,
  "repeat": "all"
}
```

The queue never contains the current song; a track is moved out of the queue
when playback starts.

### Download job

```json
{
  "id": 1,
  "name": "Radiohead - Everything In Its Right Place",
  "status": "running",
  "song_id": null,
  "error": null,
  "started_at": 1750000000,
  "finished_at": null,
  "source": "ytsearch1:Radiohead - Everything In Its Right Place"
}
```

`status` is one of `queued`, `running`, `done`, `failed`. `song_id` is filled
in once the download has been registered in the library.

### History entry and stats

```json
// HistoryEntry
{
  "id": 91,
  "song_id": 7,
  "song_name": "Radiohead - Everything In Its Right Place",
  "played_at": 1750000123,
  "duration_secs_played": 185
}
```

```json
// GET /stats
{
  "total_plays": 412,
  "total_secs": 98765,
  "top_songs": [
    { "song_id": 7, "name": "Radiohead - Everything In Its Right Place", "plays": 33, "total_secs": 7920 }
  ]
}
```

`played_at` and the other timestamps are Unix seconds.

### Search candidates

```json
// GET /search/yt
{
  "query": "radiohead everything in its right place",
  "limit": 10,
  "results": [
    {
      "id": "dQw4w9WgXcQ",
      "title": "Radiohead - Everything In Its Right Place (Official Audio)",
      "uploader": "Radiohead - Topic",
      "duration_secs": 240.0,
      "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
      "thumbnail": "https://i.ytimg.com/...",
      "view_count": 12345678
    }
  ]
}
```

`/search/yt/ranked` returns the same envelope, but each result also carries:

```json
{
  "id": "...",
  "title": "...",
  "uploader": "...",
  "duration_secs": 240.0,
  "url": "...",
  "thumbnail": "...",
  "view_count": 12345678,
  "score": 187,
  "breakdown": {
    "total": 187,
    "official_channel": 40,
    "official_phrase": 30,
    "title_match": 50,
    "title_similarity": 100,
    "artist_match": 25,
    "artist_similarity": 20,
    "duration": 10,
    "fan_upload": 0,
    "view_count": 12,
    "lyrics": 0,
    "reaction": 0,
    "remix": 0,
    "live": 0,
    "spam": 0,
    "instrumental": 0,
    "karaoke": 0,
    "cover": 0,
    "type_beat": 0,
    "compilation": 0
  },
  "flags": ["official"]
}
```

`score` is the same value as `breakdown.total`. `flags` may include `official`,
`lyrics`, `live`, `reaction`, `remix`, `nightcore`, `slowed`, `bass-boosted`,
`reverb`, `instrumental`, `karaoke`, `cover`, `type-beat`, `compilation`,
`long`, `short`, `artist-mismatch`, `fan-upload`.

### `/pick`

The response is tagged by `kind`:

```json
{
  "kind": "auto",
  "url": "https://www.youtube.com/watch?v=...",
  "title": "Radiohead - Everything In Its Right Place (Official Audio)",
  "uploader": "Radiohead - Topic",
  "score": 187,
  "duration_secs": 240.0,
  "flags": ["official"],
  "decision": { "margin": 55, "required_margin": 30, "equivalent_count": 1 }
}
```

```json
{
  "kind": "needs_choice",
  "candidates": [ RankedCandidate, ... ],
  "top_score": 187,
  "runner_up_score": 160,
  "margin": 27
}
```

```json
{ "kind": "empty", "query": "no such song", "message": "no yt-dlp results for \"no such song\"" }
```

`margin` on `/pick` is the required lead for an auto-pick (default 30); it can
be overridden with `?margin=N`. Hard veto flags (lyrics, reaction, cover,
artist-mismatch, …) and fan uploads never auto-pick.

### Metadata

```json
// GET /metadata/status
{
  "acoustid_configured": true,
  "musicbrainz_enabled": true,
  "fpcalc_available": true,
  "auto_recognize": true,
  "write_tags": true,
  "total_songs": 120,
  "complete_songs": 96,
  "missing_songs": 24
}
```

```json
// GET /metadata/settings  (POST replaces this whole object)
{
  "auto_recognize": true,
  "write_tags": true,
  "acoustid_api_key": "",
  "min_confidence": 0.7,
  "multi_match_policy": "skip",
  "use_musicbrainz": true,
  "user_agent": "sjnmusicd/0.1 (https://github.com/sjnmusic)",
  "musicbrainz_endpoint": "https://musicbrainz.org/ws/2",
  "acoustid_endpoint": "https://api.acoustid.org/v2/lookup",
  "cache_days": 30
}
```

```json
// POST /metadata/test
{ "ok": true, "message": "acoustid reachable" }
```

### Scan status

`POST /metadata/scan` returns `202` with the initial `ScanStatus`; `GET
/metadata/scan` returns the live value. `POST /metadata/scan/cancel` returns
`{"cancelled": true|false}`.

```json
{
  "running": true,
  "finished": false,
  "cancelled": false,
  "force": false,
  "total": 120,
  "already_complete": 96,
  "missing": 24,
  "processed": 5,
  "recognized": 4,
  "not_recognized": 1,
  "skipped": 0,
  "updated_database": 4,
  "updated_files": 4,
  "errors": 0,
  "current_song": "Artist - Title",
  "current_step": "fingerprinting",
  "started_at": 1750000000,
  "finished_at": null,
  "result": null,
  "message": null
}
```

`result` becomes a `ScanResult` (`total`, `already_complete`, `missing`,
`scanned`, `recognized`, `not_recognized`, `skipped`, `updated_database`,
`updated_files`, `errors`, `cancelled`) once the scan finishes.

### Other acknowledgements

| Route | Response |
| ----- | -------- |
| `POST /import` | `{"imported": [Song, ...], "failed": [{"path": "...", "error": "..."}]}` |
| `POST /init` | `{"job_id": 1, "status": "queued"}` |
| `POST /init/batch` | `{"job_ids": [1, 2, ...]}` |
| `POST /play/all` | `{"queued": 120}` |
| `POST /skip` | `{"skipped": Song}` or `{"skipped": null}` |
| `POST /queue/remove` | `{"removed": Song, "len": 11}` |
| `POST /queue/jump` | `{"playing": Song, "len": 11}` |
| `POST /queue/reorder` | `{"reordered": true, "len": 12}` |
| `POST /queue/shuffle` | `{"shuffled": true, "len": 12}` |
| `POST /queue/clear` | `{"cleared": true}` |
| `POST /pause`, `/resume` | `{"ok": true, "label": "pause"}` / `{"ok": true, "label": "resume"}` |
| `POST /seek` | `{"ok": true, "label": "seek", "secs": 12.5}` |
| `GET /repeat` | `{"mode": "off"}` |
| `POST /repeat` | `{"mode": "one"}` |
| `GET /volume` | `{"volume": 0.8, "playing": true, "paused": false}` |
| `POST /volume` | `{"volume": 0.8, "ok": true}` |
| `POST /metadata/recognize`, `/metadata/sync` | `{"queued": true, "song_id": 7}` |
| `POST /playlists/{name}/add` | `{"playlist": Playlist, "added": Song}` |
| `DELETE /playlists/{name}` | `{"deleted": "favourites"}` |

## Example session

```bash
curl -s http://127.0.0.1:14567/songs
curl -s -X POST http://127.0.0.1:14567/play -d '{"query": "kid a"}'
curl -s http://127.0.0.1:14567/now-playing
curl -s -X POST http://127.0.0.1:14567/pause
curl -s -X POST http://127.0.0.1:14567/init -d '{"song": "Radiohead - Everything In Its Right Place"}'
curl -s http://127.0.0.1:14567/downloads
```

With authentication enabled, add
`-H "Authorization: Bearer $(curl -s http://127.0.0.1:14567/auth/token | jq -r .token)"`
(or just call from the same host, where loopback peers are trusted).
