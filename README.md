# deepflow

Write mermaid on the left, see it rendered on the right, updating as you type.
`mermaid.live`, but in the terminal.

```
cargo run -- diagram.mmd     # or no argument for a scratch buffer
```

`^S` saves, `^Q` quits.

## Requirements

- Node, and a Chrome/Chromium that `puppeteer-core` can find (`channel: "chrome"`).
- `cd sidecar && npm install` once.
- Inside tmux: `set -g allow-passthrough on`. Without it the preview falls back to
  unicode halfblocks and the pane title says so.

## How it works

Real mermaid, not ASCII art. A Node sidecar (`sidecar/render.mjs`) keeps one
Puppeteer page alive and renders into it on demand, so a render costs ~85ms
instead of the 1-2s a fresh `mmdc` invocation needs. The Rust side speaks JSON
lines to it and draws the resulting PNG with `ratatui-image`.

Keystrokes are debounced 120ms, and the last good diagram stays on screen while
the next render is in flight, so editing never blanks the preview. Parse errors
appear in the status bar with the line number mermaid reports.

Measured on this machine, per preview update: 120ms debounce, then 65ms (3 nodes)
to 160ms (30 nodes) in mermaid, ~5ms to resize and encode the image, and ~0.2ms
per frame to draw it. The image path is ~2% of the total, so ASCII-art rendering
would not make this feel faster. First render pays ~700ms of Chrome boot.

## Checks

```
cargo test                      # sidecar -> png -> protocol -> painted cells
cd sidecar && npm run selfcheck  # render, parse error with line number, recovery
```

## Known limits

- The error row is named in the status bar, not marked in the editor gutter —
  `tui-textarea` keeps its scroll offset private, so an accurate marker would
  mean forking it.
- Wide diagrams get unreadable at half-window width. No zoom or pan yet.
- Graphics-capability detection runs as a child process (`--probe-graphics`);
  see the comment on `PROBE_FLAG` in `src/main.rs` for why.
