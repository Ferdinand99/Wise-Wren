# Translation glossaries

One file per interface language, named after its code (`de.md`, `pt-BR.md`, `zh-TW.md` …). Each file fixes the
words Wise Wren uses for its own concepts – update, target, agent, server, rollback, snapshot, maintenance window
and so on – and the tone of the language, so that every string says the same thing the same way. They play the same
role as [Vigilant Vole's glossaries](https://github.com/Ferdinand99/Vigilant-Vole/tree/main/locales/glossary) and
Keen Kestrel's.

All files are drafts that native speakers have not reviewed yet. Corrections are very welcome: change the term in
the glossary and in every string that uses it, in the same pull request.

## How they are used

- **Wren's own strings** (servers, agents, agent tokens, updates, notification messages) are written from English,
  one language at a time, with the words in these files.
- **Core strings** (sign-in, setup, users, API tokens, Security, General, notification channels, the layout) come
  from vole-kit and are translated there. They follow Vigilant Vole's glossary for the language, not these files.
  Where a concept appears in both products – agent, token, maintenance, notification, online / offline – the word
  here is the one Vigilant Vole already uses, and each file notes it ("VV: …").
- The writing rules are the family's: short, calm Apple-style copy; sentence case where the language uses it; the
  language's own quotation marks; an informal or neutral form of address, never stiff; no exclamation marks unless
  the English has one.

## Words that stay in English everywhere

| Term | Why |
|---|---|
| Docker, Compose (Docker Compose) | Product names; people search for them as written. |
| Unraid | Product name. |
| `agent.toml`, `WW_SETUP_TOKEN`, `WW_PUBLIC_URL`, `docker run` | File, variable and command names, always written as code. |

Other product names (Wren, ntfy, Slack, Linux) and server names such as "Tower" are never translated either.
