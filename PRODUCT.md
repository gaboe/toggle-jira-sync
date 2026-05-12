# Product Context

## Register

Product. Toggl Jira Sync is a local CLI, TUI, and Tauri desktop utility where design serves reliable task completion, not marketing expression.

## Product Purpose

Toggl Jira Sync helps a solo developer or consultant sync Toggl time entries into Jira worklogs with local state, repeatable runs, recovery from interruptions, and clear visibility into what was synced, skipped, or failed.

The interface should make potentially destructive write actions feel deliberate and understandable. Users should be able to check sync state, run dry runs, start real syncs, manage scheduling, and inspect errors without wondering what the tool will touch next.

## Primary Users

The primary user is a solo developer or consultant who tracks time in Toggl and needs that time reflected in Jira for billing, reporting, or client accountability.

They are likely using the tool during normal work, between coding, Toggl, Jira, and client administration. They value speed, local control, and confidence more than spectacle.

## Surfaces

- CLI commands for setup, validation, dry-run, sync, status, recovery, scheduling, and diagnostics.
- Ratatui TUI for keyboard-first local status review and common actions.
- Tauri desktop GUI for a calmer visual overview of sync state, configuration, and safe actions.

## Brand Personality

Quiet utility. The product should feel calm, legible, precise, and dependable. It should behave like a well-kept local tool that respects the user's time and data.

Prefer plain language, visible state, and predictable controls over motivational copy, decorative flourish, or heavy brand personality.

## Strategic Design Principles

1. Make sync safety obvious. Dry-run, real sync, scheduler state, and recovery actions must be visually and verbally distinct.
2. Optimize for scanability. Status, issue keys, durations, dates, errors, and next actions should be readable at a glance.
3. Stay keyboard-first. CLI and TUI flows should remain efficient without mouse dependence, and the GUI should not hide core actions behind pointer-only patterns.
4. Preserve local trust. Emphasize that state and credentials are local, write operations are explicit, and repeated runs are recoverable.
5. Be conservative with contrast. Prefer clear legibility over subtle low-contrast polish.
6. Keep density useful, not stressful. Tables and status summaries are appropriate, but avoid burying the user in admin-dashboard clutter.

## Anti-References

- Cyber terminal aesthetics: no neon-on-black hacker console, decorative glows, or fake command-center drama.
- Generic enterprise dashboards: avoid nested panels, interchangeable metric cards, and overloaded admin-console density.
- Atlassian mimicry: Jira is part of the workflow, but the product should not look like a copied Jira screen.
- AI-generated SaaS tropes: no gradient text, glassmorphism, hero-metric layouts, or identical icon-card grids.

## Accessibility And Usability Priorities

- Keyboard-first flows for CLI, TUI, and desktop usage.
- High scanability for status, errors, and safe write actions.
- Conservative contrast and plain labels for reliable reading under normal desktop working conditions.
- Clear feedback for loading, running sync, success, skipped work, and errors.
