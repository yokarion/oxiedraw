# OxieDraw

A drawing app, with targeting performance, simplicity and great user experience.
The idea of the project to make a competitor to ProCreate on IPad, but
for Linux and other Desktops.

Goals are ambitions, so the project Features Roadmap will be limited:

- [x] Basic pen and mouse drawing
- [x] Layers
- [x] Basic Raster Filters
- [ ] More Drawing tools
  - [ ] Shapes
  - [x] Selections
  - [ ] Masks
  - [x] Gestures (like in ProCreate)
- [ ] Full Linux Release + Flatpak
- [ ] Full Windows Release

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
