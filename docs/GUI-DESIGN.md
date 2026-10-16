# GUI design principles

The desktop window is a compact, single-column Slint window with its own title bar and a notification-area (tray) icon. Up to v0.1 it used a GNOME-inspired sidebar layout inside the native frame; v0.2.0 keeps that layout's grouping rules (boxed lists, plain labels) but fits them into a small window that can live in the tray.

The references are GNOME's [Header Bars](https://developer.gnome.org/hig/patterns/containers/header-bars.html) and [Boxed Lists](https://developer.gnome.org/hig/patterns/containers/boxed-lists.html). The goal is a quiet, readable control surface where privacy state and explicit actions are easy to distinguish.

## Ten design axioms

1. **Use a calm shell.** A 44 px title bar (logo, name, language, refresh, keep-on-top, minimise to tray, close), a status card with the pause controls, three stored-history numbers, a five-tab segmented control and the scrollable page.
2. **Group related content.** Place related status and preference rows in white, lightly bordered boxed groups. Use semantic sections and headings instead of a wall of independent cards.
3. **Let language and system fonts lead.** Default to Traditional Chinese and provide an English switch. Use a system sans-serif; on Windows prefer Microsoft JhengHei UI for CJK text. Keep labels plain and short.
4. **Reserve one accent.** Use a single blue accent for primary actions, selection, and focus. Use semantic colors for warning, blocked/error, and running states; do not use color alone to communicate state.
5. **Make focus visible.** Every action and navigation item must be keyboard-focusable with a visible focus ring and accessible role/label. Support Space/Return for buttons and navigation. In a destructive confirmation, Cancel has initial focus, so Return at that initial focus does not delete; the user can explicitly focus Confirm and activate it with Return. Escape cancels. Modal focus must trap Tab and Shift+Tab between Cancel and Confirm, and return focus to the originating action after cancellation.
6. **Work at small sizes.** The window opens at 400×660 and must stay usable at 360×560: page content scrolls, buttons use short labels, and English tabs show only icons except for the selected one (the accessible label keeps the full name).
7. **Avoid decorative noise.** Do not add purple/pink gradients, decorative card walls, fake charts, sample records, or invented status. Show only current configuration, status, real aggregate counts, and explicit messages.
8. **Require clear confirmation for consequences.** Confirm configuration application, history deletion, privacy cleanup (conflicting history and all stored analysis results), clearing a pause, and discarding unsaved edits. Explain what changes before asking the user to confirm. Validation alone never writes the config.
9. **Make the custom frame honest.** The title bar is drawn by the app so the window can stay small: it moves the window by drag, its buttons are keyboard-focusable with accessible labels, minimise means "to the tray" and close means "quit this window" (never "stop the sensor"). A 1 px border keeps the frameless window distinct from the desktop.
10. **Keep activation opt-in.** Opening the GUI must not start capture, daemon, OCR, analyzer/model requests, networking, shell hooks, agent setup, or autostart. Actions may change local config or controls only after explicit user input.

## Design tokens

| Token | Use |
|---|---|
| Window background | `#f4f5f9` light / `#0f1117` dark (follows the system colour scheme) |
| Surface (title bar, cards, groups) | White / `#181b24`, 1 px neutral border, 10–14 px radius |
| Primary accent | Indigo `#4f46e5` for the suggested action, selection and focus (white text, 6.3:1) |
| Text | `#141821` / `#e7e9ee`; muted `#5b6272` / `#9aa1b0` for explanations |
| State | Green running, amber paused (with an amber status card), red blocked/destructive, grey inactive; the logo and tray icon change colour with it |
| Icons | Original 24 px stroke SVGs in `crates/gui/ui/icons`, tinted with `colorize`; the logo is a gradient tile |
| Type | 13 px body, 15–18 px bold titles, 11–12 px secondary labels; Microsoft JhengHei UI on Windows |
| Layout | 44 px title bar, 14 px page padding, scrollable page, default window 400×660, minimum 360×560 |

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
- At 360×560, allow the page to scroll rather than shrinking text or overlapping controls.
- Never fill the interface with fixture data during normal startup. Synthetic records belong only to an isolated verification directory.

## Verification boundary

The `--verify-ui` and `--verify-native` paths render the actual Slint component and use the internal action dispatcher, synthetic storage, and Slint key events. They are not physical input tests. The native path opens only ScreenSense's own Winit window with the software renderer. Both require `SCREENSENSE_NO_SCREEN=1` and a new output directory. Candidate `747cc15` passed CI 38272721986; follow-up commits `420a24c` and `bbdae58` passed CI, and commit `970d229` passed CI 38281939681. Commit `911f0cd` passed Windows software/release GUI verification and Linux Xvfb Confirm-save checks in CI 38288020182; its Windows job stopped only during dependency license collection. Use the matching candidate CI run and installer verification report to confirm the collector correction and installer lifecycle checks. A controlled native UI Automation fixture covered selected pause, save, modal, cleanup, and wipe flows. Real-OS foreground focus remains unconfirmed because Windows denied activation. See [GUI.md](GUI.md) for exact coverage and limits.
