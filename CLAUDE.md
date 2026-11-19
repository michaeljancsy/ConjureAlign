# ConjureAlign

@~/.claude/audio-work.md

ConjureAlign is a plug-in that time-aligns a mic (the main input) to a reference mic (the
"Reference" sidechain input). How it's built, tested and released, and how it works inside:
`README.md` and the pages in `docs/` (design, reporting, audio-unit, upstream-issues, testing,
release, windows); read the one for the area you touch. The rules for every ConjureDSP plug-in:
`~/Projects/ConjureDSP/architecture.md`. How the sessions work together:
`~/Projects/ConjureDSP/how-we-work.md`.

## Rules

- Never message, reply to or draft replies for users, issue reporters or upstream projects. Our
  own issues, claim comments and PRs are fine. Workers post nothing but their PR.
- The repository is public. Keep analytics numbers, install IDs and Sentry details out of issues,
  PRs, commits and docs.
- `docs/upstream/*.md` are issue drafts for Michael. Never file or post them.
- Anything that loads the plug-in on this Mac (auval, pluginval, clap-validator, Plug-in Bench,
  `tests/au_host_test.m`) runs with `HOME` set to a scratch folder that declines both questions,
  and with the reporting addresses pointing nowhere, as `docs/testing.md` describes. With the
  real `HOME`, this Mac sends real analytics, crash and update traffic.
- Never run `plugin-bench scan`, even with `--folder`, without asking: it loads other vendors'
  Audio Units.
- Before shipping a new property more revealing than what is already sent, check whether the
  README's Privacy section needs to change.
