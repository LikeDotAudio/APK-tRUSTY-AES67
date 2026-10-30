# Delivery — PLUGIN.AES67

How it is built, tested and shipped, and where it lands.

← [Publishing](INDEX.md) · [Wiki home](../README.md)

## Build

- **No Dockerfile of its own for the plugin.** `apk-aes67` is one of the binaries in the shared plugin image `apk-plugins:local`, built by `PODS/POD.THICK/DISCOVERY.Engine/Docker/Dockerfile` (`cargo build --release --locked --bins --keep-going` over the `DISCOVERY.Engine/SRC/Cargo.toml` workspace, of which `SRC/` is a member). The runtime stage sets `cap_net_bind_service+eip` on `/usr/local/bin/apk-aes67` and creates `/var/lib/apkaudio/aes67` owned by uid 1000.
- If this crate fails to compile, the image still builds: `apk-aes67` becomes a stub that sleeps, its name is in `/usr/local/bin/FAILED`, and the container reads unhealthy (PLAN-3398.01, in that Dockerfile).
- `Docker/Dockerfile.loopback` builds `apk-aes67-loopback:local` (Debian + kmod + `SRC/loopback/aes67-loopback.sh`). Build context is the repo root.

## Test

By hand, before a push — nothing runs these automatically:

```sh
cd PODS/POD.THICK/DISCOVERY.Engine/SRC
cargo test -p apkaudio-aes67
```

The runner's self test is also reachable at run time: `GET /api/selftest`, or the `outgoing/selftest_run` leaf.

## Ship

- **Where it lands:** a container `Plugin-AES67` (and the one-shot `Plugin-AES67-Loopback`) in the compose project `apk-audio` on a bench node, started by DockTor (`K8:runner.py up`) when the node declares the `sound` or `aes67` role. `DISCOVERY.Engine/Docker/docker-compose.yml` `include:`s this repo's compose file.
- **State:** the named volume `apk-audio-aes67-state` (`settings.json`); logs on `apk-audio-logs`.
- **Live URL:** none on the public internet. On a bench: `https://apk.local/plugins/aes67/ui/` through the gateway, and `http://127.0.0.1:8130/` on the node itself.
- **Source:** the repo is pushed with the monorepo's `.apk.scripts/deploy_all.py` (every submodule).

## Not automated

- The monorepo CI (`.github/workflows/ci.yml`) builds and tests nothing here; it only FTPS-deploys the website.
- No image registry: `apk-plugins:local` is built on each bench.
- Loading `snd-aloop` persistently across reboots is a manual host step (README, *Seeing the network in your desktop's sound settings*).
