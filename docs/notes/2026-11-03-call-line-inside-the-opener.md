# Agent Note

Status: implemented
Class: architecture
Date: 2026-11-03

## Problem

`sessions/102` (Qwen 3.6, text protocol) never finished its last request,
"build-girls-prompts". The last three turns ended `bad-call` with zero hops,
and the auto-continue gave up. Over the whole session the tool notice "That
was not a valid tool call" fired 22 times. Twenty of those replies had one
shape:

    <tool_call>
    <function=run-pwsh '(Get-Content main_prompts.txt).Count' > count lines
    </function>
    </tool_call>

The model treats the expectation's ` > ` as the end of the opener tag.
`xml_tag_at` recognised `<function=name>` only when `>` came straight after
the name. So the opener was never stripped, the line began with
`<function=`, and no skill name was found. The notice then told the model
to put "the skill name and its quoted arguments on one line, followed by
` > <expectation>`", which it had already done. Its reasoning said "use the
JSON fence" each time, and each time it sent the same wrapper again.
Seventeen turns ended `bad-call` this way. The other two notices were
empty, reasoning-only replies.

A second cause cost work in the same session. The model wrote large files
in parts and left `'append=true'` off part two or later. That happened
seven times across `poses_front.txt`, `passionate_lovers.txt` and
`main_prompts.txt`. The result, `wrote N bytes to …`, looked the same
whether the file was new or replaced, so each lost part only showed up
later, through a line count. That line-count call was the one the parser
refused.

## Decision

- An opener whose name is followed by spaces and then a quote ends at the
  name (`xml_tag_at`). The rest of the line, made of quoted arguments and
  ` > expectation`, is read as the contract's call line. The strict
  `extract_known` accepts it as well, because what it reads is the
  contract's own line.
- A one-argument skill now peels trailing unquoted `key=value` words
  (`read-file '…' start=16 end=59`) the same way it peels quoted ones. Its
  path therefore stays verbatim and does not go through the
  escape-processing tokenizer, which used to turn `\test_…` into a tab.
- When `write-file` overwrites a non-empty file without `append`, the
  result now says so: `— replaced the N bytes it held; 'append=true' adds
  to the end instead`.

## Consequences

- All 20 refused replies in `sessions/102` now parse to the call the model
  meant. This was checked with a throwaway test over the log.
- New replay scenario `wrapped-call-inline-args`. It fails on the
  2026-09-27 backend with the session's notice and passes with the fix. No
  other recording moved, because no scenario overwrites an existing file.
- Prose that only names a tag still does not count as a call:
  `<function=read-file and …` has no quote after the name.

## Alternatives

- **Reword the notice to name this shape.** Rejected. The model already
  ignored its own plan to switch formats, so accepting the shape is
  cheaper than arguing with the habit (see `sessions/79`, `sessions/94`).
- **Refuse a non-append write to a file written earlier in the turn.**
  Rejected. Rewriting a file in full is legitimate, and saying what was
  replaced is enough for the model to notice.
