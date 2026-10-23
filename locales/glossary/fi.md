# Wise Wren – glossary (Finnish, `fi`)

Draft: not yet reviewed by native speakers.

Wren's own strings are written from English with these words. Core strings shared with Vigilant Vole come from
vole-kit and follow [Vigilant Vole's glossary](https://github.com/Ferdinand99/Vigilant-Vole/blob/main/locales/glossary/fi.md).

## Style
- Address: neutral. Imperatives and infinitives ("Valitse palvelin", "Lisää kanava"), never "te"; "sinä" forms only
  when unavoidable ("sinun").
- Quotation marks: ”…” (same mark on both sides).
- Capitalisation: sentence case; no capitalised nouns in the middle of a sentence.
- Compounds are written as one word (huoltoikkuna, agenttitunnus); long labels are shortened.
- Numerals take the partitive singular after n > 1 in UI counts, which equals the `other` form; `one` = exactly 1.
  Avoid case endings on `{placeholders}`; rephrase instead.
- Kept in English: Docker, Compose, Unraid, `agent.toml`, `WW_SETUP_TOKEN`, `WW_PUBLIC_URL`, `docker run`, JSON, webhook.

## Terms

| English | Suomi | Note |
|---|---|---|
| update (noun) | päivitys |  |
| update (verb) | päivittää |  |
| target | kohde | a container or Compose stack Wren updates; VV: ”kohde” |
| agent | agentti | VV: ”etäagentti” |
| server | palvelin |  |
| rollback / roll back | palautus / palauttaa edelliseen versioon |  |
| snapshot | tilannevedos |  |
| maintenance window | huoltoikkuna | VV |
| notification channel | ilmoituskanava / kanava | VV |
| token / agent token | tunnus / agentin tunnus | VV: ”tunnus”, ”API-tunnus” |
| revoke | perua | kit: ”Peru” |
| online / offline | verkossa / ei yhteyttä | VV: ”Verkossa”; kit: ”Ei yhteyttä” |
| waiting for the agent | odottaa agenttia | VV: ”Odottaa ensimmäistä yhteyttä” |
| overview | yleiskatsaus |  |
| host name | isäntänimi |  |
| operating system | käyttöjärjestelmä | as Kestrel |
| Docker / Compose / Unraid | Docker / Compose / Unraid | unchanged; product names; may take case endings (”Unraidissa”, ”Dockerin soketti”) |
| auto window | automaattiikkuna | per-server window in which automatic updates may run |
| scheduled update | ajastettu päivitys | an update approved to run at a chosen time |
| monitor (Vigilant Vole) | seurain | VV: «seurain» |
