# Agent Note

Status: implemented
Class: architecture
Date: 2026-11-03

## Problem

The idealist already filed every failure the backend saw (`backend::incident`
pairs each ERROR `LogLine` with a ticket, and `invariants` checks that a turn
error has its `TicketOpened` row). Three gaps remained.

1. **The frontend never reported anything.** `Request::ReportFrontendError`
   existed and had no caller. A GUI panic, or a backend that would not start,
   was a log-panel line and nothing more.
2. **Tickets were out of sight.** They were the fourth block of Settings ›
   Diagnostics, under the demo requests, and loaded only when that panel was
   opened. Nothing in the main window said a ticket had been filed.
3. **A ticket was hard to act on.** The card showed a module path, an origin
   tag and the last message. The root cause, the proposed fix and the files to
   touch were in the markdown body, which the app could not show. *Start fix
   session* was offered only for a diagnosed `harness_bug`.

## Decision

- **Frontend failures go through a queue file.** `frontend::fe_incident`
  appends reports to `idealist_workspace/fe-pending.jsonl` and sends them as
  `ReportFrontendError` at the next `IpcConnected`. When a report is due,
  there is often no backend to send it to. A panic on the UI thread takes the
  window down, and a start failure means the backend is not running. The file
  survives both cases. The panic hook also writes `fe-crash-<ts>.md`, like the
  backend's. Its module names the **first frame in this workspace's
  `crates/`**, not the panic location. The first panic this hook caught was
  an egui assertion, located in the cargo registry. That path is
  machine-specific and is not where the fix goes.
- **Build failures are not reported.** With auto-watch on, a compile error is
  almost always someone's half-finished edit. A ticket for each one would
  bury the real tickets.
- **Protocol v33 adds a digest and a detail request.** `TicketSummary` now
  carries `title`, `hint`, `root_cause`, `fix` and `files`, parsed from the
  body (`Ticket::digest`, each capped at 600 bytes). A card can therefore
  answer *what happened / why / how to fix* from the list alone.
  `GetTicket` returns the whole body for the in-app *Full ticket* view. The
  body is not sent in the list, because the list is re-sent on every ticket
  event and the bodies grow with each investigation.
- **The digest drops the hint's copy of the message.** The analyzer's
  fallback text embeds the failing message ("returned an error: <message>.
  Investigate …"). The card already shows the message, so the digest keeps
  only the advice. The ticket file keeps the full text for the investigator.
- **The list is always loaded.** It is requested on every connect and
  refreshed after any ticket event, with a 400 ms debounce, because a looping
  tool bumps one ticket per call. The sidebar count needs the list whether or
  not Settings is open.
- **Fix with agent works for every open ticket.** `investigate::fix_step`
  picks the prompt's working step by category. A harness bug gets code and a
  test. A model mistake gets a sharper tool description or error. An
  environment or config ticket gets a session that names the change and
  edits code only if the harness could have reported it better.

## Consequences

- Both binaries and `PROTOCOL_VERSION` moved to 33. A v32 GUI shows the
  mismatch chip against a new backend until it is restarted.
- Frontend reports carry no session id, so they are never investigated
  automatically. *Fix with agent* is how they get looked at.
- A ticket written before v33 has no `### Root cause` section if its
  investigation failed. Its card says the notes are in the full ticket.

## Alternatives

- **Have the frontend read ticket files from disk**, as Settings ›
  Integrations does for its files. Rejected: the backend owns the ticket
  store and its write lock, and an id from the frontend has to be checked
  before it becomes a path (`GetTicket` refuses anything that is not hex).
- **Put the body in `TicketSummary`.** Rejected because of the list size,
  described above.
- **Show a toast for every ticket event.** Rejected: an occurrence bump is
  not news. Only a ticket that is new or reopened gets one.
