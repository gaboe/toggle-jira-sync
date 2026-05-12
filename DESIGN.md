# Design System

## Overview

Toggl Jira Sync uses a restrained product interface for a local desktop utility. The design should make sync state, configuration, and write safety easy to scan without becoming an Atlassian clone or a terminal-themed developer tool.

The current Tauri UI establishes a light workspace, a compact navigation rail, Jira-adjacent blue accents, Inter-based typography, white panels, simple tables, status lozenges, and visible form controls. Future CLI, TUI, and desktop work should preserve that calm utility while improving consistency and accessibility.

## Design Intent

A solo developer or consultant reviews sync state on a desktop screen during normal work hours, likely while switching between Toggl, Jira, a terminal, and client work. The app should feel steady and legible, not dramatic. Light mode fits this scene because the app is used alongside browser and productivity tools in ordinary office lighting.

## Color Palette

Use OKLCH for new design tokens. Existing hex values may remain until the UI is actively refactored, but future work should migrate toward these tinted equivalents.

```css
:root {
  --color-surface: oklch(0.985 0.006 255);
  --color-surface-raised: oklch(0.998 0.004 255);
  --color-surface-muted: oklch(0.955 0.012 255);
  --color-border: oklch(0.875 0.018 255);
  --color-text: oklch(0.255 0.055 255);
  --color-text-muted: oklch(0.505 0.045 255);
  --color-accent: oklch(0.525 0.185 255);
  --color-accent-strong: oklch(0.425 0.17 255);
  --color-accent-soft: oklch(0.925 0.045 255);
  --color-success: oklch(0.55 0.13 150);
  --color-success-soft: oklch(0.94 0.045 150);
  --color-warning: oklch(0.72 0.14 80);
  --color-warning-soft: oklch(0.955 0.055 80);
  --color-danger: oklch(0.56 0.18 28);
  --color-danger-soft: oklch(0.94 0.05 28);
}
```

### Existing Palette Mapping

- Deep text: `#172b4d`, migrate to `--color-text`
- App background: `#f4f5f7`, migrate to `--color-surface-muted`
- Sidebar blue: `#0747a6`, migrate to `--color-accent-strong`
- Primary blue: `#0c66e4`, migrate to `--color-accent`
- Soft blue: `#deebff`, migrate to `--color-accent-soft`
- Muted text: `#5e6c84`, migrate to `--color-text-muted`
- Borders: `#dfe1e6`, migrate to `--color-border`

## Typography

Use Inter with system fallbacks. Keep type practical and readable.

```css
font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
```

Recommended scale:

- Page title: 28px, 700 to 800 weight, slight negative letter spacing
- Section heading: 18px, 700 weight
- Body and form labels: 14px to 15px, 400 to 600 weight
- Table text and metadata: 13px, 400 to 600 weight
- Eyebrow labels: 12px, 700 weight, uppercase with modest tracking

Keep body text under 75ch. Use weight and spacing for hierarchy before adding color.

## Layout

- Favor a compact app shell with persistent navigation for desktop GUI work.
- Keep primary actions close to their status context.
- Use tables for sync entries because rows, issue keys, dates, durations, and statuses are the natural data shape.
- Avoid nested cards. Use panels only where they clarify separate tasks such as overview, configuration, details, or recovery.
- Use rhythm rather than uniform padding everywhere: tighter table density, roomier top-level sections, compact controls.

## Components

### Navigation

Use short, concrete labels: Overview, Status, Config, Schedule, Diagnostics. The current location should be obvious without relying only on color.

### Buttons

Primary buttons are reserved for explicit user actions such as dry-run, sync, save, or retry. Dangerous or write-heavy actions need direct labels, not vague verbs.

### Status Lozenges

Use status lozenges for synced, skipped, error, pending, and running states. Pair color with text. Do not rely on color alone.

### Tables

Tables should prioritize issue key, date, duration, site, worklog status, and reason. Keep row selection visible. Preserve horizontal readability before adding decorative density.

### Forms

Configuration fields should use plain labels, helper text where needed, and visible validation. Secrets should remain redacted unless the user explicitly asks to reveal them.

## Motion

Motion should communicate progress or state changes only. Use short ease-out transitions for hover and selection. Avoid bouncing, elastic effects, decorative glows, and animated layout shifts.

## Copy

Use concise operational language:

- Prefer `Dry run complete: 3 would sync, 1 skipped` over celebratory messages.
- Prefer `Sync failed: Jira rate limit` over generic failure labels.
- Prefer `Scheduler disabled` over ambiguous inactive states.

Avoid hype, faux personality, and repeated headings. No em dashes in UI copy.

## Accessibility

- Maintain keyboard access for all primary actions.
- Keep focus states visible.
- Ensure status color is paired with text or iconography.
- Use conservative contrast for text, borders, and form controls.
- Keep tables readable at desktop widths and provide responsive alternatives for narrow windows.
