# Delivery — PLUGIN.AES67

How it is built, tested and shipped, and where it lands.

← [Publishing](INDEX.md) · [Wiki home](../README.md)

## Build

- **No Dockerfile of its own for the plugin.** `apk-aes67` is one of the binaries in the shared plugin image `apk-plugins:local`, built by `PODS/POD.THICK/DISCOVERY.Engine/Docker/Dockerfile` (`cargo build --release --locked --bins --keep-going` over the `DISCOVERY.Engine/SRC/Cargo.toml` workspace, of which `SRC/` is a member). The runtime stage sets `cap_net_bind_service+eip` on `/usr/local/bin/apk-aes67` and creates `/var/lib/apkaudio/aes67` owned by uid 1000.
- If this crate fails to compile, the image still builds: `apk-aes67` becomes a stub that sleeps, its name is in `/usr/local/bin/FAILED`, and the container reads unhealthy (PLAN-3398.01, in that Dockerfile).
- `Docker/Dockerfile.loopback` builds `apk-aes67-loopback:local` (Debian + kmod + `SRC/loopback/aes67-loopback.sh`). Build context is this repo's own `SRC/` (`docker build -f Docker/Dockerfile.loopback SRC`).

## Test

By hand, before a push — nothing runs these automatically:

```sh
cd PODS/POD.THICK/DISCOVERY.Engine/SRC
cargo test -p apkaudio-aes67
```

The runner's self test is also reachable at run time: `GET /api/selftest`, or the `outgoing/selftest_run` leaf.

## Ship

- **Where it lands:** a container `Plugin-AES67` (and the one-shot `Plugin-AES67-Loopback`) in the compose project `apk-audio` on a bench node, started by DockTor (`K8.runner.py up`) when the node declares the `sound` or `aes67` role. `DISCOVERY.Engine/Docker/docker-compose.yml` `include:`s this repo's compose file.
- **State:** the named volume `apk-audio-aes67-state` (`settings.json`); logs on `apk-audio-logs`.
- **Live URL:** none on the public internet. On a bench: `https://apk.local/plugins/aes67/ui/` through the gateway, and `http://127.0.0.1:8130/` on the node itself.
- **Source:** the repo is pushed with the monorepo's `.apk.scripts/deploy_all.py` (every submodule).

## Not automated

- This repo's CI is the SDK's reusable `plugin-ci.yml@sdk-v0.7` (the tag the crate pins); the monorepo CI (`.github/workflows/ci.yml`) builds and tests nothing here, on purpose — it only FTPS-deploys the website.
- No image registry: `apk-plugins:local` is built on each bench.
- Loading `snd-aloop` persistently across reboots is a manual host step (README, *Seeing the network in your desktop's sound settings*).

## Delivered 2026-10-31: portable file names (no `:` `|` or case twins)

- The monorepo now checks out on Windows and macOS: no tracked name holds `<>:"|?*`, a trailing dot or space, a device name or a case-only twin. Colon folders became dot folders (the POD.x convention).
- References to the renamed folders updated (`K8.runner.py`). Docs/comments only, no behaviour change.

## Delivered from the 2026-10-30 audit

PLUGIN.AES67 is one plugin (`Plugin-AES67` + the one-shot `Plugin-AES67-Loopback`) in the shared
plugin image, with its deliberate own port 8130 and state volume (unchanged). Verified with
`cargo test -p apkaudio-aes67` in the DISCOVERY.Engine workspace (31 passed),
`COMPOSE_PROFILES=aes67 docker compose -f Docker/docker-compose.yml config -q`, and
`docker build -f Docker/Dockerfile.loopback SRC` (built; the throwaway tag removed).

- **SAP, SDP and PTP parsing shared, not re-implemented** (#1, finding 3) — the readers live once in PLUGIN.RAVENNA's crate (`apkaudio-ravenna`, `default-features = false, features = ["serde"]`, pinned by `rev`): `src/sdp.rs` (338 lines) is now a re-export of `apkaudio_ravenna::sdp::{StreamDesc, Profile, parse_stream as parse, generate, conformance_level, channel_order}`; `rtp::Encoding` is `apkaudio_ravenna::sdp::Encoding`; `src/sap.rs` keeps the browser/announcer and reads/writes the header with `apkaudio_ravenna::sap::{parse, encode, msg_id_hash}`; `src/clock.rs` keeps the slave (BMCA, servo, delay) and reads the wire with `apkaudio_ravenna::ptp` (`parse`, `encode_delay_req`, `refclk_clock_id`) — PLUGIN.PTP's parser; `src/ravenna.rs` takes its `/by-name/` codec from `apkaudio_ravenna::rtsp`. New tests `the_pinned_reader_still_reads_the_corpus`, `a_2110_sender_round_trips_with_its_level`.
- **CI pinned** (#2, finding 6) — `.github/workflows/ci.yml` → `plugin-ci.yml@sdk-v0.5`.
- **Loopback image builds from this repo** (#3, finding 5) — compose `context: ../SRC`, `dockerfile: ../Docker/Dockerfile.loopback`, `COPY loopback/aes67-loopback.sh`.
- **HTTP API and settings file versioned** (#4) — every route is under `/api/v1/` (`web.rs` `API_VERSION`, `api_parts`; the pages call `api/v1/…`); the unversioned `/api/…` is the same handler, kept one release and retired at crate 0.3.0; `/api/v2/…` is refused by name. `config.rs` `SETTINGS_SCHEMA = 1` is written into every save; `migrate` reads an absent `schema` as v1 and refuses a newer one by name (the defaults run, `/status` says why). Tests `v1_and_the_legacy_alias_are_one_route_and_other_versions_are_refused`, `a_settings_file_is_versioned_and_a_newer_one_is_refused`. README documents both.
- **Gateway route by prefix** (handoff from W07, WEB.Traffic-Router #4) — README: Portal-Gateway routes `/plugins/<token>/ui/*` to the relay socket `<token>.sock`; `APK_AES67_UPSTREAM` is gone, `APK_PLUGIN_UI_DIR` re-points the socket folder.
- **Shared plugin volumes** — the copied `apk-audio-logs` / `mac-lookup-cache` declarations are `include: ../../../DISCOVERY.Engine/Docker/plugin-volumes.yml`; `aes67-state` (this plugin's own) stays declared here.
- **Kept by design: own `style.css`** (#5) — the pages are served on loopback :8130 and through the relay, neither of which serves `/FrontEnd/shared/apk-palette.css`; reason in PRINCIPLES.md.
- **Kept by design: compose `extends` plugin-base.yml, engine-built binary** (finding 4) and **no bus panel** (finding 7) — the house plugin pattern and the documented port exception; reasons in PRINCIPLES.md.
- **Shared readers by published tag; standalone build verified** — `apkaudio-ravenna` repinned from `rev = "2695d40…"` to `tag = "ravenna-v0.2.0"` (published by the coordinator); `git archive HEAD | tar -x` + `cargo test` outside the monorepo passes against the GitHub tags, and the workspace `cargo test -p` still passes.
- **SDK repinned to `sdk-v0.7`** (a6d6fe1) — `SRC/Cargo.toml` takes the runner and contracts, and `.github/workflows/ci.yml` calls `plugin-ci.yml`, at the same tag; it ships `docker/plugin-volumes.yml`, so plugin CI's standalone compose check resolves. Verified 2026-10-31: `git archive HEAD` alone, `cargo test` against GitHub — 31 pass; plugin-ci's compose step reproduced (`plugin-base.yml` + `plugin-volumes.yml` from `sdk-v0.7`, `docker compose config -q`) passes.
