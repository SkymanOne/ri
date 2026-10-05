// `@earendil-works/pi-tui` for extensions in yapi: pi's own module, with the
// terminal's capabilities pinned before any component renders. Components
// draw into yapi's cells, which carry neither OSC 8 links nor image escapes,
// so pi-tui uses its text fallbacks whatever terminal yapi runs in. Pinning
// hyperlinks also skips pi-tui's `tmux` probe, which would start a process.
import { setCapabilityOverrides } from "yapi:vendor/pi-tui";

export * from "yapi:vendor/pi-tui";

setCapabilityOverrides({ images: null, hyperlinks: false });
