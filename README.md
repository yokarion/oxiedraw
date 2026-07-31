<div align="center">

<img src="data/splash/banner.png" alt="OxieDraw" width="820">

# OxieDraw

**A fast, clean drawing app for Linux and other desktops.**<br>
An easy drawing experience - GPU-accelerated, and built in Rust.

<p>
  <a href="https://github.com/yokarion/oxiedraw/actions/workflows/ci.yml"><img src="https://github.com/yokarion/oxiedraw/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/license-GPLv3-3584e4" alt="License: GPLv3">
  <img src="https://img.shields.io/badge/built%20with-Rust-000000?logo=rust&logoColor=white" alt="Built with Rust">
  <img src="https://img.shields.io/badge/GTK4-libadwaita-3584e4?logo=gnome&logoColor=white" alt="GTK4 + libadwaita">
  <img src="https://img.shields.io/badge/Vulkan-ash-a41e22?logo=vulkan&logoColor=white" alt="Vulkan (ash)">
  <img src="https://img.shields.io/badge/platform-Linux-333333?logo=linux&logoColor=white" alt="Platform: Linux">
</p>

<p>
  <a href="https://oxiedraw.yokarion.com"><b>Documentation</b></a> &nbsp;-&nbsp;
  <a href="https://oxiedraw.yokarion.com/getting-started/download"><b>Download</b></a> &nbsp;-&nbsp;
  <a href="https://oxiedraw.yokarion.com/getting-started/roadmap"><b>Roadmap</b></a> &nbsp;-&nbsp;
  <a href="https://oxiedraw.yokarion.com/development/overview"><b>Development</b></a>
</p>

</div>

---

## Overview

OxieDraw targets performance, simplicity and a great user experience. The idea is
a real competitor to flagship proprietary apps. Linux artists deserve better
open-source tools. Just as comfortable with a stylus, mouse or trackpad.

## Highlights

- **GPU-accelerated** - a Vulkan (ash) pipeline keeps strokes low-latency even on large canvases and integrated GPUs.
- **Layers, groups and blend modes** - a full layer stack composited on the GPU.
- **Non-destructive adjustments** - Hue/Saturation, Blur and Stroke with painted masks.
- **Advanced brush engine** - parameter-driven brushes, textured grain, and a Krita-derived realistic brush.
- **Text and typography** - a Figma-like text tool with per-range styling and embedded fonts.
- **Selections, masks and guides** - shapes, gestures and ProCreate-style symmetry.

See the [Roadmap](https://oxiedraw.yokarion.com/getting-started/roadmap) for what
is done and planned (source: [docs/1.getting-started/3.roadmap.md](docs/1.getting-started/3.roadmap.md)).

## Agenda

- Simple and clean UI without reducing functionality
- Proper touchscreen support
- Runs on as many platforms as possible, without functionality deviations
- Licensed under GPLv3

## Stack

| Layer     | Technology            |
| --------- | --------------------- |
| App       | **Rust**              |
| UI        | **GTK4 + libadwaita** |
| Rendering | **Vulkan** (ash)      |

## Development

Building from source, the project layout, and the contribution rules live in the
[Development guide](https://oxiedraw.yokarion.com/development/overview)
(source: [docs/4.development/](docs/4.development/)).

## LLMs and AI usage

LLMs are fine for small, scoped, fully-reviewed changes - but automated
"implement feature X for me" agent contributions are not, and AI generative-image
features are out of scope for this project. Read the full policy before
contributing: [AI & LLM usage](https://oxiedraw.yokarion.com/development/ai-and-llm-usage)
(source: [docs/4.development/2.ai-and-llm-usage.md](docs/4.development/2.ai-and-llm-usage.md)).

## License

OxieDraw is licensed under the [GNU General Public License v3.0](LICENSE).
