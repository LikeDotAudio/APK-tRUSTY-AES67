# Smoke test — PLUGIN.AES67

Is `PLUGIN.AES67` deployed? Written by `.apk.scripts/smoke.py` from the module's own shape; what each check means is in that script's header.

← [Wiki home](../README.md)

**✅ PASS** — 3/3 checks, since 2026-10-30 12:51 EDT

| Check | Target | Result | Detail |
|---|---|---|---|
| pushed | [APK-tRUSTY-AES67](https://github.com/LikeDotAudio/APK-tRUSTY-AES67.git) | ✅ | HEAD is on origin |
| container | `Plugin-AES67` | ✅ | running, healthy |
| container | `Plugin-AES67-Loopback` | ✅ | one-shot, exited 0 |

Re-run: `python3 .apk.scripts/smoke.py PODS/POD.THICK/PLUGINS/PLUGIN.AES67`

<!-- smoke:sig ab19a4552f65 since 2026-10-30 12:51 EDT -->
