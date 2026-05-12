# Design System

## Overview

Toggl Jira Sync uses a restrained product interface for a local desktop utility. The visual system should make sync state, configuration, and write safety easy to scan without becoming an Atlassian clone or a terminal-themed developer tool.

The current Tauri UI establishes a light workspace, a compact navigation rail, Jira-adjacent blue accents, Inter-based typography, white panels, simple tables, status lozenges, and visible form controls.

## Design Intent

A person reviews sync state on a desktop screen during normal work hours, likely while switching between Toggl and Jira. The app should feel steady and legible, not dramatic. Light mode fits this scene because the app is used alongside browser and productivity tools, often in office lighting.

## Color Palette

Use OKLCH for new design tokens. Existing hex values may remain until the UI is actively refactored, but future work should migrate toward these tinted equivalents.

### Core Tokens

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

Use Inter, with system fallbacks. Keep type practical and readable.

```css
font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
```

Recommended scale:

- Page title: 28px, 700 to 800 weight, slight negative letter spacing
- Section heading: 18px, 700 weight
- Body and form labels: 14px to 15px, 400 to 600 weight
- Table text and metadata: 13px, 400 to 600 weight
- Eyebrow labels: 12px, 700 weight, uppercase with modest tracking

Keep body text under 75ch. Use weight and spacing before adding decorative color.

## Layout

The product uses an app shell:

- Narrow left navigation rail for primary views
- Topbar for page title and global actions
- Main workspace with overview and configuration views
- Overview combines metrics, worklog table, and issue details
- Configuration uses a structured form with grouped Jira site settings

Spacing should vary by information density:

- Shell rail: compact, 56px to 72px wide
- Topbar: 18px to 32px padding depending on viewport
- Workspace sections: 18px to 32px padding
- Panel heads: 16px to 18px padding
- Table rows: compact but readable, around 12px vertical padding

Avoid nesting cards. Prefer one clear panel for a table or form group, then use dividers, headings, and spacing inside it.

## Components

### Navigation Rail

A compact vertical rail is appropriate. Keep the brand mark simple and avoid making the product look officially affiliated with Toggl, Jira, or Atlassian. Use clear `aria-label` values and visible active/focus states.

### Topbar Actions

Primary actions belong in the topbar only when they affect the current view. `Dry-run` and `Sync now` must stay visually distinct. The write action should be primary, but the dry-run should remain prominent enough to encourage safe preview.

### Metrics

Metrics are useful for operational scanning, but avoid turning them into a hero-metric dashboard. Keep them compact and subordinate to the worklog list. If future designs need stronger hierarchy, use grouped status summaries rather than larger decorative numbers.

### Worklog Table

The table is a trust surface. Prioritize legibility, stable columns, issue links, and status labels. Rows should support keyboard focus and selection. Empty, loading, and error states should explain what the user can do next.

### Issue Detail Panel

Use the detail panel for inspection and recovery guidance. It should answer why an entry did or did not sync, which Jira link was used, and whether a recovery action is relevant.

### Configuration Form

Configuration should feel safe, not overwhelming. Group related fields, preserve existing secret values by default, and make enabled or scheduled states explicit. Labels should be plain enough for users who do not know the CLI config file format.

### Status Lozenges

Use status lozenges for synced, skipped, running, failed, and pending states. Do not rely on color alone; include readable text. Keep saturation modest so errors are noticeable without making the interface feel alarming.

## Interaction

Focus states must be visible on buttons, nav items, inputs, table rows, and links. Keyboard navigation should reach the rail, global actions, filters, table rows, details panel actions, and configuration fields in a predictable order.

Loading states should preserve layout. Use small inline indicators for buttons and status areas. Avoid full-screen blocking states unless the app cannot safely continue.

## Motion

Use motion sparingly. Current spinner motion is acceptable for button loading. Future animation should use opacity and transform only, with ease-out timing. Do not animate layout properties.

Recommended easing:

```css
transition-timing-function: cubic-bezier(0.16, 1, 0.3, 1);
```

## Responsive Behavior

The desktop window defaults to 1200 by 760. Support narrower windows by stacking the detail panel beneath the worklog list, moving filters into a vertical stack, and preserving the navigation rail. Avoid hiding safety-critical actions behind menus.

## Accessibility Requirements

- Preserve clear focus outlines and logical tab order
- Keep action labels explicit, especially dry-run and sync actions
- Do not communicate status by color alone
- Keep contrast strong for text, borders, form fields, and selected rows
- Ensure secret fields explain when blank values preserve existing credentials
- Make background sync and scheduler state clear in copy and controls

## Implementation Notes

Current implementation files:

- `index.html` defines the app shell, overview, table, detail panel, and configuration form
- `src/styles.css` defines the current visual language and responsive behavior
- `src/main.js` controls view switching, sync actions, table rendering, and configuration state
- `src-tauri/tauri.conf.json` defines the desktop window title and size

Future UI work should preserve the calm utility direction while migrating colors into named OKLCH tokens. Do not add decorative glass effects, gradient text, side-stripe accents, or generic SaaS card grids.
