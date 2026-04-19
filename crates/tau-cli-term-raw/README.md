# tau-cli-term-raw

Terminal prompt with async output support for tau.

## Rendering approach

We currently use an **erase-and-redraw** strategy: on every change (keystroke,
async output arrival), we erase the entire prompt area using relative cursor
movement (`CursorUp` to row 0 of the prompt, then `ClearFromCursorDown`), then
redraw the prompt and buffer from scratch. This approach is borrowed from how
fish shell clears its prompt area (see `screen.rs` in the fish-shell repo).

This is simple, correct, and handles multi-line wrapping and async interruption
well. It supports:

- Line editing with the cursor at any position
- Prompt + input wrapping across multiple physical terminal lines
- Async output printed above the prompt without corruption
- Widgets drawn below the prompt (e.g. completion menus, status lines) by
  tracking extra rows and including them in the erase region

If we need to optimize for high-frequency redraws (syntax highlighting on every
keystroke, large completion menus over slow SSH connections), we can adopt
fish's **dual-buffer diff** approach: maintain a "desired" and "actual" virtual
screen buffer, diff them line-by-line, and emit only the escape sequences
needed to transform actual into desired. This minimizes terminal I/O to only
the characters that changed, but adds significant implementation complexity.
For now the erase-and-redraw approach is sufficient.

## References

- fish shell screen rendering: <https://github.com/fish-shell/fish-shell/blob/master/src/screen.rs>
- fish `Screen::update()` for the diff-based repaint logic
