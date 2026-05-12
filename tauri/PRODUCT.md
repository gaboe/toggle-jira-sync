# Product Context

## Product

Toggl Jira Sync is a local desktop and CLI utility for syncing Toggl time entries into Jira worklogs. It keeps sync state in a local SQLite database so repeated runs can skip entries that are already synced, recover from interrupted runs, and show the user what will happen before any write happens.

This is an independent community tool. It is not an official Toggl, Jira, or Atlassian product and should not present itself as one.

## Register

Product

Design serves the app workflow: review, configure, dry-run, sync, and recover with confidence. Marketing-style spectacle should be secondary to clarity, safety, and task completion.

## Users

The primary user is a non-technical or lightly technical person who tracks time in Toggl and needs those entries reflected in Jira without learning the full CLI workflow.

Secondary users include solo developers, consultants, and small-team operators who are comfortable with local configuration but still need a calm visual interface for checking sync state and avoiding accidental writes.

## User Context

Users open the app when they need to verify recent worklogs, check whether Toggl entries mapped to the right Jira issues, inspect failures, update local credentials, or run a safe sync. They may be doing this between work sessions, at the end of a day, or after noticing that Jira worklogs look stale.

The experience should reduce anxiety around credentials, scheduling, and write operations. Dry-run and sync states must feel distinct. Recovery and error states should explain next steps without assuming command-line expertise.

## Product Purpose

Help people move time records from Toggl to Jira reliably while preserving local control and making sync state visible.

The interface should answer:

- What will sync?
- What already synced?
- What was skipped or failed?
- Which Jira issue and site did each entry resolve to?
- Is configuration safe enough to run now?
- What should I do if the state looks wrong?

## Brand Personality

Calm utility.

The product should feel trustworthy, quiet, and operationally safe. It can borrow the familiarity of Jira-adjacent patterns, but it should not feel like an enterprise admin console. It should speak plainly and help users understand consequences before writes happen.

## Voice And Copy

Use direct, practical copy. Prefer short labels and clear status messages over cleverness.

Good copy traits:

- Plain language for non-technical users
- Specific next steps after errors
- Explicit distinction between preview actions and write actions
- Calm acknowledgement of incomplete or stale state
- No exaggerated productivity claims

Avoid:

- CLI jargon unless the UI is explicitly showing a command
- Vendor-like enterprise phrasing
- Generic SaaS marketing copy
- Ambiguous labels for destructive or write actions

## Anti-References

Do not become an enterprise Jira clone. The app integrates with Jira, but it should avoid heavy admin-console density, corporate chrome, and Atlassian-product mimicry beyond useful familiarity.

Do not become a developer terminal app. The CLI exists, but the desktop surface should not assume command-line fluency or use dense terminal aesthetics as its default.

Also avoid generic SaaS dashboard tropes where possible, especially decorative metrics, unnecessary gradients, and repeated identical card grids.

## Accessibility And Usability Priorities

Keyboard-first interaction matters. Forms, navigation, filters, tables, and action buttons should have visible focus states and sensible tab order.

Low-anxiety clarity matters. The app should make it obvious when an action is only a dry-run, when it writes to Jira, when background sync is active, and when credentials are stored or preserved locally.

Use readable contrast, stable layout, and explicit status feedback. Do not rely on color alone for sync outcomes.

## Strategic Design Principles

1. Safety before speed. The user should always know whether an action previews, writes, schedules, or stores configuration.
2. Local control is a feature. Configuration, credentials, SQLite state, and scheduler behavior should feel inspectable and reversible.
3. Show the ledger, not just the result. Worklog history, issue resolution, skipped entries, and errors are the product's trust surface.
4. Make Jira familiar without copying Jira. Use integration familiarity where it helps, but keep the app lighter and calmer.
5. Reduce CLI dependency in the desktop UI. Explain states and actions in the app before sending users to commands.
6. Prefer durable clarity over decorative polish. Visual design should support scanning, confidence, and recovery.
