# Wise Wren – glossary (Hebrew, `he`)

Draft: not yet reviewed by native speakers.

Wren's own strings are written from English with these words. Core strings shared with Vigilant Vole come from
vole-kit and follow [Vigilant Vole's glossary](https://github.com/Ferdinand99/Vigilant-Vole/blob/main/locales/glossary/he.md).

## Style
- Right-to-left. Modern Hebrew, gender-neutral: infinitive (להזין, לשמור) for buttons and actions; impersonal or noun
  phrases for sentences; masculine singular only when unavoidable.
- Button labels as nouns where VV does so: Save = שמירה, Cancel = ביטול, Delete = מחיקה.
- Quotation marks “ ” as in English; no niqqud; Western digits; abbreviations with geresh ׳.
- No capitals. Mixed-script strings keep Latin terms as written; wrap them in `<bdi>` in templates.
- Plurals: one (exactly 1), two (exactly 2), other.
- Kept in Latin script: Docker, Compose, Unraid, `agent.toml`, `WW_SETUP_TOKEN`, `WW_PUBLIC_URL`, `docker run`, JSON, Webhook, and other technical terms
  and product names.

## Terms

| English | עברית | Note |
|---|---|---|
| update (noun) | עדכון |  |
| update (verb) | לעדכן |  |
| target | יעד | a container or Compose stack Wren updates; VV: “יעד” |
| agent | סוכן | VV: “סוכן מרוחק” |
| server | שרת |  |
| rollback / roll back | שחזור לגרסה קודמת / לשחזר |  |
| snapshot | תמונת מצב |  |
| maintenance window | חלון תחזוקה | VV |
| notification channel | ערוץ התראות | VV |
| token / agent token | אסימון / אסימון הסוכן | VV: “אסימון API” |
| revoke | לבטל | VV: “לבטל”; button “ביטול האסימון” |
| online / offline | מחובר / לא מחובר | VV: “מחובר”; kit: “לא מחובר” |
| waiting for the agent | ממתין לסוכן | VV: “ממתין לחיבור הראשון” |
| overview | סקירה כללית |  |
| host name | שם מארח | VV: “מארח” |
| operating system | מערכת הפעלה | as Kestrel |
| Docker / Compose / Unraid | Docker / Compose / Unraid | unchanged; product names; with maqaf: “ב־Unraid” |
