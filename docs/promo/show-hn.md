# Post template: Show HN

> Rules that matter here: the title must be `Show HN: <plain description>`, the first comment is
> yours and should carry the *why*, and the "what it doesn't do" list is the part HN actually
> reads. Replace `⟨…⟩`. Keep it short — long self-posts get skimmed.

---

**Title**: `Show HN: Toolbox Hub – a TUI that turns installed CLI tools into fillable forms`

**Body**:

I kept re-Googling the same `ffmpeg` flags, so I wrote a TUI that lists the CLI tools I already have,
shows a form for the arguments, and keeps the output history. It's a Rust binary; the same binary is
also a normal CLI (`toolbox-hub -s fzf`, etc.).

The part I use most is the shell integration. `eval "$(toolbox-hub widget bash)"` binds `Ctrl-G` to a
picker; the chosen command is **inserted into your current input line** rather than executed, so you
can edit it before hitting Enter. Execution and history stay with your shell — your history shows
`uname -r`, not the wrapper.

⟨screenshot: Ctrl-G picker with the line buffer below it⟩

A few design decisions that might be interesting:

- **Importer that refuses to guess.** `toolbox-hub import navi|cheat|tldr <file>` converts existing
  cheat sheets into recipes, but anything with pipes, redirects or command substitution is reported
  as "display only" instead of being naively split into argv. Reconstructing shell semantics from a
  string is where this goes wrong, so it says so.
- **Three safety levels.** `safe` / `caution` / `destructive`; destructive recipes need
  `--confirm-destructive` + a non-empty `--audit` note + an env var. The check is a **rule-based
  denylist, not a sandbox** — it catches `rm -fr`, `dd of=`, `… | bash`, `sudo`, but not
  `xargs rm -rf` or aliases. Both lists (caught / not caught) are test-pinned.
- **Video thumbnails are cached on disk**, keyed by path + size + mtime. Keying on path alone means
  a re-encoded file with the same name keeps showing the old frame — the classic thumbnail-cache bug.
- **Releases are minisign-signed, and the workflow fails closed**: no key in secrets means no release,
  rather than silently publishing an unsigned binary.

What it doesn't do (honestly):

- Linux only; the default build links `libalpm`, so full package-management features are Arch-specific
  (`--no-default-features` gives you the core toolbox without it).
- The zsh integration is generated but I have no zsh to test in; bash and fish are tested end-to-end
  in a PTY.
- Task queue is serial for background jobs.
- No tagged release yet — the multi-arch release pipeline has only been verified locally.

⟨GitHub link⟩ / MIT. Happy to answer anything about the shell-widget protocol or the importer's
degrade rules.

---

## Notes for posting

- Post the **first comment** yourself with the install instructions and the limitations list; don't
  put them in the body (body should stay ~200 words).
- Don't say "simple" or "just"; HN will find the edge case.
- If someone asks "why not fzf/navi/atuin?", answer concretely: those pick commands, this one
  also builds the arguments (forms) and keeps the output history — and the Ctrl-G picker is
  intentionally the *same* recipe list, so you use one thing everywhere.
