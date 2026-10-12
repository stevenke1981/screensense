# GUI design principles

The desktop window uses a GNOME-inspired layout, implemented in Slint. “Inspired” describes the visual organization; the GUI is not a GTK/libadwaita application and keeps the native operating-system window frame.

The references are GNOME's [Header Bars](https://developer.gnome.org/hig/patterns/containers/header-bars.html) and [Boxed Lists](https://developer.gnome.org/hig/patterns/containers/boxed-lists.html). The goal is a quiet, readable control surface where privacy state and explicit actions are easy to distinguish.

## Ten design axioms

1. **Use a calm shell.** Keep the native OS window frame. Inside it, use a soft-gray header and sidebar, with the current page title and only a few relevant controls in the header.
2. **Group related content.** Place related status and preference rows in white, lightly bordered boxed groups. Use semantic sections and headings instead of a wall of independent cards.
3. **Let language and system fonts lead.** Default to Traditional Chinese and provide an English switch. Use a system sans-serif; on Windows prefer Microsoft JhengHei UI for CJK text. Keep labels plain and short.
4. **Reserve one accent.** Use a single blue accent for primary actions, selection, and focus. Use semantic colors for warning, blocked/error, and running states; do not use color alone to communicate state.
5. **Make focus visible.** Every action and navigation item must be keyboard-focusable with a visible focus ring and accessible role/label. Support Space/Return for buttons and navigation. In a destructive confirmation, Cancel has initial focus, so Return at that initial focus does not delete; the user can explicitly focus Confirm and activate it with Return. Escape cancels. Modal focus must trap Tab and Shift+Tab between Cancel and Confirm, and return focus to the originating action after cancellation.
6. **Work at small sizes.** The window's minimum size is 900×600, with content in a scroll view. Keep 1024×600 usable without clipping or hiding the end of a page.
7. **Avoid decorative noise.** Do not add purple/pink gradients, decorative card walls, fake charts, sample records, or invented status. Show only current configuration, status, real aggregate counts, and explicit messages.
8. **Require clear confirmation for consequences.** Confirm configuration application, history deletion, privacy cleanup (conflicting history and all stored analysis results), clearing a pause, and discarding unsaved edits. Explain what changes before asking the user to confirm. Validation alone never writes the config.
9. **Keep the frame native.** Do not draw a fake title bar or replace system window controls. The in-window header is content; window movement and OS window management remain native.
10. **Keep activation opt-in.** Opening the GUI must not start capture, daemon, OCR, analyzer/model requests, networking, shell hooks, agent setup, or autostart. Actions may change local config or controls only after explicit user input.

## Design tokens

| Token | Use |
|---|---|
| Window background | Warm near-white (`#fafafa`) |
| Header | Soft gray (`#ebebeb`) |
| Sidebar | Light gray (`#f2f2f2`) |
| Group surface | White with a thin neutral border and modest corner radius |
| Primary accent | Blue (`#3584e4`) for selection, suggested action, and focus |
| Text | Dark neutral for primary text; muted gray for secondary explanation |
| State | Green for running, amber for paused, red for blocked/error, neutral gray for inactive |
| Type | 14 px system sans-serif body, larger bold page headings, compact muted secondary labels |
| Layout | 56 px in-window header; fixed navigation sidebar; scrollable page content; minimum window 900×600 |

These values describe the current implementation's starting palette, not a promise of pixel parity with GNOME. Keep contrast, focus, and state meaning intact when adapting them.

## Page behavior

The pages and their data boundaries are specified in [GUI.md](GUI.md). Preserve these boundaries as the UI grows:

- Overview and diagnostics may show actual local state and aggregate counts; they never show captured text or images. When a stored privacy-rule hash differs, hide counts and cached sensor notes, preserve separate daemon state, and require explicit cleanup confirmation; refresh must not delete data.
- Capture & analysis shows capabilities and configuration, not a capture preview or a model probe.
- Preferences is an editor for the real TOML file, with validation and explicit save confirmation. A non-privacy edit such as a language change must preserve history and analysis. A confirmed privacy-policy change cleans up only after atomic config replacement; an unparseable old config must not trigger implicit deletion.
- Stored data exposes an explicit, confirmed wipe, with pause/stop/settings/privacy marks retained.
- Invalid privacy state fails closed and must not leave stale counts or stale worker replies on screen.

## Interaction and responsive rules

- Keep controls reachable by keyboard and make focus visible on buttons, navigation, and modal dialogs.
- When confirmation is open, prevent changing pages or triggering background controls. Capture Escape to cancel and trap Tab/Shift+Tab between Cancel and Confirm; after cancellation, restore focus to the action that opened the dialog. Destructive actions require the explicit confirmation control.
- Keep labels and descriptions aligned within grouped rows. Use muted secondary text to explain consequences, never as the only state indicator.
- At 1024×600, allow the page to scroll rather than shrinking text or overlapping controls.
- Never fill the interface with fixture data during normal startup. Synthetic records belong only to an isolated verification directory.

## Verification boundary

The `--verify-ui` and `--verify-native` paths render the actual Slint component and use the internal action dispatcher, synthetic storage, and Slint key events. They are not physical input tests. The native path opens only ScreenSense's own Winit window with the software renderer. Both require `SCREENSENSE_NO_SCREEN=1` and a new output directory. Candidate `747cc15` passed CI 38272721986; follow-up commits `420a24c` and `bbdae58` passed CI, and commit `970d229` passed CI 38281939681. A controlled native UI Automation fixture covered selected pause, save, modal, cleanup, and wipe flows. The later tested commit `65cdbbc` has a confirmed editor regression in CI 38286292420; its current working-tree focus-token fix has not yet been tested. Real-OS foreground focus also remains unconfirmed because Windows denied activation. See [GUI.md](GUI.md) for exact coverage and limits.
