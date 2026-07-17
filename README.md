# OxieDraw

A drawing app, with targeting performance, simplicity and great user experience.
The idea of the project to make a competitor to ProCreate on IPad, but
for Linux and other Desktops.

![OxieDraw banner](data/splash/banner.png)

## Roadmap

- [x] Basic pen and mouse drawing
- [x] Layers
- [x] Basic Raster Filters
- [x] More Drawing tools
  - [x] Shapes
  - [x] Selections
  - [x] Masks
  - [x] Gestures (like in ProCreate)
- [x] Adjustment layers
  - [x] Stroke
  - [x] Blur
  - [x] Hue/Saturation/Brightness
- [ ] Advanced brush engine
  - [x] Parameters-driven brushes
  - [x] Patterns
  - [x] _realistic_ brush
  - [ ] custom shader/code driven brush
- [x] Text support
- [ ] Vector support
  - [ ] Vector import
  - [ ] Vector basic transform
  - [ ] Vector editing transform
- [ ] Advanced image tools
  - [ ] Object extraction
  - [ ] Photoshop-like patch-tool
  - [ ] Professional color-correction tools
- [ ] Linux Release
  - [ ] Binaries and AppImage
  - [ ] ARM and Asahi Linux support
  - [ ] Flatpak file
  - [ ] FlatHub publish
- [ ] Windows Release
- [ ] MacOs Release

## Agenda

- Simple & Clean UI without reducing functionality
- Proper Touchscreen support
- Should run on as many platforms as possible, without functionality deviations
- Licensing
  - This code has GPLv3 license

## Stack

- Rust
- GTK + LibAdwaita (relm4)
- Vulkan (ash)

## LLMs and AI usage (read carefully before spamming @Claude or whatever)

While I'm personally not a fan of using LLMs and AI, I think it
greatly accelerates development of project, especially in
refactoring and atomic parts.

However, it doesn't mean that you can just give an LLM
full control and ask to do features for you. So in this case,
it's strictly forbid using LLM Agents for automated contributions
in the means of "@Gok, Implement me X feature"

### Not OKs

- You ask AI to complete the process from start to finish
- You don't review nor test the result and just open PR for it
- You should **never add AI Generative Image** functionality to this project.
  This is a drawing app, not a midjourney
- You reply with an AI-Generated reply

### OKs

- You ask AI to "implement function A that has
  input A, B, C and outputs D by these specific set of rules" (at the
  end of the day, writing code is not the useful job,
  useful job is making the final product)
- You review each line of the LLM output and fix it yourself if needed.
- You have full responsibility of the code that an LLM made for you and
  you are OK to deal with it, even if you will be
  asked to fix a bug, that an LLM can't fix.
