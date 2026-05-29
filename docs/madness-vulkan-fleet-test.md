# Vulkan fleet-breadth test (the one open distribution question)

Both tracks now build green and are validated on the dev PC:
- **CPU baseline** (`minutes-desktop-windows-x64-setup.exe`): ~2.4s/call live
  whisper (audio_ctx + 16-thread default). Keeps up with natural meeting
  pace; thin margin; works on any Windows x64.
- **Vulkan** (`minutes-desktop-windows-x64-vulkan-setup.exe`): ~50–250ms/call
  on an RTX 4070 SUPER. Panel chip reads `backend = vulkan`, partials on,
  near-instant flashes, `dropped_chunks=0`.

## The decision this test settles

Can the **Vulkan build be the single universal artifact**, or do we ship
two installers?

- The Vulkan build uses *any* Vulkan device present. Almost every modern
  machine has one — including integrated Intel/AMD graphics, which are still
  far faster than CPU whisper. Only a host with no Vulkan driver at all needs
  the CPU fallback.
- If the Vulkan build (a) launches and (b) either uses the integrated GPU or
  falls back to CPU cleanly on a no-discrete-GPU laptop, then **one Vulkan
  installer covers the whole fleet** — no "which one do I download?" for
  non-technical hosts.
- If it's shaky on integrated-only hosts, we keep the two-track split (CPU
  default + optional Vulkan).

The dev PC has a discrete 4070, so it can't answer this. A representative
fleet laptop (integrated graphics, no discrete GPU) can.

## What to send the colleague

The Vulkan installer (`minutes-desktop-windows-x64-vulkan-setup.exe`) plus
the message below. Include the CPU installer too as a safety net in case the
Vulkan build won't launch at all on their machine.

## Message to forward (non-technical)

> Quick 5-minute test for the All-Hands buzzword game. Two things to know:
>
> 1. Install `minutes-desktop-windows-x64-vulkan-setup.exe`. Windows may warn
>    "unknown publisher" — click **More info → Run anyway** (it's an unsigned
>    prototype, that's expected).
> 2. Open **Minutes Madness** from the app. On first run it'll offer to
>    download a small speech model — let it (takes a few seconds).
>
> Then start a live recording and say a few buzzwords out loud — "synergy,"
> "bandwidth," "pivot," "AI," "circle back." Watch the bracket.
>
> Tell me:
> - **Did the app open and start recording OK?** (Or did it fail to launch?)
> - **At the top of the panel there's a small status line** — does it mention
>   `vulkan` or `cpu`?
> - **When you say a buzzword, how fast does it light up the bracket?**
>   Instant (~1s), a few seconds, or not at all?
> - **What kind of computer is it?** Specifically: does it have a separate
>   graphics card (gaming laptop / desktop with NVIDIA/AMD), or is it a
>   regular work laptop with built-in graphics?
>
> That's it — no settings to change. Thanks!

## Interpreting the result

| Result on integrated-graphics laptop | Conclusion |
|---|---|
| Launches, chip says `vulkan`, flashes fast (sub-second) | Integrated GPU works → **Vulkan is the universal artifact** |
| Launches, chip says `cpu`, flashes ~2-3s | Clean CPU fallback → Vulkan artifact is still safe to ship universally (degrades to the CPU baseline) |
| Fails to launch / crashes | Vulkan build is NOT universal → **keep two-track** (ship CPU default, Vulkan opt-in) |

## Distribution follow-ups (after the result)

- If single-artifact: the bundle id (`com.useminutes.desktop`) is fine as-is.
- If two-track: the CPU and Vulkan installers currently share that bundle id,
  so a host can only have one installed at a time. That's acceptable for a
  one-install-per-host fleet, but if we ever want them to coexist / upgrade
  between them, give the Vulkan build a distinct id
  (e.g. `com.useminutes.desktop.vulkan`).
