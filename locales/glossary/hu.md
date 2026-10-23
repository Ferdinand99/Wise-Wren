# Wise Wren – glossary (Hungarian, `hu`)

Draft: not yet reviewed by native speakers.

Wren's own strings are written from English with these words. Core strings shared with Vigilant Vole come from
vole-kit and follow [Vigilant Vole's glossary](https://github.com/Ferdinand99/Vigilant-Vole/blob/main/locales/glossary/hu.md).

## Style
- Register: impersonal / infinitive, no "te", no stiff formal address. Buttons: noun forms as in common Hungarian
  software ("Mentés", "Törlés"); "Adja meg…" for prompts.
- Quotation marks: „ ” (low-high). Keep `…`; dashes as in English (— –).
- Sentence case, lowercase month and weekday names. Hungarian nouns stay singular after numerals ("3 szerver"), so
  plural keys have only `one` and `other`, both singular.
- Avoid suffixes on `{placeholders}`; rephrase.
- Kept in English: Docker, Compose, Unraid, `agent.toml`, `WW_SETUP_TOKEN`, `WW_PUBLIC_URL`, `docker run`, JSON, webhook, and other product, protocol and
  provider names.

## Terms

| English | Magyar | Note |
|---|---|---|
| update (noun) | frissítés |  |
| update (verb) | frissíteni |  |
| target | cél | a container or Compose stack Wren updates; VV: „cél” |
| agent | ügynök | VV: „ügynök”, „távoli ügynök” |
| server | szerver |  |
| rollback / roll back | visszaállítás / visszaállítani |  |
| snapshot | pillanatkép |  |
| maintenance window | karbantartási ablak | VV |
| notification channel | értesítési csatorna | VV |
| token / agent token | token / ügynöktoken | VV: „API-token” |
| revoke | visszavonni | kit: „Visszavonás” |
| online / offline | online / offline | VV: „Online”; kit: „Offline” |
| waiting for the agent | várakozás az ügynökre | VV: „Várakozás az első csatlakozásra” |
| overview | áttekintés |  |
| host name | gépnév |  |
| operating system | operációs rendszer | as Kestrel |
| Docker / Compose / Unraid | Docker / Compose / Unraid | unchanged; product names; „Docker-socket”, „Unraidon” |
| auto window | automatikus ablak | per-server window in which automatic updates may run |
| scheduled update | ütemezett frissítés | an update approved to run at a chosen time |
| monitor (Vigilant Vole) | monitor | VV: „monitor” |
