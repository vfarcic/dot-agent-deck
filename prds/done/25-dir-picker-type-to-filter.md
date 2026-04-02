# PRD #25: Type-to-Filter in Directory Picker

**Status**: Complete (2026-04-02)
**Priority**: Medium
**Created**: 2026-04-01
**GitHub Issue**: [#25](https://github.com/vfarcic/dot-agent-deck/issues/25)

## Problem Statement

When creating a new pane, users must scroll through a potentially long list of directories in the DirPicker popup. There is no way to search or narrow results, making it tedious to find the target directory — especially in home directories or project roots with many subdirectories.

## Solution Overview

Add a `/`-triggered filter mode to the directory picker, consistent with the existing session filter UX pattern. When active, typed characters filter the directory list using case-insensitive substring matching on directory names.

### User Flow

1. User presses `n` to open the directory picker
2. User presses `/` to enter filter mode
3. A filter input row appears showing `/ {typed text}`
4. As the user types, the directory list narrows to matching entries
5. The `..` (parent directory) entry always remains visible regardless of filter
6. User presses `Enter` to accept the filter and return to navigation mode
7. User navigates the filtered list with `j`/`k` and selects with `Space` or enters with `l`/`Enter`
8. `Esc` clears an active filter; a second `Esc` closes the picker
9. Navigating into a subdirectory or going up resets the filter

## Scope

### In Scope
- `filter_text`, `filtering`, and `filtered_indices` fields on `DirPickerState`
- `refilter()` method that rebuilds visible indices from entries and filter text
- `/` keybinding in navigation mode to enter filter mode
- Character input, backspace, Esc, and Enter handling in filter mode
- Case-insensitive substring matching on directory name (not full path)
- `..` entry always passes filter
- Filter resets on `enter_selected()` and `go_up()` (via `refresh()`)
- Directory picker navigation wraps around when moving past the first or last entry
- Visual filter input row in the popup
- Updated footer help text showing `/: filter` hint
- Empty state message: `(no matching directories)` when filter yields no results
- Unit tests for filter behavior

### Out of Scope
- Fuzzy matching (future enhancement)
- Regex or glob pattern support (future enhancement)
- Filtering by full path rather than directory name (future enhancement)
- Persisting filter text across picker sessions (future enhancement)

## Technical Approach

### DirPickerState Extension (`src/ui.rs`)

Add three fields to the existing struct:

```rust
filter_text: String,          // current filter query
filtering: bool,              // true when user is typing in filter input
filtered_indices: Vec<usize>, // indices into `entries` matching the filter
```

Add a `refilter()` method that:
1. Clears `filtered_indices`
2. Iterates `entries` — `..` always passes; others match if `filter_text` is empty or directory name contains `filter_text` (case-insensitive)
3. Resets `selected` and `scroll_offset` to 0

### Key Handling (`src/ui.rs`)

The `handle_dir_picker_key()` function branches on `picker.filtering`:

**Filter mode (`filtering == true`):**
- `Char(c)` → append to `filter_text`, call `refilter()`
- `Backspace` → pop last char; if empty, exit filter mode; call `refilter()`
- `Esc` → clear `filter_text`, exit filter mode, call `refilter()`
- `Enter` → exit filter mode (keep filter active, return to navigation)
- `Up/Down` → navigate the filtered list

**Navigation mode (`filtering == false`):**
- `/` → enter filter mode
- `Esc` → if filter active: clear filter + `refilter()`; if no filter: close picker
- All other existing bindings preserved (`j/k/h/l/q/Space/Enter/arrows`)
- Navigation uses `filtered_indices.len()` for bounds and wraps when moving past start or end

### Rendering Changes (`src/ui.rs`)

In `render_dir_picker()`:
- When `filter_text` is non-empty or `filtering` is true, render a `/ {filter_text}` line between the current-dir header and entry list (with cursor indicator when typing). Reduce `max_visible` by 1.
- Iterate `filtered_indices` instead of raw `entries` for the directory list
- Show `(no matching directories)` when filter is active and no entries match
- Footer text variants:
  - Filtering active: `"Type to filter  Enter: done  Esc: clear  ↑↓: navigate"`
  - Filter present (not typing): `"/: edit filter  Space: select  Enter/l: open  h/BS: up  Esc: clear"`
  - No filter: `"/: filter  Space: select dir  Enter/l: open  h/BS: up  Esc: cancel"`

### Refresh Resets Filter

The existing `refresh()` method (called by `enter_selected()` and `go_up()`) will additionally clear `filter_text`, set `filtering = false`, and call `refilter()`. This ensures the filter resets when navigating directories.

## Success Criteria

- Pressing `/` in the directory picker enters filter mode with visible text input
- Typed characters progressively narrow the directory list
- Matching is case-insensitive and matches anywhere in the directory name
- `..` entry is always visible regardless of filter
- Up from the first entry jumps to the last (and Down from the last jumps to the first), honoring active filters
- `Esc` clears filter; second `Esc` closes picker
- Navigating into a directory or going up resets the filter
- Selecting a filtered entry works correctly (Space, Enter)
- Empty filter results show a clear message
- All existing tests pass
- New unit tests cover filter behavior

## Milestones

- [x] `DirPickerState` extended with `filter_text`, `filtering`, `filtered_indices` fields and `refilter()` method
- [x] `handle_dir_picker_key()` updated with filter mode branching and `/` keybinding
- [x] `enter_selected()` indexes through `filtered_indices`; `refresh()` resets filter state
- [x] Directory picker navigation wraps when moving past start/end entries
- [x] `render_dir_picker()` updated with filter input row, filtered entry iteration, and dynamic footer
- [x] Unit tests for filter narrowing, `..` always visible, Esc behavior, and filter reset on navigation
- [x] All existing tests passing

## Key Files

- `src/ui.rs` — `DirPickerState` struct, `handle_dir_picker_key()`, `render_dir_picker()`, tests

## Risks

- **Key conflict avoidance**: Using `/` to enter filter mode avoids conflicts with vim-style navigation keys (`h/j/k/l/q`). This is consistent with the existing session filter pattern.
- **Performance**: `refilter()` runs on every keystroke. For typical directory listings (< 1000 entries), this is negligible. No optimization needed.
- **Edge case — all filtered out**: When no entries match (including no `..` at root), the picker shows a message and navigation keys are no-ops. User can `Esc` to clear filter.
