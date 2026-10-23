# Wise Wren – glossary (Arabic, `ar`)

Draft: not yet reviewed by native speakers.

Wren's own strings are written from English with these words. Core strings shared with Vigilant Vole come from
vole-kit and follow [Vigilant Vole's glossary](https://github.com/Ferdinand99/Vigilant-Vole/blob/main/locales/glossary/ar.md).

## Style
- Right-to-left. Modern Standard Arabic, neutral. Nouns and the masculine singular imperative ("أدخل", "اختر"), or
  impersonal phrasing. Never formal or stiff.
- Quotation marks: « » for names and quoted text. Punctuation: ، ؛ ؟ . Digits: Western digits (0-9) everywhere.
  No tatweel.
- Plurals: all six CLDR categories (zero, one, two, few 3–10, many 11–99, other).
- Kept in Latin script: Docker, Compose, Unraid, `agent.toml`, `WW_SETUP_TOKEN`, `WW_PUBLIC_URL`, `docker run`, JSON, webhook, product names, protocols,
  URLs, file names and code. Wrap them in `<bdi>` in templates when they sit next to Arabic text.

## Terms

| English | العربية | Note |
|---|---|---|
| update (noun) | تحديث (ج. تحديثات) |  |
| update (verb) | تحديث |  |
| target | الهدف | a container or Compose stack Wren updates; VV: «الهدف» |
| agent | وكيل (ج. وكلاء) | VV: «وكيل بعيد» |
| server | خادم (ج. خوادم) | VV: «خادم» |
| rollback / roll back | التراجع / التراجع إلى الإصدار السابق |  |
| snapshot | لقطة |  |
| maintenance window | نافذة صيانة | VV |
| notification channel | قناة إشعارات / قناة | VV |
| token / agent token | رمز / رمز الوكيل | VV: «رمز API» |
| revoke | إلغاء | kit: «إلغاء الرمز» |
| online / offline | متصل / غير متصل | VV: «متصل»; kit: «غير متصل» |
| waiting for the agent | في انتظار الوكيل | VV: «في انتظار أول اتصال» |
| overview | نظرة عامة |  |
| host name | اسم المضيف | VV: «مضيف» |
| operating system | نظام التشغيل | as Kestrel |
| Docker / Compose / Unraid | Docker / Compose / Unraid | unchanged; product names; «مقبس Docker» |
| auto window | نافذة التحديث التلقائي | per-server window in which automatic updates may run |
| scheduled update | تحديث مجدول | an update approved to run at a chosen time |
| monitor (Vigilant Vole) | مراقب | VV: «المراقبات» |
