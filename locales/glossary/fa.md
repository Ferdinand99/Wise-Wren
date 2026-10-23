# Wise Wren – glossary (Persian, `fa`)

Draft: not yet reviewed by native speakers.

Wren's own strings are written from English with these words. Core strings shared with Vigilant Vole come from
vole-kit and follow [Vigilant Vole's glossary](https://github.com/Ferdinand99/Vigilant-Vole/blob/main/locales/glossary/fa.md).

## Style
- Right-to-left. Standard Iranian Persian, neutral polite imperative («وارد کنید», «انتخاب کنید»); no slang, no «شما»
  unless needed.
- Quotes « » for quoted names and values. Punctuation ، ؛ ؟ ; keep … and the em dash.
- Letters ک ی (never ك ي). Use ZWNJ (U+200C) in می‌شود, ‌ها plurals, etc.
- Literal digits in prose are Persian digits ۰-۹; code-like values (versions, URLs, image tags) stay as in English.
- No sentence capitalisation. Plurals: only `one` and `other`, both with a singular noun after the number.
- Kept in Latin script: Docker, Compose, Unraid, `agent.toml`, `WW_SETUP_TOKEN`, `WW_PUBLIC_URL`, `docker run`, JSON, and other product, protocol and
  technical terms. Wrap them in `<bdi>` in templates.

## Terms

| English | فارسی | Note |
|---|---|---|
| update (noun) | به‌روزرسانی |  |
| update (verb) | به‌روزرسانی کردن |  |
| target | مقصد | a container or Compose stack Wren updates; VV: «مقصد» |
| agent | عامل | VV: «عامل راه‌دور» |
| server | سرور |  |
| rollback / roll back | بازگردانی / بازگرداندن به نسخهٔ قبلی |  |
| snapshot | اسنپ‌شات |  |
| maintenance window | بازهٔ نگهداری | VV |
| notification channel | کانال اطلاع‌رسانی | VV |
| token / agent token | توکن / توکن عامل | VV: «توکن» |
| revoke | ابطال | VV/kit: «ابطال» |
| online / offline | آنلاین / آفلاین | VV: «آنلاین»; kit: «آفلاین» |
| waiting for the agent | در انتظار عامل | VV: «در انتظار اولین اتصال» |
| overview | نمای کلی |  |
| host name | نام میزبان | VV: «میزبان» |
| operating system | سیستم‌عامل | as Kestrel |
| Docker / Compose / Unraid | Docker / Compose / Unraid | unchanged; product names; «سوکت Docker» |
| auto window | بازهٔ خودکار | per-server window in which automatic updates may run |
| scheduled update | به‌روزرسانی زمان‌بندی‌شده | an update approved to run at a chosen time |
| monitor (Vigilant Vole) | مانیتور | VV: «مانیتور» |
